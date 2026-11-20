//! Phonon model loading and per-request option building.
//!
//! We resolve a *local* cache directory (never `hf://…`) so `ptts` stays free of the `hub`
//! feature (no TLS/download crates), matching "local-only by design".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ptts::checkpoint::{Checkpoint, ResolveOptions};
use ptts::comp::{GainConfig, LimiterConfig, ModuleConfig};
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

/// What the settings mean to ptts: the temperature dial, the level the volume dial asks for, and the
/// noise this utterance starts from. Split out from the voice lookup — which needs a loaded `Synth` —
/// so the part a test can hold stays testable.
fn speech_of(settings: &Settings, seed: u64) -> SpeechOptions {
    SpeechOptions::default()
        .temperature(settings.temperature)
        .seed(seed)
        .post_process(post_chain(settings))
}

/// How far below full scale the limiter holds every peak. A decibel and not zero, because the stream
/// is not finished when it leaves us: `atempo` resamples it inside the player and the system mixer
/// adds its own gain, and a wave already touching full scale has nothing left for either to overshoot
/// into.
const LIMITER_HEADROOM_DB: f32 = 1.0;

/// How far ahead of a peak the limiter looks. Its gain snaps down at once and releases slowly, so with
/// no lookahead the attenuation starts *on* the transient and the waveform steps where it should
/// glide; three milliseconds is shorter than the attack of a plosive, so the gain is already on its
/// way down when the peak arrives. What it costs is the utterance's first `lookahead × rate` samples —
/// 72 at 24 kHz — which are the zeros the delay line was primed with, and ptts trims them and flushes
/// the same number out at the end.
const LIMITER_LOOKAHEAD_MS: f32 = 3.0;

/// How long the limiter takes to let the level back up once a peak has passed. Speech is a succession
/// of peaks a few hundred milliseconds apart, so a release measured in tens of milliseconds is a pump
/// you hear breathing between words; a hundred leaves each peak its own dip and has the gain back
/// where it was before the next one arrives.
const LIMITER_RELEASE_MS: f32 = 100.0;

