#!/usr/bin/env python3
"""Edge cases for llama.cpp slot KV save/restore, the ones a launcher has to survive.

Runs next to `spike.py` (which measures the happy path) and answers what happens
when the relaunch is not identical to the launch that wrote the file, because a
reload through the launcher can differ in every one of these ways: a `--fit`
that lands a smaller context, a different server build, fewer slots, or a file
that got truncated.

Each case prints one JSON object: what was attempted, what the engine said, and
whether the slot stayed usable afterwards. `slot_usable_after` matters more than
the error itself — a failed restore that wedges the slot is a launcher bug,
while a failed restore that just means "process the prompt normally" is fine.

usage:
    python3 scripts/bench/slot-kv/edge_cases.py --model <small.gguf> --other-model <small2.gguf>
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import sys

import requests

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from spike import Server, tail_metrics  # noqa: E402

PROMPT = "Explain the history of lighthouses in Europe. " * 60
OTHER = "Describe the migration patterns of Arctic terns. " * 60

# Varied prose, because a repeated prompt makes greedy decoding degenerate: an
# identical continuation of "Explain the history of lighthouses" repeated 48
# times would look like agreement even from a corrupted cache.
IDENTITY_PROMPT = (
    "The harbour master kept three ledgers. The first recorded every vessel that "
    "passed the breakwater, its flag, and the hour. The second held the names of "
    "the crew, copied from the manifest in a hand that shrank in winter. The "
    "third was private, and in it he argued with himself about whether the new "
    "buoy had been set too far east.\n\n"
    "On the ninth of March a fishing boat came back without its engine, towed "
    "four hours through a chop by a coaster that refused payment. The master "
    "logged the coaster's name and nothing else, because the coaster's captain "
    "asked him not to write the rest, and he had known that captain since both "
    "were boys on the same grey quay.\n\n"
    "Summarise what these three entries reveal about the man, in three short "
    "sentences."
)


def ask(srv: Server, prompt: str, extra: dict | None = None) -> tuple[dict, float]:
  payload = {"prompt": prompt, "n_predict": 1, "ignore_eos": True}
  payload.update(extra or {})
  return srv.post("/completion", payload)


def hit(srv: Server, prompt: str, extra: dict | None = None) -> dict:
  """One request plus the log facts that show whether the cache was reused."""
  _, dt = ask(srv, prompt, extra)
  m = tail_metrics(srv, "prompt eval time")
  return {"wall_s": round(dt, 3), "eval_tokens": m["eval_tokens"], "selected_by": m["selected_by"]}


def err_text(exc: Exception, srv: Server) -> str:
  body = ""
  if isinstance(exc, requests.HTTPError) and exc.response is not None:
    body = exc.response.text[:300]
  else:
    body = str(exc)[:300]
  return body


def case_slot_hint(args) -> dict:
  """Does the request body accept `id_slot`, i.e. can a launcher place a prompt?"""
  d = os.path.join(args.save_dir, "slot_hint")
  shutil.rmtree(d, ignore_errors=True)
  srv = Server(args.llama_server, args.model, 16384, 4, d, args.ngl)
  try:
    srv.start()
    _, _ = ask(srv, PROMPT, {"id_slot": 2})
    log = srv.log_text()
    return {"log_says_selected_by_id": "selected slot by id (2)" in log}
  finally:
    srv.stop()


def case_cross_slot(args) -> dict:
  """Restore a file saved from slot 0 into slot 3 of a fresh process."""
  d = os.path.join(args.save_dir, "cross_slot")
  shutil.rmtree(d, ignore_errors=True)
  srv = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    srv.start()
    ask(srv, PROMPT)
    srv.post("/slots/0?action=save", {"filename": "x.bin"})
  finally:
    srv.stop()
  out = {}
  srv2 = Server(args.llama_server, args.model, 16384, 4, d, args.ngl)
  try:
    srv2.start()
    try:
      j, _ = srv2.post("/slots/3?action=restore", {"filename": "x.bin"})
      out["restored_into_3"] = j.get("n_restored")
    except requests.HTTPError as exc:
      out["restore_error"] = err_text(exc, srv2)
      return out
    out["hit_on_matching_prompt"] = hit(srv2, PROMPT)
    return out
  finally:
    srv2.stop()


def case_smaller_ctx(args) -> dict:
  """A prompt saved under a big context, restored into a slot too small for it."""
  d = os.path.join(args.save_dir, "smaller_ctx")
  shutil.rmtree(d, ignore_errors=True)
  long_prompt = PROMPT * 12  # about 6.6k tokens, above the small server's window
  big = Server(args.llama_server, args.model, 131072, 1, d, args.ngl)
  try:
    big.start()
    ask(big, long_prompt)
    big.post("/slots/0?action=save", {"filename": "x.bin"})
  finally:
    big.stop()
  out = {}
  small = Server(args.llama_server, args.model, 4096, 1, d, args.ngl)
  try:
    small.start()
    try:
      small.post("/slots/0?action=restore", {"filename": "x.bin"})
      out["restore_error"] = None
    except requests.HTTPError as exc:
      out["restore_error"] = err_text(exc, small)
    out["slot_usable_after"] = hit(small, PROMPT[:200])
    return out
  finally:
    small.stop()


def case_other_model(args) -> dict:
  """A file from one model restored by another model's process."""
  d = os.path.join(args.save_dir, "other_model")
  shutil.rmtree(d, ignore_errors=True)
  src = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    src.start()
    ask(src, PROMPT)
    src.post("/slots/0?action=save", {"filename": "x.bin"})
  finally:
    src.stop()
  out = {}
  # The stand-in second model is a smaller-context model, so the restore process
  # gets a context it accepts; the file being restored came from the first model.
  dst = Server(args.llama_server, args.other_model, 4096, 1, d, args.ngl)
  try:
    dst.start()
    try:
      dst.post("/slots/0?action=restore", {"filename": "x.bin"})
      out["restore_error"] = None
    except requests.HTTPError as exc:
      out["restore_error"] = err_text(exc, dst)
    out["slot_usable_after"] = hit(dst, PROMPT[:200])
    return out
  finally:
    dst.stop()


