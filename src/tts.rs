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
///
/// The inference pool is sized here, before anything reaches a worker: `ptts` fixes its pool width
/// at first use, so a size set after the first request is too late (see `apply_ptts_env`).
pub fn load_synth(catalog: &[VoiceInfo]) -> Result<Synth, String> {
    apply_ptts_env();
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

/// Size the inference worker pool, once, before the model load.
///
/// The variable that actually moves this pipeline is `RAYON_NUM_THREADS`: `ptts` computes through
/// `xn`, whose pool width is read from that variable and then fixed at first use
/// (`xn-0.2.12/src/utils.rs`, `xn-0.2.12/src/threadpool.rs`). `OMP_NUM_THREADS` — what this
/// function used to set alone — is not read by that pool at all, so a `ZOT_TTS_THREADS` pin written
/// into it changed nothing (measured 2026-10-28 and again 2026-10-29: identical timings for
/// `OMP_NUM_THREADS=8` and `16` in `examples/stream_smoke.rs`). The `ort` variables stay as they
/// are; nothing in the current dependency tree is `ort`, and they are harmless.
///
/// Default is one worker per *physical* core. Measured on an 8-core / 16-thread box
/// (`examples/stream_smoke.rs`, one 8.00 s utterance, 3 reps per point, 2026-10-29; the 2026-10-28
/// run gave the same shape, and `agents/history/2026-10-29-inference-threads-and-quantization.md`
/// holds both tables):
/// `RAYON_NUM_THREADS=16` TTFB 49-52 ms, total 923-959 ms; `8` TTFB 29-32 ms, total 505-515 ms;
/// `4` TTFB 33-39 ms, total 547-597 ms; `2` TTFB 83-102 ms, total 801-837 ms. A hyperthread adds
/// dispatch and no throughput to a vectorised gemm, so one worker per logical CPU is the worst
/// point on an SMT box — 1.8x slower than physical cores here. `ZOT_TTS_THREADS=<n>` pins the
/// answer for anyone who knows their box better; an unparsable pin falls back to the rule.
fn apply_ptts_env() {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let physical = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .as_deref()
        .and_then(physical_cores_from_cpuinfo);
    let pin = std::env::var("ZOT_TTS_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok());
    let threads = worker_threads(pin, logical, physical).to_string();
    std::env::set_var("RAYON_NUM_THREADS", &threads);
    std::env::set_var("OMP_NUM_THREADS", &threads);
    std::env::set_var("ORT_DISABLE_THREAD_POOLS", "1");
    std::env::set_var("ORT_SESSION_INTRA_OP_THREAD_POOL_SIZE", "0");
}

/// The thread rule, kept apart from reading the environment so it can be tested outright:
/// a positive pin wins, else physical cores clamped to what the box calls logical, else logical.
fn worker_threads(pin: Option<usize>, logical: usize, physical: Option<usize>) -> usize {
    if let Some(pinned) = pin.filter(|n| *n > 0) {
        return pinned;
    }
    let logical = logical.max(1);
    physical
        .map(|cores| cores.clamp(1, logical))
        .unwrap_or(logical)
}

/// Physical cores as Linux's `/proc/cpuinfo` describes them: the distinct (`physical id`,
/// `core id`) pairs, so SMT siblings count once and sockets add up. `None` where the file names no
/// cores at all (some ARM and VM kernels), which is the caller's cue to fall back to logical CPUs.
fn physical_cores_from_cpuinfo(text: &str) -> Option<usize> {
    let mut seen = std::collections::HashSet::new();
    let mut package = 0usize;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(number) = value.trim().parse::<usize>() else {
            continue;
        };
        match key.trim() {
            "physical id" => package = number,
            "core id" => {
                seen.insert((package, number));
            }
            _ => {}
        }
    }
    (!seen.is_empty()).then_some(seen.len())
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

    /// The worker pool is a measured knob (see `apply_ptts_env`), so the rule is spelled out here
    /// instead of being left to whatever the box happens to report.
    #[test]
    fn worker_threads_pins_first_and_physical_cores_otherwise() {
        struct Case {
            pin: Option<usize>,
            logical: usize,
            physical: Option<usize>,
            want: usize,
            why: &'static str,
        }
        const CASES: &[Case] = &[
            Case {
                pin: Some(3),
                logical: 16,
                physical: Some(8),
                want: 3,
                why: "a pin wins, however small",
            },
            Case {
                pin: Some(32),
                logical: 16,
                physical: Some(8),
                want: 32,
                why: "a pin wins, however large",
            },
            Case {
                pin: Some(0),
                logical: 16,
                physical: Some(8),
                want: 8,
                why: "a pin of zero is not a pin",
            },
            Case {
                pin: None,
                logical: 16,
                physical: Some(8),
                want: 8,
                why: "SMT box: physical cores, not logical CPUs",
            },
            Case {
                pin: None,
                logical: 8,
                physical: Some(8),
                want: 8,
                why: "no SMT: nothing changes",
            },
            Case {
                pin: None,
                logical: 16,
                physical: None,
                want: 16,
                why: "no core info: logical CPUs",
            },
            Case {
                pin: None,
                logical: 4,
                physical: Some(8),
                want: 4,
                why: "physical cores above the logical count are clamped",
            },
            Case {
                pin: None,
                logical: 0,
                physical: None,
                want: 1,
                why: "a box that reports nothing still gets one worker",
            },
        ];
        for case in CASES {
            assert_eq!(
                worker_threads(case.pin, case.logical, case.physical),
                case.want,
                "{}",
                case.why
            );
        }
    }

    /// `/proc/cpuinfo` carries one block per logical CPU, so counting distinct (`physical id`,
    /// `core id`) pairs is what turns 16 blocks into 8 cores and keeps two sockets additive. A
    /// kernel that names no core ids gets `None`, which is the caller's cue to fall back.
    #[test]
    fn physical_cores_count_distinct_package_and_core_ids() {
        let cpu = |package: usize, core: usize| {
            format!("processor : x\nphysical id : {package}\ncpu cores : n/a\nsiblings : n/a\ncore id : {core}\n")
        };
        let smt: String = (0..8)
            .flat_map(|core| [cpu(0, core), cpu(0, core)])
            .collect();
        let no_smt: String = (0..4).map(|core| cpu(0, core)).collect();
        let sockets: String = (0..2)
            .flat_map(|package| (0..4).map(move |core| cpu(package, core)))
            .collect();
        let cases: &[(&str, Option<usize>, &str)] = &[
            (
                &smt,
                Some(8),
                "8 cores / 16 threads, as this dev box reports itself",
            ),
            (&no_smt, Some(4), "4 cores, no SMT"),
            (&sockets, Some(8), "2 sockets x 4 cores"),
            ("", None, "an empty file says nothing about cores"),
            (
                "processor : 0\nmodel name : ARMv7 Processor rev 0\nFeatures : fp asimd",
                None,
                "a kernel that names no core ids",
            ),
        ];
        for (text, want, why) in cases {
            assert_eq!(physical_cores_from_cpuinfo(text), *want, "{why}");
        }
    }
}
