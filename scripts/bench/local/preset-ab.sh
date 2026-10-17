#!/usr/bin/env bash
# A/B two named presets from the user's real config, alternating to cancel thermal drift.
#
# Answers "which preset should I code with?" -- so it measures the agent loop, not
# raw decode: one cold prefill, then follow-up turns that append monotonically the
# way a coding session does.
#
#   scripts/bench/preset-ab.sh <llamastash-binary> <model-substring> <preset>...
#   ROUNDS=4 scripts/bench/preset-ab.sh ...        # default 2
#
# Takes any number of presets. Point LS_REAL_CONFIG at a generated config to
# bench candidate presets without adding them to the user's real config.
#
# Two traps this harness exists to avoid:
#
#   last_params bleed. A named preset is self-contained: the daemon skips the
#   model's last_params layer for an explicit selection, so back-to-back A/B runs
#   no longer contaminate each other (B does not inherit knobs A set). The harness
#   still stops and restarts the daemon between launches so each preset starts from
#   a clean supervisor. Verify with `tr '\0' '\n' < /proc/<pid>/cmdline` if a
#   number looks wrong.
#
#   Order bias. Whichever preset runs first gets the cool GPU. Rounds alternate the
#   order, so an odd ROUNDS count still leaves a bias -- use an even number.
#
# Preset definitions are copied verbatim from the live config into a sandbox config
# dir, so 0.2.0's preset write path can never rewrite the real file.
# Aborts and stops all models if the battery falls below MIN_BATT.
set -uo pipefail