def case_missing_file(args) -> dict:
  """Restore naming a file that is not in the save dir."""
  d = os.path.join(args.save_dir, "missing")
  shutil.rmtree(d, ignore_errors=True)
  out = {}
  srv = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    srv.start()
    try:
      srv.post("/slots/0?action=restore", {"filename": "absent.bin"})
      out["restore_error"] = None
    except requests.HTTPError as exc:
      out["restore_error"] = err_text(exc, srv)
    out["slot_usable_after"] = hit(srv, PROMPT[:200])
    return out
  finally:
    srv.stop()


def case_truncated_file(args) -> dict:
  """A file cut in half, the shape of a crash mid-write."""
  d = os.path.join(args.save_dir, "truncated")
  shutil.rmtree(d, ignore_errors=True)
  src = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    src.start()
    ask(src, PROMPT)
    src.post("/slots/0?action=save", {"filename": "x.bin"})
  finally:
    src.stop()
  path = os.path.join(d, "x.bin")
  size = os.path.getsize(path)
  with open(path, "r+b") as fh:
    fh.truncate(size // 2)
  out = {"truncated_to": size // 2}
  srv = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    srv.start()
    try:
      srv.post("/slots/0?action=restore", {"filename": "x.bin"})
      out["restore_error"] = None
    except requests.HTTPError as exc:
      out["restore_error"] = err_text(exc, srv)
    out["slot_usable_after"] = hit(srv, PROMPT[:200])
    return out
  finally:
    srv.stop()


def case_two_conversations(args) -> dict:
  """Two conversations in two slots, both saved and both restored."""
  d = os.path.join(args.save_dir, "two")
  shutil.rmtree(d, ignore_errors=True)
  src = Server(args.llama_server, args.model, 32768, 2, d, args.ngl)
  try:
    src.start()
    ask(src, PROMPT)
    ask(src, OTHER)
    saved = []
    for slot in (0, 1):
      try:
        j, _ = src.post(f"/slots/{slot}?action=save", {"filename": f"s{slot}.bin"})
        saved.append((slot, j.get("n_saved")))
      except requests.HTTPError as exc:
        saved.append((slot, err_text(exc, src)))
    out = {"saved": saved}
  finally:
    src.stop()
  srv = Server(args.llama_server, args.model, 32768, 2, d, args.ngl)
  try:
    srv.start()
    restored = []
    for slot, _ in saved:
      try:
        j, _ = srv.post(f"/slots/{slot}?action=restore", {"filename": f"s{slot}.bin"})
        restored.append((slot, j.get("n_restored")))
      except requests.HTTPError as exc:
        restored.append((slot, err_text(exc, srv)))
    out["restored"] = restored
    out["first_prompt"] = hit(srv, PROMPT)
    out["second_prompt"] = hit(srv, OTHER)
    return out
  finally:
    srv.stop()


def case_save_missing_slot(args) -> dict:
  """save/restore against a slot id the process does not have."""
  d = os.path.join(args.save_dir, "bad_id")
  shutil.rmtree(d, ignore_errors=True)
  out = {}
  srv = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    srv.start()
    for action in ("save", "restore"):
      try:
        srv.post(f"/slots/7?action={action}", {"filename": "x.bin"})
        out[action] = None
      except requests.HTTPError as exc:
        out[action] = err_text(exc, srv)
    return out
  finally:
    srv.stop()


def case_bad_filename(args) -> dict:
  """`fs_validate_filename` gates the filename; a launcher must not trip it."""
  d = os.path.join(args.save_dir, "bad_name")
  shutil.rmtree(d, ignore_errors=True)
  out = {}
  srv = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    srv.start()
    ask(srv, PROMPT[:200])
    for name in ("../escape.bin", "sub/dir.bin", "ok-1_2.bin"):
      try:
        srv.post("/slots/0?action=save", {"filename": name})
        out[name] = "accepted"
      except requests.HTTPError as exc:
        out[name] = "rejected" if "Invalid filename" in err_text(exc, srv) else err_text(exc, srv)[:120]
    return out
  finally:
    srv.stop()


def case_output_identity(args) -> dict:
  """Does a restored slot generate what the same prompt generates warm?

  Three greedy runs of one prompt, compared as text: cold in a fresh process,
  warm on a second request in that same process (llama.cpp's ordinary in-process
  prefix cache), and after a save, a restart and a restore. A restored slot
  inherits the numerics of a prefix-cached run — the engine evaluates one token
  instead of the whole prompt, so batch shapes differ and a greedy continuation
  can part ways with the cold run — so the pair that has to agree is warm vs
  restored. Cold vs warm is printed next to it to show which side of that line
  any difference sits on.
  """
  d = os.path.join(args.save_dir, "identity")
  shutil.rmtree(d, ignore_errors=True)
  gen = {"n_predict": 48, "temperature": 0.0}
  src = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    src.start()
    cold, _ = ask(src, IDENTITY_PROMPT, gen)
    warm, _ = ask(src, IDENTITY_PROMPT, gen)
    src.post("/slots/0?action=save", {"filename": "x.bin"})
  finally:
    src.stop()
  out = {
    "cold": cold.get("content", "")[:220],
    "warm": warm.get("content", "")[:220],
    "cold_matches_warm": cold.get("content") == warm.get("content"),
  }
  srv = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    srv.start()
    srv.post("/slots/0?action=restore", {"filename": "x.bin"})
    after, dt = ask(srv, IDENTITY_PROMPT, gen)
    out["restored"] = after.get("content", "")[:220]
    out["restored_matches_warm"] = after.get("content") == warm.get("content")
    out["restored_matches_cold"] = after.get("content") == cold.get("content")
    out["after_wall_s"] = round(dt, 3)
    out["after_metrics"] = tail_metrics(srv, "prompt eval time")
    return out
  finally:
    srv.stop()


def case_offload_change(args) -> dict:
  """State saved with every layer on the GPU, restored into a CPU-only context."""
  d = os.path.join(args.save_dir, "offload")
  shutil.rmtree(d, ignore_errors=True)
  src = Server(args.llama_server, args.model, 16384, 1, d, args.ngl)
  try:
    src.start()
    ask(src, PROMPT)
    src.post("/slots/0?action=save", {"filename": "x.bin"})
  finally:
    src.stop()
  out = {}
  cpu = Server(args.llama_server, args.model, 16384, 1, d, "0")
  try:
    cpu.start()
    try:
      j, _ = cpu.post("/slots/0?action=restore", {"filename": "x.bin"})
      out["restored"] = j.get("n_restored")
    except requests.HTTPError as exc:
      out["restore_error"] = err_text(exc, cpu)
      return out
    out["hit"] = hit(cpu, PROMPT)
    return out
  finally:
    cpu.stop()


CASES = [
  case_output_identity,
  case_offload_change,
  case_slot_hint,
  case_cross_slot,
  case_two_conversations,
  case_smaller_ctx,
  case_other_model,
  case_missing_file,
  case_truncated_file,
  case_save_missing_slot,
  case_bad_filename,
]


def main() -> int:
  ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  ap.add_argument("--model", required=True)
  ap.add_argument("--other-model", default=None)
  ap.add_argument("--llama-server", default="llama-server")
  ap.add_argument("--ngl", default="99")
  ap.add_argument("--save-dir", default=os.path.expanduser("~/.cache/llamastash-slot-spike/edge"))
  ap.add_argument("--only", default=None, help="comma list of case function names")
  args = ap.parse_args()

  os.makedirs(args.save_dir, exist_ok=True)
  names = args.only.split(",") if args.only else None
  for fn in CASES:
    if names and fn.__name__ not in names:
      continue
    try:
      result = fn(args)
    except Exception as exc:  # keep the sweep going, one broken case must not hide the rest
      result = {"case_error": f"{type(exc).__name__}: {exc}"[:300]}
    print(json.dumps({fn.__name__: result}, indent=2), flush=True)
  return 0


if __name__ == "__main__":
  sys.exit(main())
