# zot-speaks — one entrypoint for every dev task. Run `just` to list recipes.
# Any CARGO_* variable already in the environment is honoured by cargo as usual.

export CARGO_TARGET_DIR := env_var_or_default('CARGO_TARGET_DIR', 'target')

BIN := CARGO_TARGET_DIR + "/release/zot-speaks"

# Release assets live here; override RELEASE_URL to rehearse the recipes against a mirror
RELEASE_URL := env_var_or_default('RELEASE_URL', 'https://github.com/lovehumans/zot-speaks/releases')

# list recipes
default:
    @just --list

# ---- one-time bootstrap ---------------------------------------------------------------
# zot-speaks never downloads: it only reads the Hugging Face cache. These recipes are the
# network step, run once. They need the upstream `ptts` CLI: `just fetch-ptts` fetches the
# version this artifact is built against, or install it yourself from
# https://github.com/gradium-ai/xn-ptts/releases, put it on PATH, or set PTTS=/path/to/ptts.

# Which model commit a cache warm resolves is decided by the ptts library, so a helper older than the
# one the artifact builds against warms the cache with an older one's weights, and ./ptts is gitignored
# so nothing would ever flag it. Lands where the two fetch recipes look first.
# fetch the upstream ptts CLI at the version Cargo.toml names, verified, as ./ptts
fetch-ptts:
    @set -eu; \
    url=https://github.com/gradium-ai/xn-ptts/releases/download; \
    ver=$(sed -n 's/^ptts = .*version = "\([0-9.]*\)".*/\1/p' Cargo.toml | head -1); \
    [ -n "$ver" ] || { echo "fetch-ptts: no ptts version in Cargo.toml"; exit 1; }; \
    case "$(uname -s) $(uname -m)" in \
    "Linux x86_64") target=x86_64-unknown-linux-gnu ;; \
    "Linux aarch64") target=aarch64-unknown-linux-gnu ;; \
    "Darwin x86_64") target=x86_64-apple-darwin ;; \
    "Darwin arm64") target=aarch64-apple-darwin ;; \
    *) echo "fetch-ptts: no upstream archive named for $(uname -s) $(uname -m); install the ptts CLI yourself"; exit 1 ;; \
    esac; \
    asset=ptts-$ver-$target.tar.gz; \
    tmp=$(mktemp -d); trap 'rm -rf "$tmp"' INT TERM EXIT; \
    curl -fsSL -o "$tmp/$asset" "$url/v$ver/$asset"; \
    curl -fsSL -o "$tmp/SHA256SUMS" "$url/v$ver/SHA256SUMS"; \
    grep "  $asset\$" "$tmp/SHA256SUMS" > "$tmp/check" || \
        { echo "fetch-ptts: $asset is not listed in the release checksums"; exit 1; }; \
    ( cd "$tmp" && if command -v sha256sum >/dev/null 2>&1; then sha256sum -c check; else shasum -a 256 -c check; fi ); \
    tar -xzf "$tmp/$asset" -C "$tmp"; \
    mv -f "$tmp/ptts-$ver-$target/ptts" ./ptts; \
    chmod +x ./ptts; \
    echo "fetch-ptts: ./ptts is now $ver for $target, the version Cargo.toml names"

# Cache the model (~102 MB) and one voice together. VOICE picks the voice: just fetch-model VOICE=Aria
fetch-model VOICE="Miles":
    @ptts="${PTTS:-}"; [ -n "$ptts" ] || { if [ -x ./ptts ]; then ptts=./ptts; else ptts=ptts; fi; }; \
    command -v "$ptts" >/dev/null || { \
        echo "fetch-model: no 'ptts' found ($ptts). Install it from https://github.com/gradium-ai/xn-ptts/releases"; \
        echo "fetch-model: or point at it: PTTS=/path/to/ptts just fetch-model"; exit 1; }; \
    "$ptts" --voice {{VOICE}} --lang en -o "${TMPDIR:-/tmp}/zot-speaks-warmup.wav" "warm up"; \
    echo "fetch-model: cached gradium/phonon-31m-en and the voice '{{VOICE}}'"

# Fetch every English catalog voice (~9 MiB), skipping cached ones. Subset: VOICES="Alfie Zoey"
fetch-voices VOICES="":
    dev/prefetch-en-voices.sh {{VOICES}}

