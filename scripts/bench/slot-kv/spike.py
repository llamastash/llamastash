#!/usr/bin/env python3
"""Spike: llama.cpp slot KV save/restore measured against plain reprocessing.

Answers four questions before any of it goes into the launcher:

  - how long does `POST /slots/{id}?action=save` take, and how big is the file
  - how long does `action=restore` take into a fresh process
  - what does a returning request cost after a restore, against the cost of
    reprocessing the same prompt from zero
  - does the restored slot actually get *selected* for the returning prompt

WHY A RESTART BETWEEN SAVE AND RESTORE. The whole point is the cross-process
case: llama.cpp keeps the KV cache in the child process, so an eviction that
SIGTERMs it loses the prompt. Saving and restoring inside one process would
measure the prompt cache, not this endpoint pair.

WHY THE TIME COMPARISON IS WALL CLOCK. The quantity we care about is what a
caller waits for on the first request after a reload, and that wait includes
HTTP and tokenisation. `tokens eval` from the engine's own timing block is
printed next to it so the two can be told apart.

Long-context prompts are built by repeating a corpus until `/tokenize` reports
the target token count, so the measured prompt length is the engine's own
count rather than a word estimate.

usage:
    python3 scripts/bench/slot-kv/spike.py \
        --model /path/to/small.gguf --targets 8192,32768,100000
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

import requests

CORPUS = (
    "The lighthouse keeper logged the weather every hour on the hour: wind from "
    "the north-east, sea state four, visibility narrowing as the rain came in off "
    "the point. He had kept the light for nineteen years and he trusted the log "
    "more than his own memory of any single night. "
)

def free_port() -> int:
  with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    return s.getsockname()[1]


class Server:
  """One `llama-server` lifetime, with the log kept so slot selection can be read back."""

  def __init__(self, binary: str, model: str, ctx: int, slots: int, save_dir: str, gpu_layers: str):
    self.binary = binary
    self.model = model
    self.ctx = ctx
    self.slots = slots
    self.save_dir = save_dir
    self.gpu_layers = gpu_layers
    self.port = free_port()
    self.log_path = os.path.join(save_dir, f"server-{self.port}.log")
    self.proc: subprocess.Popen | None = None
    self.base = f"http://127.0.0.1:{self.port}"

  def start(self) -> None:
    # --slots is on by default; --slot-save-path is what enables save/restore.
    argv = [
      self.binary,
      "-m",
      self.model,
      "--host",
      "127.0.0.1",
      "--port",
      str(self.port),
      "-c",
      str(self.ctx),
      "-np",
      str(self.slots),
      "-ngl",
      self.gpu_layers,
      "--slot-save-path",
      self.save_dir + os.sep,
    ]
    os.makedirs(self.save_dir, exist_ok=True)
    self.log = open(self.log_path, "w")
    self.proc = subprocess.Popen(argv, stdout=self.log, stderr=subprocess.STDOUT)
    deadline = time.time() + 600
    while time.time() < deadline:
      if self.proc.poll() is not None:
        raise RuntimeError(f"server exited early, see {self.log_path}")
      try:
        if requests.get(f"{self.base}/health", timeout=2).status_code == 200:
          return
      except requests.RequestException:
        pass
      time.sleep(0.5)
    raise RuntimeError(f"server did not become healthy, see {self.log_path}")

  def stop(self) -> None:
    if self.proc and self.proc.poll() is None:
      self.proc.send_signal(signal.SIGTERM)
      try:
        self.proc.wait(timeout=60)
      except subprocess.TimeoutExpired:
        self.proc.kill()
        self.proc.wait()
    if getattr(self, "log", None):
      self.log.close()
      self.log = None

  def log_text(self) -> str:
    try:
      with open(self.log_path, errors="replace") as fh:
        return fh.read()
    except FileNotFoundError:
      return ""

  def post(self, path: str, payload: dict, timeout: float = 1800) -> tuple[dict, float]:
    t0 = time.perf_counter()
    r = requests.post(f"{self.base}{path}", json=payload, timeout=timeout)
    dt = time.perf_counter() - t0
    r.raise_for_status()
    return r.json(), dt


def count_tokens(srv: Server, text: str) -> int:
  j, _ = srv.post("/tokenize", {"content": text}, timeout=300)
  return len(j.get("tokens", []))


def build_prompt(srv: Server, target: int) -> str:
  """Repeat the corpus until the engine's own token count reaches the target."""
  unit = count_tokens(srv, CORPUS)
  repeats = max(1, int(target / max(1, unit)))
  text = CORPUS * repeats
  n = count_tokens(srv, text)
  while n < target:
    repeats = max(1, int(repeats * (target / max(1, n)) * 1.02) + 1)
    text = CORPUS * repeats
    n = count_tokens(srv, text)
  return text


