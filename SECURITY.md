# Security Policy

## Supported versions

llamatui is pre-1.0. Only the `main` branch is supported. Security fixes will land on `main` and ship in the next tagged release.

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security reports. Instead, email the maintainer at `d4udts@gmail.com` with:

- a description of the issue,
- reproduction steps,
- the version (`llamatui --version`) and platform,
- any proof-of-concept code or scripts.

You can expect an acknowledgement within a few business days. If you don't hear back, please follow up — email gets dropped.

## Threat model summary

llamatui is **single-user, loopback-only**:

- The daemon binds a Unix socket at `$XDG_RUNTIME_DIR/llamatui/daemon.sock` (Linux) or `$TMPDIR/llamatui-$UID/daemon.sock` (macOS), mode `0600`, with peer-credential auth (`SO_PEERCRED` / `getpeereid`). Non-owner UIDs are rejected at connect.
- `llama-server` children listen on `127.0.0.1` only.
- v1 does **not** bind any network socket. Network exposure (HTTP, MCP) is a v2-only feature and will require explicit opt-in.
- The daemon does **not** persist or transmit telemetry.

Issues we treat as in scope:

- Any path that lets a non-owner connect to the daemon socket or impersonate the owner.
- Any path that lets a remote attacker reach a `llama-server` instance llamatui launched (e.g., accidentally binding to `0.0.0.0`).
- Memory-safety, deserialization, or path-traversal bugs in the daemon or CLI.
- Lockfile / state-file races that allow privilege confusion between users.

Out of scope:

- `llama.cpp` / `llama-server` upstream behaviour (please report there).
- Local denial-of-service by a malicious user against their own daemon.
- Issues that require the attacker to already have shell access as the daemon owner.
