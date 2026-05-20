# Troubleshooting

Quick reference for the common ways LlamaStash can refuse to do what you want, with concrete remediation steps.

## `llama-server` not on `PATH`

**Symptom:** `llamastash start <ref>` exits `70` (`BINARY_NOT_FOUND`); the message names both the `--llama-server` flag and the `LLAMASTASH_LLAMA_SERVER` env var.

**Fix:** install llama.cpp's server build, then either put it on your `PATH`, set `LLAMASTASH_LLAMA_SERVER=/abs/path/to/llama-server`, or pass `--llama-server /abs/path/to/llama-server`. If `which llama-server` returns multiple hits (e.g. `llama-server-cuda` + `llama-server`), LlamaStash logs them and uses the first; pin a specific one via flag/env to avoid the surprise.

## GPU not detected

**Symptom:** `llamastash status --json | jq .gpu` returns `"CpuOnly"` even though you have a GPU. Memory estimates show only RAM, not VRAM.

**Fixes per backend:**

- **NVIDIA:** confirm `nvidia-smi` works. LlamaStash uses `nvml-wrapper`; if NVML isn't installed (driver-only install), the daemon falls back to CPU-only. Install the NVML library that ships with your CUDA toolkit.
- **AMD:** LlamaStash shells out to `rocm-smi --showmeminfo vram --json`. Make sure `rocm-smi` is on `PATH` and that ROCm is initialised.
- **Apple Silicon:** LlamaStash parses `system_profiler SPDisplaysDataType -json`. If this is empty, the macOS install is unusual — try the command manually and file an issue with the output.
- **Intel macOS:** there is no Metal support to detect; LlamaStash falls back to CPU-only and that's correct.

## Daemon socket already exists (stale)

**Symptom:** `llamastash daemon start` complains about an existing socket, or `llamastash list` exits `65` because the socket file is there but no listener is.

**Fix:** LlamaStash auto-detects stale sockets on `daemon start` and unlinks them. If you hit this anyway, remove the socket manually:

```bash
ls -l "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/llamastash/daemon.sock"
rm -- "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/llamastash/daemon.sock"
llamastash daemon start
```

On macOS the socket lives under `$TMPDIR/llamastash-$UID/daemon.sock`.

## Stale PID lockfile after a crash

**Symptom:** `llamastash daemon start` reports `AlreadyRunning(pid)` but `ps -p <pid>` shows nothing.

**Fix:** llamastash validates the lockfile against `kill -0 pid` and clears stale entries. If it's still wedged, delete it:

```bash
rm -- "$XDG_STATE_HOME/llamastash/daemon.pid"
```

The state directory defaults to `~/.local/state/llamastash/` on Linux and `~/Library/Application Support/llamastash/` on macOS.

## Port range exhausted

**Symptom:** `llamastash start ...` exits `67` with `port allocation failed: NoFreePort`.

**Fix:** widen the range in your config or pin a specific port:

```yaml
port_range:
  start: 41100
  end: 41500
```

```bash
llamastash start <ref> --port 41250
```

## Wayland clipboard yank does nothing

**Symptom:** `y` / `Y` / `p` in the TUI flashes a toast but the system clipboard stays empty (Wayland sessions are the usual culprit).

**Fix:** LlamaStash uses `arboard` first, then falls back to `wl-copy`, `xclip`, and `xsel` (in that order). Install at least one fallback:

```bash
# Wayland
sudo apt install wl-clipboard
# X11
sudo apt install xclip
```

The toast prints the URL inline when every backend fails, so you can still paste manually.

## Daemon disconnect during `logs --follow`

**Symptom:** `LlamaStash logs <id> -f` exits `65` mid-stream.

**Fix:** the daemon was shut down or crashed. Restart it with `llamastash daemon start`. Running children survive daemon exit; you can re-attach to the same launch id once the daemon is back (orphan re-adoption verifies PID + port + `/v1/models` match).

## "model already running" surprise

**Symptom:** the TUI launch picker shows a "model is already running on port N" line.

**This is the design.** v1 has no duplicate-prevention; a second launch creates a new instance on a different port. Stop the original first if you don't want two instances. The `--port` flag pins a specific port if you want to reuse one explicitly.

## `state.json` corruption after a SIGKILL

**Symptom:** daemon refuses to start; log says state-store parse failed.

**Fix:** llamastash quarantines a corrupt `state.json` as `state.json.broken-<ts>` and starts with defaults. You'll lose favorites, presets, last-params, and the running snapshot for this restart — but the daemon will come up. If you have a recent backup of `state.json`, restore it and try again.

## HuggingFace pull does nothing

**This is intentional.** The in-app HF pull worker is deferred to v2 (R46). The `pull` subcommand is hidden from `--help` and exits unimplemented. Use `huggingface-cli download ...` for now; llamastash discovers the downloaded files via its cache scanner.
