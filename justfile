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
# network step, run once. They need the upstream `ptts` CLI — install it from
# https://github.com/gradium-ai/xn-ptts/releases, put it on PATH, or set PTTS=/path/to/ptts.

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

# show what the extension can see: model snapshot + voices ready (hub precedence as in cache.rs)
cache:
    @hub="${HF_HUB_CACHE:-}"; \
    if [ -z "$hub" ]; then \
        if [ -n "${HF_HOME:-}" ]; then hub="$HF_HOME/hub"; \
        elif [ -n "${XDG_CACHE_HOME:-}" ]; then hub="$XDG_CACHE_HOME/huggingface/hub"; \
        else hub="$HOME/.cache/huggingface/hub"; fi; \
    fi; \
    model="$hub/models--gradium--phonon-31m-en"; voices="$hub/models--gradium--phonon-voices"; \
    snap=$(ls -d "$model/snapshots"/*/ 2>/dev/null | head -1); \
    if [ -z "$snap" ]; then echo "model: not cached under $hub -> run 'just bootstrap'"; \
    else echo "model: $snap ($(du -sh "$model" | cut -f1) cached)"; fi; \
    if [ -d "$voices" ]; then \
        echo "voices: $(find "$voices" -name '*.safetensors' | wc -l) embedding(s) cached, 37 in the English catalog"; \
    else echo "voices: no catalog cached -> run 'just bootstrap'"; fi

# ---- install from the GitHub Release (the recommended route) ---------------------------
# GitHub Actions builds one binary per version tag and publishes it as the Release asset
# `zot-speaks` together with `SHA256SUMS`. These recipes are the README's curl + sha256sum +
# zot ext install, written down: no cargo, no toolchain. Run them from the clone root — the
# clone supplies extension.json. Pin a version with TAG: just install-release TAG=v1.7.6

# download the release binary, verify its digest, stage it with extension.json, install it
install-release TAG="latest":
    @set -eu; \
    url="{{RELEASE_URL}}/download/{{TAG}}"; \
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' INT TERM EXIT; \
    curl -fsSL -o "$tmp/zot-speaks" "$url/zot-speaks"; \
    curl -fsSL -o "$tmp/SHA256SUMS" "$url/SHA256SUMS"; \
    ( cd "$tmp" && sha256sum -c SHA256SUMS ); \
    chmod +x "$tmp/zot-speaks"; \
    stage="$tmp/stage/zot-speaks"; mkdir -p "$stage"; \
    cp extension.json "$stage/extension.json"; \
    mv "$tmp/zot-speaks" "$stage/zot-speaks"; \
    zot ext install "$stage"

# update an installed copy from a Release: verify first, then atomic swap; settings.json untouched.
# Override INSTALLED for a project-local install: INSTALLED=<project>/.zot/extensions/zot-speaks
update-release TAG="latest":
    @set -eu; \
    installed="${INSTALLED:-${ZOT_HOME:-$HOME/.local/state/zot}/extensions/zot-speaks}"; \
    test -d "$installed" || { echo "update-release: not installed: $installed"; exit 1; }; \
    url="{{RELEASE_URL}}/download/{{TAG}}"; \
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' INT TERM EXIT; \
    curl -fsSL -o "$tmp/zot-speaks" "$url/zot-speaks"; \
    curl -fsSL -o "$tmp/SHA256SUMS" "$url/SHA256SUMS"; \
    ( cd "$tmp" && sha256sum -c SHA256SUMS ); \
    cp "$tmp/zot-speaks" "$installed/zot-speaks.new"; \
    chmod +x "$installed/zot-speaks.new"; \
    mv -f "$installed/zot-speaks.new" "$installed/zot-speaks"; \
    echo "updated $installed from {{TAG}} (settings.json untouched)"

# ---- build and install from source -----------------------------------------------------

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
    cp {{BIN}} "$stage/zot-speaks/zot-speaks"; \
    zot ext install "$stage/zot-speaks"

# update an installed copy in place: swap the binary, keep settings.json where it sits.
# (`zot ext install` refuses to overwrite, and `zot ext remove` would delete the state file.)
# Override INSTALLED for a project-local install: INSTALLED=<project>/.zot/extensions/zot-speaks
update-ext: build
    @installed="${INSTALLED:-${ZOT_HOME:-$HOME/.local/state/zot}/extensions/zot-speaks}"; \
    test -d "$installed" || { echo "not installed: $installed"; exit 1; }; \
    cp {{BIN}} "$installed/zot-speaks.new"; mv -f "$installed/zot-speaks.new" "$installed/zot-speaks"; \
    echo "updated $installed (settings.json untouched)"
