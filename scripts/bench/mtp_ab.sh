#!/bin/sh
# MTP speculative decoding on/off, for one model, driven through llamastash.
#
# Launches the model twice per prompt (`--mtp off`, then `--mtp on`) and reports
# wall-clock decode rate from the OpenAI response, plus whether llama.cpp
# actually engaged the draft path (`params.mtp.active`) and its acceptance rate.
# `active` is the row that matters: a missing or broken draft head shows up as
# `active=false` or a failed launch, not as a slow run.
#
# The model needs a draft head llamastash can pair. Either embedded MTP layers,
# or a sibling named `mtp-<model-basename>.gguf` (Gemma-style heads are ordinary
# 4-layer models by header, so the name is what makes them detectable).
#
# Every row records battery percentage and platform_profile. On a laptop the
# same binary and prompt can differ by ~30% between power states, so a decode
# rate without its power context is not comparable to anything.
#
# Usage: scripts/bench/mtp_ab.sh <model-path-or-ref> <label> [out.md]
set -u
BIN=${LLAMASTASH_BIN:-target/debug/llamastash}
MODEL=$1; LABEL=$2; OUT=${3:-mtp-ab-$(echo "$LABEL" | tr ' /' '__').md}
CODE='Write a Python function that merges two sorted lists into one sorted list. Code only.'
PROSE='Explain in three sentences why merge sort is O(n log n).'

power() {
  b=$(cat /sys/class/power_supply/BAT0/capacity 2>/dev/null || echo "-")
  p=$(cat /sys/firmware/acpi/platform_profile 2>/dev/null || echo "-")
  echo "$b% $p"
}

run() {
  mtp=$1; prompt=$2; tag=$3
  $BIN stop --all --yes >/dev/null 2>&1; sleep 1
  $BIN start "$MODEL" --mtp "$mtp" >/dev/null 2>&1
  st=none; port=""
  for _ in $(seq 1 120); do
    st=$($BIN status --json 2>/dev/null | jq -r '.models[0].state // "none"')
    port=$($BIN status --json 2>/dev/null | jq -r '.models[0].port // empty')
    [ "$st" = ready ] && break
    case "$st" in error|stopped) break;; esac
    sleep 2
  done
  if [ "$st" != ready ]; then
    printf '| %s | %s | - | FAILED (%s) | - | - | %s |\n' "$tag" "$mtp" "$st" "$(power)" >> "$OUT"
    printf '%-6s %-4s FAILED (%s)\n' "$tag" "$mtp" "$st"
    $BIN logs "${port:-41100}" 2>&1 | tail -4
    return
  fi
  t0=$(date +%s.%N)
  resp=$(curl -s --max-time 900 "http://127.0.0.1:$port/v1/chat/completions" \
    -H 'Content-Type: application/json' \
    -d "$(jq -n --arg c "$prompt" '{model:"m",temperature:0,max_tokens:256,
          messages:[{role:"user",content:$c}]}')")
  t1=$(date +%s.%N)
  tok=$(echo "$resp" | jq -r '.usage.completion_tokens // 0')
  acc=$($BIN status --json 2>/dev/null | jq -r '.models[0].params.mtp.acceptance // "-"')
  act=$($BIN status --json 2>/dev/null | jq -r '.models[0].params.mtp.active // "-"')
  awk -v g="$tag" -v m="$mtp" -v t="$tok" -v a="$t0" -v b="$t1" -v ac="$acc" \
      -v av="$act" -v pw="$(power)" \
      'BEGIN{printf "| %s | %s | %d | %.2f t/s | %s | %s | %s |\n", g,m,t,t/(b-a),av,ac,pw}' >> "$OUT"
  awk -v g="$tag" -v m="$mtp" -v t="$tok" -v a="$t0" -v b="$t1" -v av="$act" \
      'BEGIN{printf "%-6s %-4s %3d tok  %7.2f t/s  active=%s\n", g,m,t,t/(b-a),av}'
}

{
  echo "# $LABEL: MTP vs no MTP"
  echo
  echo "$(uname -sm), $(date -Is). Power at start: $(power)."
  echo "Model: $MODEL"
  echo "greedy, 256 max tokens, wall-clock t/s from the API response."
  echo
  echo "| prompt | mtp | tokens | rate | active | acceptance | power |"
  echo "|---|---|---:|---:|---|---:|---|"
} > "$OUT"

run off "$CODE"  code1
run on  "$CODE"  code1
run off "$CODE"  code2
run on  "$CODE"  code2
run off "$PROSE" prose
run on  "$PROSE" prose

$BIN stop --all --yes >/dev/null 2>&1
echo; cat "$OUT"
