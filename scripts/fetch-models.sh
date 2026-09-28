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
# These are the speech models, and the only models Arc loads: Moonshine for
# recognition, Kokoro for speech, plus the VAD and wake-word models. They all
# run on this machine. Reasoning is not here -- it goes through the hermes
# proxy, and there is no local language model.
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
"kokoro-en-v0_19|$BASE/tts-models/kokoro-en-v0_19.tar.bz2|tar|\
kokoro-en-v0_19/model.onnx=10ff414106a038ce7e9e0126c6461e4dc8a86efaa89dc91d2009d69fe635e339 \
kokoro-en-v0_19/tokens.txt=4f31c71282d14af4e926cd12462078fe9d20d00c589e63fe2750a8f56d6d7f7b \
kokoro-en-v0_19/voices.bin=a372c67b056ef0b695c375d39b99630d23fb07ad4c8d87aa32a19a62fca523ad"
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
