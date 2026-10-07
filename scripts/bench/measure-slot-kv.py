#!/usr/bin/env python3
"""Measure llama.cpp slot KV save/restore against plain prompt reprocessing.

WHY THIS EXISTS. R14's "keep a model's prompt cache across an unload" hinged on
whether `--slot-save-path` plus `POST /slots/{id}?action=save|restore` beats
reprocessing the prompt after a relaunch. The answer is in the table this
prints; the finding and the decisions taken from it are in
`docs/spikes/2026-10-07-slot-kv-persistence.md`. Re-run it after a llama.cpp
bump that touches the state IO path or the KV layout, since bytes-per-token
tracks the layout.

For every prompt size it does: start a server, send the prompt cold (reprocess
cost), save the slot it landed on (save time + file size), restart the server,
restore the file into that slot id, send the prompt again (what the first
returning request pays). It drives a plain `llama-server`, not llamastash, so
nothing of the launcher is in the numbers.

  ./measure-slot-kv.py --model ~/models/Qwen3.5-4B-Q4_K_M.gguf
  ./measure-slot-kv.py --sizes 6000,24000 --ctx 40960
"""

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

FILLER = (
    "The quick brown fox jumps over the lazy dog while the compiler optimizes "
    "the attention kernel on the gpu. "
)


def req(port, path, body=None, timeout=3600):
    """One JSON HTTP call. Returns (parsed, wall_seconds, error_text)."""
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST" if body is not None else "GET",
    )
    t = time.time()
    try:
        return json.load(urllib.request.urlopen(r, timeout=timeout)), time.time() - t, None
    except urllib.error.HTTPError as e:
        return None, time.time() - t, e.read().decode()[:300]


class Server:
    def __init__(self, args, save_dir, log):
        self.port = args.port
        self.argv = [
            args.binary, "-m", args.model, "--host", "127.0.0.1",
            "--port", str(args.port), "-c", args.ctx, "-ngl", "99",
            "--slot-save-path", save_dir,
        ]
        self.log = open(log, "w")
        self.proc = None

    def __enter__(self):
        self.proc = subprocess.Popen(self.argv, stdout=self.log, stderr=subprocess.STDOUT)
        for _ in range(600):
            try:
                h, _, _ = req(self.port, "/health", timeout=5)
                if h and h.get("status") == "ok":
                    return self
            except Exception:
                pass
            time.sleep(1)
        sys.exit("server did not become healthy; see " + self.log.name)

    def __exit__(self, *_):
        self.proc.send_signal(signal.SIGTERM)
        self.proc.wait(timeout=120)
        self.log.close()


def prompt_for(port, tokens, text=None):
    """Text whose token count, per the server's own tokenizer, is near `tokens`.

    The uuid prefix matters: prompts built from one repeated paragraph share a
    prefix, and the engine routes slots by longest common prefix, so a second
    size would silently read the first size's KV cache instead of reprocessing.
    """
    text = (uuid.uuid4().hex + "\n" + text if text else
            uuid.uuid4().hex + "\n" + FILLER * max(1, int(tokens * 6 / len(FILLER))))
    for _ in range(3):
        n, _, err = req(port, "/tokenize", {"content": text})
        if err:
            sys.exit("/tokenize failed: " + err)
        got = len(n["tokens"])
        if abs(got - tokens) <= tokens * 0.02:
            return text, got
        text = text[: max(len(FILLER), int(len(text) * tokens / max(1, got)))]
    got = len(req(port, "/tokenize", {"content": text})[0]["tokens"])
    return text, got


def used_slot(port, n_tok):
    """The slot that holds `n_tok` prompt tokens; a saved run holds one per size."""
    slots, _, _ = req(port, "/slots")
    busy = [s for s in slots if s.get("n_prompt_tokens")]
    if not busy:
        sys.exit("no slot reports n_prompt_tokens; /slots shape changed?")
    return min(busy, key=lambda s: abs(s["n_prompt_tokens"] - n_tok))["id"]


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--model", required=True)
    ap.add_argument("--binary", default="llama-server")
    ap.add_argument("--port", type=int, default=18790)
    ap.add_argument("--ctx", default="131072")
    ap.add_argument("--sizes", default="6000,24000,120000", help="comma-separated prompt token targets")
    ap.add_argument("--workdir", default=os.path.expanduser("~/.cache/slot-kv-measure"))
    ap.add_argument("--prompt-file", help="use this text (prefixed by a uuid) instead of generated filler")
    args = ap.parse_args()

    model = os.path.expanduser(args.model)
    save_dir = os.path.join(args.workdir, "saves")
    shutil.rmtree(args.workdir, ignore_errors=True)
    os.makedirs(save_dir)
    os.makedirs(os.path.join(args.workdir, "logs"), exist_ok=True)
    log = lambda n: os.path.join(args.workdir, "logs", f"{n}.log")  # noqa: E731

    override = None
    if args.prompt_file:
        override = open(os.path.expanduser(args.prompt_file)).read()

    rows = []
    for size in [int(s) for s in args.sizes.split(",")]:
        # One server per size, so every cold number is a genuinely empty cache
        # and a big prompt is not squeezed for cells by the smaller ones.
        text, n_tok = None, None
        with Server(args, save_dir, log(f"cold-{size}")):
            text, n_tok = prompt_for(args.port, size, override)
            r, wall, err = req(
                args.port, "/completion",
                {"prompt": text, "n_predict": 1, "cache_prompt": True},
            )
            if err:
                sys.exit(f"prompt at {size} failed: {err}")
            slot = used_slot(args.port, n_tok)
            name = f"measure-{size}.bin"
            s, save_wall, err = req(args.port, f"/slots/{slot}?action=save", {"filename": name})
            if err:
                sys.exit(f"save at {size} failed: {err}")
            row = {
                "tokens": n_tok, "slot": slot, "name": name,
                "reprocess_s": round(r["timings"]["prompt_ms"] / 1000, 2),
                "save_s": round(s["timings"]["save_ms"] / 1000, 3),
                "save_wall_s": round(save_wall, 2),
                "bytes": os.path.getsize(os.path.join(save_dir, name)),
                "n_saved": s["n_saved"],
            }
        with Server(args, save_dir, log(f"restore-{size}")):
            r, wall, err = req(
                args.port, f"/slots/{slot}?action=restore", {"filename": name}
            )
            row["restore_s"] = err or round(wall, 2)
            if not err:
                r, wall, err = req(
                    args.port, "/completion",
                    {"prompt": text, "n_predict": 1, "cache_prompt": True},
                )
                row["after_restore_ms"] = err or round(r["timings"]["prompt_ms"], 1)
        rows.append(row)
    print(json.dumps(rows, indent=1))


if __name__ == "__main__":
    main()
