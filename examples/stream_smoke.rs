// Slice 3 validation harness (not shipped as a binary; only `cargo run --example`).
//
// Proves the ptts pipeline the extension depends on:
//   * the model resolves from the local HF cache (no hub/TLS),
//   * a CPU F32 synth with the "default" voice streams real audio,
//   * the FIRST audio chunk arrives well before the whole utterance (TTFB),
//   * dropping the SpeechStream mid-stream stops promptly (cooperative stop).
//
// Run: cargo run --release --example stream_smoke [text]

use std::time::Instant;

use ptts::checkpoint::{Checkpoint, ResolveOptions};
use ptts::preprocess::Lang;
use ptts::synth::{DeviceKind, Quant, Synth};

fn snapshot() -> Option<String> {
    // Cache precedence identical to src/cache.rs and the HF downloaders: HF_HUB_CACHE,
    // HF_HOME/hub, XDG_CACHE_HOME/huggingface/hub, ~/.cache/huggingface/hub.
    let home = std::env::var("HOME").unwrap_or_default();
    let roots = [
        std::env::var("HF_HUB_CACHE").ok().map(std::path::PathBuf::from),
        std::env::var("HF_HOME")
            .ok()
            .map(|dir| std::path::PathBuf::from(dir).join("hub")),
        std::env::var("XDG_CACHE_HOME")
            .ok()
            .map(|dir| std::path::PathBuf::from(dir).join("huggingface").join("hub")),
        Some(
            std::path::PathBuf::from(home)
                .join(".cache")
                .join("huggingface")
                .join("hub"),
        ),
    ];
    let repo = "models--gradium--phonon-31m-en";
    for root in roots.into_iter().flatten() {
        let Ok(snapshots) = std::fs::read_dir(root.join(repo).join("snapshots")) else {
            continue;
        };
        for entry in snapshots.flatten() {
            let rev = entry.path();
            if rev.join("config.json").is_file() {
                return Some(rev.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let text = std::env::args().nth(1).unwrap_or_else(|| {
        "The quick brown fox jumps over the lazy dog. This sentence is long enough \
         to span several streaming chunks, so we can see the first audio arrive long \
         before the last one is generated."
            .into()
    });

    let model = snapshot().expect("no cached phonon snapshot; download it first");
    println!("model: {model}");

    let t0 = Instant::now();
    let checkpoint = Checkpoint::resolve(
        &model,
        ResolveOptions {
            quant: Quant::F32,
            weights: None,
        },
    )?;
    let mut tts: Synth = checkpoint
        .builder(Lang::En)
        .device(DeviceKind::Cpu)
        .build()?;
    checkpoint.register_voices(&mut tts);
    let rate = tts.sample_rate();
    println!("loaded+built in {:.1?}  sample_rate={rate}", t0.elapsed());

    // (a) full stream: measure time-to-first-chunk and total.
    let t1 = Instant::now();
    let stream = tts.stream(&text)?;
    let mut chunks = 0usize;
    let mut samples = 0usize;
    let mut first = None;
    for chunk in stream {
        let c = chunk?;
        if first.is_none() {
            first = Some(t1.elapsed());
        }
        chunks += 1;
        samples += c.len();
    }
    let total = t1.elapsed();
    println!(
        "FULL   : chunks={chunks} samples={samples} ({:.2?} audio) ttfb={:.1?} total={:.1?}",
        samples as f32 / rate as f32,
        first.unwrap_or_default(),
        total
    );
    assert!(samples > 0, "no audio produced");
    assert!(chunks >= 2, "expected more than one streaming chunk");
    assert!(
        first.unwrap() < total,
        "first chunk should arrive before the stream ends"
    );

    // (b) cooperative stop: drop the stream after the first chunk; must return promptly.
    let t2 = Instant::now();
    let first_chunk;
    {
        let mut stream = tts.stream(&text)?;
        first_chunk = stream.next().unwrap()?.len();
        drop(stream); // <-- cooperative stop: generation workers stop writing
    }
    let stop = t2.elapsed();
    println!("STOP   : dropped after first chunk ({first_chunk} samples) in {stop:.1?}");
    assert!(
        stop < total,
        "drop should be faster than generating the whole utterance"
    );

    println!("OK");
    Ok(())
}
