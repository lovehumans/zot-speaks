# macOS

macOS is supported by the same code path as Linux: synthesize, stream mono `s16le` PCM into a
player's stdin. Only the player differs. The Release binary is x86-64 Linux only, so on a Mac you
build from source.

**Verified on Linux, not yet on a Mac.** The player chain, the exact `ffplay` argv, the PCM bytes,
first-sound latency, stop-with-kill and the error paths are all executed against a fake player by
the project's sink-chain test. Nothing here has been heard on Apple hardware; the checklist at the
end is for the person who has one.

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

| player | command |
|---|---|
| `paplay` | `paplay --raw --format=s16le --channels=1 --rate <rate>` |
| `ffplay` | `ffplay -nodisp -autoexit -loglevel error -f s16le -ar <rate> -ch_layout mono -` |

One raw PCM stream, one write loop, one stop path, no `cfg!`: the first player found on `PATH` is
used, `ffplay` before `paplay` (ADR 2026-10-28). On a Mac that means `ffplay`, and the notification
says which one it is: `zot-speaks: speaking as Alice at temp 0.7, vol 100% through ffplay`.

Override it with `ZOT_SPEAKS_SINK=paplay|ffplay` (empty or unset picks automatically; an unknown
name is an error, not a silent fallback). `paplay` is not something a Mac has out of the box — the
pin is here for Linux boxes that want the native PipeWire/PulseAudio player instead of ffmpeg.
`-ac 1` is deliberately not used: newer ffmpeg prefers `-ch_layout mono`, and `ffmpeg#11009` is
about raw-PCM input parameters, not about that flag.

`afplay` is not a player here: it reads a file or URL, never a pipe, so using it would mean writing
the whole utterance to a temp file before you hear anything. `ffplay` reads stdin and starts as soon
as bytes arrive.

Playback speed is still a macOS-only idea (`ffplay` supports `atempo`, `paplay` has no equivalent),
and it stays unimplemented until it is measured on a Mac.

If `ffplay` starts and then gives up, the first line of its stderr is reported as an error
(`zot-speaks: ffplay stopped: <its own reason>; or try ZOT_SPEAKS_SINK=paplay`) — never a silent
failure, and no player is retried behind your back. A player that is not installed at all is
reported as `brew install ffmpeg`, or as the pin to set.

## Verification checklist (a Mac is required)

Run `zot`, load the extension, and report each row back — including "worked fine".

| # | Do | What it answers |
|---|---|---|
| 1 | `/tts` after a short message | Is there sound, within ~1 s, and does the notification say `through ffplay`? |
| 2 | `/tts` after a long answer | Does it start while the reply is still being synthesized (no whole-utterance delay)? |
| 3 | `/tts`, then `/tts stop` mid-utterance | How long is the tail of silence after the notification? Then `pgrep -lf ffplay`: any leftover process? |
| 4 | `/tts-settings` → `v` → play and preview a voice (`p`, then arrow keys) | Does the preview play, and does an unprefetched voice report a failure instead of hanging? |
| 5 | Watch the screen during 1–4 | Does any window appear (SDL opens one for `ffplay` unless it is suppressed)? Off-screen or minimized counts as a problem to fix |
| 6 | Launch zot the way you normally launch it — terminal, and separately from the GUI (dock/Spotlight) | If `brew install ffmpeg` succeeded but a GUI-launched zot reports no player, `PATH` is the cause: report which launch worked |
| 7 | `ZOT_SPEAKS_SINK=ffplay zot`, then `ZOT_SPEAKS_SINK=paplay zot` | Does the pin work, and does an unavailable pin name itself in the error? |
| 8 | Hide ffmpeg (`brew unlink ffmpeg`) and run `/tts` | Does the error say `brew install ffmpeg`? |
| 9 | Auto speak (`/tts-settings` → `a`), with and without `/tts stop` | Does it speak after a real turn, and does stop work there too? |
| 10 | A long utterance at `vol 100`, then at `vol 60` | Is it clean, and does the volume setting take effect? |

What to send back: the notification lines you saw, the answers to 3, 5 and 6, and anything that
sounded wrong. The unknowns are exactly the things only a Mac can answer: whether SDL opens a
window, how long `ffplay`'s stop tail really is, and whether Homebrew's `/opt/homebrew/bin` is on
`PATH` for however zot is started.
