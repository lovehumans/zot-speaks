# Changelog

What each published version brings, newest first. This file is where a Release's text comes from:
`dev/publish.sh` refuses to publish a version that has no `## v<version>` section here, and the
published CI feeds that section to `gh release create --notes-file`. A Release's text can be edited
afterwards; a published tag cannot — which is why the notes live in a tracked file instead of in the
tag message.

## v1.9.0 — One player: `ffplay`, and ffmpeg is a prerequisite

* **`ffplay` is the only player, and ffmpeg is a prerequisite rather than a preference.** The second
  candidate (`paplay`, ADR-0010) is gone, and with it the fallback that could quietly discard the speed you
  had set: a `paplay` run spoke at `1.0x` and had to say so (`speed 1.0x through paplay`). Speed is now
  asked of the player on every platform this extension runs on — ffmpeg's `atempo`, the same `0.5x`–`2.0x`
  ladder, no capability question in between.
* **The cost, stated plainly:** a Linux box that has `paplay` (from `pulseaudio-utils`) and no `ffmpeg`
  spoke in 1.8.x, and will not speak in 1.9.0 until ffmpeg is installed. Nothing else needs installing:
  `ffplay` already was the player 1.8.2 used on this box, on every platform, by default.
* **`ZOT_SPEAKS_SINK` is gone, not repurposed.** It selected a candidate; there is no choice to select.
  Setting it in 1.9.0 does nothing at all, which `docs/MACOS.md` now says where the variable used to be
  documented (its checklist is one row shorter).
* **A box with no player gets one sentence, on every platform:**
  `no audio player found: install ffmpeg so that \`ffplay\` is on your PATH`. It names no package manager —
  which one to use is your business — and no player this release removes. `brew install ffmpeg` is still in
  `docs/MACOS.md`, where it belongs.
* **The spoken note stopped naming the player:** `zot-speaks: speaking as Azgorath at temp 0.35, vol 100%,
  speed 1.2x`. With one candidate the word carried no choice, so `ffplay` now appears in user-visible text
  only where it is actionable: in the failure path.
* **Under the hood, with no behaviour change:** the crate is a library (`zot_speaks`: `cache`, `settings`,
  `tts`, `voices`, `sink`) plus the extension binary, so `examples/stream_smoke.rs` measures the pipeline the
  extension ships instead of a copy of it that can drift; and everything about how audio reaches the player
  — the argv, the s16le conversion and its gain clamp, the pacing to real time, the drain-or-kill stop, the
  fact that `ffplay` exits 0 even when it played nothing — now sits behind one module whose rules six unit
  tests assert. 59 Rust tests in total, and CI runs all of them.
* The panel's identity row and `extension.json` now read `1.9.0`.

## v1.8.2 — One worker per physical core

* **Inference now asks for one worker thread per physical core, not per logical CPU.** The request is
  made through `RAYON_NUM_THREADS`, which is the variable the inference pool actually reads. Until now
  the extension set `OMP_NUM_THREADS`, and nothing in the dependency tree reads that: the request was
  ignored, and the pool took every logical CPU it could find.
* **What that costs on an SMT box is measured, on this machine** (8 physical cores / 16 logical CPUs, the
  shipped `f32` weights, `examples/stream_smoke.rs`, one 6.7 s utterance, three runs each): **40–45 ms to
  the first sound and 392–402 ms end to end at 8 workers, against 51–70 ms and 718–746 ms at 16** — about
  1.8x end to end. Peak memory is not part of this trade: the same runs peak at 334–341 MB with 8 workers
  and 320–336 MB with 16, a band rather than a lever. A machine without SMT names the same number of
  physical and logical CPUs, so it is unaffected.
* **`ZOT_TTS_THREADS=<n>` pins the width and is now documented.** Any positive number overrides the rule
  for someone who knows their box. `1` is the smallest and quietest setting measured — 301–308 MB of peak
  RSS against 334–341 MB at 8 workers — at two to three times the time to first sound. A value that does
  not parse is not a pin, and the physical-core rule is used instead.
* **New docs: *Tuning the inference* in `docs/DESIGN.md`, and `docs/Quantization.md`.** The latter records
  why full-precision weights stay shipped: `q8_0` measured 50–66 MB lighter at 8 workers with first sound
  no earlier, and it produced a different take rather than a degraded one, so quantization is not shipped
  and has no setting.
* The panel's identity row and `extension.json` now read `1.8.2`.

## v1.8.1 — The Temperature row says what it does

- The **Temperature** row now speaks while it is the selected row, in the voice you picked:
  *Temperature adds variety to how a sentence is pronounced.* It starts when the cursor arrives and
  keeps repeating for as long as the row holds the cursor — the bound is the selection, not a count.
- An arrow on the value does not restart the sentence already running. The **next** repetition is
  synthesized at the value you just stepped, so the repeated sentence is also the demonstration: the
  same line at `0.30`, then at `0.70`.
