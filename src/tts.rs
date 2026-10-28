//! Phonon model loading and per-request option building.
//!
//! We resolve a *local* cache directory (never `hf://…`) so `ptts` stays free of the `hub`
//! feature (no TLS/download crates), matching "local-only by design".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Build per-request options from settings, for one utterance drawn from `seed`. The second return
/// value is the voice name that will actually be used (`None` = model default); callers compare it
/// with the requested voice to warn when a selected voice is no longer registered locally.
pub fn build_options(
    synth: &Synth,
    settings: &Settings,
    seed: u64,
) -> (SpeechOptions, Option<String>) {
    let mut options = speech_of(settings, seed);
    let mut used = None;
    if !settings.voice.is_empty() && synth.voices().iter().any(|voice| voice == &settings.voice) {
        options.voice = Some(settings.voice.clone());
        used = Some(settings.voice.clone());
    }
    (options, used)
}

/// What the settings mean to ptts: the temperature dial, and the noise this utterance starts from.
/// Split out from the voice lookup — which needs a loaded `Synth` — so the part a test can hold
/// stays testable.
fn speech_of(settings: &Settings, seed: u64) -> SpeechOptions {
    SpeechOptions::default()
        .temperature(settings.temperature)
        .seed(seed)
}

/// `ZOT_SPEAKS_SEED` pins the noise sequence: set it to a number and the utterances of a run are
/// reproducible in order. Unset, every utterance is a fresh take and so is every session.
pub const SEED_ENV: &str = "ZOT_SPEAKS_SEED";

/// The noise seeds, one per utterance: `base + n`.
///
/// ptts builds a fresh `NormalRng` per request from `SpeechOptions.seed`, and its own default is a
/// constant, so a request that leaves the seed unset replays the same noise every time: the same
/// sentence is literally the same take, however often it is spoken. Temperature scales that noise
/// rather than drawing new noise, so with a fixed seed the dial changes the amplitude of one take
/// instead of adding any variety — which is the one thing the Temperature row claims to do, out loud.
///
/// Counting from a base instead of drawing randomly keeps a pinned base useful: `ZOT_SPEAKS_SEED=42`
/// replays a run as utterance 42, 43, 44…
pub struct SeedPlan {
    base: u64,
    drawn: AtomicU64,
}

impl SeedPlan {
    /// The plan for this process: `ZOT_SPEAKS_SEED` when it names a number, otherwise a base drawn
    /// from the clock. A value that does not parse is reported and ignored — the reason to set it is
    /// reproducibility, and the usual reason it fails is a typo.
    pub fn from_env() -> Self {
        let raw = std::env::var(SEED_ENV).ok();
        if let Some(raw) = &raw {
            if parse_seed_base(raw).is_none() {
                eprintln!(
                    "zot-speaks: {SEED_ENV}={raw:?} is not a number; speaking from a fresh seed base \
                     instead (it wants e.g. {SEED_ENV}=42)"
                );
            }
        }
        Self::with_base(
            raw.as_deref()
                .and_then(parse_seed_base)
                .unwrap_or_else(clock_base),
        )
    }

    /// A plan whose first seed is `base`: the sequence held still, for tests and for anyone who wants
    /// to reproduce a run.
    pub fn with_base(base: u64) -> Self {
        Self {
            base,
            drawn: AtomicU64::new(0),
        }
    }

    /// The seed for the next utterance. Wrapping is left unguarded on purpose: at one utterance per
    /// second the counter needs 5.8e11 years to reach it.
    pub fn next(&self) -> u64 {
        self.base
            .wrapping_add(self.drawn.fetch_add(1, Ordering::Relaxed))
    }
}

/// A seed base that differs between processes: wall-clock nanoseconds, mixed with the process id so
/// two extensions started in the same instant still part company.
fn clock_base() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (u64::from(std::process::id())).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Parse `ZOT_SPEAKS_SEED`: a bare number, surrounding blanks allowed, nothing else. Negative and
/// out-of-range values are rejected rather than folded, because a silently folded one is not the
/// sequence the user asked to replay.
fn parse_seed_base(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Temperature row's whole claim rests on this: each utterance of the same sentence starts
    /// from new noise, so the takes differ in the way the setting says they should.
    #[test]
    fn each_utterance_draws_its_own_seed() {
        let plan = SeedPlan::with_base(7);
        assert_eq!([plan.next(), plan.next(), plan.next()], [7, 8, 9]);
    }

    /// `ZOT_SPEAKS_SEED` is worth having because a run can be replayed: same base, same order.
    #[test]
    fn a_pinned_seed_base_replays_the_same_sequence() {
        let first = SeedPlan::with_base(42);
        let again = SeedPlan::with_base(42);
        for expected in 42..46 {
            assert_eq!(first.next(), expected);
            assert_eq!(again.next(), expected);
        }
    }

    /// Counting past `u64::MAX` is the next seed, not an error.
    #[test]
    fn the_seed_sequence_wraps_instead_of_failing() {
        let plan = SeedPlan::with_base(u64::MAX);
        assert_eq!(plan.next(), u64::MAX);
        assert_eq!(plan.next(), 0);
    }

    /// The dial and this utterance's noise both have to reach ptts. A temperature that arrives
    /// without a seed is the bug this change exists to fix.
    #[test]
    fn the_options_carry_the_dial_and_this_utterance_noise() {
        let settings = Settings {
            temperature: 0.70,
            ..Settings::default()
        };
        let options = speech_of(&settings, 4242);
        assert_eq!(options.temperature, Some(0.70));
        assert_eq!(options.seed, Some(4242));
    }

    /// A pinned base has to mean exactly one number. Folding a typo into something else would
    /// replay a sequence the user never asked for.
    #[test]
    fn a_seed_base_must_be_a_bare_number() {
        assert_eq!(parse_seed_base("42"), Some(42));
        assert_eq!(parse_seed_base(" 42 "), Some(42));
        assert_eq!(parse_seed_base("0"), Some(0));
        for bad in ["", " ", "0.5", "-1", "42x", "0x2a", "18446744073709551616"] {
            assert_eq!(parse_seed_base(bad), None, "{bad:?} is not a seed");
        }
    }
}
