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

/// The model revision this artifact loads: `ptts` 0.7.1's own default, and the one `just fetch-model`
/// downloads. The figures published before 1.10.1 were measured against the revision ptts 0.4.0 pinned
/// (`8c64b55e…`), so the level and timing tables carry the revision they were taken from — see
/// `docs/Quantization.md`.
const MODEL_REVISION: &str = "92741c5b07d085ff840e26fea8ce43110b87852f";

/// The spoken language this model (`phonon-31m-en`, built with `Lang::En`) supports. Catalog
/// voices whose `language` differs are neither offered in the picker nor registered, so a
/// pre-populated foreign voice can never be fed to the English model.
pub const MODEL_LANGUAGE: &str = "en";

/// Locate the already-cached `gradium/phonon-31m-en` snapshot under the HF cache. Candidates
/// follow the downloaders' precedence (see `cache::hf_hub`), so a model fetched by `ptts`,
/// `huggingface-cli`, or `hf-hub` is found regardless of a custom `HF_*` location.
///
/// A cache can hold more than one snapshot of this model — an older bootstrap ran a `ptts` whose
/// default revision was a different commit. The named revision is the answer; a cache that does not
/// have it falls back to its other snapshot, sorted so the same cache gives the same answer twice, and
/// `load_synth` says out loud that it did.
struct Snapshot {
    dir: PathBuf,
    /// `false` when this is the cache's offering rather than the revision the artifact names.
    pinned: bool,
}

fn local_snapshot(hub: &Path) -> Option<Snapshot> {
    let snapshots = cache::repo_dir(hub, MODEL_REPO).join("snapshots");
    let Ok(entries) = std::fs::read_dir(&snapshots) else {
        return None;
    };
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| dir.join("config.json").is_file() && dir.join("model.safetensors").is_file())
        .collect();
    if let Some(dir) = candidates
        .iter()
        .find(|dir| dir.file_name().and_then(|name| name.to_str()) == Some(MODEL_REVISION))
    {
        return Some(Snapshot {
            dir: dir.clone(),
            pinned: true,
        });
    }
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .map(|dir| Snapshot { dir, pinned: false })
}

/// What `load_synth` says out loud when the cache's offering is not the revision this build names.
/// The wording is pinned by a test because the revision a user's cache will offer is not something
/// they can see otherwise (`just cache` reports the one this build names).
fn unmeasured_revision_note(loaded: &str) -> String {
    format!(
        "zot-speaks: loading revision {loaded}, not the revision this build names \
         ({MODEL_REVISION}); its levels and timings are unmeasured here"
    )
}

