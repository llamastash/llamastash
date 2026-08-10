#!/bin/sh
# Three-way ds4 decode comparison for DeepSeek-V4 Flash, driven through
# llamastash so the measured path is the one users get:
#
#   1. pre-0731 Flash   (no speculation)
#   2. Flash 0731       (no speculation)
#   3. Flash 0731 + DSpark
#
# There is deliberately no pre-0731 + DSpark row: the support GGUF is
# checkpoint-specific and antirez's README states it "must not be paired with
# an older Flash model" — measured, it drafts nothing (proposed=0) and costs
# pure overhead.
#
# Each config launches via a named preset, runs the same greedy prompts, and
# reports ds4-server's own decode rate. DSpark configs also report ds4's
# cumulative acceptance counters, which flush only on a clean exit — so those
# come from a direct ds4-server run, not a llamastash `stop`.
#
# Do NOT set LLAMASTASH_BENCH_DISABLE_DEFAULTS here: it collapses knob
# resolution to User layers only, stripping the preset backend_knobs that carry
# `--mtp` / `--dspark`.
#
# Usage: scripts/bench/dspark_ab.sh <work-dir> <out.md>
#   work-dir  scratch for the isolated state/config/cache dirs
#   out.md    markdown report written here
set -u

WORK=$1; OUT=$2
BIN=target/debug/llamastash
DS4_BIN=${DS4_SERVER_BIN:-/mnt/work/ds4-build/ds4-server}
HUB=${HF_HOME:-$HOME/.cache/huggingface}/hub/models--antirez--deepseek-v4-gguf/snapshots

