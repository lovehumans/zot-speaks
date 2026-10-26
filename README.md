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

Install once; zot runs it from then on. Nothing is ever downloaded at runtime.

The repository holds sources only — no binary is committed. GitHub Actions compiles one on every
version tag and attaches it to that tag's [Release](https://github.com/lovehumans/zot-speaks/releases)
as `zot-speaks` (x86-64 Linux) together with a `SHA256SUMS` file; the release notes print the same
digest. Fetch, verify, install:

```sh
git clone https://github.com/lovehumans/zot-speaks
cd zot-speaks
curl -fsSL -o zot-speaks https://github.com/lovehumans/zot-speaks/releases/latest/download/zot-speaks
curl -fsSL -o SHA256SUMS https://github.com/lovehumans/zot-speaks/releases/latest/download/SHA256SUMS
sha256sum -c SHA256SUMS        # must print: zot-speaks: OK
stage="$(mktemp -d)/zot-speaks"; mkdir -p "$stage"
cp extension.json "$stage/"; mv zot-speaks "$stage/"
zot ext install "$stage"
```

The clone supplies `extension.json` — the manifest zot needs — and the license; the release supplies
the binary. Verify before installing: `sha256sum -c` fails loudly on a truncated or tampered
download, and you can cross-check the printed hash against the release notes. To pin a version
instead of taking the newest, replace `latest` with the tag in both URLs:
`releases/download/v1.7.5/…`.

Restart zot: `zot ext doctor` should list `zot-speaks` with `/tts` and `/tts-settings` active.

**Updating.** Download the new binary and swap it inside the installed directory; your settings
(voice, favourites, per-project auto-speak) live in `settings.json` there and travel untouched:

```sh
installed="$HOME/.local/state/zot/extensions/zot-speaks"   # or <project>/.zot/extensions/…
tmp="$(mktemp -d)"
curl -fsSL -o "$tmp/zot-speaks" https://github.com/lovehumans/zot-speaks/releases/latest/download/zot-speaks
curl -fsSL -o "$tmp/SHA256SUMS" https://github.com/lovehumans/zot-speaks/releases/latest/download/SHA256SUMS
( cd "$tmp" && sha256sum -c SHA256SUMS )
cp "$tmp/zot-speaks" "$installed/zot-speaks.new"
mv -f "$installed/zot-speaks.new" "$installed/zot-speaks"   # atomic swap on one filesystem
```

A running zot keeps the old binary open; the new one takes effect at the next zot start.

### Installing by building from source

The same result, compiled on your machine instead of the Actions runner — the route to take on
architectures other than x86-64, or whenever you prefer to build what you run:

```sh
git clone https://github.com/lovehumans/zot-speaks
cd zot-speaks
just install-ext      # cargo build --release, then zot ext install
```

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

**Updating a source install.** `zot ext install` refuses to overwrite an existing installation. To
keep your settings, swap the freshly built binary inside the installed directory:

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

`zot-speaks` never downloads: it only reads the Hugging Face cache. Filling that cache is the one
network step, and it needs the upstream `ptts` CLI on your `PATH` — get it from the [official
releases](https://github.com/gradium-ai/xn-ptts/releases).

```sh
just bootstrap   # the model (~102 MB) plus all 37 English voices (~9 MiB), once
just cache       # what the extension can see right now: snapshot, and how many voices are ready
```

Or in pieces. `just fetch-model` fetches the model and one voice in a single run (`just fetch-model
VOICE=Aria` chooses the voice). `just fetch-voices` fills the rest of the English catalog and skips
whatever is already cached, so re-running it is also the coverage check: it prints
`fetched/skipped/failed` counts. Both are thin wrappers over `dev/prefetch-en-voices.sh` and the
`ptts` CLI, so `ptts --voice Miles "warm up"` does the first half by hand too.

You can listen to individual voices [here](https://huggingface.co/gradium/phonon-voices).

If you skip this step, `/tts` says so instead of guessing: it reports the missing snapshot, the
path it looked under, and `just fetch-model` as the fix — and it never reaches for the network.

Do this before starting zot, or restart zot afterwards: a running extension has already read its
catalog, so voices you fetch mid-session join the picker on the next zot start.

## Build

This is also exactly how the release binary is produced: CI runs `cargo build --release` on
every version tag, so `just build` and the Release asset are the same kind of artifact.

```sh
just build      # cargo build --release   ->  target/release/zot-speaks
just test       # the test suite
just smoke      # streaming smoke test: prints time-to-first-sound
just lint       # cargo clippy
just run CMD="/tts hello"   # one command against the built binary
```

`just` is a thin wrapper: every build recipe is the cargo command it names, so you can run those
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