/// Load the model once and register every locally-cached catalog voice by name.
///
/// The inference pool is sized here, before anything reaches a worker: `ptts` fixes its pool width
/// at first use, so a size set after the first request is too late (see `apply_ptts_env`).
pub fn load_synth(catalog: &[VoiceInfo]) -> Result<Synth, String> {
    apply_ptts_env();
    let hub = cache::hf_hub();
    let picked = local_snapshot(&hub).ok_or_else(|| {
        format!(
            "Phonon 31m-en snapshot not found under {}; bootstrap once: `just fetch-model` \
             (needs the ptts CLI), or by hand `ptts --voice Miles \"warm up\"` / \
             `huggingface-cli download {MODEL_REPO}`",
            cache::repo_dir(&hub, MODEL_REPO).display()
        )
    })?;
    if !picked.pinned {
        let loaded = picked
            .dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?");
        eprintln!("{}", unmeasured_revision_note(loaded));
    }

    // Quant::F32 selects `model.safetensors`, matching the cached blob.
    let checkpoint = Checkpoint::resolve(
        &picked.dir,
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

/// The text a voice is asked to read: `text` with what a voice cannot read taken out, so that the
/// agent may write code and tables and still close a turn in phrasing one model can speak.
///
/// `ptts` normalizes the rest for us (measured in `examples/spoken_text_probe.rs`): braces, square
/// brackets, `*`, bullets, arrows and emoji become whitespace, `(` and `;` become a comma, so words an
/// agent writes in parentheses are already pronounced. What it does not handle is what an agent writes
/// on purpose: a
/// backtick becomes an apostrophe, a fenced block reaches the tokenizer as `'''rust let x equals foo,
/// bar, '''`, and a table arrives with its pipes. So:
///
///   * a fenced block is dropped whole, language tag included,
///   * a line starting with `|` is dropped, which takes a markdown table with it,
///   * a heading marker at the start of a line is dropped, the heading's words are spoken, and a full
///     stop is added after them so the voice takes a breath there (see [`spoken_heading`], ADR-0031),
///   * a blockquote marker at the start of a line is replaced by the word Quote, spoken once for a run
///     of quoted lines, which is what names the quote to a listener and gives it its breath (see
///     [`blockquote_body`], ADR-0044),
///   * an inline code span is dropped wherever it sits, parenthesized or not: what an agent wants the
///     voice to say has to be in words, and a name in backticks is for the eye alone (ADR-0039,
///     superseding the gloss and flag-hyphen rules ADR-0031 recorded here),
///   * a parenthesis pair left holding no words is dropped with the span inside it, so "the formatter
///     (`cargo fmt`)" is read as "the formatter" and not as a pause with nothing in it (see
///     [`drop_wordless_parens`]).
///
/// Kept parts are joined with single spaces, and a run of whitespace inside one collapses to a single
/// space. `ptts` collapses every run of whitespace anyway; that is only what makes the result readable
/// in a test.
///
/// Limits, pinned by tests: only a ``` fence is a block, only a leading `|` is a table row, only a
/// leading run of one to six `#` followed by a space or nothing is a heading marker, a heading that
/// already ends in `.`, `!`, `?`, `:` or `;` gets no second stop, only a paired span is dropped so a
/// line with an odd number of backticks keeps its tail, only a leading run of one or more `>` is a
/// blockquote marker, a quote block is one run of consecutive quoted lines and is announced once, and
/// the span rule reaches inside a span only, so a `~~~` fence, a `cmd | grep x` line, a prose `--`, a
/// prose `a > b`, a line starting with `-->` and a `#` that is not a heading
/// marker (`#42`) are read as written.
///
/// The order above is the order the rulings land in. The fence split and the parenthesis pass run over
/// the whole text, because each needs to see across lines; [`spoken_line`] holds the rulings that run
/// over one line, in the order they are ruled to apply.
pub fn spoken_text(text: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    for (block, chunk) in text.split("```").enumerate() {
        if block % 2 == 1 {
            continue; // fenced code: the model cannot read it and neither can a listener
        }
        // A quote block is a run of consecutive quoted lines, and a fence ends one: every chunk between
        // fences carries a fresh run.
        let mut quote = QuoteRun::default();
        for line in chunk.lines() {
            if line.trim_start().starts_with('|') {
                continue; // a table row is invisible to a voice, so it cannot end a run either
            }
            kept.push(spoken_line(line, &mut quote));
        }
    }
    // The parenthesis pair is judged on the joined text, so a pair opened at one line end and closed on
    // the next is one pair.
    let joined = collapse_words(kept.iter().map(String::as_str));
    collapse_words([drop_wordless_parens(&joined).as_str()])
}

/// One line as a voice reads it: the marker rules, then the word that opens a quote block, then the
/// span rule.
fn spoken_line(line: &str, quote: &mut QuoteRun) -> String {
    quote.mark(line);
    let line = blockquote_body(line);
    let line = match spoken_heading(line) {
        Some(heading) => heading,
        None => line.to_string(),
    };
    // The word goes on after the heading rule, so a heading inside a quote still loses its `#`, and it
    // is judged before the span rule, so a quoted line whose only content is a span still gets the word.
    let line = quote.intro(line);
    drop_inline_spans(&line)
}

/// Which quote block the line being read belongs to, and whether that block has been announced yet.
#[derive(Default)]
struct QuoteRun {
    quoting: bool,
    announced: bool,
}

impl QuoteRun {
    /// Note where this line sits: a line that is not quoted ends the run, and the first quoted line of
    /// a new one is still waiting to be announced.
    fn mark(&mut self, line: &str) {
        if !line.trim_start().starts_with('>') {
            self.quoting = false;
        } else if !self.quoting {
            self.quoting = true;
            self.announced = false;
        }
    }

    /// The word that opens a quote block, on its first line holding any words at all, and nothing
    /// after it.
    fn intro(&mut self, line: String) -> String {
        if self.quoting && !self.announced && !line.trim().is_empty() {
            self.announced = true;
            format!("{QUOTE_INTRO} {line}")
        } else {
            line
        }
    }
}

/// A line with its paired inline code spans gone.
fn drop_inline_spans(line: &str) -> String {
    let pieces: Vec<&str> = line.split('`').collect();
    // Odd pieces are inside a span. The last piece of a line holding an odd number of backticks has no
    // opening partner, and a stray backtick must not silence the sentence.
    let unpaired = pieces.len().is_multiple_of(2);
    pieces
        .iter()
        .enumerate()
        .filter(|(piece, _)| {
            let inside_span = *piece % 2 == 1;
            let stray = unpaired && *piece + 1 == pieces.len();
            !(inside_span && !stray)
        })
        .map(|(_, content)| *content)
        .collect::<Vec<&str>>()
        .join("")
}

/// The kept parts as one line of words: every run of whitespace becomes a single space.
fn collapse_words<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .flat_map(str::split_whitespace)
        .collect::<Vec<&str>>()
        .join(" ")
}

/// A parenthesis pair holding no words is a pause with nothing in it, which is what a dropped span
/// leaves behind: "the formatter (`cargo fmt`)" keeps "the formatter", not "the formatter ( )". Inner
/// commas and a nested pair count as nothing, and one pass is enough because an inner pair is emptied
/// before the outer one is judged. An unmatched bracket stays as the agent wrote it.
fn drop_wordless_parens(text: &str) -> String {
    let mut kept: Vec<char> = Vec::with_capacity(text.len());
    let mut open: Vec<usize> = Vec::new();
    for ch in text.chars() {
        match ch {
            '(' => {
                open.push(kept.len());
                kept.push(ch);
            }
            ')' => {
                let wordless = open.last().is_some_and(|&start| {
                    let inner = &kept[start + 1..];
                    !inner.iter().copied().any(char::is_alphanumeric)
                });
                if wordless {
                    kept.truncate(open.pop().expect("an open pair was pushed above"));
                } else {
                    kept.push(ch);
                }
            }
            _ => kept.push(ch),
        }
    }
    kept.into_iter().collect()
}

/// The words of a blockquote line, marker gone; any other line exactly as it was written.
///
/// `ptts` leaves a `>` exactly as written (measured in `examples/spoken_text_probe.rs`: a line holding
/// `> quoted words` reaches the phonemizer as `> quoted words`), so a reply that quotes the owner or a
/// document hands the marker to the model. CommonMark's shape is where this stops: the marker is a `>`
/// at the start of a line, after any indentation, and a run of them is a nested quote, which leaves
/// `a > b` in prose and a line starting with `-->` as written.
///
/// The marker is not simply removed. [`spoken_text`] puts the word Quote in its place, once for a run of
/// quoted lines, so a listener hears that a quote has started and hears a breath where the marker used
/// to sit (the owner's ruling of 2026-11-25, ADR-0044, amending ADR-0043, which removed the marker in
/// silence and left the breath to him).
const QUOTE_INTRO: &str = "Quote.";

fn blockquote_body(line: &str) -> &str {
    let body = line.trim_start();
    if !body.starts_with('>') {
        return line;
    }
    body.trim_start_matches('>').trim_start()
}

/// A heading line as a voice reads it: marker off, full stop on, so the heading gets a breath after
/// it. `None` for a line that is not a heading.
///
/// `ptts` leaves a `#` exactly as written (measured in `examples/spoken_text_probe.rs`: a line holding
/// `# heading` reaches the phonemizer as `# heading`), so a heading an agent writes for the eye would
/// be heard as its marker, and a heading with no stop after it runs straight into the sentence below
/// it. Two rulings of the owner on 2026-11-24: a heading marker is never spoken, and a heading is
/// handed over with a full stop on it. CommonMark's own shape is where this stops, so a `#` that is not
/// a marker stays: a run of one to six `#` counts only when a space or the line end follows it, which
/// leaves `#42` and `#######` as written. A heading that already ends in terminal punctuation gets no
/// second stop, and an empty one gets nothing at all.
fn spoken_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let marker = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if marker == 0 || marker > 6 {
        return None;
    }
    let rest = &trimmed[marker..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None; // `#42` and `#######` are not markers
    }
    let text = rest.trim_start();
    if text.is_empty() || matches!(text.chars().last(), Some('.' | '!' | '?' | ':' | ';')) {
        return Some(text.to_string());
    }
    Some(format!("{text}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ptts::comp::{AudioModule, Chain, Gain};
    use std::sync::atomic::AtomicU32;

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

    /// A throwaway hub holding one snapshot directory per revision named, each carrying the two files
    /// a candidate has to have. Written under the temp dir; never touches the real HF cache.
    fn temp_model_hub(tag: &str, revisions: &[&str]) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let hub =
            std::env::temp_dir().join(format!("zot-speaks-hub-{tag}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&hub);
        for revision in revisions {
            let dir = cache::repo_dir(&hub, MODEL_REPO)
                .join("snapshots")
                .join(revision);
            std::fs::create_dir_all(&dir).unwrap();
            for file in ["config.json", "model.safetensors"] {
                std::fs::write(dir.join(file), "").unwrap();
            }
        }
        hub
    }

    /// The other English snapshot a user's cache can hold: what an older bootstrap fetched, the commit
    /// ptts 0.4.0 named as its default and the one the pre-1.10.1 figures were measured against.
    const EARLIER_REVISION: &str = "8c64b55ec301728e2d1695bfea87a08a267d0c2d";

    /// A snapshot that would win a sorted fallback, so the test below cannot pass by sorting alone.
    const FIRST_IN_ORDER_REVISION: &str = "0000000000000000000000000000000000000000";

    /// Several snapshots of one model in one cache, and `read_dir` hands them out in filesystem order.
    /// The revision this artifact names is the one that loads, whichever way the listing came out and
    /// whichever way they sort.
    #[test]
    fn a_cache_holding_several_snapshots_loads_the_pinned_revision() {
        let hub = temp_model_hub(
            "pinned-created-last",
            &[FIRST_IN_ORDER_REVISION, EARLIER_REVISION, MODEL_REVISION],
        );
        let picked = local_snapshot(&hub).expect("the scratch hub holds three snapshots");
        assert_eq!(
            picked.dir.file_name().and_then(|name| name.to_str()),
            Some(MODEL_REVISION),
            "the snapshot a listing happens to yield first is not the one to load"
        );
        assert!(
            picked.pinned,
            "a hit on the named revision is not a fallback"
        );
    }

    /// A cache that does not hold the named revision still has to speak, so it uses what it has. That
    /// fallback is the same snapshot on every run of the same cache, not whatever the listing yields.
    #[test]
    fn a_cache_without_the_pinned_revision_falls_back_to_the_same_one_every_time() {
        let late_first = temp_model_hub("fallback-late-first", &["eee1111", "aaa2222"]);
        let early_first = temp_model_hub("fallback-early-first", &["aaa2222", "eee1111"]);
        let want = "aaa2222";
        for hub in [&late_first, &early_first] {
            let picked = local_snapshot(hub).expect("the scratch hub holds two snapshots");
            assert_eq!(
                picked.dir.file_name().and_then(|name| name.to_str()),
                Some(want),
                "the fallback has to be the same answer whichever order the cache was written in"
            );
            assert!(
                !picked.pinned,
                "this cache has no named revision, so it is a fallback"
            );
        }
    }

    /// The fallback is only half of it: `load_synth` has to say which revision it settled for. The
    /// wording is pinned here because a user cannot see the revision their cache offers any other way
    /// (`just cache` reports the one this build names).
    #[test]
    fn a_fallback_says_which_revision_it_loaded() {
        let hub = temp_model_hub("fallback-note", &[EARLIER_REVISION]);
        let picked = local_snapshot(&hub).expect("the scratch hub holds one older snapshot");
        assert!(!picked.pinned);
        let loaded = picked.dir.file_name().unwrap();
        assert_eq!(
            super::unmeasured_revision_note(loaded.to_str().unwrap()),
            format!(
                "zot-speaks: loading revision {EARLIER_REVISION}, not the revision this build names \
                 ({MODEL_REVISION}); its levels and timings are unmeasured here"
            )
        );
    }

    /// What a voice is handed: fenced code, table rows and every inline code span are gone, a heading
    /// is spoken without its `#` marker and with a full stop after it, a quote block is announced with
    /// the word Quote and without its `>` marker, and a parenthesis pair that held only a dropped span
    /// goes with it (ADR-0031 as amended by ADR-0039 and ADR-0044).
    #[test]
    fn what_a_voice_gets_out_of_markdown() {
        let cases: &[(&str, &str)] = &[
            // The house shape: the words carry the sentence, the name is for the eye alone. A stop the
            // agent wrote after a dropped span stands as its own token here, and `ptts` closes the space
            // before it: the probe reads both cases as "… the provided command." and "… version one ten
            // one." (see `examples/spoken_text_probe.rs`).
            (
                "Run the formatter over all targets with the provided command `cargo fmt --all -- --check`.",
                "Run the formatter over all targets with the provided command .",
            ),
            (
                "The GitHub command line tool (`gh`) bullet carries the reading.",
                "The GitHub command line tool bullet carries the reading.",
            ),
            ("see (`gh`)", "see"),
            // Two names in one pair leave the commas, and a pair holding only commas is dropped too.
            (
                "the harness (`dev/test.sh`, `dev/test-sink.sh`) runs the units",
                "the harness runs the units",
            ),
            ("The tag is version one ten one (`v1.10.1`).", "The tag is version one ten one ."),
            // Nothing inside backticks is spoken: a command, a flag, a target triple, a version.
            ("Run `cargo fmt --all` now.", "Run now."),
            ("Filter `git -C public log` now.", "Filter now."),
            ("`x86_64-unknown-linux-gnu` is the target.", "is the target."),
            // A span holds its own text: the `#` inside it is no heading marker, and it is silent.
            ("see `## v1.11.0` now", "see now"),
            ("Only a span: `git status`", "Only a span:"),
            ("`git status`", ""),
            // A stray backtick has no partner, so it must not silence the rest of the line.
            (
                "text with a stray ` backtick keeps its tail",
                "text with a stray backtick keeps its tail",
            ),
            // Prose is untouched: the rule reaches inside a span only.
            ("the fix -- which is small", "the fix -- which is small"),
            // Parentheses holding words are kept, wherever a span used to sit next to them.
            (
                "Run `cargo fmt` (the formatter) now.",
                "Run (the formatter) now.",
            ),
            (
                "the `filter`(drop code (not the words around it) when dropped) works",
                "the (drop code (not the words around it) when dropped) works",
            ),
            // A pair opened at one line end and closed on the next is one pair, because the pair rule is
            // judged after the lines are joined.
            (
                "notes (`dev/test.sh`\n`dev/test-sink.sh`) end",
                "notes end",
            ),
            (
                "Done.\n```rust\nlet x = foo(bar);\n```\nAll green.",
                "Done. All green.",
            ),
            (
                "| card | state |\n|---|---|\n| v1.10.1 | done |\nThree cards are in ready.",
                "Three cards are in ready.",
            ),
            ("```sh\ngit status\n```", ""),
            // The limit, pinned so a change here is noticed: only ``` fences are a block.
            ("note\n~~~\ncode\n~~~\nend", "note ~~~ code ~~~ end"),
            // A heading is its words, marker and indentation gone, plus the full stop the extension
            // adds so the voice takes a breath after it (ADR-0031).
            ("## The gates", "The gates."),
            (
                "###   One commit carries the change",
                "One commit carries the change.",
            ),
            ("   # indented heading\ntext", "indented heading. text"),
            ("##", ""),
            // A heading that already ends in terminal punctuation is not given a second stop.
            ("## Done.", "Done."),
            ("## Is it fast?", "Is it fast?"),
            ("## Listen:", "Listen:"),
            // A trailing marker on a heading line is still spoken: `## F#` has to keep its `#`.
            ("## Gates ##", "Gates ##."),
            // A `#` that is not a marker stays as written, and so does a run past six.
            ("#42 is open", "#42 is open"),
            ("####### seven hashes", "####### seven hashes"),
            // A heading whose words end in a span keeps its stop as a token of its own.
            ("## Run `cargo fmt`", "Run ."),
            // A heading marker inside a fenced block is gone with the block.
            ("```md\n## Gates\n```\nAfter.", "After."),
            // A quote block is the word Quote and its words: the marker is replaced, once per block
            // (ADR-0044).
            ("> The owner wrote this, and I am quoting it.", "Quote. The owner wrote this, and I am quoting it."),
            (
                "> First quoted line.\n> Second quoted line.\nBack to my own words.",
                "Quote. First quoted line. Second quoted line. Back to my own words.",
            ),
            ("   > an indented quote", "Quote. an indented quote"),
            (">> a nested quote", "Quote. a nested quote"),
            (">", ""),
            // A heading inside a quote loses both markers and keeps the heading's stop.
            ("> ## Quoted gates", "Quote. Quoted gates."),
            // The limit, pinned so a change here is noticed: only a leading `>` is a marker.
            ("a > b compares two numbers", "a > b compares two numbers"),
            ("--> not a marker", "--> not a marker"),
            // A `>` that is a marker with no space after it, as CommonMark reads it.
            (">no space after the marker", "Quote. no space after the marker"),
            // Once means once: the rest of the run is spoken without the word.
            ("> One.\n> Two.\n> Three.", "Quote. One. Two. Three."),
            // Any line that is not quoted ends a block, a blank line included, so two blocks are
            // announced twice.
            ("> One block.\n\n> Another block.", "Quote. One block. Quote. Another block."),
            ("> A quote.\nMy own line.\n> A second quote.", "Quote. A quote. My own line. Quote. A second quote."),
            // A quoted line with no words of its own does not spend the word: the first quoted line
            // holding any is the one announced.
            (">\n> Text after an empty marker.", "Quote. Text after an empty marker."),
            // A fence ends a block, and its own lines are never quoted.
            ("> Quoted.\n```rust\nlet x = 1;\n```\n> After the fence.", "Quote. Quoted. Quote. After the fence."),
            // A table row is invisible to a voice, so it cannot end a block either.
            ("> Quoted.\n| a | b |\n> Still the same block.", "Quote. Quoted. Still the same block."),
            // A quoted line whose only content is a span: the span is gone, the word is not.
            ("> `git status`", "Quote."),
        ];
        for (written, spoken) in cases {
            assert_eq!(spoken_text(written), *spoken, "for {written:?}");
        }
    }
}
