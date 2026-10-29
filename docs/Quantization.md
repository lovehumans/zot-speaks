# Quantization: what was measured, and why `zot-speaks` ships the full-precision weights

**Short answer.** `zot-speaks` asks `ptts` for `Quant::F32` — the fp32 weights in
`model.safetensors`. Quantized weight files exist in the model repository, and `ptts` can use them,
but on a desktop CPU `q8_0` saves about 50 MB of peak memory and costs *more* time to the first sound
than it saves anywhere else. The measurement is below; the conclusion is that the trade is not worth a
setting, so there is none. If your machine is memory-starved rather than latency-sensitive, this page
is also the recipe for measuring it on your own box.

## What is actually on disk

`gradium/phonon-31m-en` (snapshot `8c64b55e…`) publishes several weight encodings of the same model.
What your cache holds is what you get — `zot-speaks` reads the cache and never downloads:

| file | size | what it is |
|---|---|---|
| `model.safetensors` | 102 823 920 B | fp32 — what `Quant::F32` loads, and what this extension loads |
| `model.q8.gguf` | 64 512 352 B | GGUF, `q8_0` (8-bit blocks) |
| `model.q4k.gguf` | 55 075 168 B | GGUF, `q4_k` (4-bit, k-quant) |
| `model.gguf`, `model.t8.gguf` | absent | older filenames ptts does not look for in 0.4.0 |

`ptts 0.4.0` picks between them by the quantization it is asked for (`checkpoint::weight_candidates`):
`Q4k` → `model.q4k.gguf` else safetensors; `Q80` → `model.q8.gguf` else safetensors; anything else,
including `F32`, → safetensors else `model.q8.gguf`. Quantization here is CPU-only: `ptts` exposes
`q8_0` and `q4k` for the CPU path and nothing for a GPU.

## The measurement

`AMD Ryzen 7 7800X3D`, 8 physical cores / 16 logical CPUs, one inference process at a time and nothing
else loaded. The utterance is 7.68 s of speech — *"The quick brown fox jumps over the lazy dog.
Quantizing the weights does not change the words, only how fast and how much memory they take."* —
voice `Miles`, seed `42`, temperature `0.3`, model passed as a local snapshot directory (so no Hub API
call). Mean of 3 repetitions with `ptts 0.4.0`. Every number below is one the CLI reports about
itself: `first chunk` from its `generated … (RTF=…, first chunk …ms)` line, `rtf` from the same line,
`peak RSS` from its `INFO peak RSS: … MB` line, load from its `loading model` / `model loaded`
timestamps, and `wall` from the whole process. These are box-specific numbers: compare columns inside
one table, not across machines.

**8 worker threads — the count `zot-speaks` now asks for (one per physical core):**

| variant | load | first sound | rtf | peak RSS | wall |
|---|---|---|---|---|---|
| `f32` (safetensors) | 47 ms | **37 ms** | 0.0519 | 349.1 MB | 475 ms |
| `q8_0`, weights resolved (picks `model.q8.gguf`) | 34 ms | 41 ms | 0.0428 | 301.1 MB | 391 ms |
| `q8_0`, `--weights model.q8.gguf` | 35 ms | 41 ms | 0.0423 | 299.9 MB | 387 ms |
| `q8_0`, `--weights model.safetensors` (quantized on the way in) | **65 ms** | 44 ms | 0.0426 | 296.4 MB | 420 ms |
| `q4_k`, `--weights model.q4k.gguf` | 32 ms | 52 ms | 0.0429 | 298.4 MB | 389 ms |
| `q5_0` (no matching file cached, converted in memory) | 60 ms | 128 ms | 0.0617 | 284.8 MB | 562 ms |

**16 worker threads (one per logical CPU — what the `ptts` CLI defaults to, and what this extension
used to ask for):**

| variant | load | first sound | rtf | peak RSS | wall |
|---|---|---|---|---|---|
| `f32` | 50 ms | **49 ms** | 0.0940 | 339.3 MB | 803 ms |
| `q8_0`, weights resolved | 38 ms | 64 ms | 0.0829 | 273.4 MB | 702 ms |
| `q8_0`, `model.q8.gguf` | 36 ms | 65 ms | 0.0813 | 282.6 MB | 690 ms |
| `q8_0`, safetensors quantized on the way in | 66 ms | 66 ms | 0.0828 | 283.6 MB | 731 ms |
| `q4_k`, `model.q4k.gguf` | 33 ms | 213 ms | 0.0985 | 279.6 MB | 818 ms |
| `q5_0` | 60 ms | 338 ms | 0.1238 | 272.2 MB | 1041 ms |

A control run in a checkpoint directory holding **no GGUF at all** (only `config.json`,
`tokenizer.json`, a voice embedding and `model.safetensors`) reproduced the same shape at 8 threads:
`q8_0` first sound 34–46 ms and 298–308 MB, `f32` in the same directory 36–39 ms and 355–362 MB.
Nothing here argues for shipping a quantized file alongside the model.

## Reading it

