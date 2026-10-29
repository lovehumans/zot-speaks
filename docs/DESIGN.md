# zot-speaks — design and internals for developers

Rationale and mechanics that a user installing the extension does not need. For what the
extension does and how to run it, see [../README.md](../README.md).

## How it works

* Zot Extension Protocol: JSON-lines over stdin/stdout — `hello` → `hello_ack` →
  `register_command` (`tts`, `tts-settings`) → `subscribe assistant_message, turn_end` → `ready`.
  Panels use `open_panel` / `panel_render` / `panel_close` with `panel_key` events.
* Inference: `gradium/phonon-31m-en`, `Quant::F32`, `DeviceKind::Cpu`, loaded once on a
  background thread; `stream_with` yields mono float PCM chunks so time-to-first-audio is
  independent of utterance length.
* Audio: each chunk is scaled by `Settings::gain` (twice the volume dial), converted to mono s16le,
  and written to the stdin of `ffplay`, the one external program this extension needs:
  `ffplay -nodisp -autoexit -loglevel error -f s16le -ar 24000 -ch_layout mono -`, plus
  `-af atempo=<speed>` whenever the Speed row is off `1.0x`. ffmpeg is a prerequisite, not a
  preference (ADR-0019): `paplay` was dropped along with `ZOT_SPEAKS_SINK`, because a second player
  meant a capability ladder that could quietly discard the speed you had set.
  `afplay` is not a candidate: it reads files, not a pipe, so using it would mean synthesizing the
  whole utterance before hearing any of it.
* Writes are paced to real time with a 40 ms lead, which is what keeps time-to-first-sound low and
  the player's buffer small.
* Stop is cooperative: the `SpeechStream` is dropped and the player's stdin is closed (EOF). An
  utterance that ran to its end gets to drain whatever the player holds; one that was cut short gets
  a 150 ms grace and is then killed rather than left orphaned, because `ffplay` sits on an SDL audio
  buffer. A player that gives up on its own is reported with its
  own words as a notification; one killed on purpose is not. `ffplay`'s exit status is not trusted:
  measured with n9.0.2, an unopenable audio device, an unreadable input and an unknown filter each
  exit **0**, so its stderr is read whatever the status claims and only its fatal lines count as
  failure — the warnings a healthy PipeWire box writes to the same pipe stay silent.

## The settings panel

```
> Voice        Everly  (37 ready of 37 en voices)
  Volume       55%
  Speed        1.0x
  Temperature  0.30
  Auto speak   off
```

Keys: `up`/`down` move the cursor · `left`/`right` (or `-`/`+`) adjust the focused row — a
**Volume or Speed change then says the selected voice out loud at the new level or rate**, so a
level and a rate are something you hear rather than a number you read, and on **Temperature** they
start that row explaining itself out loud, repeating while it stays selected (see *The Temperature
row explains itself*) · on **Auto speak** they step that row around its three values (`off` →
`on · here` → `on · everywhere`) · `enter` on the
**Voice** row opens the picker · `s` speaks the last agent message in the selected voice · `esc`
closes the panel (or returns from the picker). A pinned voice is marked on the Voice row
(`Everly ★`).

Those keys are the panel's footer. Its last row belongs to the body and names what is running:
`zot-speaks v<version>`, taken from the build itself and right-aligned, so it reads as a caption and
not as a sixth setting. Installing an update is the main way this extension changes under someone, and
the panel is where you find out whether it landed. Like the body, every footer row stays within 70
columns, so moving the cursor never changes the panel's height; the caption is padded to exactly that
width, so it never wraps either. It sits in the body rather than in the footer because a footer is one
string: zot prints a newline inside it while still counting it as a single row, so each repaint left a
row of the previous frame standing under the panel.

Voice picker — every **English** catalog voice in three blocks separated by a blank line (pinned
favorites, ready voices, not-yet-downloaded voices), in two columns:

