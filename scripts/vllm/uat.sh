#!/usr/bin/env bash
# vLLM end-to-end UAT against a real native vLLM.
# Usage: vllm_uat.sh [clean|launch|replay|preset|chat|stop]
set -uo pipefail
cd /mnt/work/Workspace/oss-libs/llamastash-rs/llamastash

export LLAMASTASH_STATE_DIR="$HOME/.cache/ls-vllm-e2e/state"
export LLAMASTASH_CONFIG_DIR="$HOME/.cache/ls-vllm-e2e/config"
export LLAMASTASH_CACHE_DIR="$HOME/.cache/ls-vllm-e2e/cache"
export HF_HOME=/mnt/work/huggingface HF_HUB_OFFLINE=1
LS=./target/debug/llamastash
DLOG="$LLAMASTASH_CACHE_DIR/logs/llamastash.log"
MODEL="Qwen/Qwen2.5-0.5B-Instruct"

ram() { free -m | awk '/^Mem:/{print $3}'; }
state() { $LS status --json 2>/dev/null | jq -r '.models[0].state // "none"'; }
pid_of() { $LS status --json 2>/dev/null | jq -r '.models[0].pid // empty'; }
argv_of() { local p; p=$(pid_of); [ -n "$p" ] && tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null; }
cap_of() { argv_of | grep -oE '\--kv-cache-memory-bytes [0-9]+' || echo "MISSING"; }

wait_settled() {
  for _ in $(seq 1 90); do
    s=$(state); [ "$s" = "ready" ] || [ "$s" = "error" ] && { echo "$s"; return; }
    command sleep 2
  done
  echo timeout
}

do_clean() {
  $LS stop --all --yes >/dev/null 2>&1
  $LS daemon stop >/dev/null 2>&1
  pkill -f "venvs/vllm/bin/python" 2>/dev/null
  pkill -f "VLLM::" 2>/dev/null
  command sleep 3
  echo "clean: RAM=$(ram)M  vllm_procs=$(ps -eo comm | grep -c '^vllm$')"
}

do_boot() {
  $LS daemon start --proxy-port 21435 --force 2>&1 | tail -1
  for _ in $(seq 1 30); do
    n=$($LS list --json 2>/dev/null | jq '.models|length'); [ "${n:-0}" -gt 0 ] && break
    command sleep 1
  done
  echo "catalog=${n:-0}"
}

case "${1:-launch}" in
  clean) do_clean ;;
  boot)  do_boot ;;
  launch)
    before=$(grep -c "vllm: capping" "$DLOG" 2>/dev/null || echo 0)
    $LS start "$MODEL" --ctx 3072 --mode chat 2>&1 | tail -1
    command sleep 4
    echo "cap-in-argv: $(cap_of)"
    after=$(grep -c "vllm: capping" "$DLOG" 2>/dev/null || echo 0)
    echo "cap-log-lines: before=$before after=$after"
    echo "settled: $(wait_settled)  RAM=$(ram)M"
    ;;
  replay)
    # No flags: everything must come from last_params, and the auto cap must
    # still be applied on the replayed launch.
    $LS start "$MODEL" 2>&1 | tail -1
    command sleep 4
    echo "cap-in-argv: $(cap_of)"
    echo "max-model-len: $(argv_of | grep -oE '\--max-model-len [0-9]+' || echo none)"
    echo "settled: $(wait_settled)"
    $LS status --json | jq -c '.models[0] | {state, resolved_ctx}'
    ;;
  preset)
    $LS start "$MODEL" --preset vllm-small 2>&1 | tail -1
    command sleep 4
    echo "cap-in-argv: $(cap_of)"
    echo "argv: $(argv_of | grep -oE '\--(kv-cache-memory-bytes|max-model-len|enforce-eager) ?[0-9]*' | tr '\n' ' ')"
    echo "settled: $(wait_settled)"
    ;;
  chat)
    p=$(pid_of); port=$($LS status --json | jq -r '.models[0].port')
    echo "direct child:"
    curl -s -o /dev/null -w "  exact -> %{http_code}\n" -X POST "http://127.0.0.1:$port/v1/chat/completions" \
      -H 'content-type: application/json' -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"max_tokens\":3}"
    echo "  revision-hash name (what the TUI used to send):"
    rev=$($LS status --json | jq -r '.models[0].model_path' | xargs basename)
    curl -s -o /dev/null -w "    $rev -> %{http_code}\n" -X POST "http://127.0.0.1:$port/v1/chat/completions" \
      -H 'content-type: application/json' -d "{\"model\":\"$rev\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"max_tokens\":3}"
    ;;
  stop) $LS stop --all --yes 2>&1 | tail -1; $LS daemon stop 2>&1 | tail -1; echo "RAM=$(ram)M" ;;
esac
