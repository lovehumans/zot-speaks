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
* Audio: each chunk is scaled by `Settings::gain` (twice the volume dial) and piped to
  `paplay --raw --format=s16le --channels=1 --rate 24000`, paced to real time.
* Stop is cooperative: the `SpeechStream` is dropped and `paplay`'s stdin is closed (EOF),
  so generation and playback halt without signals.

## The settings panel

```
> Voice        Everly  (37 ready of 37 en voices)
  Volume       55%
  Temperature  0.30
  Auto speak   off
```

Keys: `up`/`down` move the cursor · `left`/`right` (or `-`/`+`) adjust the focused row — a
**Volume change then says the selected voice out loud at the new level**, so a level is something
you hear rather than a number you read, and on **Auto speak** they step that row around its three
values (`off` → `on · here` → `on · everywhere`) · `enter` on the **Voice** row opens the picker ·
`s` speaks the last agent message in the selected voice · `esc` closes the panel (or returns from
the picker). A pinned voice is marked on the Voice row (`Everly ★`).

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
* On the main view a **Volume** change is followed by the selected voice saying its own name at
  the new level: the same short line at 40% and at 90% is a faster way to choose a level than
  reading a percentage. Temperature changes stay silent, because temperature is not a level.

## Settings

Settings persist to `<data_dir>/settings.json` (written on the first run with a sensible
default voice, then on every change), so voice / volume / temperature / favorites / auto-speak
survive across sessions and are picked up by the next `/tts`.

`data_dir` is the extension's own installed directory — `<project>/.zot/extensions/zot-speaks`,
or `$ZOT_HOME/extensions/zot-speaks` for a global install — so that file lives beside its
manifest and travels with the directory.

```json
{
  "voice": "Everly",
  "temperature": 0.3,
  "volume": 0.55,
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
* **Temperature** — how much random noise Phonon puts into generating the speech, `0.0`–`1.0`,
  passed per request (`SpeechOptions.temperature`). Phonon renders speech by **flow matching**:
  the sampler integrates a latent path that starts from seeded Gaussian noise with standard
  deviation `sqrt(temperature)`. So temperature is neither loudness nor speed — it is how far a
  take drifts from the model's most likely delivery. `0.0` removes the noise, and the same text,
  voice and seed then render identically every time; `0.3` (the default) is the model's own
  prosody; above roughly `0.6` the drift becomes audible as variation and roughness rather than
  expression. It is the knob for "say that again differently", not for "say it louder".
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

Cache the model once (the extension then only reads it):

```sh
./ptts --voice Miles "warm-up"                 # fetches model + one voice together
# or explicitly:
hf download gradium/phonon-31m-en
hf download gradium/phonon-voices voices.jsonl voices/<file>.safetensors
```

To make the **whole English catalog** ready in one go, use the companion script. It reads
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
and would stutter `paplay`; the release build runs at about realtime. See `Cargo.toml`
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

* **No speed / time-stretch control.** Phonon exposes no native tempo control, so a
  pitch-preserving time-stretch would have to be implemented client-side. Open, not scheduled.
* **Quantisation is untried.** The Rust API exposes `Quant` (`q8_0`, `q4k`, CPU-only) and `ptts`
  exposes `--quant`; this extension uses `Quant::F32`. Worth measuring before changing it.
  Note that the shipped model weights are what your cache holds — a quantised build needs the
  matching weights file locally.
* **Auto speak needs a session that outlives its turn.** `turn_end` and `shutdown` arrive together
  when zot is run one-shot (`zot -p "…"`, or a prompt on a pipe), so there is no time left to
  synthesize and play before the host tears the extension down; auto speak is an interactive-session
  feature. `/tts` in a one-shot run is cut off the same way.
* Mono, 24 kHz output through `paplay`; CPU inference only.