/// The volume dial as the synthesis sees it: the dial's gain, then a ceiling. Two stages, per
/// utterance, because the dial moves between them while the loaded model stays put — these are
/// descriptions, and ptts builds the chain from them at the model's own sample rate, runs it over each
/// streaming chunk, and hands the chunks out already at the level the dial asked for.
///
/// Before this, the level was applied by the writer that produced the bytes, which could only flatten
/// whatever came out above full scale (see `sink::sample_to_s16`). Flattened peaks are what the top of
/// the dial sounded like.
pub fn post_chain(settings: &Settings) -> Vec<ModuleConfig> {
    vec![
        ModuleConfig::Gain(GainConfig {
            gain_db: settings.gain_db(),
        }),
        ModuleConfig::Limiter(LimiterConfig {
            lookahead_ms: LIMITER_LOOKAHEAD_MS,
            headroom_db: LIMITER_HEADROOM_DB,
            release_ms: LIMITER_RELEASE_MS,
        }),
    ]
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
    use ptts::comp::{AudioModule, Chain, Gain};

    /// How far above the ceiling a limited signal is allowed to land, in the tests below: a tenth of a
    /// decibel. The window that decides a sample's gain starts one sample *after* the sample it governs,
    /// so a peak is caught by the energy around it rather than by itself, and the gain that governs it
    /// has already started releasing back up. Before this change the same samples landed on full scale
    /// and stayed there; a tenth of a decibel is the difference between those two stories.
    const CEILING_SLACK: f32 = 1e-2;

    /// The dial is no longer a multiplier the writer applies to the bytes: it is a gain stage and a
    /// ceiling, in that order, and the ceiling is the part that used to be a flattening clamp.
    #[test]
    fn the_volume_dial_becomes_gain_then_limiter() {
        let chain = post_chain(&Settings {
            volume: 1.0,
            ..Settings::default()
        });
        assert_eq!(chain.len(), 2, "the dial's gain, then the ceiling");
        match &chain[0] {
            ModuleConfig::Gain(gain) => assert!(
                (gain.gain_db - 6.0206).abs() < 1e-3,
                "100% is twice the level, which is +6 dB, not {}",
                gain.gain_db
            ),
            other => panic!("the first stage must be the gain, not {other:?}"),
        }
        match &chain[1] {
            ModuleConfig::Limiter(limiter) => assert_eq!(
                (
                    limiter.lookahead_ms,
                    limiter.headroom_db,
                    limiter.release_ms
                ),
                (
                    LIMITER_LOOKAHEAD_MS,
                    LIMITER_HEADROOM_DB,
                    LIMITER_RELEASE_MS
                ),
                "the ceiling is the one this module documents"
            ),
            other => panic!("the second stage must be the limiter, not {other:?}"),
        }
        // Every request carries it, including the previews the panel auditions.
        let options = speech_of(&Settings::default(), 3);
        assert!(
            options
                .post_process
                .as_ref()
                .is_some_and(|chain| chain.len() == 2),
            "an utterance built from settings carries the chain"
        );
    }

    /// `0%` has to stay silence, and nothing downstream of this chain multiplies the samples any more.
    /// `0 dB` is not a decibel silence can be expressed in, so this is the one rung that has to be
    /// checked where the multiplication happens.
    #[test]
    fn zero_percent_silences_through_the_gain_stage() {
        let ModuleConfig::Gain(gain) = &post_chain(&Settings {
            volume: 0.0,
            ..Settings::default()
        })[0] else {
            panic!("the first stage is the gain");
        };
        let mut samples = vec![0.5, -0.5, 1.0];
        Gain::new(gain.gain_db).process(&mut samples);
        assert_eq!(samples, vec![0.0, 0.0, 0.0], "0% is silence");
    }

    /// The claim the whole swap rests on, on a signal hot enough to have been flattened: a peak meets
    /// a ceiling and the wave keeps its shape, where the clamp used to pin it to full scale.
    ///
    /// One second of 1 kHz tone, at `0.3` of full scale except for a hundred milliseconds at `3.0` —
    /// four times full scale before the gain, twelve times after it at `100%`.
    #[test]
    fn a_peak_meets_a_ceiling_instead_of_a_flat_top() {
        let rate: u32 = 24_000;
        let raw: Vec<f32> = (0..rate)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let amplitude = if (0.05..0.15).contains(&t) { 3.0 } else { 0.3 };
                amplitude * (2.0 * std::f32::consts::PI * 1000.0 * t).sin()
            })
            .collect();

        let mut chain = Chain::from_modules(
            rate,
            &post_chain(&Settings {
                volume: 1.0,
                ..Settings::default()
            }),
        );
        let trimmed = chain.latency();
        assert_eq!(
            trimmed,
            (LIMITER_LOOKAHEAD_MS * 0.001 * rate as f32).round() as usize,
            "the lookahead is the delay the streaming path trims off the head"
        );
        let mut out = raw.clone();
        chain.process(&mut out);
        // What the streaming path hands out: the primed zeros off the head, so output index `i` is
        // input sample `i` at the gain the limiter had when it saw the lookahead window ahead of it.
        let out = &out[trimmed..];

        let ceiling = 10f32.powf(-LIMITER_HEADROOM_DB / 20.0);
        assert!(
            out.iter()
                .all(|sample| sample.abs() <= ceiling + CEILING_SLACK),
            "nothing may pass the ceiling: worst was {}",
            out.iter().fold(0.0f32, |worst, s| worst.max(s.abs()))
        );
        let peak = out.iter().fold(0.0f32, |best, s| best.max(s.abs()));
        assert!(
            peak > ceiling - CEILING_SLACK,
            "the ceiling has to be where the peaks actually land, and the peak here is {peak}"
        );

        // What the old clamp did with the same samples: twice the level, then everything above full
        // scale flattened against it. That is the difference, counted.
        let clamped: Vec<f32> = raw
            .iter()
            .take(out.len())
            .map(|s| (s * 2.0).clamp(-1.0, 1.0))
            .collect();
        let flattened = clamped.iter().filter(|s| **s >= 1.0).count();
        let limited = out
            .iter()
            .filter(|s| **s >= ceiling - CEILING_SLACK)
            .count();
        assert!(
            flattened > 1000,
            "this signal must be hot enough for the clamp to have visibly flattened it: {flattened}"
        );
        assert!(
            limited * 10 < flattened,
            "a limiter touches the ceiling on its way past it ({limited} samples), where the clamp \
             pinned the whole passage to it ({flattened})"
        );

        // Quiet passages are untouched, which is the other half of not pumping: the limiter acts
        // around peaks, not around everything it is handed.
        let quiet_index = 240; // 10 ms in, a hundred milliseconds clear of the hot passage
        assert!(
            (out[quiet_index] - raw[quiet_index]).abs() < 1e-6,
            "a quiet sample must pass through unchanged: {} vs {}",
            out[quiet_index],
            raw[quiet_index]
        );
    }

    /// One sample of four times full scale in the middle of a quiet stretch, twelve times that after
    /// the `100%` gain: the lookahead cannot see a spike from the sample it governs — the window it
    /// reads starts one sample later — so what catches it is the attenuation the limiter is still
    /// holding from the samples *after* it.
    #[test]
    fn an_isolated_spike_is_held_by_the_release_not_the_lookahead() {
        let rate = 24_000;
        let mut samples = spike_at(rate, 3.0);
        let mut chain = Chain::from_modules(rate, &post_chain(&dial_at(1.0)));
        let trimmed = chain.latency();
        chain.process(&mut samples);
        let ceiling = 10f32.powf(-LIMITER_HEADROOM_DB / 20.0);
        let worst = samples[trimmed..]
            .iter()
            .fold(0.0f32, |w, s| w.max(s.abs()));
        assert!(
            worst <= ceiling + CEILING_SLACK,
            "a spike twelve times full scale must still be stopped at the ceiling: it reached {worst}, \
             the ceiling is {ceiling}"
        );
    }

    /// The same spike, the same lookahead, a release of one millisecond instead of a hundred: by the
    /// time the spike itself is emitted the gain has already let go, and it lands above full scale —
    /// which is the writer's clamp being needed again, flattened against it. This is why
    /// `LIMITER_RELEASE_MS` is a hundred milliseconds and not something quicker.
    #[test]
    fn a_release_shorter_than_the_lookahead_lets_the_spike_through() {
        let rate = 24_000;
        let mut samples = spike_at(rate, 3.0);
        let mut modules = post_chain(&dial_at(1.0));
        modules[1] = ModuleConfig::Limiter(LimiterConfig {
            lookahead_ms: LIMITER_LOOKAHEAD_MS,
            headroom_db: LIMITER_HEADROOM_DB,
            release_ms: 1.0,
        });
        let mut chain = Chain::from_modules(rate, &modules);
        let trimmed = chain.latency();
        chain.process(&mut samples);
        let worst = samples[trimmed..]
            .iter()
            .fold(0.0f32, |w, s| w.max(s.abs()));
        assert!(
            worst > 1.0,
            "a release quicker than the lookahead must let the spike past full scale: {worst}"
        );
    }

    /// One second of `0.05` with one sample of `spike` in the middle.
    fn spike_at(rate: u32, spike: f32) -> Vec<f32> {
        let mut samples = vec![0.05; rate as usize];
        samples[(rate / 2) as usize] = spike;
        samples
    }

    /// Settings at a given rung of the volume dial.
    fn dial_at(volume: f32) -> Settings {
        Settings {
            volume,
            ..Settings::default()
        }
    }

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
