# Changelog

What each published version brings, newest first. This file is where a Release's text comes from:
`dev/publish.sh` refuses to publish a version that has no `## v<version>` section here, and the
published CI feeds that section to `gh release create --notes-file`. A Release's text can be edited
afterwards; a published tag cannot — which is why the notes live in a tracked file instead of in the
tag message.

## v1.10.1 — The artifact names the model snapshot it loads

- **One revision, named in the binary.** A Hugging Face cache can hold two snapshots of
  `gradium/phonon-31m-en`: a cache warm fetches the revision that the `ptts` release running it names as its
  default, and until now the extension played whichever entry the directory listed first. It now loads the
  revision `src/tts.rs` names, `92741c5b07d085ff840e26fea8ce43110b87852f` — the one `ptts` 0.7.1 and
  `just fetch-model` fetch. The weights are not in question: both snapshots hold the same 102 823 920-byte
  fp32 `model.safetensors` and the same 277 967-byte `tokenizer.json` (`sha256sum` read through the symlinks),
  and they differ in `config.json` (2 851 bytes against 4 634) and in the voice bundled beside the weights
  (see [docs/Quantization.md](docs/Quantization.md), *What is actually on disk*).
- **A cache holding only the other snapshot still speaks, and says which one it loaded:**
  `zot-speaks: loading revision 8c64b55e…, not the revision this build names (92741c5b…); its levels and
  timings are unmeasured here`. The wording is asserted in a unit test, so this note cannot drift into a
  sentence that no longer names the revision it loaded.
- **`just cache` now reports the snapshot the extension will load.** The `v1.10.0` recipe listed the first
  entry of the snapshot directory (`ls -d "$model/snapshots"/*/ | head -1`), so on a cache holding both it
  could name the revision the build does not use. It names the pinned one, and says so plainly when that
  revision is not cached.
- **The level figures published for 1.10.0 hold for the revision now pinned.** `dev/level-probe.sh` ran against
  each cached snapshot — `RUNGS="0.25 0.5 0.8 1.0" SEED=42`, one fixed 4.2 s take in Alfie — and gave the same
  figures at every rung: 101 760 samples, peak 0.3980 / 0.7961 / 0.8912 / 0.8913 full scale, 0 samples at the
  ceiling, no flat run there, rms −26.62 / −20.60 / −17.16 / −16.37 dBFS. First sound arrived 14–17 ms into the
  run on the older snapshot and 17–20 ms on this one, which is machine noise at that scale.

## v1.10.0 — A `voice-mode` skill, and a volume dial that limits instead of clipping

- **The extension ships a skill** (`skills/voice-mode/SKILL.md`): a phrasing guide for answers that will be read
  aloud — short sentences with full stops in them, prose instead of markdown, no quoted code, no commit hashes,
  capitals only where they change how a word is said (`TTS`, but `readme`), a version number written the way it is
  said (`1 dot 10 dot 0`, not `1.10.0`), counts instead of lists of names, one question at a time. It is
  **user-invoked**: the agent never picks it up on its own initiative and auto-speak does not imply it. Run
  `/skill:zot-speaks:voice-mode`, or open `/skills` and press `p` to pin it to the project, which is the setting
  worth choosing if you always speak (a pin applies to fresh conversations). Every install and update recipe carries
  `skills/` next to the manifest, and an update clears the installed skills directory first, so a skill that has
  been removed cannot linger.
- **The volume dial means what it always meant and now does less damage.** `50%` is still the voice at its own
  level and `100%` twice that level; what changed is where the scaling happens. It used to be a multiplier with a
  hard clamp in the audio writer; it is now a gain stage in decibels followed by a look-ahead limiter inside the
  synthesis chain. Measured on one fixed take — voice Alfie, `ZOT_SPEAKS_SEED=42`, one 4.2 s sentence, whose raw peak is
  0.796 FS — at `100%` the old path drove **349 samples onto full scale, 13 of them in a row**, and left them flat
  there; the new chain leaves **0** samples at the ceiling and no flat run, its peak at 0.891 FS (−1.00 dBFS)
  instead of pinned at 1.000 FS. Holding the peaks costs 1.7 dB of average level on that take (rms −14.70 →
  −16.37 dBFS). At `25%` and `50%` the two paths agree sample for sample, because the limiter is idle whenever the
  peak is already under its ceiling; on this take it starts engaging just above `55%`. The figures and the chain
  are documented in `docs/DESIGN.md`, *Volume*.
