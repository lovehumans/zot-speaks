//! Phonon model loading and per-request option building.
//!
//! We resolve a *local* cache directory (never `hf://…`) so `ptts` stays free of the `hub`
//! feature (no TLS/download crates), matching "local-only by design".

use std::path::{Path, PathBuf};

use ptts::checkpoint::{Checkpoint, ResolveOptions};
use ptts::preprocess::Lang;
use ptts::synth::{DeviceKind, Quant, SpeechOptions, Synth};

use crate::cache;
use crate::settings::Settings;
use crate::voices::{self, VoiceInfo};

const MODEL_REPO: &str = "gradium/phonon-31m-en";

/// The spoken language this model (`phonon-31m-en`, built with `Lang::En`) supports. Catalog
/// voices whose `language` differs are neither offered in the picker nor registered, so a
/// pre-populated foreign voice can never be fed to the English model.
pub const MODEL_LANGUAGE: &str = "en";

/// Locate the already-cached `gradium/phonon-31m-en` snapshot under the HF cache. Candidates
/// follow the downloaders' precedence (see `cache::hf_hub`), so a model fetched by `ptts`,
/// `huggingface-cli`, or `hf-hub` is found regardless of a custom `HF_*` location.
fn local_snapshot(hub: &Path) -> Option<PathBuf> {
    let snapshots = cache::repo_dir(hub, MODEL_REPO).join("snapshots");
    let Ok(dirs) = std::fs::read_dir(&snapshots) else {
        return None;
    };
    for entry in dirs.flatten() {
        let dir = entry.path();
        if dir.join("config.json").is_file() && dir.join("model.safetensors").is_file() {
            return Some(dir);
        }
    }
    None
}

/// Load the model once and register every locally-cached catalog voice by name.
pub fn load_synth(catalog: &[VoiceInfo]) -> Result<Synth, String> {
    let hub = cache::hf_hub();
    let dir = local_snapshot(&hub).ok_or_else(|| {
        format!(
            "Phonon 31m-en snapshot not found under {}; bootstrap once: `just fetch-model` \
             (needs the ptts CLI), or by hand `ptts --voice Miles \"warm up\"` / \
             `huggingface-cli download {MODEL_REPO}`",
            cache::repo_dir(&hub, MODEL_REPO).display()
        )
    })?;

    // Quant::F32 selects `model.safetensors`, matching the cached blob.
    let checkpoint = Checkpoint::resolve(
        &dir,
        ResolveOptions {
            quant: Quant::F32,
            weights: None,
        },
    )
    .map_err(|e| e.to_string())?;
    let mut synth = checkpoint
        .builder(Lang::En)
        .device(DeviceKind::Cpu)
        .build()
        .map_err(|e| e.to_string())?;

    checkpoint.register_voices(&mut synth); // any voices baked beside the model
    let added = voices::register(&mut synth, catalog);
    eprintln!(
        "zot-speaks: voices available: {} ({} from catalog)",
        synth.voices().join(", "),
        added.len()
    );
    Ok(synth)
}

/// Build per-request options from settings. The second return value is the voice name that will
/// actually be used (`None` = model default); callers compare it with the requested voice to warn
/// when a selected voice is no longer registered locally.
pub fn build_options(synth: &Synth, settings: &Settings) -> (SpeechOptions, Option<String>) {
    let mut options = SpeechOptions::default();
    let mut used = None;
    if !settings.voice.is_empty() && synth.voices().iter().any(|voice| voice == &settings.voice) {
        options.voice = Some(settings.voice.clone());
        used = Some(settings.voice.clone());
    }
    options.temperature = Some(settings.temperature);
    (options, used)
}
