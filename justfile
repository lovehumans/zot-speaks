# zot-speaks — one entrypoint for every dev task. Run `just` to list recipes.
# Any CARGO_* variable already in the environment is honoured by cargo as usual.

export CARGO_TARGET_DIR := env_var_or_default('CARGO_TARGET_DIR', 'target')

BIN := CARGO_TARGET_DIR + "/release/zot-speaks"

# list recipes
default:
    @just --list

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
