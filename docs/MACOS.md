# macOS

macOS is supported by the same code path as Linux: synthesize, stream mono `s16le` PCM into a
player's stdin. Only the player differs. The Release binary is x86-64 Linux only, so on a Mac you
build from source.

**Verified on Linux, not yet on a Mac.** The `ffplay` argv, the PCM bytes, first-sound latency,
stop-with-kill and the error paths are all executed against a fake player by the project's sink test.
Nothing here has been heard on Apple hardware; the checklist at the end is for the person who has one.

## Install

```sh
brew install ffmpeg          # the player: ffplay. Also fine on Linux.
xcode-select --install       # C toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust
```

Then build and install the extension from a clone (this is `just install-ext`, spelled out):

```sh
git clone https://github.com/lovehumans/zot-speaks.git && cd zot-speaks
cargo build --release
mkdir -p "${XDG_DATA_HOME:-$HOME/.local/share}/zot/extensions/zot-speaks"
cp target/release/zot-speaks extension.json \
  "${XDG_DATA_HOME:-$HOME/.local/share}/zot/extensions/zot-speaks/"
zot ext enable zot-speaks
```

`just install-ext` does the same copy and enables the extension. Nothing else in the justfile
installs anything on a Mac: `install-release` and `update-release` fetch the Linux Release asset, and
`fetch-model`/`fetch-voices` only warm the cache of a machine that can already run the binary.

## What plays

`ffplay`, from ffmpeg — the only player zot-speaks uses, and the reason ffmpeg is a prerequisite
rather than a nicety (ADR-0019).

| player | command |
|---|---|
| `ffplay` | `ffplay -nodisp -autoexit -loglevel error -f s16le -ar <rate> -ch_layout mono [-af atempo=<speed>] -` |

One raw PCM stream, one write loop, one stop path, no `cfg!`. The notification names the voice and
the rate it is spoken at — `zot-speaks: speaking as Alice at temp 0.7, vol 100%, speed 1.2x` — and
the player is mentioned only when it fails.

`ZOT_SPEAKS_SINK` and `paplay` existed up to 1.8.2 and are gone: setting that variable now does
nothing at all. `-ac 1` is deliberately not used: newer ffmpeg prefers `-ch_layout mono`, and
`ffmpeg#11009` is about raw-PCM input parameters, not about that flag.

`afplay` is not a player here: it reads a file or URL, never a pipe, so using it would mean writing
the whole utterance to a temp file before you hear anything. `ffplay` reads stdin and starts as soon
as bytes arrive.

The **Speed** setting is a player request, not a model one: Phonon synthesizes at its own 24 kHz
natural rate and `ffplay` is asked for the tempo (`-af atempo=1.2`), which stretches the time without
shifting the pitch.

If `ffplay` starts and then gives up, its own words are reported as an error
(`zot-speaks: ffplay stopped: <its own reason>`) — never a silent failure. Its words have to be the
report: measured with `ffplay` n9.0.2, an unopenable audio device, an unreadable input and an unknown
filter each exit with status **0**, so the exit code alone would call a run that played nothing a
success. Only lines that name a fatal failure count; the warnings a healthy box writes to the same
pipe (pipewire, ALSA) stay silent. A player that is not installed at all is reported as
`no audio player found: install ffmpeg so that \`ffplay\` is on your PATH`.

## Verification checklist (a Mac is required)

Run `zot`, load the extension, and report each row back — including "worked fine".

| # | Do | What it answers |
|---|---|---|
| 1 | `/tts` after a short message | Is there sound, within ~1 s, and does the notification name the voice and the speed? |
| 2 | `/tts` after a long answer | Does it start while the reply is still being synthesized (no whole-utterance delay)? |
| 3 | `/tts`, then `/tts stop` mid-utterance | How long is the tail of silence after the notification? Then `pgrep -lf ffplay`: any leftover process? |
| 4 | `/tts-settings` → `v` → play and preview a voice (`p`, then arrow keys) | Does the preview play, and does an unprefetched voice report a failure instead of hanging? |
| 5 | Watch the screen during 1–4 | Does any window appear (SDL opens one for `ffplay` unless it is suppressed)? Off-screen or minimized counts as a problem to fix |
| 6 | Launch zot the way you normally launch it — terminal, and separately from the GUI (dock/Spotlight) | If `brew install ffmpeg` succeeded but a GUI-launched zot reports no player, `PATH` is the cause: report which launch worked |
| 7 | Hide ffmpeg (`brew unlink ffmpeg`) and run `/tts` | Does the error tell you to install ffmpeg, and does it name `ffplay`? |
| 8 | Auto speak (`/tts-settings` → `a`), with and without `/tts stop` | Does it speak after a real turn, and does stop work there too? |
| 9 | A long utterance at `vol 100`, then at `vol 60` | Is it clean, and does the volume setting take effect? |
| 10 | `/tts-settings` → `s` → `0.5x`, then `2.0x`, and `/tts` at each | Does the rate change while the voice keeps its pitch, and does the note read `speed 0.5x` and `speed 2.0x`? |

What to send back: the notification lines you saw, the answers to 3, 5, 6 and 10, and anything that
sounded wrong. The unknowns are exactly the things only a Mac can answer: whether SDL opens a
window, how long `ffplay`'s stop tail really is, and whether Homebrew's `/opt/homebrew/bin` is on
`PATH` for however zot is started.
