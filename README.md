# zot-speaks

A [zot](https://www.zot.sh) extension that speaks the last agent message aloud, using
[Gradium Phonon](https://huggingface.co/gradium/phonon-31m-en) text-to-speech on your own
machine. No network access, no account, no cloud voice.

[Features](#features-and-anti-features) · [Requirements](#requirements) · [Usage](#usage) ·
[Install](#install) · [For developers](#for-developers) · [Acknowledgements](#acknowledgements) ·
[License](#license) · [Design notes](#design-notes) · [Changelog](CHANGELOG.md)

## Features and Anti-Features

| ✅ We do | ❌ We deliberately don't |
|---|---|
| ✅ Fully offline | ❌ Cloud / online TTS of any kind, network access |
| ✅ CPU-only inference → the GPU stays free for your local LLM | ❌ Telemetry, accounts, anything that phones home |
| ✅ A small English model, `gradium/phonon-31m-en`, with 37 voices | ❌ Voice cloning |
| ✅ Audio streams as it is generated, and stops cleanly |  |
| ✅ Pitch-invariant speed control | |
| ✅ Auto-speak of the final answer, scoped per project | |

Phonon is a 31 M-parameter model: natural speech with imperfect prosody, not a commercial studio
voice. Output is mono 24 kHz, streamed into `ffplay`'s stdin while it is being generated.

## Requirements

- `ffplay`, which comes with `ffmpeg` (`apt install ffmpeg`, `brew install ffmpeg`). It is the one external program zot-speaks needs: it reads raw PCM from a pipe, so speech starts before the sentence is finished, and it is the player with a tempo control — which is what makes the Speed setting mean something.
- CPU only — no GPU, no CUDA, no Metal. That is the point: the GPU stays free for your local LLM.
- [The model](https://huggingface.co/gradium/phonon-31m-en) (99 MiB) and [the voices](https://huggingface.co/gradium/phonon-voices) (9 MiB) it uses must already be in your Hugging Face cache (helper scripts provided); this extension
  reads the cache and never downloads. Cache location: `$HF_HUB_CACHE`, `$HF_HOME/hub`,
  `$XDG_CACHE_HOME/huggingface/hub`, or `$HOME/.cache/huggingface/hub`.
- zot - obviously

## Usage

| Command | What it does |
|---|---|
| `/tts` | Speak the most recent agent message. |
| `/tts stop` | Cut the current utterance off cleanly — no audio tail. |
| `/tts-settings` | Open a panel to pick voice, volume, temperature, speed and auto-speak. |

The settings panel:

```
> Voice        Everly  (37 ready of 37 en voices)
  Volume       55%
  Speed        1.0x
  Temperature  0.30
  Auto speak   off
```

`↑`/`↓` move, `left`/`right` adjust the focused row, `enter` opens the voice picker, `s`
speaks the last message in the highlighted voice, `esc` closes the panel. Backing out of the
picker uses `b`: zot binds Esc to closing an extension panel itself, so no view of the panel can
claim it. Volume and Speed changes speak the selected voice at the new level or rate, and while
the **Temperature** row is selected it keeps saying what it does — *Temperature adds variety to
how a sentence is pronounced.* — repeating until you move off it. In the picker, moving the cursor
**says each voice out loud** (ready voices only), `f` pins a favourite, and voices whose embedding
is not in your cache yet are listed greyed as `not downloaded`; the bootstrap section below gives
the command that fetches them.

Auto-speak reads the agent's final answer aloud when a turn ends. It is scoped per project, and
the row cycles `off → here → everywhere`.

**Speed** is a tempo setting, which stretches the time without shifting
the pitch: it is handed to `ffplay` as ffmpeg's `atempo` filter, never re-timed in the extension.

**Repeats are new takes.** Every utterance is synthesized from its own noise seed, so saying the same
line twice is not a replay — which is what makes the Temperature row audible as you step it. Set
`ZOT_SPEAKS_SEED=42` in the environment when you want a session's utterances reproducible in order.

**Threads.** Inference uses one worker per physical core. Set `ZOT_TTS_THREADS=<n>` to override that on a
machine you know — `1` is the smallest and quietest setting measured, at two to three times the time to
start speaking. The numbers behind both rules are in *Tuning the inference* in [docs/DESIGN.md](docs/DESIGN.md).

## Install

### Linux 
Install once; zot runs it from then on. Nothing is ever downloaded at runtime.

Grab the binary from the latest [Release](https://github.com/lovehumans/zot-speaks/releases)
named `zot-speaks` (x86-64 Linux) together with the `SHA256SUMS` file; the release notes print the same
digest. Every Release lists what changed in it, and that text is [CHANGELOG.md](CHANGELOG.md) in this
repository. On macOS there is no Release binary to grab: build from source, below.

If you have [just](https://github.com/casey/just), two recipes do exactly this — they are
plain `curl` + `sha256sum` + `zot ext install`, readable in the [justfile](justfile):

```sh
git clone https://github.com/lovehumans/zot-speaks
cd zot-speaks
just install-release                    # or pin one: just install-release v1.7.8
```

Then run `just bootstrap` once, before your first `/tts`: `zot-speaks` only ever reads the
Hugging Face cache, so speech stays silent — with a loud warning about the missing snapshot —
until it is filled. See [One-time bootstrap](#one-time-bootstrap) below.

Or fetch, verify and install by hand (no `just` needed):

<details>
<summary>curl + sha256sum + zot ext install, step by step</summary>

```sh
git clone https://github.com/lovehumans/zot-speaks
cd zot-speaks
curl -fsSL -o zot-speaks https://github.com/lovehumans/zot-speaks/releases/latest/download/zot-speaks
curl -fsSL -o SHA256SUMS https://github.com/lovehumans/zot-speaks/releases/latest/download/SHA256SUMS
sha256sum -c SHA256SUMS        # must print: zot-speaks: OK
chmod +x zot-speaks
stage="$(mktemp -d)/zot-speaks"; mkdir -p "$stage"
cp extension.json "$stage/"; mv zot-speaks "$stage/"
zot ext install "$stage"
```

</details>

The clone supplies `extension.json` — the manifest zot needs — and the license; the release supplies
the binary. Verify before installing: `sha256sum -c` fails loudly on a truncated or tampered
download, and you can cross-check the printed hash against the release notes. To pin a version
instead of taking the newest, replace `latest` with the tag in both URLs:
`releases/download/v1.7.8/…`.

Restart zot: `zot ext doctor` should list `zot-speaks` with `/tts` and `/tts-settings` active.

**Updating.** Swap the new files inside the installed directory; your settings (voice, favourites,
per-project auto-speak) live in `settings.json` there and travel untouched. `just update-release`
downloads the binary **and the `extension.json` manifest** the Release publishes, verifies both
against its `SHA256SUMS`, and swaps them; `just update-ext` does the same from a source build. The
manifest matters because zot reads it at load: `zot ext list` reports the version *it* says, so a
swap that carries only the binary leaves an installation advertising a version it does not run.
Releases before v1.7.15 publish the binary alone, and then the recipe says so instead of pretending.
The `/tts-settings` panel reports the same identity on its last row, so the version you are actually
running is readable without leaving zot.

<details>
<summary>Or update by hand (no `just` needed)</summary>

```sh
# zot's state dir: $ZOT_HOME, else $XDG_STATE_HOME/zot, else ~/.local/state/zot
installed="${ZOT_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}/zot}/extensions/zot-speaks"   # or <project>/.zot/extensions/…
tmp="$(mktemp -d)"
base=https://github.com/lovehumans/zot-speaks/releases/latest/download
curl -fsSL -o "$tmp/zot-speaks" "$base/zot-speaks"
curl -fsSL -o "$tmp/extension.json" "$base/extension.json"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
( cd "$tmp" && sha256sum -c SHA256SUMS )
chmod +x "$tmp/zot-speaks"
cp "$tmp/zot-speaks" "$installed/zot-speaks.new"
mv -f "$installed/zot-speaks.new" "$installed/zot-speaks"   # atomic swap on one filesystem
cp "$tmp/extension.json" "$installed/extension.json"        # the version zot reports
```

</details>

A running zot keeps the old binary open; the new one takes effect at the next zot start.

### Building from source

<details>
<summary>Installing by building from source (alternative to the Release binary)</summary>

The same result, compiled on your machine instead of the Actions runner — the route to take on
architectures other than x86-64, **the route on macOS**, and whenever you prefer to build what you
run. On macOS, install the player first: `brew install ffmpeg`; [docs/MACOS.md](docs/MACOS.md)
lists what to expect there and what still needs a Mac to confirm.

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
# zot's state dir: $ZOT_HOME, else $XDG_STATE_HOME/zot, else ~/.local/state/zot
installed="${ZOT_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}/zot}/extensions/zot-speaks"   # or <project>/.zot/extensions/…
cp target/release/zot-speaks "$installed/zot-speaks.new"
mv -f "$installed/zot-speaks.new" "$installed/zot-speaks"
```

`zot ext remove zot-speaks` followed by a fresh install also works, but it deletes the directory,
`settings.json` included.

</details>

### macOS

macOS is source-build only — there is no macOS Release binary. You need a Rust toolchain and
[just](https://github.com/casey/just); see [docs/MACOS.md](docs/MACOS.md).

### Windows

Windows isn't tested yet, but should theoretically work just the same as macOS: build from source, with
`ffplay` from ffmpeg on your `PATH`. If you
test it, let me know!

### One-time bootstrap

`zot-speaks` never downloads: it only reads the Hugging Face cache. Filling that cache is the one
network step, and it needs the upstream `ptts` CLI in the cloned `zot-speaks` dir or on your `PATH` — get it from the [official
releases](https://github.com/gradium-ai/xn-ptts/releases).

```sh
just bootstrap   # the model (~99 MiB) plus all 37 English voices (~9 MiB), once
just cache       # what the extension can see right now: snapshot, and how many voices are ready
```

Or in individual steps. `just fetch-model` fetches the model and one voice in a single run (`just fetch-model
VOICE=Aria` chooses the voice). `just fetch-voices` fills the rest of the English catalog and skips
whatever is already cached, so re-running it is also the coverage check: it prints
`fetched/skipped/failed` counts. Both are thin wrappers over `dev/prefetch-en-voices.sh` and the
`ptts` CLI, so `ptts --voice Miles "warm up"` does the first half by hand too.

You can listen to individual voices [here](https://huggingface.co/gradium/phonon-voices).

If you skip this step, `/tts` warns you: it reports the missing snapshot, the
path it looked under, and `just fetch-model` as the fix — and it never reaches for the network.

Do this before starting zot, or restart zot afterwards: a running extension has already read its
catalog, so voices you fetch mid-session join the picker on the next zot start.

## For developers

### Building

Building, testing and running from a clone — the recipes a fork or a pull request uses, and the
same ones CI uses: CI produces the Release binary by running `cargo build --release` on every
version tag, so `just build` and the Release asset are the same kind of artifact.

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

### Design notes

How it works internally, the settings file and auto-speak scoping, the voice catalog and what it
takes to build, install and test this extension: [docs/DESIGN.md](docs/DESIGN.md).

## Acknowledgements
Thanks to: 

- [xn-ptts](https://github.com/gradium-ai/xn-ptts) — Gradium's Phonon inference stack. This
  extension builds against its `ptts` crate, `just bootstrap` drives its `ptts` CLI to fill the
  model cache, and the checkpoints it serves —
  [phonon-31m-en](https://huggingface.co/gradium/phonon-31m-en) and
  [phonon-voices](https://huggingface.co/gradium/phonon-voices) — are theirs as well.
- [zot](https://github.com/patriceckhart/zot) — the agent this extension speaks for, and the
  extension protocol (`extension.json`, slash commands) it exposes.

## License

MIT only — see [LICENSE](LICENSE). This covers the sources in this repository. Third-party
artifacts keep their own terms: the `ptts` crate this builds against is `MIT OR Apache-2.0`, and the
Phonon model and voice embeddings you cache are upstream downloads under the terms on their Hugging
Face pages (also `MIT OR Apache-2.0`).


