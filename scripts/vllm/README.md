# vLLM scripts

## `probe.sh`

Stands up the ROCm vLLM container and answers the questions you cannot get
from docs: what the real flag surface is, whether the GPU is visible, and how
the server behaves at startup. Used to verify the facts the vLLM backend is
built on — see `docs/plans/2026-08-10-001-feat-vllm-backend-plan.md`.

> **Warning — this allocates system RAM on an APU.** On unified-memory hosts
> the GPU has no separate pool, so `--gpu-memory-utilization` spends DRAM. The
> `serve` mode caps it, but check `free -g` before and during a run, and do not
> leave one unattended: an uncapped vLLM on a 121 GB Strix Halo box has frozen
> the machine outright.

```bash
scripts/vllm/probe.sh version   # vllm + torch versions, GPU visibility
scripts/vllm/probe.sh help      # `vllm serve --help` (needs the GPU devices)
scripts/vllm/probe.sh serve     # serve a small model on VLLM_PORT (default 8010)
scripts/vllm/probe.sh shell     # interactive shell in the image
```

The image tag and test model are constants at the top of the script. Re-verify
against a live server before trusting any flag list — vLLM moves fast, and
`--help` requires the GPU devices mounted (it infers the device type while
building its parser).

For the wrapper script that makes a containerised vLLM usable as a LlamaStash
backend binary, see `docs/vllm-setup.md` — that is a different thing from this
probe harness.