# both, in the order that works: model first, then the whole English catalog
bootstrap: fetch-model fetch-voices

# show what the extension can see: model snapshot + voices ready (hub precedence as in cache.rs).
# The snapshot named is the one the artifact will load: `MODEL_REVISION` in src/tts.rs, read here so
# there is one place that says which revision that is. A cache that lacks it gets the one it has, and
# this says which of the two is being reported.
cache:
    @hub="${HF_HUB_CACHE:-}"; \
    if [ -z "$hub" ]; then \
        if [ -n "${HF_HOME:-}" ]; then hub="$HF_HOME/hub"; \
        elif [ -n "${XDG_CACHE_HOME:-}" ]; then hub="$XDG_CACHE_HOME/huggingface/hub"; \
        else hub="$HOME/.cache/huggingface/hub"; fi; \
    fi; \
    model="$hub/models--gradium--phonon-31m-en"; voices="$hub/models--gradium--phonon-voices"; \
    want=$(sed -n 's/^const MODEL_REVISION: &str = "\([0-9a-f]*\)".*/\1/p' src/tts.rs | head -1); \
    snaps=$(ls -d "$model/snapshots"/* 2>/dev/null); \
    snap=$(printf '%s\n' "$snaps" | grep "/${want}$" | head -1); \
    if [ -z "$snap" ]; then snap=$(printf '%s\n' "$snaps" | head -1); fi; \
    if [ -z "$snap" ]; then echo "model: not cached under $hub -> run 'just bootstrap'"; \
    elif [ -n "$want" ] && [ "${snap##*/}" != "$want" ]; then \
        echo "model: $snap ($(du -sh "$model" | cut -f1) cached) — this build names $want, which is"; \
        echo "       not cached; the extension loads this one and says so on stderr"; \
    else \
        echo "model: $snap ($(du -sh "$model" | cut -f1) cached)"; \
    fi; \
    if [ -d "$voices" ]; then \
        echo "voices: $(find "$voices" -name '*.safetensors' | wc -l) embedding(s) cached, 37 in the English catalog"; \
    else echo "voices: no catalog cached -> run 'just bootstrap'"; fi

# ---- install from the GitHub Release (the recommended route) ---------------------------
# GitHub Actions builds one binary per version tag and publishes it as the Release asset
# `zot-speaks` together with `SHA256SUMS`. These recipes are the README's curl + sha256sum +
# zot ext install, written down: no cargo, no toolchain. Run them from the clone root — the
# clone supplies extension.json. Pin a version: just install-release v1.7.8
# Linux x86-64 only: it is the one platform a Release binary is shipped for. On macOS build from
# source with `install-ext` below, after `brew install ffmpeg`.

# download the release binary, verify its digest, stage it with extension.json, install it
install-release TAG="latest":
    @set -eu; \
    if [ "{{TAG}}" = latest ]; then url="{{RELEASE_URL}}/latest/download"; else url="{{RELEASE_URL}}/download/{{TAG}}"; fi; \
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' INT TERM EXIT; \
    curl -fsSL -o "$tmp/zot-speaks" "$url/zot-speaks"; \
    curl -fsSL -o "$tmp/SHA256SUMS" "$url/SHA256SUMS"; \
    ( cd "$tmp" && sha256sum -c SHA256SUMS ); \
    chmod +x "$tmp/zot-speaks"; \
    stage="$tmp/stage/zot-speaks"; mkdir -p "$stage"; \
    cp extension.json "$stage/extension.json"; \
    if [ -d skills ]; then cp -R skills "$stage/skills"; else echo "no skills/ in this clone: the manifest would name a skill bundle that is not there"; fi; \
    mv "$tmp/zot-speaks" "$stage/zot-speaks"; \
    zot ext install "$stage"