- **The synthesis library is ptts 0.7.1** (it was 0.4.0). Inference is still CPU-only — no CUDA, Metal, Vulkan,
  WebGPU or Accelerate backend is enabled, and none ever will be here — and the upgrade is level-neutral: on the
  same fixed take at `25%`, `50%`, `80%` and `100%`, peak level, the number of samples reaching the ceiling and the
  average level are the same at every rung (the widest gap is the `80%` peak, 0.8912 against 0.8913 FS), and the
  first sound still arrives 15–18 ms after the request. The waveforms are not bit-identical: the two agree sample
  for sample for the first 1.25 s of the take and afterwards differ by up to 150 LSB of the raw 16-bit signal.
- **A new recipe fetches the cache-warming helper.** `just fetch-ptts` downloads the upstream `ptts` CLI at
  the version `Cargo.toml` names, checks the archive against that release's published `SHA256SUMS`, and installs
  it as `./ptts` — where `just fetch-model` and `just fetch-voices` look first. The version is the point: which
  model commit a cache warm resolves is decided by that library, and a stale `./ptts` — gitignored, so nothing
  ever flagged it — kept warming the cache with the weights an older release pinned. Linux and macOS, four target
  triples; Windows is told to install the CLI itself.
- **Sending a message cuts off the answer it replaces.** The extension now subscribes to the host's
  `user_prompt_submit` event as well, and on it asks for the same clean stop `/tts stop` gives: an
  auto-spoken answer, a `/tts`, or a voice audition that is still playing is cut off, quietly, the moment
  the new message is accepted. A request that had been queued but had not started is dropped rather than
  started. `dev/test.sh` measures it: the player stops being fed 94–1716 ms after the message across five runs
  (the harness asserts 2000 ms), while the same sentence left alone runs 2.8–2.9 s and 138 240–142 080 bytes of
  audio — so what the cut removes is a second or more of speech. There is no setting for it and it says nothing
  when it happens: a message always ends the talk it replaces.
- **Nothing you have stored changes, and nothing downloads.** A `settings.json` written by 1.9.x is read exactly as
  it was — same `settings_version`, same `volume` dial, same speed ladder — and the extension still reads the
  Hugging Face cache without ever fetching at runtime.
- Commands, panel, keys, the voice picker and the PCM stream are the ones 1.9.1 shipped, apart from the level chain
  above, and apart from a new message now ending one.

## v1.9.3 — A force push, and what an older clone has to do

- **`main` and the tags `v1.8.0` … `v1.9.2` were rewritten, and a force push was necessary.** A clone made before
  2026-11-11 still has those refs naming commits from before the rewrite, so a plain `git pull` does not settle them.
- **An older clone fixes itself with** `git fetch origin --tags --force && git reset --hard origin/main`, or by
  cloning again. Every Release keeps its text and its assets; what changed is the commit each tag names.
- **Nothing was published under this number and no behaviour changed.** It documents git refs, not the artifact: the
  sources, settings format, model, player and protocol each version carries are the ones it shipped with.

## v1.9.2 — The README opens with a logo

- README leads with the project logo, `zot_speaks.jpeg`.

## v1.9.1 — The voice picker has a way back: `b`

* **`b` returns from the voice picker to the settings page.** The picker's footer had promised
  `esc back`, but zot binds Esc to closing an extension panel and answers it itself: an extension is
  never sent a `panel_key esc` at all, so Esc closed the whole panel from either view and someone
  browsing the list of voices had no way back. `b` is a key the host does forward. (ADR-0021)
* **The picker footer reads `enter pick   s last message   f fav   b back`.** It names only what the
  screen cannot show by itself: `f favorite` has become `f fav`, the `s` hint stays, and no clause
  explains that the arrows move the cursor — the cursor is right there to be moved. 44 of the 70 columns
  the row is capped at, so the panel still never changes height between views.
* **The settings-page footers drop the same clause** — `↑/↓ move …` is gone from the Voice row, and from
  Volume and Speed. Nothing else there changes: `arrows or -/+ (audible)`, `· repeats while selected`,
  `left/right off · here · everywhere`, `esc close` all stay.
* **`esc` closes the panel, from the picker as well as from the main view** — which is what it always
  did. Whatever voice was previewing still stops on the way out.
* Under the hood, with no change to settings, model, player or protocol: the panel harness had been
  playing the host badly. It forwarded `esc` to the extension, so every panel check passed while the
  shipped panel stranded the user. It now presses `b` where it means "leave the picker", and sends
  `panel_close` the way the host does, so a preview started inside the picker is asserted to end with
  the panel rather than with a key nobody sends.

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
