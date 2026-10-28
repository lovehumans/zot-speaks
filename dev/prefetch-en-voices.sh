#!/usr/bin/env bash
# zot-speaks bootstrap: pre-populate the English Phonon voice embeddings once, so the offline
# `/tts-settings` picker lists every English voice as ready instead of "(not downloaded)".
#
# zot-speaks itself is local-only and never downloads at runtime. This helper is the one-time
# fetch step: it walks the cached `voices.jsonl` catalog and runs `./ptts --voice <id>` for each
# English voice whose embedding is not already cached. A `./zot-speaks` started after this sees
# them all; one that is already running keeps the availability it read at startup.
#
# Usage:
#   dev/prefetch-en-voices.sh                 # fetch every not-yet-cached English voice
#   dev/prefetch-en-voices.sh Alfie Zoey      # or pass voice names / voice IDs to fetch only those
#   VOICE_LANG=fr dev/prefetch-en-voices.sh   # (a phonon-31m-* model for that language is required)
#   PTTS=/path/to/ptts dev/prefetch-en-voices.sh
#
# Each iteration synthesizes a tiny phrase purely to make `ptts` resolve and cache the voice;
# the audio is written to a scratch file, not to the repo. Already-cached voices are skipped,
# so the script is safe to re-run.

set -euo pipefail

PTTS="${PTTS:-}"
if [ -z "$PTTS" ]; then
    # A ./ptts kept next to this script wins (the internal convention); otherwise use the
    # `ptts` CLI the user installed from the upstream releases.
    if [ -x ./ptts ]; then PTTS=./ptts; else PTTS=ptts; fi
fi
VOICE_LANG="${VOICE_LANG:-en}"
MODEL="${MODEL:-}" # e.g. hf://gradium/phonon-31m-en@<commit>; leave empty to use ptts' default
SCRATCH_DIR="${TMPDIR:-/tmp}"
OUT="${SCRATCH_DIR%/}/zot-speaks-prefetch.wav"

die() { echo "prefetch: $*" >&2; exit 1; }

# Locate a cached voices.jsonl, following the same cache precedence as ptts / hf-hub.
find_catalog() {
    local hubs=()
    [ -n "${HF_HUB_CACHE:-}" ] && hubs+=("$HF_HUB_CACHE")
    [ -n "${HF_HOME:-}" ]     && hubs+=("$HF_HOME/hub")
    [ -n "${XDG_CACHE_HOME:-}" ] && hubs+=("$XDG_CACHE_HOME/huggingface/hub")
    hubs+=("${HOME}/.cache/huggingface/hub")
    local hub file
    for hub in "${hubs[@]}"; do
        for file in "$hub"/models--gradium--phonon-voices/snapshots/*/voices.jsonl; do
            [ -e "$file" ] && { echo "$file"; return 0; }
        done
    done
    return 1
}

command -v "$PTTS" >/dev/null 2>&1 || [ -x "$PTTS" ] || \
    die "ptts not found as '$PTTS'. Install the upstream CLI, or set PTTS=/path/to/ptts — it is
    the one command here that is allowed to download."

CATALOG="$(find_catalog)" || die \
"no cached voices.jsonl found. Fetch the catalog once first, e.g.:
    $PTTS --voice Miles --lang en \"warm up\" -o '$OUT'
or:  hf download gradium/phonon-voices voices.jsonl"

SNAPSHOT_DIR="$(dirname "$CATALOG")"
VOICES_DIR="$SNAPSHOT_DIR/voices"
echo "catalog: $CATALOG"
echo "voice embeddings live in: $VOICES_DIR"

# Build aligned id/name arrays for the target language only. voice_id has no spaces and is the
# safest thing to hand to `--voice`; names often contain spaces, so we resolve them by exact
# string match within this language (a foreign voice name is therefore never matched here).
# Aligned arrays only, no associative array: the bash that macOS ships (3.2) has no `declare -A`.
declare -a LANG_IDS LANG_NAMES
while IFS= read -r line; do
    id="$(sed -E 's/.*"voice_id"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/' <<<"$line")"
    nm="$(sed -E 's/.*"name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/' <<<"$line")"
    [ -n "$id" ] && [ "$id" != "$line" ] || continue
    LANG_IDS+=("$id"); LANG_NAMES+=("$nm")
done < <(grep -E "\"language\"[[:space:]]*:[[:space:]]*\"$VOICE_LANG\"" "$CATALOG")
[ "${#LANG_IDS[@]}" -gt 0 ] || die "no '$VOICE_LANG' voices in the catalog."

# The two arrays are aligned by construction, so a name is a lookup in them (37 voices: cheap).
name_of() {
    local i
    for i in "${!LANG_IDS[@]}"; do
        [ "${LANG_IDS[$i]}" = "$1" ] && { echo "${LANG_NAMES[$i]}"; return; }
    done
    echo "$1"   # an id is a perfectly good name for a voice nobody named
}

EN_IDS=("${LANG_IDS[@]}")

# Optional explicit selection: names or voice IDs given on the command line (this language only).
if [ "$#" -gt 0 ]; then
    want=("$@")
    selected=()
    for w in "${want[@]}"; do
        found=""
        for i in "${!LANG_IDS[@]}"; do
            if [ "${LANG_IDS[$i]}" = "$w" ] || [ "${LANG_NAMES[$i]}" = "$w" ]; then
                found="${LANG_IDS[$i]}"; break
            fi
        done
        [ -n "$found" ] || die "no '$VOICE_LANG' catalog voice named '$w'. List them with: $PTTS --list-voices"
        selected+=("$found")
    done
    EN_IDS=("${selected[@]}")
fi

echo "target language: $VOICE_LANG; considering ${#EN_IDS[@]} voice(s)."

fetched=0 skipped=0 failed=0
for id in "${EN_IDS[@]}"; do
    if [ -e "$VOICES_DIR/$id.safetensors" ]; then
        skipped=$((skipped + 1))
        continue
    fi
    name="$(name_of "$id")"
    echo "  + fetching voice '$name' ($id)"
    cmd=("$PTTS" --voice "$id" --lang "$VOICE_LANG" --temperature 0.1 -o "$OUT" "hello")
    [ -n "$MODEL" ] && cmd+=(--model "$MODEL")
    if "${cmd[@]}"; then
        if [ -e "$VOICES_DIR/$id.safetensors" ]; then
            fetched=$((fetched + 1))
        else
            echo "  ! '$name' ran but embedding still not under $VOICES_DIR" >&2
            failed=$((failed + 1))
        fi
    else
        echo "  ! failed to fetch '$name' ($id)" >&2
        failed=$((failed + 1))
    fi
done

echo "prefetch done: fetched=$fetched skipped(cached)=$skipped failed=$failed"
if [ "$failed" -gt 0 ]; then exit 1; fi
