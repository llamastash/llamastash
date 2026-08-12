#!/usr/bin/env bash
# End-to-end UAT for the vLLM backend against a real native vLLM.
#
# Every stage asserts and exits non-zero on failure — a stage that cannot run
# is a failure, not a pass. Runs entirely inside an isolated state dir so it
# never touches the daemon or models you are actually using.
#
# Usage: scripts/vllm/uat.sh <clean|boot|launch|replay|preset|chat|stop|all>
set -euo pipefail

# Repo root from this script's location, not a hardcoded path: with a wrong cd
# and no `set -e`, every command failed and the script still exited 0.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$ROOT"

UAT_HOME="${LS_UAT_HOME:-$HOME/.cache/ls-vllm-e2e}"
export LLAMASTASH_STATE_DIR="$UAT_HOME/state"
export LLAMASTASH_CONFIG_DIR="$UAT_HOME/config"
export LLAMASTASH_CACHE_DIR="$UAT_HOME/cache"
export HF_HOME="${HF_HOME:-$HOME/.cache/huggingface}"
export HF_HUB_OFFLINE="${HF_HUB_OFFLINE:-1}"

LS=./target/debug/llamastash
DLOG="$LLAMASTASH_CACHE_DIR/logs/llamastash.log"
MODEL="${LS_UAT_MODEL:-Qwen/Qwen2.5-0.5B-Instruct}"
PROXY_PORT="${LS_UAT_PROXY_PORT:-21435}"

die() { echo "FAIL: $*" >&2; exit 1; }
ok()  { echo "  ok: $*"; }
ram() { free -m | awk '/^Mem:/{print $3}'; }

[ -x "$LS" ] || die "$LS missing — run: cargo build --bin llamastash"

# --- launch-scoped helpers -------------------------------------------------
# Every accessor selects by LaunchId. Reading `.models[0]` meant later stages
# reported the *first* launch's argv, so replay and preset precedence were
# never actually observed.
row_of() { $LS status --json | jq -c --arg id "$1" '.models[] | select(.launch_id == $id)'; }
state_of() { row_of "$1" | jq -r '.state // "none"'; }
pid_of() { row_of "$1" | jq -r '.pid // empty'; }
argv_of() {
  local p; p=$(pid_of "$1")
  [ -n "$p" ] && tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null || true
}
# Whole token, so a `1G` value is never reported as `1`.
flag_val() { argv_of "$1" | tr ' ' '\n' | grep -A 1 -x -- "$2" | tail -1; }

start_and_capture() {
  local out
  out=$("$@" 2>&1) || { echo "$out" >&2; die "start failed"; }
  echo "$out" | grep -oE 'launch_id=L[0-9]+' | head -1 | cut -d= -f2
}

wait_ready() {
  local id=$1 s
  for _ in $(seq 1 90); do
    s=$(state_of "$id")
    [ "$s" = "ready" ] && { ok "$id ready"; return 0; }
    [ "$s" = "error" ] && die "$id went to error"
    sleep 2
  done
  die "$id never settled"
}

stop_all() { $LS stop --all --yes >/dev/null 2>&1 || true; sleep 2; }

# --- stages ----------------------------------------------------------------
do_clean() {
  stop_all
  $LS daemon stop >/dev/null 2>&1 || true
  # Only this daemon's own children. `pkill -f venvs/vllm` matched every vLLM
  # process on the host, including models the user's real daemon was serving.
  sleep 2
  echo "clean: RAM=$(ram)M"
}

do_boot() {
  $LS daemon start --proxy-port "$PROXY_PORT" --force >/dev/null 2>&1 || true
  local avail
  avail=$($LS status --json | jq -r '.backends[] | select(.id=="vllm") | .enabled')
  [ "$avail" = "true" ] || die "the vllm backend is not available — set backend.vllm.servers[0].binary"
  ok "backend available"
  for _ in $(seq 1 30); do
    $LS list --json | jq -e --arg m "$MODEL" '.models[] | select(.name == $m)' >/dev/null 2>&1 && {
      ok "$MODEL discovered"; return 0; }
    sleep 1
  done
  die "$MODEL never reached the catalog"
}

do_launch() {
  local id; id=$(start_and_capture $LS start "$MODEL" --ctx 3072 --mode chat)
  sleep 4
  local cap; cap=$(flag_val "$id" --kv-cache-memory-bytes)
  [ -n "$cap" ] || die "no KV cap in argv — vLLM would size its cache against the whole pool"
  ok "KV cap $cap"
  wait_ready "$id"
  # `actuals` are fetched on the Loading -> Ready transition and persisted
  # just after, so a single read right at Ready can legitimately miss them.
  local ctx=null
  for _ in $(seq 1 15); do
    ctx=$(row_of "$id" | jq -r '.resolved_ctx // "null"')
    [ "$ctx" != "null" ] && break
    sleep 1
  done
  [ "$ctx" = "3072" ] || die "resolved_ctx=$ctx, expected 3072"
  ok "resolved_ctx $ctx"
  echo "$id" > "$UAT_HOME/.last_id"
}

do_replay() {
  stop_all
  local id; id=$(start_and_capture $LS start "$MODEL")
  sleep 4
  local mml; mml=$(flag_val "$id" --max-model-len)
  [ "$mml" = "3072" ] || die "last_params did not replay --ctx (got '${mml:-none}')"
  ok "last_params replayed ctx $mml"
  [ -n "$(flag_val "$id" --kv-cache-memory-bytes)" ] || die "KV cap missing on the replayed launch"
  ok "KV cap re-resolved"
  wait_ready "$id"
}

do_preset() {
  stop_all
  local id; id=$(start_and_capture $LS start "$MODEL" --preset vllm-small)
  sleep 4
  local cap; cap=$(flag_val "$id" --kv-cache-memory-bytes)
  [ "$cap" = "1G" ] || die "preset cap not honoured (got '${cap:-none}', expected 1G)"
  ok "preset cap $cap beat the auto cap"
  wait_ready "$id"
}

do_chat() {
  local id port rev
  id=$(cat "$UAT_HOME/.last_id" 2>/dev/null || $LS status --json | jq -r '.models[0].launch_id')
  port=$(row_of "$id" | jq -r '.port')
  code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$port/v1/chat/completions" \
    -H 'content-type: application/json' \
    -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"max_tokens\":3}")
  [ "$code" = "200" ] || die "repo id returned $code"
  ok "repo id -> 200"
  rev=$(basename "$(row_of "$id" | jq -r '.model_path')")
  code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$port/v1/chat/completions" \
    -H 'content-type: application/json' \
    -d "{\"model\":\"$rev\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"max_tokens\":3}")
  [ "$code" = "404" ] || die "the revision hash returned $code — expected 404 (it is not a served name)"
  ok "revision hash -> 404, as it must"
}

do_stop() { stop_all; $LS daemon stop >/dev/null 2>&1 || true; echo "stopped: RAM=$(ram)M"; }

case "${1:-all}" in
  clean) do_clean ;; boot) do_boot ;; launch) do_launch ;;
  replay) do_replay ;; preset) do_preset ;; chat) do_chat ;; stop) do_stop ;;
  all) do_clean; do_boot; do_launch; do_chat; do_replay; do_stop; echo "ALL STAGES PASSED" ;;
  *) die "usage: $0 <clean|boot|launch|replay|preset|chat|stop|all>" ;;
esac