# update an installed copy from a Release: fetch the binary and the manifest that Release publishes,
# verify both against its SHA256SUMS, then swap in place; settings.json untouched. Releases from
# v1.7.15 carry extension.json; older ones carry only the binary, and then this says so.
# A running zot picks the swap up with `/reload-ext`: it re-reads manifests, respawns subprocesses and
# rebuilds the tool registry, cancelling the turn in progress first. No restart needed.
# The target follows zot's own lookup: $ZOT_HOME, else $XDG_STATE_HOME/zot, else ~/.local/state/zot.
# Override INSTALLED for a project-local install: INSTALLED=<project>/.zot/extensions/zot-speaks
update-release TAG="latest":
    @set -eu; \
    installed="${INSTALLED:-${ZOT_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}/zot}/extensions/zot-speaks}"; \
    test -d "$installed" || { echo "update-release: not installed: $installed"; exit 1; }; \
    if [ "{{TAG}}" = latest ]; then url="{{RELEASE_URL}}/latest/download"; else url="{{RELEASE_URL}}/download/{{TAG}}"; fi; \
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' INT TERM EXIT; \
    curl -fsSL -o "$tmp/zot-speaks" "$url/zot-speaks"; \
    curl -fsSL -o "$tmp/SHA256SUMS" "$url/SHA256SUMS"; \
    manifest=no; \
    if curl -fsSL -o "$tmp/extension.json" "$url/extension.json" 2>/dev/null; then manifest=yes; fi; \
    ( cd "$tmp" && sha256sum -c SHA256SUMS ); \
    cp "$tmp/zot-speaks" "$installed/zot-speaks.new"; \
    chmod +x "$installed/zot-speaks.new"; \
    mv -f "$installed/zot-speaks.new" "$installed/zot-speaks"; \
    if [ "$manifest" = yes ]; then \
      cp "$tmp/extension.json" "$installed/extension.json"; \
      rm -rf "$installed/skills"; cp -R skills "$installed/skills"; \
      echo "updated $installed from {{TAG}} (binary + extension.json + skills/, settings.json untouched)"; \
    else \
      echo "updated $installed from {{TAG}} (binary only: this Release publishes no extension.json, so zot still reports the version the installed manifest claims)"; \
    fi

# ---- build and install from source -----------------------------------------------------
# The route on macOS: `brew install ffmpeg` for the `ffplay` player, a Rust toolchain, then
# `just install-ext`. Nothing else in this justfile installs anything on a Mac.

# build the release binary
build:
    cargo build --release

# run the test suite
test:
    cargo test

# lint
lint:
    cargo clippy --all-targets

# streaming smoke test: prints the first chunk's time-to-first-sound
smoke TEXT="hi":
    cargo run --release --example stream_smoke {{TEXT}}

# run one extension command against the built binary
run CMD="/tts hello":
    {{BIN}} --print {{CMD}}

# stage extension.json + the built binary, then install them into zot
# (just hands recipe text to the shell unescaped: `$stage`, not make's `$$stage`)
install-ext: build
    @stage="$(mktemp -d)"; mkdir -p "$stage/zot-speaks"; \
    cp extension.json "$stage/zot-speaks/extension.json"; \
    cp -R skills "$stage/zot-speaks/skills"; \
    cp {{BIN}} "$stage/zot-speaks/zot-speaks"; \
    zot ext install "$stage/zot-speaks"

# update an installed copy in place: swap binary and manifest, keep settings.json where it sits.
# (`zot ext install` refuses to overwrite, and `zot ext remove` would delete the state file.)
# The manifest travels with the binary on purpose: `zot ext list` reports the manifest's version, so
# a stale one would advertise a version the binary does not have, and never carry a new command.
# A running zot picks the swap up with `/reload-ext`: it re-reads manifests, respawns subprocesses and
# rebuilds the tool registry, cancelling the turn in progress first. No restart needed.
# The target follows zot's own lookup: $ZOT_HOME, else $XDG_STATE_HOME/zot, else ~/.local/state/zot.
# Override INSTALLED for a project-local install: INSTALLED=<project>/.zot/extensions/zot-speaks
update-ext: build
    @installed="${INSTALLED:-${ZOT_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}/zot}/extensions/zot-speaks}"; \
    test -d "$installed" || { echo "not installed: $installed"; exit 1; }; \
    cp {{BIN}} "$installed/zot-speaks.new"; mv -f "$installed/zot-speaks.new" "$installed/zot-speaks"; \
    cp extension.json "$installed/extension.json"; \
    rm -rf "$installed/skills"; cp -R skills "$installed/skills"; \
    echo "updated $installed from source $(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^\"]*\)".*/v\1/p' extension.json | head -1) (settings.json untouched)"
