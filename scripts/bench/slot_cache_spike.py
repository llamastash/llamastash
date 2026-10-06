#!/usr/bin/env python3
"""Time llama-server slot save / restore against reprocessing the same prompt.

Starts a raw llama-server with --slot-save-path, fills slot 0 with a prompt of
--tokens tokens, saves the slot, restarts the server, restores the slot and
resends the prompt. Prints one JSON object with the timings and the file size.

  scripts/bench/slot_cache_spike.py --model /path/to/model.gguf \
      --tokens 100000 --ctx 110000 --save-dir target/slot-spike

Stdlib only. Keep --save-dir off tmpfs: the file is the whole KV cache.
"""

import argparse
import json
import os
import random
import subprocess
import sys
import time
import urllib.request

WORDS = (
  "the of and to in is that for it as was with be by on not he this are or his from at which but "
  "have an had they you were their one all we can her has there been if more when will would who "
  "so no out up into than them its time only could new these two may then do first any my now "
  "such like our over man me even most made after also did many before must through years where "
  "much your way well down should because each just those people how too little state good very "
  "make world still own see men work long get here between both life being under never day same "
  "another know while last might us great old year off come since against go came right used take"
).split()


def call(port, path, body=None, timeout=3600):
  data = json.dumps(body).encode() if body is not None else None
  req = urllib.request.Request(
    f"http://127.0.0.1:{port}{path}",
    data=data,
    method="POST" if body is not None else "GET",
    headers={"Content-Type": "application/json"},
  )
  with urllib.request.urlopen(req, timeout=timeout) as resp:
    return json.loads(resp.read())


def start(args, log):
  argv = [
    args.server, "-m", args.model, "--host", "127.0.0.1", "--port", str(args.port),
    "-c", str(args.ctx), "-np", "1", "-ngl", str(args.ngl), "-t", str(args.threads),
    "--slot-save-path", args.save_dir, "--cache-ram", "0",
  ]
  began = time.monotonic()
  proc = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)
  while True:
    if proc.poll() is not None:
      sys.exit(f"llama-server exited with {proc.returncode}; see {log.name}")
    try:
      call(args.port, "/health", timeout=2)
      return proc, time.monotonic() - began
    except OSError:
      time.sleep(0.2)


def stop(proc):
  proc.terminate()
  proc.wait(timeout=60)


def complete(port, tokens):
  began = time.monotonic()
  out = call(port, "/completion", {"prompt": tokens, "n_predict": 1, "cache_prompt": True, "id_slot": 0})
  timings = out["timings"]
  return {
    "wall_s": round(time.monotonic() - began, 3),
    "prompt_n": timings["prompt_n"],
    "prompt_ms": round(timings["prompt_ms"], 1),
    "cache_n": timings.get("cache_n"),
  }


def main():
  ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
  ap.add_argument("--model", required=True)
  ap.add_argument("--server", default="llama-server")
  ap.add_argument("--port", type=int, default=41977)
  ap.add_argument("--tokens", type=int, default=100_000)
  ap.add_argument("--ctx", type=int, default=110_000)
  ap.add_argument("--ngl", type=int, default=0)
  ap.add_argument("--threads", type=int, default=os.cpu_count() // 2)
  ap.add_argument("--save-dir", required=True)
  args = ap.parse_args()

  os.makedirs(args.save_dir, exist_ok=True)
  args.save_dir = os.path.abspath(args.save_dir)
  log = open(os.path.join(args.save_dir, "server.log"), "w")
  result = {"tokens_requested": args.tokens, "ctx": args.ctx, "ngl": args.ngl, "threads": args.threads}

  proc, load_s = start(args, log)
  try:
    result["load_s"] = round(load_s, 2)
    rng = random.Random(7)
    text = " ".join(rng.choice(WORDS) for _ in range(int(args.tokens * 1.3)))
    tokens = call(args.port, "/tokenize", {"content": text, "add_special": True})["tokens"][: args.tokens]
    result["tokens"] = len(tokens)
    result["reprocess"] = complete(args.port, tokens)

    saved = call(args.port, "/slots/0?action=save", {"filename": "spike.bin"})
    result["save"] = {"n_saved": saved["n_saved"], "n_written": saved["n_written"], "save_ms": round(saved["timings"]["save_ms"], 1)}
    result["file_bytes"] = os.path.getsize(os.path.join(args.save_dir, "spike.bin"))
  finally:
    stop(proc)

  proc, load_s = start(args, log)
  try:
    result["reload_s"] = round(load_s, 2)
    restored = call(args.port, "/slots/0?action=restore", {"filename": "spike.bin"})
    result["restore"] = {"n_restored": restored["n_restored"], "n_read": restored["n_read"], "restore_ms": round(restored["timings"]["restore_ms"], 1)}
    result["after_restore"] = complete(args.port, tokens)
  finally:
    stop(proc)
    os.remove(os.path.join(args.save_dir, "spike.bin"))

  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
