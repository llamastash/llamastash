#!/bin/sh
# DeepSeek-V4 Flash three-way decode comparison, driven through the `ds4` CLI
# directly (not llamastash), gated on a usable power state:
#
#   1. pre-0731 Flash   (no speculation)
#   2. Flash 0731       (no speculation)
#   3. Flash 0731 + DSpark
#
# Waits for battery >= $BATTERY_TARGET on AC before starting. That gate is not
# decoration: the same binary and prompt measured 9.5 t/s at 13% battery and
# 13.9 t/s once the machine had headroom, a ~30% swing, while
# /sys/firmware/acpi/platform_profile read "performance" in BOTH cases. Peak GPU
# package power is recorded per row as the real tell (~80 W healthy, ~20 W
# throttled).
#
# There is deliberately no pre-0731 + DSpark row: the support GGUF is
# checkpoint-specific to Flash 0731 and drafts nothing against an older
# checkpoint (proposed=0), per antirez's README.
#
# Flags reach ds4 via `shift` + "$@". Do not collapse them into a single
# variable: zsh does not word-split an unquoted "$var", so "--mtp path --dspark"
# would arrive as one argv element and ds4 answers with its usage text.
#
# Usage: BATTERY_TARGET=78 scripts/bench/ds4_dspark_charged.sh [out.md]
set -u
DS4=${DS4_DIR:-$HOME/Workspace/llms/ds4}
HUB=${HF_HOME:-$HOME/.cache/huggingface}/hub/models--antirez--deepseek-v4-gguf/snapshots
OUT=${1:-ds4-dspark-charged.md}
TARGET=${BATTERY_TARGET:-78}
TMP=${TMPDIR:-/tmp}
P='Write a Python function that merges two sorted lists into one sorted list. Code only.'

OLD=$(ls "$HUB"/*/DeepSeek-V4-Flash-IQ2XXS-*-chat-v2-imatrix.gguf 2>/dev/null | head -1)
NEW=$(ls "$HUB"/*/DeepSeek-V4-Flash-IQ2XXS-*-imatrix-0731.gguf 2>/dev/null | head -1)
SUP=$(ls "$HUB"/*/DeepSeek-V4-Flash-DSpark-support-0731.gguf 2>/dev/null | head -1)
cd "$DS4" || exit 1

while :; do
  cap=$(cat /sys/class/power_supply/BAT0/capacity 2>/dev/null || echo 100)
  ac=$(cat /sys/class/power_supply/AC0/online 2>/dev/null || echo 1)
  [ "$cap" -ge "$TARGET" ] && [ "$ac" = "1" ] && break
  echo "[$(date +%H:%M)] battery ${cap}%, waiting for ${TARGET}% on AC"
  sleep 300
done

pkill -x ds4-server 2>/dev/null; pkill -x llamastash 2>/dev/null; sleep 3

run() {
  label=$1; model=$2; shift 2
  [ -n "$model" ] || { echo "| $label | missing GGUF | - | - | - |" >> "$OUT"; return; }
  b0=$(cat /sys/class/power_supply/BAT0/capacity)
  ( for _ in $(seq 1 90); do rocm-smi --showpower 2>/dev/null \
      | grep -oE "[0-9]+\.[0-9]+" | tail -1; sleep 4; done > "$TMP/pw_$label.log" ) &
  samp=$!
  DS4_DSPARK_STATS=1 timeout 1800 ./ds4 -m "$model" "$@" \
    --temp 0 --nothink --tokens 200 -p "$P" > "$TMP/out_$label.txt" 2>"$TMP/err_$label.log"
  kill $samp 2>/dev/null
  rate=$(grep -oE 'generation: [0-9.]+ t/s' "$TMP/err_$label.log" | tail -1)
  peak=$(sort -rn "$TMP/pw_$label.log" 2>/dev/null | head -1)
  acc=$(grep -oE 'accept_rate=[0-9.]+%' "$TMP/err_$label.log" | tail -1)
  printf '| %s | %s | %s W peak | %s%%->%s%% | %s |\n' "$label" "${rate:-FAILED}" \
    "${peak:-?}" "$b0" "$(cat /sys/class/power_supply/BAT0/capacity)" "${acc:--}" >> "$OUT"
  printf '%-14s %-26s peak %sW\n' "$label" "${rate:-FAILED}" "${peak:-?}"
}

{
  echo "# ds4 three-way decode (charged run)"
  echo
  echo "$(uname -sm), ds4 $(git rev-parse --short HEAD). $(date -Is)"
  echo "Profile: $(cat /sys/firmware/acpi/platform_profile 2>/dev/null), battery ${cap}% on AC."
  echo "greedy, --nothink, 200 tokens."
  echo
  echo "| config | decode | GPU power | battery | acceptance |"
  echo "|---|---:|---:|---|---:|"
} > "$OUT"

run pre-0731      "$OLD"
run 0731          "$NEW"
run "0731+dspark" "$NEW" --mtp "$SUP" --dspark

{
  echo
  echo '```'
  grep -oE "proposed=[0-9]+ accepted_draft=[0-9]+ accept_rate=[0-9.]+%" "$TMP/err_0731+dspark.log" | tail -1
  grep -oE "replay=[0-9.]+|net_saved=[-0-9.]+|verify=[0-9.]+" "$TMP/err_0731+dspark.log" | tail -3
  echo '```'
  diff -q "$TMP/out_0731.txt" "$TMP/out_0731+dspark.txt" >/dev/null 2>&1 \
    && echo "DSpark output matches plain greedy." \
    || echo "DSpark output DIFFERS from plain greedy (upstream fails 3/5 fixture cases)."
} >> "$OUT"

cat "$OUT"
