# Changelog

What each published version brings, newest first. This file is where a Release's text comes from:
`dev/publish.sh` refuses to publish a version that has no `## v<version>` section here, and the
published CI feeds that section to `gh release create --notes-file`. A Release's text can be edited
afterwards; a published tag cannot — which is why the notes live in a tracked file instead of in the
tag message.

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
