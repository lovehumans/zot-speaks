// Slice 3 validation harness (not shipped as a binary; only `cargo run --example`).
//
// Proves the ptts pipeline the extension ships, measured through the shipped loader
// (`tts::load_synth`: cache precedence, `RAYON_NUM_THREADS` sizing, `Quant::F32`, voice
// registration) rather than a re-implementation of it:
//   * the model resolves from the local HF cache (no hub/TLS),
//   * a CPU F32 synth speaking the settings' default voice streams real audio,
//   * the FIRST audio chunk arrives well before the whole utterance (TTFB),
//   * dropping the SpeechStream mid-stream stops promptly (cooperative stop).
//
// Run: cargo run --release --example stream_smoke [text]

use std::time::Instant;

use zot_speaks::{cache, settings, tts, voices};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let text = std::env::args().nth(1).unwrap_or_else(|| {
        "The quick brown fox jumps over the lazy dog. This sentence is long enough \
         to span several streaming chunks, so we can see the first audio arrive long \
         before the last one is generated."
            .into()
    });

    let t0 = Instant::now();
    let catalog = voices::catalog(&cache::hf_hub(), tts::MODEL_LANGUAGE);
    let synth = tts::load_synth(&catalog)?;
    let rate = synth.sample_rate();
    println!(
        "loaded+built in {:.1?}  sample_rate={rate}  voices={}  RAYON_NUM_THREADS={}",
        t0.elapsed(),
        synth.voices().len(),
        std::env::var("RAYON_NUM_THREADS").unwrap_or_else(|_| "?".to_string()),
    );

    let settings = settings::Settings::default();
    let (options, used_voice) = tts::build_options(&synth, &settings, 1);
    println!(
        "voice in use: {}",
        used_voice.unwrap_or_else(|| "(model default)".to_string())
    );

    // (a) full stream: measure time-to-first-chunk and total.
    let t1 = Instant::now();
    let stream = synth.stream_with(&text, &options)?;
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
        let mut stream = synth.stream_with(&text, &options)?;
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