```
Choose a voice (4 ready of 37 en voices)

  ★ 1 favorite
> Everly                     │ female · Middle-age · en-us

  3 ready
  Harper                     │ female · Adult · en-us
  Juniper                    │ female · Young Adult · en-us
  Miles                      │ male · Young Adult · en-us

  Alfie                      │ male · Young Adult · en-gb  (not downloaded)
  Captain Silas Blackwater   │ characters  (not downloaded)
  The Thing Under the Stairs │ characters  (not downloaded)
  ...

  not downloaded — fetch it once:
  ./ptts --voice "Alfie" "hi"
```

Picker keys: `up`/`down` move (and preview) · `enter` picks · `s` speaks the last message in the
highlighted voice · **`f` pins or unpins the highlighted voice as a favorite** · `esc` returns.
`f` moves only that one voice: it drops to the favorite block at the top (or back out of it), the
rest of the list keeps the catalog's order, and the cursor stays on the voice you pressed it on.

The name column widens to the longest name in the catalog (English names reach 26 characters),
so the `│` delimiter always lands in one column and metadata never collides with a name. The
block under the list is always exactly two rows — descriptions are wrapped and truncated to 70
columns by the panel itself — because a frame of a different height would make zot re-center
the panel and the whole list would jump every time the cursor moved.

`enter` selects a **ready** voice and returns to the main view. A `(not downloaded)`
voice is greyed and shows the exact `./ptts` command that would fetch it; selecting one
is a safe no-op (the panel stays open and your saved voice is unchanged). To make the
whole English catalog ready, run the bootstrap in *Local-only by design* below.

### Voice previews

The picker is chosen by ear: moving the cursor speaks the voice it lands on.

```
Hello, my name is Juniper. Warm, expressive voice with a bright, agile delivery…
```

* The line is always `Hello, my name is <name>. <description>`, and only whole sentences of the
  description are read — at most 160 characters of them, falling back to whole words when a
  description has no full stop. A preview is a sentence or two, not the whole catalog entry.
* Moving again cuts the previous preview off within a few tens of milliseconds, so the voice you
  just passed is gone before the new one starts.
* Holding a key and walking past five voices does not queue five auditions: only the voice the
  cursor finally stops on is spoken.
* A `(not downloaded)` voice has no embedding to synthesize, so landing on it goes quiet instead
  of leaving the previous voice talking — and it is never selected.
* `s` speaks the **last agent message** in the highlighted voice: a longer audition than an
  introduction, in a voice you have not committed. `esc` back to the main view, or closing the
  panel, stops whatever is speaking.
* `f` re-auditions the voice it just pinned — hearing it is how you know the pin landed.
* On the main view a **Volume** or **Speed** change is followed by the selected voice saying its own
  name at the new level or rate: the same short line at 40% and at 90%, at 0.8x and at 1.4x, is a
  faster way to choose than reading a number. Temperature gets no such audition, because it is
  neither a level nor a rate — it changes *which* take you get, and a name-only line cannot show
  that. It is spoken about instead, out loud.

### The Temperature row explains itself

Temperature is the one setting a number on a row cannot explain, so while the **Temperature** row
holds the cursor the panel keeps saying what it does, in the voice you selected:

```
Temperature adds variety to how a sentence is pronounced.
```

* It starts when the cursor arrives on the row, and it goes on repeating. What bounds it is the row
  being selected, not a number of repetitions.
* An arrow on the value does not restart the sentence already running. The **next** repetition is
  synthesized at the value you just stepped — which is the point: the same sentence heard at `0.30`
  and then at `0.70` is the demonstration as well as the explanation.
* It ends when the cursor leaves the row, when the panel closes, when `/tts stop` is asked for, or
  when anything else speaks: `s`, another row's audition, an auto-spoken answer.
* Leaving the row and returning starts it again, so an interrupted row is not permanently silent.

The panel schedules no audio and owns no timer. It asks for the repeat and remembers that it asked
(`Preview::Explain`); the playback thread is what repeats the line, re-reading the live settings
before each repetition, until a Stop, a newer request, or an unplayable line ends it — so a broken
player cannot turn an explanation into an endless retry.

## Settings