BIN="${1:?usage: preset-ab.sh <binary> <model> <preset>... }"
MODEL="${2:?usage: preset-ab.sh <binary> <model> <preset>... }"
shift 2
PRESETS=("$@")
[[ ${#PRESETS[@]} -ge 1 ]] || { echo "need at least one preset"; exit 2; }
ROUNDS="${ROUNDS:-2}"
TURNS=4
CTX_BYTES=64000       # ~16k tokens of real source as the standing context
MAX_TOKENS=600        # must clear the reasoning block or `content` comes back empty

ROOT="${BENCH_ROOT:-$HOME/.cache/llamastash-preset-ab}"
SRC_CONFIG="${LS_REAL_CONFIG:-$HOME/.config/llamastash/config.yaml}"
export LLAMASTASH_STATE_DIR="$ROOT/state"
export LLAMASTASH_CONFIG_DIR="$ROOT/config"
export LLAMASTASH_CACHE_DIR="$ROOT/cache"
mkdir -p "$LLAMASTASH_STATE_DIR" "$LLAMASTASH_CONFIG_DIR" "$LLAMASTASH_CACHE_DIR"

# Real config verbatim: same servers list, same preset bodies, same model paths.
cp -L "$SRC_CONFIG" "$LLAMASTASH_CONFIG_DIR/config.yaml"

. "$(dirname "$0")/lib-power.sh"
power_watchdog_start "$$"
power_traps_install

# --force: skip the backend precheck so a host without a usable llama-server
# still starts the sandbox daemon.
start_daemon() { "$BIN" daemon start --force >/dev/null 2>&1; sleep 3; }

# Stop the previous model and restart the daemon so the next preset starts from a
# clean supervisor. No state.json wipe: the daemon isolates a named preset from
# last_params, so there is nothing to drop.
reset_state() {
  "$BIN" stop --all --yes >/dev/null 2>&1
  "$BIN" daemon stop    >/dev/null 2>&1; sleep 1
  start_daemon
}

# The standing context is real source, not generated filler: synthetic repetition
# inflates MTP acceptance and would flatter whichever preset speculates harder.
CTX_FILE="$ROOT/context.txt"
find src -name '*.rs' -size +4k | sort | xargs cat 2>/dev/null | head -c "$CTX_BYTES" > "$CTX_FILE"

"$BIN" daemon stop >/dev/null 2>&1; sleep 1
start_daemon

echo "context: $(wc -c < "$CTX_FILE") bytes of real source"
echo "model:   $MODEL"
echo "presets: ${PRESETS[*]}"
echo "rounds:  $ROUNDS x ${#PRESETS[@]} presets x $TURNS turns, max_tokens=$MAX_TOKENS"
echo "power:   TDP $(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')W  AC $(on_ac)  battery $(battery)%"
echo

for r in $(seq 1 "$ROUNDS"); do
  if (( r % 2 == 1 )); then
    order=("${PRESETS[@]}")
  else
    order=(); for ((i=${#PRESETS[@]}-1; i>=0; i--)); do order+=("${PRESETS[i]}"); done
  fi
  for preset in "${order[@]}"; do
    wait_for_charge
    guard
    reset_state
    launch="$("$BIN" start "$MODEL" --preset "$preset" --wait --json 2>&1)"
    port="$(printf '%s' "$launch" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("port",""))' 2>/dev/null)"
    if [[ -z "$port" ]]; then
      echo "round $r $preset: LAUNCH FAILED"
      printf '%s\n' "$launch" | head -5
      continue
    fi
    srv_pid="$(pgrep -f "llama-server .*--port $port" | head -1)"
    echo "round $r $preset  argv: $(tr '\0' ' ' < /proc/$srv_pid/cmdline 2>/dev/null | sed 's|.*llama-server ||' | cut -c1-240)"
    PORT="$port" PRESET="$preset" ROUND="$r" TURNS="$TURNS" MAX_TOKENS="$MAX_TOKENS" \
    CTX_FILE="$CTX_FILE" BATT="$(battery)" AC="$(on_ac)" python3 - <<'PY'
import json, os, statistics, time, urllib.request

port, preset, rnd = os.environ['PORT'], os.environ['PRESET'], os.environ['ROUND']
turns, batt, ac = int(os.environ['TURNS']), os.environ['BATT'], os.environ['AC']
maxtok = int(os.environ['MAX_TOKENS'])
URL = f'http://127.0.0.1:{port}/v1/chat/completions'
src = open(os.environ['CTX_FILE'], errors='replace').read()

def call(msgs, max_tokens):
    body = json.dumps({'model': 'q', 'messages': msgs, 'max_tokens': max_tokens}).encode()
    req = urllib.request.Request(URL, body, {'Content-Type': 'application/json'})
    t0 = time.time()
    d = json.loads(urllib.request.urlopen(req, timeout=3600).read())
    ch = d['choices'][0]
    return ch['message'], ch.get('finish_reason', '?'), d.get('timings', {}), time.time() - t0

warm = ('You are reviewing this Rust codebase. Answer briefly.\n\n' + src
        + '\n\nName the single most repeated pattern above.')
msgs = [{'role': 'user', 'content': warm}]
m, fin, t, wall = call(msgs, maxtok)
print('  COLD  %6d tok prefill  %6.1fs  (%5.0f t/s)  wall %5.1fs'
      % (t.get('prompt_n', 0), t.get('prompt_ms', 0) / 1000,
         t.get('prompt_per_second', 0), wall))

follow = ['Suggest one refactor that removes that duplication.',
          'Write a unit test for your suggestion.',
          'What edge case would that test miss?',
          'Summarize your three answers in two lines.']
dec, acc, pre, walls, empty, truncated = [], [], [], [], 0, 0
for i in range(turns):
    msgs = msgs + [{'role': 'assistant', 'content': m.get('content') or 'ok'},
                   {'role': 'user', 'content': follow[i % len(follow)]}]
    m, fin, t, wall = call(msgs, maxtok)
    txt = (m.get('content') or '').strip()
    # Distinguish the two failure shapes: no answer at all, versus an answer the
    # token cap cut off. Only the first is the model misbehaving.
    if not txt:
        empty += 1
    if fin == 'length':
        truncated += 1
    a = 100 * t.get('draft_n_accepted', 0) / max(t.get('draft_n', 1), 1)
    dec.append(t.get('predicted_per_second', 0)); acc.append(a)
    pre.append(t.get('prompt_ms', 0) / 1000); walls.append(wall)
    print('    turn %d  reprefill %6d tok %5.1fs   decode %6.2f t/s   accept %5.1f%%   '
          'wall %5.1fs  %4d chars  finish=%s'
          % (i + 1, t.get('prompt_n', 0), pre[-1], dec[-1], a, wall, len(txt), fin))
print('  >> %-10s median decode %6.2f t/s  reprefill %4.1fs  wall %5.1fs  accept %4.1f%%  '
      'empty %d/%d  truncated %d/%d  batt %s%% ac %s'
      % (preset, statistics.median(dec), statistics.median(pre), statistics.median(walls),
         statistics.median(acc), empty, turns, truncated, turns, batt, ac))
print()
PY
  done
done

"$BIN" stop --all --yes >/dev/null 2>&1
"$BIN" daemon stop >/dev/null 2>&1
echo "sandbox: $ROOT   (real config untouched)"