OLD=$(ls "$HUB"/*/DeepSeek-V4-Flash-IQ2XXS-*-chat-v2-imatrix.gguf 2>/dev/null | head -1)
NEW=$(ls "$HUB"/*/DeepSeek-V4-Flash-IQ2XXS-*-chat-v2-imatrix-0731.gguf 2>/dev/null | head -1)
SUPPORT=$(ls "$HUB"/*/DeepSeek-V4-Flash-DSpark-support-0731.gguf 2>/dev/null | head -1)

export LLAMASTASH_STATE_DIR=$WORK/state
export LLAMASTASH_CONFIG_DIR=$WORK/config
export LLAMASTASH_CACHE_DIR=$WORK/cache
mkdir -p "$LLAMASTASH_STATE_DIR" "$LLAMASTASH_CONFIG_DIR" "$LLAMASTASH_CACHE_DIR"

PROMPT_CODE='Write a Python function that merges two sorted lists into one sorted list. Code only.'
PROMPT_PROSE='Explain in three sentences why merge sort is O(n log n).'

# ssd_streaming is pinned false everywhere: streaming and a draft head are
# mutually exclusive in ds4-server, so letting it auto-enable would silently
# disarm DSpark and make the two halves incomparable.
{
  echo "backend:"
  echo "  ds4:"
  echo "    enabled: true"
  echo "    servers:"
  echo "      - binary: $DS4_BIN"
  echo "  lemonade:"
  echo "    enabled: false"
  echo "presets:"
  for m in "$OLD" "$NEW"; do
    [ -n "$m" ] || continue
    echo "  $(basename "$m"):"
    echo "    entries:"
    echo "      plain:"
    echo "        ctx: 4096"
    echo "        backend_knobs:"
    echo "          ssd_streaming: \"false\""
    [ "$m" = "$NEW" ] || continue
    echo "      dspark:"
    echo "        ctx: 4096"
    echo "        backend_knobs:"
    echo "          ssd_streaming: \"false\""
    echo "          dspark: \"true\""
    echo "          mtp: $SUPPORT"
  done
} > "$LLAMASTASH_CONFIG_DIR/config.yaml"

run_config() {
  label=$1; model=$2; preset=$3
  [ -n "$model" ] || { echo "| $label | missing GGUF | — | — |" >> "$OUT"; return; }
  $BIN stop --all --yes >/dev/null 2>&1
  $BIN daemon stop >/dev/null 2>&1; sleep 2
  $BIN daemon start --force --proxy-port 21435 >/dev/null 2>&1; sleep 4
  $BIN start "$model" --backend ds4 --preset "$preset" >/dev/null 2>&1
  st=none
  for _ in $(seq 1 300); do
    st=$($BIN status --json 2>/dev/null | jq -r '.models[0].state // "none"')
    [ "$st" = ready ] && break
    case "$st" in error|stopped) break;; esac
    sleep 5
  done
  if [ "$st" != ready ]; then
    echo "| $label | FAILED ($st) | — | — |" >> "$OUT"
    $BIN logs 41100 2>&1 | tail -5
    $BIN daemon stop >/dev/null 2>&1
    return
  fi
  # One prompt at a time, reading ds4's rate straight after each: a single
  # generation emits an `avg=` line per 50-token chunk, so a trailing `tail -2`
  # would take two chunks of the *last* prompt, not one rate per prompt.
  rates=""
  for p in "$PROMPT_CODE" "$PROMPT_PROSE"; do
    curl -s --max-time 900 http://127.0.0.1:41100/v1/chat/completions \
      -H 'Content-Type: application/json' \
      -d "$(jq -n --arg c "$p" '{model:"deepseek-v4-flash",temperature:0,max_tokens:200,
             messages:[{role:"user",content:$c}]}')" >/dev/null
    r=$($BIN logs 41100 2>&1 | grep -oE "avg=[0-9.]+ t/s" | tail -1 | grep -oE "[0-9.]+")
    rates="$rates ${r:-—}"
  done
  set -- $rates
  echo "| $label | ${1:-—} | ${2:-—} | ok |" >> "$OUT"
  $BIN stop --all --yes >/dev/null 2>&1
  $BIN daemon stop >/dev/null 2>&1
}

accept_rate() {
  label=$1; model=$2; log=$LLAMASTASH_CACHE_DIR/accept-$label.log
  [ -n "$model" ] && [ -n "$SUPPORT" ] || return
  DS4_DSPARK_STATS=1 "$DS4_BIN" -m "$model" --host 127.0.0.1 --port 41200 \
    --ctx 4096 --mtp "$SUPPORT" --dspark > "$log" 2>&1 &
  pid=$!
  for _ in $(seq 1 300); do
    curl -s --max-time 3 http://127.0.0.1:41200/v1/models >/dev/null 2>&1 && break
    kill -0 $pid 2>/dev/null || return
    sleep 5
  done
  curl -s --max-time 900 http://127.0.0.1:41200/v1/chat/completions \
    -H 'Content-Type: application/json' \
    -d "$(jq -n --arg c "$PROMPT_CODE" '{model:"deepseek-v4-flash",temperature:0,max_tokens:200,
           messages:[{role:"user",content:$c}]}')" >/dev/null
  kill -INT $pid 2>/dev/null
  for _ in $(seq 1 30); do kill -0 $pid 2>/dev/null || break; sleep 2; done
  kill -9 $pid 2>/dev/null
  echo "- **$label**: $(grep -oE 'cycles=[0-9]+ first_tokens=[0-9]+ proposed=[0-9]+ accepted_draft=[0-9]+ accept_rate=[0-9.]+%' "$log" | tail -1 || echo 'no stats line')" >> "$OUT"
}

{
  echo "# ds4 DSpark three-way decode comparison"
  echo
  echo "Host: $(uname -sm), $(nproc) cores, $(free -g | awk '/^Mem:/{print $2}') GB RAM"
  echo "ds4-server: $DS4_BIN"
  echo "Generated: $(date -Is)"
  echo
  echo "Decode rate (ds4-server \`avg=\` t/s), greedy, ctx 4096, 200 max tokens."
  echo
  echo "| config | code prompt | prose prompt | status |"
  echo "|---|---:|---:|---|"
} > "$OUT"

run_config "pre-0731"      "$OLD" plain
run_config "0731"          "$NEW" plain
run_config "0731 + DSpark" "$NEW" dspark

echo >> "$OUT"
echo "## DSpark acceptance (direct ds4-server run, clean exit)" >> "$OUT"
echo >> "$OUT"
accept_rate "0731" "$NEW"

echo >> "$OUT"
echo "Report: $OUT"
cat "$OUT"
