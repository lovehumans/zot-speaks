# zot-speaks

A [zot](https://www.zot.sh) extension that speaks the last agent message aloud, using
[Gradium Phonon](https://huggingface.co/gradium/phonon-31m-en) text-to-speech on your own
machine. No network access, no account, no cloud voice.

## Features and Anti-Features

| ✅ We do | ❌ We deliberately don't |
|---|---|
| ✅ Offline, no network access | ❌ Cloud / online TTS of any kind |
| ✅ CPU-only inference → the GPU stays free for your local LLM | ❌ Telemetry, accounts, anything that phones home |
| ✅ A small permissive English model, `gradium/phonon-31m-en` | ❌ Voice cloning — this checkpoint has no speaker encoder |
| ✅ Streams while generating; `/tts stop` cuts off cleanly | ❌ Speed / tempo control |
| ✅ Auto-speak of the final answer, scoped per project | |

Phonon is a 31 M-parameter model: natural speech with imperfect prosody, not a commercial studio
voice. Output is mono 24 kHz through `paplay`. Speaking needs a live interactive session — a
one-shot `zot -p "…"` tears the extension down before the audio finishes.

## Requirements

- Linux with `paplay` (PipeWire / PulseAudio).
- CPU only — no GPU, no CUDA, no Metal. That is the point: the GPU stays free for your local LLM.
- The model and the voices it uses must already be in your Hugging Face cache; this extension
  reads the cache and never downloads. Cache location: `$HF_HUB_CACHE`, `$HF_HOME/hub`,
  `$XDG_CACHE_HOME/huggingface/hub`, or `$HOME/.cache/huggingface/hub`.
- English only: `phonon-31m-en` is an English model, so only English catalog voices are offered.

## Usage

| Command | What it does |
|---|---|
| `/tts` | Speak the most recent agent message. |
| `/tts stop` | Cut the current utterance off cleanly — no audio tail. |
| `/tts-settings` | Open a panel to pick voice, volume, temperature and auto-speak. |

The settings panel:

```
> Voice        Everly  (37 ready of 37 en voices)
  Volume       55%
  Temperature  0.30
  Auto speak   off
```

`up`/`down` move, `left`/`right` adjust the focused row, `enter` opens the voice picker, `s`
speaks the last message in the highlighted voice, `esc` closes. In the picker, moving the cursor
**says each voice out loud** (ready voices only), `f` pins a favourite, and voices whose embedding
is not in your cache yet are listed greyed as `not downloaded`; the bootstrap section below gives
the command that fetches them.

Auto-speak reads the agent's final answer aloud when a turn ends. It is scoped per project, and
the row cycles `off → here → everywhere`.

## Install

You build it once; zot runs it from then on. Nothing is downloaded at runtime.

```sh
git clone https://github.com/lovehumans/zot-speaks
cd zot-speaks
just install-ext      # cargo build --release, then zot ext install
```

Restart zot: `zot ext doctor` should list `zot-speaks` with `/tts` and `/tts-settings` active.
Without `just`, the same two steps are:

```sh
cargo build --release
stage="$(mktemp -d)/zot-speaks"; mkdir -p "$stage"
cp extension.json "$stage/"; cp target/release/zot-speaks "$stage/zot-speaks"
zot ext install "$stage"
```

Copying the directory into `$ZOT_HOME/extensions/zot-speaks/` (or
`<project>/.zot/extensions/zot-speaks/` for a single project) works too, provided the binary
named by `extension.json` (`./zot-speaks`) sits next to `extension.json`. Settings are kept in
`settings.json` inside the installed directory, so they travel with it.

**Updating.** `zot ext install` refuses to overwrite an existing installation. To keep your
settings (voice, favourites, per-project auto-speak), swap the binary inside the installed
directory:

```sh
just update-ext        # same directory, new binary, settings.json untouched
```

Or by hand — write the new binary under a temporary name and rename it into place:

```sh
installed="$HOME/.local/state/zot/extensions/zot-speaks"   # or <project>/.zot/extensions/…
cp target/release/zot-speaks "$installed/zot-speaks.new"
mv -f "$installed/zot-speaks.new" "$installed/zot-speaks"
```

`zot ext remove zot-speaks` followed by a fresh install also works, but it deletes the directory,
`settings.json` included.

### One-time bootstrap

Cache the model and at least one voice (250 kiB) once, with the upstream `ptts` CLI on your `PATH`
(a `./ptts` next to these scripts works too). The first command fetches model and voice together:

```sh
ptts --voice Miles "warm-up"
```

You can listen to individual voices [here](https://huggingface.co/gradium/phonon-voices).

Get ptts from the [official releases](https://github.com/gradium-ai/xn-ptts/releases).

To make the whole English catalog (9 MiB) ready, run the companion script. It reads the voice catalog from
your cache, skips what is already there, and synthesises once per missing voice so `ptts` fetches
that voice's embedding:

```sh
dev/prefetch-en-voices.sh                        # every en voice not yet cached
dev/prefetch-en-voices.sh Alfie "Captain Silas Blackwater"   # just these
```

Do this before starting zot, or restart zot afterwards: a running extension has already read its
catalog, so voices you fetch mid-session join the picker on the next zot start.

## Build

```sh
just build      # cargo build --release   ->  target/release/zot-speaks
just test       # the test suite
just smoke      # streaming smoke test: prints time-to-first-sound
just lint       # cargo clippy
just run CMD="/tts hello"   # one command against the built binary
```

`just` is a thin wrapper: every recipe is the cargo command it names, so you can run those
directly instead. Use the release build for live playback; the debug build generates audio slower
than real time. If `cargo` cannot write `~/.cargo` or cannot reach crates.io, pin `CARGO_HOME` and
`CARGO_TARGET_DIR` to directories you can write, run `cargo fetch` once with network, and build
with `--offline` afterwards.

## License

MIT only — see [LICENSE](LICENSE). This covers the sources in this repository. Third-party
artifacts keep their own terms: the `ptts` crate this builds against is `MIT OR Apache-2.0`, and the
Phonon model and voice embeddings you cache are upstream downloads under the terms on their Hugging
Face pages.

## Design notes

How it works internally, the settings file and auto-speak scoping, the voice catalog and what it
takes to build, install and test this extension: [docs/DESIGN.md](docs/DESIGN.md).