EVAL_RE = re.compile(r"prompt eval time =\s+([\d.]+) ms /\s+(\d+) tokens")
NPAST_RE = re.compile(r"n_past = (\d+)")
SELECT_RE = re.compile(r"selected slot by (\w+) similarity")


def tail_metrics(srv: Server, marker: str) -> dict:
  """Pull the timing block for the last request that logged `marker`.

  `eval_tokens` is the engine's own counter for how much of the prompt it had
  to process, which is the quantity the restored cache is supposed to remove.
  """
  lines = srv.log_text().splitlines()
  out = {"eval_ms": None, "eval_tokens": None, "n_past": None, "selected_by": None}
  hits = [i for i, line in enumerate(lines) if marker in line]
  if not hits:
    return out
  window = lines[max(0, hits[-1] - 200) : hits[-1] + 400]
  for line in reversed(window):
    if out["eval_ms"] is None:
      if m := EVAL_RE.search(line):
        out["eval_ms"] = float(m.group(1))
        out["eval_tokens"] = int(m.group(2))
    if out["n_past"] is None:
      if m := NPAST_RE.search(line):
        out["n_past"] = int(m.group(1))
    if out["selected_by"] is None:
      if m := SELECT_RE.search(line):
        out["selected_by"] = m.group(1)
    if out["eval_ms"] is not None and out["n_past"] is not None and out["selected_by"] is not None:
      break
  return out


