#!/usr/bin/env python3
"""Measure llama-server slot save/restore against plain prompt reprocessing.

Starts `llama-server` three times on one model and prints, for a prompt of
`--tokens` tokens:

- run 1: cold prompt processing, slot save (time, bytes), the same prompt warm
- run 2: restore with the file in page cache, then two returning prompts, one
  that keeps the whole saved prompt and one that diverges at 90% of it
- run 3: restore after the file was dropped from page cache, then a returning
  prompt

Talks to llama-server directly, not through llamastash, so the numbers are the
engine's own. Needs only the Python standard library.

    scripts/bench/slot_save_spike.py --model <gguf> --slot-dir <dir on real disk> \
        --tokens 100000 --ctx 131072 --ngl 99 --out result.json

`--slot-dir` must not be on tmpfs: the file is the KV cache, several GiB at
100k tokens. Record the power state next to the result (see README.md).
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request


def http(port, method, path, body=None, timeout=3600):
  data = None if body is None else json.dumps(body).encode()
  req = urllib.request.Request(
    f"http://127.0.0.1:{port}{path}",
    data=data,
    method=method,
    headers={"Content-Type": "application/json"},
  )
  start = time.monotonic()
  try:
    with urllib.request.urlopen(req, timeout=timeout) as resp:
      payload = json.loads(resp.read() or b"{}")
      status = resp.status
  except urllib.error.HTTPError as err:
    payload = json.loads(err.read() or b"{}")
    status = err.code
  return status, payload, time.monotonic() - start


class Server:
  def __init__(self, args, log_path):
    self.args = args
    self.log_path = log_path
    self.proc = None
    self.load_secs = None

  def start(self):
    cmd = [
      self.args.server,
      "-m", self.args.model,
      "-c", str(self.args.ctx),
      "-np", str(self.args.parallel),
      "-ngl", str(self.args.ngl),
      "--host", "127.0.0.1",
      "--port", str(self.args.port),
      "--slot-save-path", self.args.slot_dir,
    ] + self.args.extra
    log = open(self.log_path, "ab")
    log.write(f"\n=== {' '.join(cmd)}\n".encode())
    log.flush()
    start = time.monotonic()
    self.proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    while True:
      if self.proc.poll() is not None:
        sys.exit(f"llama-server exited early, see {self.log_path}")
      try:
        status, _, _ = http(self.args.port, "GET", "/health", timeout=2)
        if status == 200:
          break
      except (urllib.error.URLError, OSError):
        pass
      time.sleep(0.2)
    self.load_secs = time.monotonic() - start

  def stop(self):
    self.proc.send_signal(signal.SIGTERM)
    self.proc.wait(timeout=60)


def filler_tokens(port, want):
  """`want` token ids of varied text, tokenized by the server itself."""
  tokens = []
  line = 0
  while len(tokens) < want:
    chunk = "".join(
      f"Record {n:07d}: station {n * 7919 % 9973} logged value {n * 104729 % 100003} "
      f"under tag {n * 31 % 997:03d}-{n * 17 % 89:02d}.\n"
      for n in range(line, line + 2000)
    )
    line += 2000
    status, out, _ = http(port, "POST", "/tokenize", {"content": chunk, "add_special": not tokens})
    if status != 200:
      sys.exit(f"/tokenize failed: {out}")
    tokens.extend(out["tokens"])
  return tokens[:want]


def complete(port, tokens):
  status, out, wall = http(
    port,
    "POST",
    "/completion",
    {"prompt": tokens, "n_predict": 4, "cache_prompt": True, "temperature": 0},
  )
  if status != 200:
    sys.exit(f"/completion failed ({status}): {out}")
  timings = out.get("timings", {})
  return {
    "wall_secs": round(wall, 3),
    "prompt_tokens": len(tokens),
    "processed_tokens": timings.get("prompt_n"),
    "cached_tokens": timings.get("cache_n", out.get("tokens_cached")),
    "prompt_ms": timings.get("prompt_ms"),
    "prompt_per_second": timings.get("prompt_per_second"),
    "id_slot": out.get("id_slot"),
  }


def slot_action(port, slot, action, filename):
  status, out, wall = http(port, "POST", f"/slots/{slot}?action={action}", {"filename": filename})
  return {"status": status, "wall_secs": round(wall, 3), **out}


def drop_from_page_cache(path):
  fd = os.open(path, os.O_RDONLY)
  try:
    os.fsync(fd)
    os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
  finally:
    os.close(fd)


def main():
  ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
  ap.add_argument("--server", default="llama-server")
  ap.add_argument("--model", required=True)
  ap.add_argument("--slot-dir", required=True)
  ap.add_argument("--tokens", type=int, default=100_000)
  ap.add_argument("--ctx", type=int, default=131_072)
  ap.add_argument("--parallel", type=int, default=1)
  ap.add_argument("--ngl", type=int, default=99)
  ap.add_argument("--port", type=int, default=41990)
  ap.add_argument("--out")
  ap.add_argument("extra", nargs="*", help="extra llama-server args, after --")
  args = ap.parse_args()

  os.makedirs(args.slot_dir, exist_ok=True)
  filename = "spike-slot0.bin"
  slot_file = os.path.join(args.slot_dir, filename)
  server = Server(args, os.path.join(args.slot_dir, "llama-server.log"))
  result = {"args": {k: v for k, v in vars(args).items() if k != "out"}}

  server.start()
  result["run1_load_secs"] = round(server.load_secs, 2)
  base = filler_tokens(args.port, args.tokens)
  tail = filler_tokens(args.port, 64)[1:]
  result["cold"] = complete(args.port, base)
  result["save"] = slot_action(args.port, result["cold"]["id_slot"] or 0, "save", filename)
  result["file_bytes"] = os.path.getsize(slot_file)
  result["warm_same_prompt"] = complete(args.port, base)
  server.stop()

  server.start()
  result["run2_load_secs"] = round(server.load_secs, 2)
  result["restore_file_in_page_cache"] = slot_action(args.port, 0, "restore", filename)
  result["returning_keeps_all"] = complete(args.port, base + tail)
  result["restore_again"] = slot_action(args.port, 0, "restore", filename)
  result["returning_diverges_at_90pct"] = complete(args.port, base[: args.tokens * 9 // 10] + tail)
  server.stop()

  drop_from_page_cache(slot_file)
  server.start()
  result["run3_load_secs"] = round(server.load_secs, 2)
  result["restore_file_from_disk"] = slot_action(args.port, 0, "restore", filename)
  result["returning_after_disk_restore"] = complete(args.port, base + tail)
  server.stop()

  text = json.dumps(result, indent=2)
  print(text)
  if args.out:
    with open(args.out, "w") as fh:
      fh.write(text + "\n")


if __name__ == "__main__":
  main()
