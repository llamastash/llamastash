#!/usr/bin/env python3
"""Measure the per-backend VRAM overhead band for llamastash's recommender.

Runs ``llama-server`` against a 7B Q4_K_M GGUF, samples peak GPU memory
once the ``/health`` endpoint reports ready, repeats ``--runs`` times,
and writes a JSON summary. The residual (measured peak minus the
recommender's own ``estimate_peak_bytes``) is the per-backend overhead
band that ``data/benchmark-snapshot.json::recommender_weights.
overhead_band_bytes`` is meant to capture.

Background: ``docs/spikes/2026-05-19-vram-overhead-band.md``.
Runbook:    ``docs/runbooks/measure-vram-overhead-band.md``.

Not used in CI — runs on dev hosts across the CUDA / HIP / Vulkan /
Metal matrix. Per-host JSONs are merged into the snapshot manually
once the four backends are covered.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import socket
import statistics
import subprocess
import sys
import time
import urllib.request
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------
# Constants — mirror ``src/init/recommender.rs``. Keep in sync if the
# estimator drifts, otherwise the residual stops meaning "overhead".
# ---------------------------------------------------------------------
ACTIVATIONS_OVERHEAD = 1.20
KV_FRACTION_AT_4K_F16 = 0.15
REFERENCE_CTX = 4096

DEFAULT_RUNS = 5
DEFAULT_CTX = 4096
DEFAULT_NGL = 99
DEFAULT_PORT = 8089
DEFAULT_HEALTH_TIMEOUT = 180
TEARDOWN_SETTLE_SECONDS = 3
POST_READY_SETTLE_SECONDS = 1.0

# Conservative pick = mean + 1·stddev, rounded up to this granularity.
ROUND_TO_BYTES = 32 * 1024 * 1024


def estimate_peak_bytes(weights_bytes: int, ctx: int) -> int:
    """Port of ``src/init/recommender.rs::estimate_peak_bytes``."""
    ctx_scale = ctx / REFERENCE_CTX
    activations = weights_bytes * ACTIVATIONS_OVERHEAD
    kv = weights_bytes * KV_FRACTION_AT_4K_F16 * ctx_scale
    return int(activations + kv)


# ---------------------------------------------------------------------
# Backend samplers — each returns *total* GPU bytes in use right now
# (per-process on Metal where unified-memory makes "total VRAM" useless).
# Run loop takes deltas from baseline for the total-GPU samplers.
# ---------------------------------------------------------------------

def _sample_nvidia_total(gpu_id: int) -> int:
    out = subprocess.check_output(
        [
            "nvidia-smi",
            "--query-gpu=memory.used",
            "--format=csv,noheader,nounits",
            f"--id={gpu_id}",
        ],
        text=True,
        stderr=subprocess.DEVNULL,
    )
    return int(out.strip()) * 1024 * 1024


def _sample_amd_sysfs(gpu_id: int) -> int:
    path = Path(f"/sys/class/drm/card{gpu_id}/device/mem_info_vram_used")
    return int(path.read_text().strip())


def _sample_amd_rocm_smi(gpu_id: int) -> int:
    out = subprocess.check_output(
        ["rocm-smi", "--showmeminfo", "vram", "--json"],
        text=True,
        stderr=subprocess.DEVNULL,
    )
    data = json.loads(out)
    # rocm-smi schema has varied across versions — try a few keys.
    candidate_keys = (
        "VRAM Total Used Memory (B)",
        "VRAM Total Used Memory(B)",
        "vram_used_memory",
    )
    target_card = f"card{gpu_id}"
    for card, info in data.items():
        if card.lower() != target_card:
            continue
        for key in candidate_keys:
            if key in info:
                return int(info[key])
    raise RuntimeError(
        f"rocm-smi: could not find VRAM-used field for {target_card}; "
        f"keys seen: {list(data.get(target_card, {}).keys())}"
    )


def _sample_amd_total(gpu_id: int) -> int:
    # Prefer sysfs — no root needed, immune to rocm-smi schema drift.
    sysfs_path = Path(f"/sys/class/drm/card{gpu_id}/device/mem_info_vram_used")
    if sysfs_path.exists():
        return _sample_amd_sysfs(gpu_id)
    if shutil.which("rocm-smi"):
        return _sample_amd_rocm_smi(gpu_id)
    raise RuntimeError(
        "no AMD VRAM sampler available — install rocm-smi or run on Linux "
        "with the AMDGPU driver loaded"
    )


def _sample_metal_proc(pid: int) -> int:
    """RSS of the llama-server process — ≈ ``phys_footprint`` on macOS.

    Unified memory means GPU-resident bytes show up in the process's
    resident set, so the harness reads RSS directly via ``ps`` rather
    than chasing a per-process VRAM counter (which doesn't exist).
    """
    out = subprocess.check_output(
        ["ps", "-o", "rss=", "-p", str(pid)],
        text=True,
        stderr=subprocess.DEVNULL,
    )
    return int(out.strip()) * 1024  # ps reports KiB on macOS


def sample_for_backend(backend: str, pid: int, gpu_id: int) -> int:
    if backend == "cuda":
        return _sample_nvidia_total(gpu_id)
    if backend == "hip":
        return _sample_amd_total(gpu_id)
    if backend == "vulkan":
        # Vulkan piggybacks on the vendor's accounting. Pick whichever
        # sampler is wired up for the GPU under test.
        if shutil.which("nvidia-smi"):
            try:
                return _sample_nvidia_total(gpu_id)
            except subprocess.CalledProcessError:
                pass
        return _sample_amd_total(gpu_id)
    if backend == "metal":
        return _sample_metal_proc(pid)
    raise ValueError(f"unknown backend: {backend}")


def autodetect_backend() -> str:
    if platform.system() == "Darwin":
        return "metal"
    if shutil.which("nvidia-smi"):
        try:
            subprocess.check_output(
                ["nvidia-smi", "-L"],
                text=True,
                stderr=subprocess.DEVNULL,
            )
            return "cuda"
        except subprocess.CalledProcessError:
            pass
    if shutil.which("rocm-smi") or Path(
        "/sys/class/drm/card0/device/mem_info_vram_used"
    ).exists():
        return "hip"
    return "vulkan"


# ---------------------------------------------------------------------
# Run loop
# ---------------------------------------------------------------------

def wait_for_health(port: int, timeout: int) -> bool:
    deadline = time.monotonic() + timeout
    url = f"http://127.0.0.1:{port}/health"
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as resp:
                body = json.loads(resp.read())
                # llama-server reports `{"status":"ok"}` once layers
                # are loaded and the slot pool is initialised.
                if body.get("status") == "ok":
                    return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


def _human(n: float) -> str:
    units = ("B", "KiB", "MiB", "GiB", "TiB")
    val = float(n)
    for unit in units:
        if abs(val) < 1024:
            return f"{val:.1f} {unit}"
        val /= 1024
    return f"{val:.1f} PiB"


def _popen_kwargs() -> dict:
    if os.name == "posix":
        return {"start_new_session": True}
    if os.name == "nt":
        return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}
    return {}


def _server_cmd(args: argparse.Namespace) -> list[str]:
    cmd = [
        args.llama_server,
        "--model", str(args.model),
        "--ctx-size", str(args.ctx),
        "--n-gpu-layers", str(args.ngl),
        "--port", str(args.port),
        "--host", "127.0.0.1",
    ]
    if args.no_mmap:
        cmd.append("--no-mmap")
    return cmd


def run_one(args: argparse.Namespace, run_idx: int) -> dict:
    label = f"[run {run_idx + 1}/{args.runs}]"

    # Baseline — for total-GPU samplers we subtract this from the peak;
    # for per-process samplers (Metal) it's always zero.
    if args.backend == "metal":
        baseline = 0
    else:
        baseline = sample_for_backend(args.backend, pid=0, gpu_id=args.gpu_id)
    print(f"{label} baseline GPU bytes: {_human(baseline)}", file=sys.stderr)

    cmd = _server_cmd(args)
    print(f"{label} spawn: {' '.join(cmd)}", file=sys.stderr)
    log = open(args.log_dir / f"run-{run_idx + 1}.log", "w") if args.log_dir else subprocess.DEVNULL
    try:
        proc = subprocess.Popen(
            cmd,
            stdout=log,
            stderr=subprocess.STDOUT,
            **_popen_kwargs(),
        )
    except FileNotFoundError as exc:
        raise SystemExit(f"failed to spawn llama-server: {exc}") from exc

    try:
        if not wait_for_health(args.port, args.health_timeout):
            raise SystemExit(
                f"llama-server did not reach /health=ok within "
                f"{args.health_timeout}s — check logs in {args.log_dir}"
            )
        time.sleep(POST_READY_SETTLE_SECONDS)
        peak = sample_for_backend(args.backend, pid=proc.pid, gpu_id=args.gpu_id)
        used = peak if args.backend == "metal" else peak - baseline
        print(
            f"{label} used={_human(used)} (peak={_human(peak)}, "
            f"baseline={_human(baseline)})",
            file=sys.stderr,
        )
    finally:
        try:
            proc.terminate()
            try:
                proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        finally:
            if hasattr(log, "close"):
                log.close()

    time.sleep(TEARDOWN_SETTLE_SECONDS)
    return {
        "run": run_idx + 1,
        "baseline_bytes": baseline,
        "peak_bytes": peak,
        "used_bytes": used,
    }


def summarize(runs: list[dict], estimate: int) -> dict:
    used = [r["used_bytes"] for r in runs]
    overhead = [u - estimate for u in used]
    mean_o = statistics.fmean(overhead)
    stdev_o = statistics.pstdev(overhead) if len(overhead) > 1 else 0.0
    conservative = max(0, int(mean_o + stdev_o))
    rounded = ((conservative + ROUND_TO_BYTES - 1) // ROUND_TO_BYTES) * ROUND_TO_BYTES
    return {
        "used_mean_bytes": int(statistics.fmean(used)),
        "used_stdev_bytes": int(statistics.pstdev(used) if len(used) > 1 else 0.0),
        "used_min_bytes": min(used),
        "used_max_bytes": max(used),
        "overhead_mean_bytes": int(mean_o),
        "overhead_stdev_bytes": int(stdev_o),
        "overhead_min_bytes": min(overhead),
        "overhead_max_bytes": max(overhead),
        "recommended_band_bytes": rounded,
    }


def _amd_label_from_lspci(gpu_id: int) -> Optional[str]:
    """Map /sys/class/drm/cardN to a human GPU name via lspci.

    ``product_name`` only exists on Instinct / some Pro cards, so for
    consumer AMD (Strix Halo, RX, etc.) we fall back here.
    """
    uevent = Path(f"/sys/class/drm/card{gpu_id}/device/uevent")
    if not uevent.exists() or not shutil.which("lspci"):
        return None
    slot = None
    for line in uevent.read_text().splitlines():
        if line.startswith("PCI_SLOT_NAME="):
            slot = line.split("=", 1)[1].strip()
            break
    if not slot:
        return None
    try:
        out = subprocess.check_output(
            ["lspci", "-s", slot, "-nn"],
            text=True, stderr=subprocess.DEVNULL,
        ).strip()
    except subprocess.CalledProcessError:
        return None
    # "c4:00.0 Display controller [0380]: ... Strix Halo [...] [1002:1586] (rev c1)"
    line = out.split("\n", 1)[0]
    parts = line.split(": ", 1)
    if len(parts) != 2:
        return line
    import re
    rhs = re.sub(r"\s*\[[0-9a-fA-F]{4}:[0-9a-fA-F]{4}\].*$", "", parts[1])
    return rhs.strip() or None


def detect_gpu_label(backend: str, gpu_id: int) -> Optional[str]:
    try:
        if backend == "cuda" and shutil.which("nvidia-smi"):
            out = subprocess.check_output(
                ["nvidia-smi", "--query-gpu=name", "--format=csv,noheader",
                 f"--id={gpu_id}"],
                text=True, stderr=subprocess.DEVNULL,
            )
            return out.strip()
        if backend in ("hip", "vulkan"):
            product = Path(f"/sys/class/drm/card{gpu_id}/device/product_name")
            if product.exists():
                return product.read_text().strip()
            return _amd_label_from_lspci(gpu_id)
        if backend == "metal":
            out = subprocess.check_output(
                ["system_profiler", "SPDisplaysDataType"],
                text=True, stderr=subprocess.DEVNULL,
            )
            for line in out.splitlines():
                line = line.strip()
                if line.startswith("Chipset Model:"):
                    return line.split(":", 1)[1].strip()
    except Exception:
        return None
    return None


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--llama-server", required=True,
                    help="path to the llama-server binary built for the target backend")
    ap.add_argument("--model", required=True, type=Path,
                    help="path to a 7B Q4_K_M GGUF (canonical: qwen2.5-7b-instruct-q4_k_m.gguf)")
    ap.add_argument("--backend", choices=["cuda", "hip", "vulkan", "metal"],
                    help="GPU backend (auto-detected if omitted)")
    ap.add_argument("--runs", type=int, default=DEFAULT_RUNS,
                    help=f"sample count (default: {DEFAULT_RUNS})")
    ap.add_argument("--ctx", type=int, default=DEFAULT_CTX,
                    help=f"--ctx-size passed to llama-server (default: {DEFAULT_CTX})")
    ap.add_argument("--ngl", type=int, default=DEFAULT_NGL,
                    help=f"--n-gpu-layers (default: {DEFAULT_NGL})")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--gpu-id", type=int, default=0,
                    help="GPU index (matters on multi-GPU / iGPU+dGPU hosts)")

    mmap_grp = ap.add_mutually_exclusive_group()
    mmap_grp.add_argument(
        "--no-mmap", dest="no_mmap", action="store_true", default=True,
        help="pass --no-mmap to llama-server so weights commit to VRAM "
             "(default; matches the recommender's estimator assumption)",
    )
    mmap_grp.add_argument(
        "--mmap", dest="no_mmap", action="store_false",
        help="allow llama-server to mmap weights (lower measured peak)",
    )

    ap.add_argument("--health-timeout", type=int, default=DEFAULT_HEALTH_TIMEOUT,
                    help=f"seconds to wait for /health=ok (default: {DEFAULT_HEALTH_TIMEOUT})")
    ap.add_argument("--log-dir", type=Path, default=None,
                    help="directory to capture llama-server stdout/stderr per run")
    ap.add_argument("--out", default="-",
                    help="output JSON path or '-' for stdout (default: stdout)")
    args = ap.parse_args()

    if not Path(args.llama_server).exists():
        ap.error(f"llama-server not found: {args.llama_server}")
    if not args.model.exists():
        ap.error(f"model not found: {args.model}")

    args.backend = args.backend or autodetect_backend()

    if args.log_dir:
        args.log_dir.mkdir(parents=True, exist_ok=True)

    weights_bytes = args.model.stat().st_size
    estimate = estimate_peak_bytes(weights_bytes, args.ctx)

    print(f"backend:        {args.backend}", file=sys.stderr)
    print(f"host:           {socket.gethostname()}", file=sys.stderr)
    print(f"os:             {platform.system()} {platform.release()}", file=sys.stderr)
    gpu_label = detect_gpu_label(args.backend, args.gpu_id)
    if gpu_label:
        print(f"gpu:            {gpu_label}", file=sys.stderr)
    print(f"model:          {args.model.name} ({_human(weights_bytes)})", file=sys.stderr)
    print(f"ctx:            {args.ctx}", file=sys.stderr)
    print(f"estimate_peak:  {_human(estimate)}", file=sys.stderr)
    print(f"runs:           {args.runs}", file=sys.stderr)
    print(file=sys.stderr)

    runs = []
    for i in range(args.runs):
        runs.append(run_one(args, i))

    summary = summarize(runs, estimate)
    out = {
        "schema_version": 1,
        "tool": "measure-overhead-band.py",
        "host": socket.gethostname(),
        "os": f"{platform.system()} {platform.release()}",
        "gpu": gpu_label,
        "backend": args.backend,
        "gpu_id": args.gpu_id,
        "model_path": str(args.model),
        "model_bytes": weights_bytes,
        "ctx": args.ctx,
        "ngl": args.ngl,
        "no_mmap": args.no_mmap,
        "estimate_peak_bytes": estimate,
        "runs": runs,
        "summary": summary,
    }

    print(file=sys.stderr)
    print(
        f"overhead_mean = {_human(summary['overhead_mean_bytes'])}  "
        f"stdev = {_human(summary['overhead_stdev_bytes'])}",
        file=sys.stderr,
    )
    print(
        f"recommended overhead_band_bytes[{args.backend}] = "
        f"{summary['recommended_band_bytes']}  "
        f"({_human(summary['recommended_band_bytes'])})",
        file=sys.stderr,
    )

    text = json.dumps(out, indent=2)
    if args.out == "-":
        print(text)
    else:
        out_path = Path(args.out)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(text + "\n")
        print(f"wrote {out_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