def run_target(args, target: int, save_dir: str) -> dict:
  result = {"target_tokens": target}
  os.makedirs(save_dir, exist_ok=True)
  slot = 0
  first = Server(args.llama_server, args.model, args.ctx, args.slots, save_dir, args.ngl)
  second = Server(args.llama_server, args.model, args.ctx, args.slots, save_dir, args.ngl)
  try:
    first.start()
    prompt = build_prompt(first, target)
    result["prompt_tokens"] = count_tokens(first, prompt)

    # Baseline: an empty process reprocesses every token.
    _, t_regen = first.post("/completion", {"prompt": prompt, "n_predict": 1, "ignore_eos": True})
    m = tail_metrics(first, "prompt eval time")
    result["reprocess_s"] = round(t_regen, 2)
    result["reprocess_eval_ms"] = m["eval_ms"]

    # A distinct prompt of the same size, so a hit on the restored slot can be
    # told apart from a generic speed-up.
    other = ("Nothing about the lighthouse appears in this second passage. " * (target // 12 + 1))[: len(prompt)]
    control_slot = args.slots - 1
    if control_slot != slot:
      _, t_control = first.post("/completion", {"prompt": other, "n_predict": 1, "ignore_eos": True})
      result["control_s"] = round(t_control, 2)

    j, t_save = first.post(f"/slots/{slot}?action=save", {"filename": args.filename})
    result["save_s"] = round(t_save, 2)
    result["save_bytes"] = j.get("n_written")
    result["save_n_tokens"] = j.get("n_saved")
    result["file_mb"] = round((j.get("n_written") or 0) / 1e6, 1)

    first.stop()
    first = None

    # A second process, same argv, same save dir: the eviction round trip.
    second.start()
    try:
      j, t_restore = second.post(f"/slots/{slot}?action=restore", {"filename": args.filename})
    except requests.HTTPError as err:
      result["restore_error"] = str(err)[:200] + " | " + second.log_text()[-400:]
      return result
    result["restore_s"] = round(t_restore, 2)
    result["restore_n_tokens"] = j.get("n_restored")
    result["restore_bytes"] = j.get("n_read")
    result["on_disk_mb"] = round(os.path.getsize(os.path.join(save_dir, args.filename)) / 1e6, 1)

    _, t_rerun = second.post("/completion", {"prompt": prompt, "n_predict": 1, "ignore_eos": True})
    m = tail_metrics(second, "prompt eval time")
    result["rerun_s"] = round(t_rerun, 2)
    result["rerun_eval_ms"] = m["eval_ms"]
    result["rerun_eval_tokens"] = m["eval_tokens"]
    result["rerun_n_past"] = m["n_past"]
    result["slot_selected_by"] = m["selected_by"]
    return result
  finally:
    for srv in (first, second):
      if srv:
        srv.stop()


def run_control_after_reuse(args, target: int, save_dir: str) -> dict:
  """A mismatched prompt sent to a slot holding a restored cache must not lie."""
  out = {}
  first = Server(args.llama_server, args.model, args.ctx, args.slots, save_dir, args.ngl)
  try:
    first.start()
    prompt = build_prompt(first, target)
    first.post("/completion", {"prompt": prompt, "n_predict": 1, "ignore_eos": True})
    first.post("/slots/0?action=save", {"filename": args.filename})
  finally:
    first.stop()
  second = Server(args.llama_server, args.model, args.ctx, args.slots, save_dir, args.ngl)
  try:
    second.start()
    second.post("/slots/0?action=restore", {"filename": args.filename})
    other = ("An unrelated passage about harbours and fishing boats. " * (target // 10 + 1))[: len(prompt)]
    _, t = second.post("/completion", {"prompt": other, "n_predict": 1, "ignore_eos": True})
    out["unrelated_prompt_s"] = round(t, 2)
    out["unrelated_eval_ms"] = tail_metrics(second, "prompt eval time")["eval_ms"]
  except requests.HTTPError as err:
    out["error"] = str(err)[:200]
  finally:
    second.stop()
  return out


def main() -> int:
  ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  ap.add_argument("--model", required=True)
  ap.add_argument("--llama-server", default="llama-server")
  ap.add_argument("--targets", default="8192,32768,100000")
  ap.add_argument("--ctx", type=int, default=131072)
  ap.add_argument("--slots", type=int, default=1)
  ap.add_argument("--ngl", default="99", help="layers to offload, -ngl 0 pins to CPU")
  ap.add_argument("--save-dir", default=os.path.expanduser("~/.cache/llamastash-slot-spike"))
  ap.add_argument("--filename", default="spike.bin")
  ap.add_argument("--json-out", default=None)
  args = ap.parse_args()

  all_rows = []
  for target in [int(t) for t in args.targets.split(",")]:
    save_dir = os.path.join(args.save_dir, f"t{target}")
    shutil.rmtree(save_dir, ignore_errors=True)
    row = run_target(args, target, save_dir)
    if args.slots > 1:
      row.update(run_control_after_reuse(args, target, save_dir))
    all_rows.append(row)
    print(json.dumps(row, indent=2), flush=True)

  if args.json_out:
    with open(args.json_out, "w") as fh:
      json.dump(all_rows, fh, indent=2)

  print("\n| target | prompt | reprocess s | save s | save MB | restore s | rerun s | rerun eval tokens | selected by |")
  print("|---:|---:|---:|---:|---:|---:|---:|---:|---|")
  for r in all_rows:
    print(
      f"| {r.get('target_tokens')} | {r.get('prompt_tokens')} | {r.get('reprocess_s')} "
      f"| {r.get('save_s')} | {r.get('on_disk_mb')} | {r.get('restore_s')} | {r.get('rerun_s')} "
      f"| {r.get('rerun_eval_tokens')} | {r.get('slot_selected_by')} |"
    )
  return 0


if __name__ == "__main__":
  sys.exit(main())