* **`q8_0` is a memory trade, not a speed trade.** Peak RSS is 48–53 MB lower at 8 workers (349.1 MB →
  296–301 MB; the no-GGUF control put the same gap at 47–64 MB, 355–362 → 298–308) and 56–66 MB lower at
  16 threads (339.3 → 273–284). Wall time drops 12–18 % (475 → 387–391 ms at 8 workers, 803 → 690–702 ms
  at 16; about 12 % when the safetensors are converted on the way in). **First sound is the number a
  listener actually waits for, and quantization does not win it.** Across every pass measured here it
  ranged from 3 ms better to 17 ms worse — the passes that isolated it (8 workers in the table above, 16 in
  the cross-check) both put `q8_0` behind, the earlier CSV put it 3 ms ahead. That is inside run-to-run
  drift, and the honest reading is that there is no speed case for quantization on this box in either
  direction.
* **A pre-quantized GGUF buys load time, not listening time.** Reading `model.q8.gguf` starts ~30 ms
  sooner than converting `model.safetensors` on the way in (36–38 ms against 66 ms of load), and from there
  the two are within noise of each other: first sound 64–65 ms against 66 ms, `rtf` 0.0813–0.0829 against
  0.0828, peak RSS 273–284 MB against 284 MB. So the file you might ship would speed up a step the user
  never notices.
* **Below `q8_0` it degrades fast.** `q5_0`, which has no matching file cached and therefore converts
  in memory, costs 128 ms (8 threads) to 338 ms (16 threads) of first sound. `q4_k` from its GGUF is
  fine at 8 threads (52 ms) but unstable at 16, where first sound measured 213 ms in one pass and
  279 ms in another.
* **More threads is not the same lever.** The same model, the extension's own library path, one 8.00 s
  utterance: 8 workers give 29–32 ms to first sound and 505–515 ms end to end; 16 workers give 49–52 ms
  and 923–959 ms; 4 workers are within noise of 8. Full-precision weights with one worker per physical
  core beat every quantized variant measured at 16 threads on first sound, on wall time *and* on peak
  RSS. See *Tuning the inference* in [docs/DESIGN.md](DESIGN.md) and `ZOT_TTS_THREADS` there.
* **The audio is a different take, not a degraded one.** Same seed and the same fixed noise input,
  `f32` against `q8_0` from `model.q8.gguf`: the two waveforms diverge past an audible threshold 0.40 s
  in, rms difference 2272 on a ±32767 scale, durations 7.84 s against 7.76 s. Waveform distance on its
  own is not a quality judgement — but on this model quantization changes which take you get, and the
  measured latencies are small enough that the question is not worth a listening session for a desktop
  box.

## So there is no `--quant` setting

`q8_0` on a desktop: ~50 MB of peak memory for a few milliseconds of first sound, plus a weight file
that has to be present locally (the extension never downloads one) and a second copy of every decision
above to keep honest. That is not a trade worth putting in a settings panel, and it is not one users
should have to think about. The model is 31 M parameters and runs in about 350 MB; the interesting
latency knob on this box was the worker count, not the weight encoding.

**Closed, not deferred.** Ruling by the owner, 2026-10-29: there will be **no listening comparison of `f32`
against `q8_0`**. The gain is tens of megabytes and a tenth of the wall time, with first sound no earlier;
that is not worth a listening session on a desktop, and the waveform divergence above is recorded as an
observation rather than as a question to be settled by ear. (ADR-0017.)

**When to reopen it.** On a memory-starved target — a 2 GB Jetson-class box, where 50 MB is a real
number and first-sound milliseconds are not the binding constraint — the measurement above is the one
to repeat, on that box, before anything ships. `ptts` reports what you need in its own log, so the
comparison needs nothing beyond the CLI:

```sh
snapshot="$HOME/.cache/huggingface/hub/models--gradium--phonon-31m-en/snapshots/8c64b55ec301728e2d1695bfea87a08a267d0c2d"
text="The quick brown fox jumps over the lazy dog. Quantizing the weights does not change the words, only how fast and how much memory they take."

# full precision, what zot-speaks asks for
ptts -m "$snapshot" --lang en --voice Miles --seed 42 -t 0.3 --threads 8 --quant f32 \
    -o /tmp/f32.wav "$text"

# q8_0, letting ptts resolve the weights (it prefers model.q8.gguf when the cache has one)
ptts -m "$snapshot" --lang en --voice Miles --seed 42 -t 0.3 --threads 8 --quant q8_0 \
    -o /tmp/q8.wav "$text"

# q8_0 from a named file; --weights is relative to the checkpoint root, an absolute path is refused
ptts -m "$snapshot" --lang en --voice Miles --seed 42 -t 0.3 --threads 8 --quant q8_0 \
    --weights model.safetensors -o /tmp/q8-from-f32.wav "$text"
```

Read the last three `INFO` lines of each run: `generated 7.68s in 0.40s (RTF=0.0514, first chunk 31ms)` and
`peak RSS: 363.59 MB` are one such run of the first command above, and the gap between `loading model` and
`model loaded` is the load column. Run each command three times and average before comparing: a single run reads
a few milliseconds and a few tens of MB away from its own mean, which is why the tables above are means and that
log line is not one of them. `--threads` is the same lever as `ZOT_TTS_THREADS`, one step below the extension.
Pass the snapshot directory rather than `hf://…`: with the default model spec `ptts` asks the Hub for the repo
tree before it looks in your cache, which needs network and fails offline.
