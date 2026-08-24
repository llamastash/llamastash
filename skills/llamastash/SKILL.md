---
name: llamastash
description: >
  Control local llama.cpp-backed models through the LlamaStash CLI. Use when an
  agent needs to install or initialize LlamaStash, list models, start or stop a
  model, inspect daemon or proxy health, pull GGUFs from HuggingFace, or obtain
  the local OpenAI-compatible proxy address for harnesses like Claude Code,
  OpenClaw, OpenCode, Pi, and other shell-capable AgentSkills clients.
compatibility: Requires llamastash installed locally and available on PATH. Linux or macOS. Designed for Claude Code, OpenClaw, OpenCode, and similar shell-capable AgentSkills clients.
license: MIT
allowed-tools: Bash(llamastash *)
metadata:
  author: deepu105
  version: '0.1'
  openclaw:
    emoji: "🦙"
    requires:
      bins:
        - llamastash
    os:
      - darwin
      - linux
---

# LlamaStash CLI

Use the `llamastash` CLI to install, inspect, and control local models managed
by LlamaStash.

## Current status

- Runtime status: !`llamastash status --json 2>/dev/null || echo '{"error":{"code":"not_ready","message":"llamastash is not installed, the daemon is not running, or the CLI is not configured yet"}}'`

## First-time setup

If `llamastash` is missing or not configured yet, bring it up in this order:

1. Install the binary using the machine's native path:

   ```bash
   # macOS
   brew install llamastash/llamastash/llamastash

   # Linux
   curl -fsSL https://llamastash.dev/install.sh | sh

   # Portable fallback
   cargo install llamastash
   ```

2. Verify the binary:

   ```bash
   llamastash --version
   ```

3. Run the non-interactive first-run flow:

   ```bash
   llamastash init --recommended --json
   ```

4. Verify the install and branch on findings, not on `doctor`'s exit code:

   ```bash
   llamastash doctor --json
   ```

## When to use this skill

- The user wants an agent to manage local models through `llamastash`
- The agent needs the discovered model list before selecting a model
- The agent needs to start or stop a model and verify the result
- The agent needs daemon, host, GPU, or proxy state from `status --json`
- The agent needs to pull a GGUF from HuggingFace with `llamastash pull`
- The agent needs the local OpenAI-compatible base URL from `proxy.listen`
- The user wants LlamaStash installed or repaired on their machine

## Key patterns

### Prefer JSON everywhere it exists

For agent use, prefer:

```bash
llamastash init --recommended --json
llamastash doctor --json
llamastash list --json
llamastash status --json
llamastash recommend --json
llamastash pull <owner/repo[:filename.gguf]> --json
```

Do not parse the human-readable table or colored output.

### Resolve exact model names first

Before `start` or `stop`, read the catalog first and reuse the exact discovered
name from `list --json`.

```bash
llamastash list --json
llamastash start <exact-model-name>
llamastash status --json
```

`start` and `stop` are not the primary machine contract surfaces. Confirm the
result with `status --json`.

### Use `doctor` findings, not its exit code

`llamastash doctor --json` always exits `0`. Escalate when `findings` is not
empty.

### Read the proxy address from CLI state

When wiring another harness to the built-in OpenAI-compatible proxy, read the
bound address from `status --json` and build the base URL as
`http://<proxy.listen>/v1`.

Default mode usually lands on `127.0.0.1:11435`. Ollama-compat mode usually
lands on `127.0.0.1:11434`. If the base port is busy, LlamaStash may bind the
next free port in the documented scan window, so check the live value first.

### Branch on exit codes

Important codes:

| Code | Meaning |
| ---- | ------- |
| `64` | bad CLI usage |
| `65` | daemon unreachable |
| `66` | model reference matched zero or multiple models |
| `67` | launch failed |
| `68` | stop failed |
| `69` | pull failed |
| `70` | llama.cpp server binary not found (`llama-server` or `llama`) |
| `71` | unexpected error |
| `72` | `init` aborted before substantive work |
| `73` | `init` download failed |
| `74` | `init` smoke failed |

See `references/commands.md` for command patterns and examples.
See `INSTALL.md#for-ai-agents` for agent installation patterns and examples.