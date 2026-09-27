#!/usr/bin/env bash
# Download the voice models Arc uses into $ARC_DATA_DIR/models
# (default ~/.local/share/arc/models) and verify them.
#
#   scripts/fetch-models.sh            # fetch anything missing, verify all
#   scripts/fetch-models.sh --verify   # verify only, download nothing
#   scripts/fetch-models.sh --force    # re-download everything
#
# Every model file that Arc loads is pinned by SHA-256 below. A mismatch is
# fatal: the file is left in place and nothing is overwritten silently.
# Language models (GGUF for llama.cpp) are handled separately by the
# installer because of their size.
set -euo pipefail

BASE="https://github.com/k2-fsa/sherpa-onnx/releases/download"
MODELS="${ARC_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/arc}/models"
MODE=fetch
case "${1:-}" in
    --verify) MODE=verify ;;
    --force) MODE=force ;;
    "") ;;
    *) echo "usage: $0 [--verify|--force]" >&2; exit 2 ;;
esac

# name | url | kind (file|tar) | pinned files: "relative/path=sha256 ..."
MANIFEST=(
"silero_vad.onnx|$BASE/asr-models/silero_vad.onnx|file|silero_vad.onnx=9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"
"sherpa-onnx-moonshine-base-en-quantized-2026-02-27|$BASE/asr-models/sherpa-onnx-moonshine-base-en-quantized-2026-02-27.tar.bz2|tar|\
sherpa-onnx-moonshine-base-en-quantized-2026-02-27/encoder_model.ort=7c66495948d0d08ec1af454cd4b5514862ae6511e94712a60e6d83eaec8dc8cf \
sherpa-onnx-moonshine-base-en-quantized-2026-02-27/decoder_model_merged.ort=d9d7b333af34bc552580576ddcf248a1c6c839e0d3b43b09afb9376ed009899d \
sherpa-onnx-moonshine-base-en-quantized-2026-02-27/tokens.txt=2870d843e14c1e187bf1913a521562a63b53933814bd7f2145120468f494a049"
"vits-piper-en_GB-alan-medium|$BASE/tts-models/vits-piper-en_GB-alan-medium.tar.bz2|tar|\
vits-piper-en_GB-alan-medium/en_GB-alan-medium.onnx=d907c48857000940104a9ad3248a94617df917d1a525c9495490fdbf87fd54b2 \
vits-piper-en_GB-alan-medium/tokens.txt=87c8ef66eae5473ed0cc0366b3964c736ca6c5f676c979522ea31234e47430b9"
)

say() { printf '%s\n' "$*" >&2; }

verify_entry() { # pins -> 0 if all good
    local ok=0 pin rel want got
    for pin in $1; do
        rel="${pin%%=*}"; want="${pin#*=}"
        if [[ ! -f "$MODELS/$rel" ]]; then
            say "  missing   $rel"; ok=1; continue
        fi
        got="$(sha256sum "$MODELS/$rel" | cut -d' ' -f1)"
        if [[ "$got" == "$want" ]]; then
            say "  ok        $rel"
        else
            say "  MISMATCH  $rel"
            say "            expected $want"
            say "            got      $got"
            ok=2
        fi
    done
    return $ok
}

download() { # url dest
    curl --fail --location --proto '=https' --tlsv1.2 --retry 3 --retry-delay 2 \
        --progress-bar -o "$2" "$1"
}

mkdir -p "$MODELS"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
failed=0

for entry in "${MANIFEST[@]}"; do
    IFS='|' read -r name url kind pins <<<"$entry"
    say "$name"
    rc=0; verify_entry "$pins" 2>/dev/null || rc=$?
    if [[ $MODE == verify ]] || { [[ $MODE == fetch ]] && [[ $rc -eq 0 ]]; }; then
        verify_entry "$pins" || failed=1
        continue
    fi
    if [[ $rc -eq 2 && $MODE != force ]]; then
        verify_entry "$pins" || true
        say "  refusing to overwrite a modified model; rerun with --force"
        failed=1
        continue
    fi
    say "  downloading $url"
    if [[ $kind == file ]]; then
        download "$url" "$tmp/$name"
        mv -f "$tmp/$name" "$MODELS/$name"
    else
        download "$url" "$tmp/$name.tar.bz2"
        # Extract to a staging dir, then swap in atomically.
        mkdir -p "$tmp/x"
        tar -xjf "$tmp/$name.tar.bz2" -C "$tmp/x" --no-same-owner
        [[ -d "$tmp/x/$name" ]] || { say "  archive did not contain $name/"; failed=1; continue; }
        rm -rf "$MODELS/$name.old"
        [[ -e "$MODELS/$name" ]] && mv "$MODELS/$name" "$MODELS/$name.old"
        mv "$tmp/x/$name" "$MODELS/$name"
        rm -rf "$MODELS/$name.old" "$tmp/x" "$tmp/$name.tar.bz2"
    fi
    verify_entry "$pins" || failed=1
done

if [[ $failed -ne 0 ]]; then
    say "model check FAILED"
    exit 1
fi
say "all voice models present and verified in $MODELS"