- It ends when you leave the row, close the panel, ask for `/tts stop`, or say anything else (`s`,
  another row's audition, an auto-spoken answer). Leaving and returning starts it again.
- Only an utterance that really produced audio is repeated, so a missing or failing player cannot
  turn an explanation into an endless retry.
- The `/tts-settings` panel now names what you are running: its last row is `zot-speaks v1.8.1`,
  right-aligned under the settings and the voice detail, so "did the update land?" is answerable from
  the panel.

- The **noise seed varies per utterance**: every request is synthesized from its own seed, counting
  up from a base drawn when the extension starts, so the repeated explanation, a re-run `/tts` and an
  auto-spoken answer are each a new take rather than the same audio replayed. `ZOT_SPEAKS_SEED=<n>`
  pins that base, which replays a session's utterances in order; at temperature `0.0` there is no
  noise to vary, so repeats stay identical there by design.

## v1.8.0 — Speak at a chosen speed

- New **Speed** setting: `0.5x` to `2.0x` in `0.1` steps, default `1.0x`. It sits on the
  `/tts-settings` panel between Volume and Temperature, and in `settings.json` as `"speed"`.
- Speed is asked of the player, not of the model: `ffplay` runs with `-af atempo=<speed>`, so the
  voice keeps its pitch and its quality and the audio is paced at the rate actually spoken. At the
  default `1.0x` the player command is unchanged.
- `paplay` has no tempo to ask for, so with `ZOT_SPEAKS_SINK=paplay` it says so once per session and
  speaks at natural rate instead of pretending.
- A player that failed is no longer reported as having spoken. `ffplay` exits `0` even when it opened
  no audio device or could not build its filter graph, so its error output is now read and shown.

## v1.7.15 — ffplay by default, manifest next to the binary

- `ffplay` (from ffmpeg) is the default player on every platform, streaming the same mono `s16le` PCM
  on stdin instead of buffering a whole utterance. `paplay` stays the automatic fallback and the
  documented pin (`ZOT_SPEAKS_SINK=paplay`); an unknown value is an error, not a silent fallback.
- Each Release publishes `extension.json` beside the binary and checksums both, so `just
  update-release` changes what `zot ext list` reports, not only what runs.
- New `docs/MACOS.md`: what to expect on a Mac, as a checklist.

## v1.7.14 — documentation only

- `docs/DESIGN.md` explains `temperature` the way upstream does (xn-ptts #254). No behaviour change.

## v1.7.13 — documentation only

- README reworked; the lockfile caught up. No behaviour change.

## v1.7.12 — documentation only

- README: Acknowledgements for `xn-ptts` and `zot`.

## v1.7.11 — documentation only

- README: the bootstrap reminder moved into the install route; "Build" became "For developers".

## v1.7.10 — documentation only

- README: one-line table of contents; the non-primary install and update paths collapsed.

## v1.7.9 — the Release recipes fetch from the right URLs

- `just install-release` and `just update-release` download the assets from the URLs GitHub actually
  serves, and a pinned version works: `just update-release v1.7.9`.

## v1.7.8 — the Release recipes find the installed copy

- The installed copy is located the way zot locates it: `$ZOT_HOME`, else `$XDG_STATE_HOME/zot`, else
  `~/.local/state/zot`.

## v1.7.7 — install and update from a Release, with just

- `just install-release` and `just update-release`: fetch the binary, verify `SHA256SUMS`, swap the
  installed copy in place, leave `settings.json` alone.

## v1.7.6 — faster CI

- The crate registry and the compiled dependency graph are cached, keyed on `Cargo.lock`, so a steady
  run compiles only this crate.

## v1.7.5 — the Release assets actually arrived

- CI checkout fetches tags (`fetch-tags: true`), so the run triggered by a tagged commit can see its
  own tag; on v1.7.4 that was not so, and its packaging steps were silently skipped. Downloading from
  the Release became the documented main install route.

## v1.7.4 — CI compiles once per run

- The Release is packaged inside the job that already built the binary, instead of a second run
  triggered by the tag compiling the same commit twice. Tests run in `--release` mode to reuse that
  build.

## v1.7.3 — CI fixed

- A zero-indented line inside a `run:` block had ended the block scalar and broken the workflow, so
  the packaging steps behind it never ran. Release text now goes through `--notes-file`.

## v1.7.2 — Release assets

- CI builds the release binary and attaches it, with `SHA256SUMS`, to each version tag's Release.

## v1.7.1 — no behaviour change

- `Cargo.lock` caught up with the version bump.

## v1.7.0 — the first published artifact

- `/tts`, `/tts stop` and `/tts-settings`: 37 English voices for `phonon-31m-en` with previews and
  favourites, volume, temperature, auto speak. Local-only: it reads the Hugging Face cache and never
  downloads at runtime.
