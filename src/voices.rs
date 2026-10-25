//! Voice catalog discovery, fully offline.
//!
//! The `gradium/phonon-voices` cache holds a `voices.jsonl` catalog (name, voice_id, filename,
//! language, locale, gender, age, description) plus a `voices/` folder that only contains the
//! embedding files actually fetched so far. We read the catalog and mark each voice available
//! only when its `.safetensors` is present locally, then register those as named voices. Nothing
//! here downloads, and the scan happens once, at startup: a voice fetched later with
//! `./ptts --voice Name …` joins the picker on the next zot start.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ptts::synth::Synth;
use serde_json::Value;

use crate::cache;

const VOICE_REPO: &str = "gradium/phonon-voices";
const CATALOG_FILE: &str = "voices.jsonl";

#[derive(Clone, Debug)]
pub struct VoiceInfo {
    pub name: String,
    pub id: String,
    pub path: PathBuf,
    pub available: bool,
    // Retained from the catalog for completeness (the picker shows `locale`, which already encodes
    // the language). Language filtering happens during `catalog`, so this field is informational.
    #[allow(dead_code)]
    pub language: Option<String>,
    pub locale: Option<String>,
    pub gender: Option<String>,
    pub age: Option<String>,
    pub description: Option<String>,
}

impl VoiceInfo {
    /// A compact one-line descriptor for the picker, e.g. `male · Young Adult · en-us`.
    pub fn tag(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for value in [&self.gender, &self.age, &self.locale] {
            if let Some(part) = value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                parts.push(part.to_string());
            }
        }
        parts.join(" · ")
    }
}

/// Read the cached voice catalog, keeping only voices the model can speak (`language ==
/// model_language`). Returns them with availability set, ordered available-first then by name.
pub fn catalog(hub: &Path, model_language: &str) -> Vec<VoiceInfo> {
    let snapshots = cache::repo_dir(hub, VOICE_REPO).join("snapshots");
    let mut found: HashMap<String, VoiceInfo> = HashMap::new();

    let Ok(dirs) = std::fs::read_dir(&snapshots) else {
        eprintln!("zot-speaks: no voice catalog under {}", snapshots.display());
        return Vec::new();
    };
    for entry in dirs.flatten() {
        let snapshot = entry.path();
        // read_to_string follows the snapshot symlink to the blob.
        let Ok(text) = std::fs::read_to_string(snapshot.join(CATALOG_FILE)) else {
            continue;
        };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(object) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(name) = object.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(filename) = object.get("filename").and_then(Value::as_str) else {
                continue;
            };
            // Offer only voices this model can speak: skip a different (or missing) language.
            let language = field(&object, "language");
            let speaks = language
                .as_deref()
                .map(|l| l.eq_ignore_ascii_case(model_language))
                .unwrap_or(false);
            if !speaks {
                continue;
            }
            let path = snapshot.join(filename);
            let available = std::fs::metadata(&path).is_ok(); // follows the symlink; broken => false
            let id = object
                .get("voice_id")
                .and_then(Value::as_str)
                .unwrap_or(filename)
                .to_string();
            let voice = VoiceInfo {
                name: name.to_string(),
                id: id.clone(),
                path,
                available,
                language,
                locale: field(&object, "locale"),
                gender: field(&object, "gender"),
                age: field(&object, "age"),
                description: field(&object, "description"),
            };
            // Dedupe by voice id, preferring a snapshot where the file is present.
            if let Some(existing) = found.get_mut(&voice.id) {
                if !existing.available {
                    *existing = voice;
                }
            } else {
                found.insert(voice.id.clone(), voice);
            }
        }
    }

    let mut voices: Vec<VoiceInfo> = found.into_values().collect();
    voices.sort_by(|a, b| b.available.cmp(&a.available).then(a.name.cmp(&b.name)));
    voices
}

/// Register every locally-present catalog voice under its human name. Returns the names that
/// registered, in catalog order.
pub fn register(tts: &mut Synth, voices: &[VoiceInfo]) -> Vec<String> {
    let mut registered = Vec::new();
    for voice in voices.iter().filter(|v| v.available) {
        // `add_voice_file`'s path argument is `&std::path::Path` (ptts aliases it internally as
        // `FsPath`); a `&PathBuf` coerces to it directly.
        match tts.add_voice_file(&voice.name, &voice.path) {
            Ok(()) => registered.push(voice.name.clone()),
            Err(e) => eprintln!("zot-speaks: could not register voice {}: {e}", voice.name),
        }
    }
    registered
}

fn field(object: &Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    // A throwaway hub holding one snapshot with an en voice present, an en voice missing, and a
    // French voice present. Written under the temp dir; never touches the real HF cache.
    fn temp_hub() -> PathBuf {
        let tag = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("zot-speaks-voices-{}-{tag}", std::process::id()));
        let snap = dir
            .join("models--gradium--phonon-voices")
            .join("snapshots")
            .join("testsha");
        fs::create_dir_all(snap.join("voices")).unwrap();
        let jsonl = concat!(
            "{\"filename\":\"voices/en_present.safetensors\",\"name\":\"Miles\",\"voice_id\":\"en_present\",\"language\":\"en\",\"locale\":\"en-us\",\"gender\":\"male\",\"age\":\"Adult\",\"description\":\"reporter\"}\n",
            "{\"filename\":\"voices/en_missing.safetensors\",\"name\":\"Zoey\",\"voice_id\":\"en_missing\",\"language\":\"en\",\"locale\":\"en-us\",\"gender\":\"female\",\"age\":\"Adult\",\"description\":\"gen z\"}\n",
            "{\"filename\":\"voices/fr_present.safetensors\",\"name\":\"Noemie\",\"voice_id\":\"fr_present\",\"language\":\"fr\",\"locale\":\"fr-fr\",\"gender\":\"female\",\"age\":\"Adult\",\"description\":\"parisienne\"}\n",
        );
        fs::write(snap.join("voices.jsonl"), jsonl).unwrap();
        fs::write(snap.join("voices").join("en_present.safetensors"), b"x").unwrap();
        fs::write(snap.join("voices").join("fr_present.safetensors"), b"x").unwrap();
        dir
    }

    #[test]
    fn catalog_keeps_only_the_model_language_and_marks_availability() {
        let dir = temp_hub();
        let en = catalog(&dir, "en");
        // Only the two English voices are offered; the French one is filtered out even though it
        // is downloaded, so a pre-populated foreign voice never reaches the picker or registration.
        assert_eq!(en.len(), 2, "foreign voices must be filtered out");
        assert!(en.iter().any(|v| v.name == "Miles" && v.available));
        assert!(en.iter().any(|v| v.name == "Zoey" && !v.available));
        assert!(!en.iter().any(|v| v.name == "Noemie"));
        // Available-first ordering: the ready voice precedes the not-downloaded one.
        assert!(en[0].available, "ready voices sort first");
        assert!(!en[en.len() - 1].available);

        let fr = catalog(&dir, "fr");
        assert_eq!(fr.len(), 1);
        assert_eq!(fr[0].name, "Noemie");

        let _ = fs::remove_dir_all(&dir); // tidy scratch; not the real cache
    }

    #[test]
    fn catalog_of_unknown_language_is_empty() {
        let dir = temp_hub();
        assert!(catalog(&dir, "zz").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
