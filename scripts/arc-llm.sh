#!/usr/bin/env bash
# Start llama-server for Arc's local model, using [ai.local] from the Arc config.
# Run by arc-llm.service; also fine to run by hand.
set -euo pipefail

CONFIG="${ARC_CONFIG:-$HOME/.config/arc/config.toml}"
SECRETS="$HOME/.config/arc/secrets.env"
DATA="$HOME/.local/share/arc"

# Pull a key from the [ai.local] table (simple TOML: key = "value" / key = number).
local_cfg() {
    awk -v k="$1" '
        /^\[/ { in_t = ($0 == "[ai.local]") ; next }
        in_t && $1 == k { sub(/^[^=]*=[ \t]*/, ""); gsub(/^"|"[ \t]*$/, ""); print; exit }
    ' "$CONFIG"
}

endpoint=$(local_cfg endpoint);        endpoint=${endpoint:-http://127.0.0.1:8765/v1}
model_file=$(local_cfg model_file);    model_file=${model_file:-Qwen3.5-2B-Q4_K_M.gguf}
server_binary=$(local_cfg server_binary); server_binary=${server_binary:-auto}
gpu_layers=$(local_cfg gpu_layers);    gpu_layers=${gpu_layers:--1}
context=$(local_cfg context);          context=${context:-8192}
key_env=$(local_cfg api_key_env);      key_env=${key_env:-ARC_LLM_API_KEY}

hostport=${endpoint#*://}; hostport=${hostport%%/*}
host=${hostport%:*}; port=${hostport##*:}

[[ $model_file = /* ]] || model_file="$DATA/models/$model_file"
[[ -f $model_file ]] || { echo "arc-llm: model not found: $model_file" >&2; exit 1; }

if [[ $server_binary == auto ]]; then
    if command -v nvidia-smi >/dev/null && nvidia-smi -L >/dev/null 2>&1 && [[ -x $DATA/bin/llama-cuda/llama-server ]]; then
        server_binary=$DATA/bin/llama-cuda/llama-server
    elif [[ -x $DATA/bin/llama-vulkan/llama-server ]]; then
        server_binary=$DATA/bin/llama-vulkan/llama-server
    else
        server_binary=$(command -v llama-server) || { echo "arc-llm: no llama-server found" >&2; exit 1; }
    fi
fi

# The local server's API key lives in secrets.env; generate it once.
umask 077
mkdir -p "$(dirname "$SECRETS")"
touch "$SECRETS"
if ! grep -q "^${key_env}=" "$SECRETS"; then
    printf '%s=%s\n' "$key_env" "$(head -c 24 /dev/urandom | base64 | tr -d '/+=')" >> "$SECRETS"
fi
api_key=$(grep "^${key_env}=" "$SECRETS" | head -1 | cut -d= -f2-)

ngl=$gpu_layers; [[ $ngl == -1 ]] && ngl=auto

extra=()
while IFS= read -r a; do [[ -n $a ]] && extra+=("$a"); done < <(
    awk '/^\[/ { in_t = ($0 == "[ai.local]"); next }
         in_t && $1 == "extra_args" { sub(/^[^=]*=[ \t]*\[/, ""); sub(/\][ \t]*$/, ""); n = split($0, a, ","); for (i = 1; i <= n; i++) { gsub(/^[ \t]*"|"[ \t]*$/, "", a[i]); print a[i] } }' "$CONFIG")

echo "arc-llm: $server_binary  model=$(basename "$model_file")  $host:$port  ngl=$ngl ctx=$context"
exec "$server_binary" \
    -m "$model_file" --host "$host" --port "$port" \
    --ctx-size "$context" --n-gpu-layers "$ngl" \
    --api-key "$api_key" --no-webui --jinja --reasoning off \
    "${extra[@]}"