Settings persist to `<data_dir>/settings.json` (written on the first run with a sensible
default voice, then on every change), so voice / volume / speed / temperature / favorites /
auto-speak survive across sessions and are picked up by the next `/tts`.

`data_dir` is the extension's own installed directory — `<project>/.zot/extensions/zot-speaks`,
or `$ZOT_HOME/extensions/zot-speaks` for a global install — so that file lives beside its
manifest and travels with the directory.

```json
{
  "voice": "Everly",
  "temperature": 0.3,
  "volume": 0.55,
  "speed": 1.2,
  "favorites": ["Everly", "Juniper"],
  "auto_speak": false,
  "auto_speak_projects": {"/work/repo": true},
  "settings_version": 2
}
```

* **Voice** — chosen from the local voice catalog (see *Which voices show up*); passed to the
  model as `SpeechOptions.voice(name)`.
* **Volume** — a dial from `0%` to `100%` in 5% steps, default `50%`. The gain applied when
  converting the model's float PCM to 16-bit samples is **twice** the dial, so `50%` is unity:
  the voice at its own level, neither boosted nor attenuated. `100%` doubles it, which is the
  headroom a quiet voice needs, and the clamp keeps `100%` from clipping anything louder.
* **Speed** — a ladder from `0.5x` to `2.0x` in `0.1` steps, default `1.0x`, stored as the plain
  number you would say (`1.2`). It is a **player** setting, not a model setting: Phonon always
  synthesizes at its own natural rate, and the player is asked for the tempo — `ffplay` through
  ffmpeg's `atempo` filter (`-af atempo=1.2`), which stretches time without shifting pitch. A value
  hand-edited off the ladder (`3.5`) is honoured as the nearest rung and announced, not discarded.
  The real-time pacing of the PCM writer follows the rate asked of the player, so a faster request
  drains faster instead of starving it.
* **Temperature** — how much random noise Phonon puts into generating the speech, `0.0`–`1.0`,
  passed per request (`SpeechOptions.temperature`). Phonon renders speech by **flow matching**:
  it speaks one short frame at a time, and for each frame the model starts from seeded noise and
  shapes it into the next bit of speech. Temperature sets how strong that noise is — exactly,
  the noise is Gaussian with standard deviation `sqrt(temperature)`. This is the upstream
  explanation from [xn-ptts issue #254](https://github.com/gradium-ai/xn-ptts/issues/254#issuecomment-6437586855);
  the ladder it describes, for the default model:
  * `0` removes the noise: one fixed reading per text and voice, and the seed has no effect.
  * Raising it lets timing, pitch and emphasis vary more, so two seeds give more different
    takes. The upstream default is `0.3`, which we keep; from roughly `0.6` up the variation
    becomes clearly audible.
  * Too high and words start to slip: that begins around `2` for the default model, and by `5`
    the speech is often garbled. Other models may differ. Negative values are rejected.

  Each utterance is synthesized from its own noise seed — one per request, counting up from a base
  drawn when the extension starts — so the same sentence spoken twice is two takes, and the line the
  Temperature row repeats while you hold the arrow really does change as you step it. Setting
  `ZOT_SPEAKS_SEED=<n>` pins that base, which replays a run's utterances in order; at temperature `0`
  there is no noise for the seed to vary, so repeats there are identical by design.

  So temperature is neither loudness nor speed — it is the knob for "say that again
  differently", not for "say it louder". The panel clamps it to `0.0`–`1.0`, which keeps the
  settings inside the usable part of that ladder and the degradation regime out of reach.
  Further reading named in the same reply: [How to generate text](https://huggingface.co/blog/how-to-generate)
  (temperature on discrete next-token probabilities), [Continuous Audio Language Models](https://arxiv.org/abs/2509.06926)
  §4.5 (defines it exactly as `sqrt(temperature)` noise for continuous audio frames), and
  [Glow](https://arxiv.org/abs/1807.03039) §6 (an early noise-temperature flow model, but it
  multiplies the standard deviation by `T` itself, so its numbers are not comparable to these).
* **Favorites** — voices pinned with `f`, stored as names, listed in their own block at the top
  of the picker and marked with `★` on the Voice row.
* **Auto speak** — speak the last agent message when the agent finishes, for the project the
  session is in. The row is a ring of three values and the arrows are the only way to move it; the
  two fields below are how those three values are stored, nothing more (`auto_speak` is the default
  every project follows, `auto_speak_projects` holds the projects that answer for themselves).
  A project that agrees with the default stores nothing, so the file lists only exceptions.
* **`settings_version`** — how the numbers in the file are read. v1 files stored `volume` as the
  gain itself (`0.0`–`2.0`, `1.0` = unity); reading one halves it, which is the dial that produces
  the same loudness, so `1.1` becomes `0.55` and nothing sounds different after an upgrade.

### Auto speak

With auto-speak on, the extension subscribes to `turn_end` as well as `assistant_message` and
speaks the cached message when the agent finishes. The stop reason is what makes "finished" mean
something: `turn_end` fires for **every** model step, so a step that ends in `tool_use` is still
working and stays quiet, and the step that produced the answer is the one that gets read out.
The host's own vocabulary decides the name — zot reports the final step as `stop: "end"`, older
documentation still shows the provider's `end_turn` — so the gate lists the reasons that
mean *not finished* (`tool_use`, `error`, `aborted`, `cancelled`, `refusal`, and the empty reason
that arrives with a `turn_end` carrying no stop at all) and treats everything else as an answer.
Guessing the other way round would mean silently never speaking, which is the worse way to be
wrong about a host's names.

Which message is read out follows the **session** whose turn ended. One extension process serves
every session of a project and zot sessions are long-lived, so the cache keeps the latest message
per `session_id` (zot names it on both `assistant_message` and `turn_end`) and a `turn_end` may
only read out what *that* session last said. Otherwise a session you had moved on from would speak
its last answer over the one you are listening to, or a session that never said anything would
speak whatever the other one left in the cache. `/tts` and the picker's `s` are the deliberate
exception: they are you asking for what you last saw, so they read the newest message in the
process whichever session wrote it.

It is scoped per project, because the project you are listening to and the project you are typing
in are not always the same thing. The row cycles, in both directions:

```
  Auto speak   off              quiet here, and no project is on by default
  Auto speak   on · here        this project speaks; the default is still off
  Auto speak   on · everywhere  every project speaks unless it says otherwise
```

`off → here → everywhere → off` for the right arrow, the reverse for the left. Because the row is
the whole storage, a step can only land on one of those three: `here` turns the default off and
records this project as an override, `everywhere` turns the default on and drops this project's
override, `off` turns the default off and drops it too. Two consequences worth knowing: an override
written by one project's `here` is what makes that project speak while everything else stays quiet,
and stepping a project off also steps the default off, so a project that was speaking only because
it followed `everywhere` goes quiet with it. A project whose answer agrees with the default stores
nothing, so the settings file lists only genuine exceptions and a new project inherits the default.
The project is the `cwd` zot reports on `hello_ack` and on every event frame, canonicalized, so the
same directory reached two ways is one entry — and a panel left open follows the session when that
cwd moves, rather than going on editing the project it happened to open in.

## Local-only by design

`zot-speaks` **reads** the Hugging Face cache; it **never downloads** models or voices at runtime.
This is a deliberate design choice, not an accident:

* `ptts` is built with its **`hf`** feature (cache resolution) and deliberately **not**
  its **`hub`** feature. `hub` pulls `ureq → rustls →` platform TLS / assembly crates (`ring`,
  `blake3` AVX2 asm), but runtime downloads are
  simply not wanted here.
* Consequence: a voice or model that is not already in the cache is simply unavailable;
  the extension reports it instead of fetching it.

### Cache location precedence

Model and voice snapshots are looked up under the first of:

1. `$HF_HUB_CACHE`
2. `$HF_HOME/hub`
3. `$XDG_CACHE_HOME/huggingface/hub`
4. `$HOME/.cache/huggingface/hub`

### One-time bootstrap

`just bootstrap` fills the cache the extension then reads (`just fetch-model` and `just fetch-voices`
are its two halves; `just cache` reports the result). Underneath they are the `ptts` CLI and
`dev/prefetch-en-voices.sh`, and the same thing by hand is:

```sh
ptts --voice Miles "warm up"                   # fetches model + one voice together
# or explicitly, with the huggingface CLI:
hf download gradium/phonon-31m-en
hf download gradium/phonon-voices voices.jsonl voices/<file>.safetensors
```

To make the **whole English catalog** ready in one go, the companion script reads
`voices.jsonl` straight out of the cache, skips what is already there, and synthesises once per
missing voice so `ptts` fetches that voice's embedding:

```sh
dev/prefetch-en-voices.sh                      # every en voice not yet cached
dev/prefetch-en-voices.sh Alfie "Captain Silas Blackwater"   # just these (by name or voice_id)
VOICE_LANG=fr dev/prefetch-en-voices.sh        # other languages, explicitly
```

Do this before starting zot, or restart zot afterwards: an extension that is already running has
already read its catalog, so it keeps the availability it saw at startup.

### Which voices show up

Two filters decide what is offered:

1. **Language.** The model is `gradium/phonon-31m-en`, an English model, so `zot-speaks`
   only offers catalog entries whose `language` is `en` (`tts::MODEL_LANGUAGE`). Foreign
   voices are ignored **even if already cached** — the picker stays readable and the model
   is not fed embeddings it cannot use.
2. **Local availability.** The snapshot carries a full `voices.jsonl` (name, voice_id, filename,
   language, locale, gender, age, description), but a voice only becomes *selectable* once its
   embedding `.safetensors` is present locally, because that is the only way to register it without
   a download. Not-yet-downloaded voices are still listed in the picker, marked `(not downloaded)`,
   with the `./ptts` command that would fetch them. That availability is decided when `zot-speaks`
   starts: the catalog is read once, at startup, so a voice you fetch while zot is already running
   joins the picker on the next zot start, not in the session that fetched it.

## Tuning the inference

**`ZOT_TTS_THREADS=<n>`** sets how many worker threads inference may use. It is exported as
`RAYON_NUM_THREADS`, because that is the variable the pool actually reads: `ptts` parallelises
through `xn`, whose work-stealing pool is rayon's, and rayon sizes that pool from `RAYON_NUM_THREADS`
— and fixes its width the first time it is used, which is why the pin is applied before the model is
loaded rather than per request.

The default is one worker per **physical** core: the distinct (`physical id`, `core id`) pairs in
`/proc/cpuinfo`, clamped to what `available_parallelism()` reports — 8 here, on a Ryzen 7 7800X3D with 16
logical CPUs. A kernel that names no core ids (some ARM ones do not) gets the logical count instead.
`ZOT_TTS_THREADS=<n>` overrides the rule with any positive number, larger than the core count if you know
your box better; `0` or a value that does not parse is not a pin, and the rule is used without saying so.
The `OMP_NUM_THREADS` and `ORT_*` variables the same function still exports alongside `RAYON_NUM_THREADS`
are read by nothing in the dependency tree (there is no ONNX Runtime in `Cargo.lock`), so a pin written
into them changes nothing — which is exactly what this extension did before 2026-10-29.

Measured through the extension's own library path (`examples/stream_smoke.rs`, one 8.00 s utterance, 3
reps per point): time to first sound 29–32 ms at 8 workers, 33–39 ms at 4, 49–52 ms at 16, 83–102 ms at 2,
and 505–515 ms / 547–597 ms / 923–959 ms / 801–837 ms end to end for those counts. One worker per logical
CPU is the worst point on an SMT box — a hyperthread adds dispatch and no throughput to a vectorised
gemm. The same rule through the `ptts` CLI on the shipped `Quant::F32` weights (7.68 s sentence, voice
`Miles`, seed `42`, temperature `0.3`) reads 33–35 ms at 8 threads, 38–40 at 4, 44–57 at 16, 103–115 at 1.
Peak RSS stays in a 300–380 MB band at every count tried — 301–308 MB at 1 worker, 334–341 MB at 8 and
320–336 MB at 16 in the most recent three-run pass — so this knob is about latency rather than memory: a
single worker is the quietest setting measured here and two to three times slower to first sound.

Why there is no sibling `ZOT_SPEAKS_QUANT`, and what quantization would buy, is in
[Quantization.md](Quantization.md).

## Naming and installed state

The manifest `name` in `extension.json` must match the `name` the extension sends in its own
`hello` frame (`main::NAME`), and the state file lives inside the
installed directory. Moving the extension therefore means renaming the installed directory, which
carries `settings.json` along:

```sh
mv ~/.local/state/zot/extensions/zot-speaks ~/.local/state/zot/extensions/<new-name>
```

Installing two directories that both register `/tts` gives first-come-first-served shadowing: one
of the two commands is silently dead, and the newcomer starts from an empty state file.
`zot ext logs` keys on the manifest name, so a rename starts a fresh log. `zot ext remove` accepts
either the manifest name or the directory name.

## Build and test

```sh
just build      # -> target/release/zot-speaks, the binary extension.json execs
just test       # unit tests: settings, voice catalog filtering/ordering, panel state machine
just smoke      # real inference: synthesise a phrase, report time-to-first-sound
just lint       # cargo clippy
```

`just` wraps cargo, one recipe per command; run the cargo commands directly if you prefer.

Use the **release** build for live playback. The debug build generates audio slower than real time
and would stutter playback; the release build runs at about realtime. See `Cargo.toml`
`[profile.release]` (`opt-level=3`, `lto`, `panic="abort"`).

### Installing what you built

`just install-ext` stages `extension.json` plus the release binary in a scratch directory and
gives that to `zot ext install`, which copies it to `$ZOT_HOME/extensions/zot-speaks/`. Build
first, install second — zot does not build anything itself.

Do not overwrite `./zot-speaks` in place while zot has the extension loaded: the file is being
executed and the write fails with `ETXTBSY`. Write to a temporary name and rename into place, or
install a fresh copy.

`cargo` needs a writable `CARGO_HOME` and `CARGO_TARGET_DIR`. If `~/.cargo` is not writable, point
them somewhere you can write, run `cargo fetch` once with network, and build with `--offline`
afterwards. An offline build that reports `no matching package named \`ptts\` found` is missing its
crate cache, not a source problem.

## Known limits and open questions

* **Speed rides on ffmpeg's filter, not on the model.** Phonon exposes no native tempo control, so
  `atempo` (see the *Speed* setting) is what makes the dial mean anything, and it is a linear
  time-stretch: it cannot repair prosody, and the artifacts it does introduce are ffmpeg's. A
  player-independent stretch (WSOLA or a phase vocoder in our own write path) would free the setting
  from `ffplay`. Not scheduled.
* **Quantisation is measured, and declined.** The Rust API exposes `Quant` (`q8_0`, `q4k`, CPU-only)
  and `ptts` exposes `--quant`; this extension uses `Quant::F32`. On this box `q8_0` saves about 50 MB
  of peak memory and costs the first sound — roughly 4 ms at 8 threads, 15–17 ms at 16 — so it is
  neither shipped nor exposed. [Quantization.md](Quantization.md) holds the matrix, the controls, and
  the recipe for re-measuring on a box where memory binds instead of latency. Note that a quantised
  build needs the matching weights file locally: the extension reads the cache and never downloads
  one.
* **Auto speak needs a session that outlives its turn.** `turn_end` and `shutdown` arrive together
  when zot is run one-shot (`zot -p "…"`, or a prompt on a pipe), so there is no time left to
  synthesize and play before the host tears the extension down; auto speak is an interactive-session
  feature. `/tts` in a one-shot run is cut off the same way.
* Mono, 24 kHz output, piped to `ffplay`; ffmpeg is a prerequisite (ADR-0019); CPU inference only.
* **macOS playback has not been heard on a Mac by this project's CI.** The `ffplay` path is real and
  its contract is tested against a stand-in player, but first sound, stop tail and any SDL behaviour
  on macOS are a human check on real hardware.
