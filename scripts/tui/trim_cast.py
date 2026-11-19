#!/usr/bin/env python3
"""Trim an asciinema v2 cast: clamp idle gaps and optionally rescale time.

The harness records a cast in real time, so it carries every pause the driven
session sat through — a model loading, a smoke probe, a `wait:` we left
generous. This rewrites the inter-event deltas so long pauses collapse to a
readable beat while the typing/animation cadence is preserved.

  python3 scripts/tui/trim_cast.py in.cast out.cast \
      [--max-gap 1.2] [--speed 1.0] [--fast A:B:F ...] [--hold T:DUR ...]

--max-gap S   clamp any gap between two output events to at most S seconds
              (default 1.2). This is what kills the dead air.
--speed F     divide every (already-clamped) delta by F; >1 is faster
              (default 1.0, i.e. unchanged).
--fast A:B:F  play the raw span (A, B] F times faster on top of --speed, e.g.
              a run of repeated arrow keys. Repeatable.
--hold T:DUR  freeze on the frame at (or just after) raw timestamp T for an
              extra DUR seconds — let a payoff frame breathe (e.g. the init
              summary) without slowing the rest. Repeatable.
--end T       drop every event after raw timestamp T (trim a ragged tail, e.g.
              a stray quit). Default: keep everything.
--band LO:HI  even out the cadence: leave sub-frame repaint bursts (gaps under
              BURST seconds) untouched, but clamp every *pause between actions*
              into [LO, HI] — long waits collapse to HI, abrupt jumps stretch to
              LO — so the whole clip plays at one steady rate. Applied after
              --max-gap/--speed, before --hold. Default: off.

The header is copied verbatim (width/height/theme/env preserved); only the
event timestamps are rewritten, monotonically from 0.
"""
import json
import sys


def main():
    args = sys.argv[1:]

    def take(name, default):
        if name in args:
            i = args.index(name)
            val = args[i + 1]
            del args[i : i + 2]
            return val
        return default

    def take_all(name):
        vals = []
        while name in args:
            i = args.index(name)
            vals.append(args[i + 1])
            del args[i : i + 2]
        return vals

    max_gap = float(take("--max-gap", "1.2"))
    speed = float(take("--speed", "1.0"))
    end = float(take("--end", "inf"))
    # gaps below this are sub-frame repaint bursts (one screen update); leave them.
    BURST = 0.08
    band = take("--band", None)
    band_lo, band_hi = (float(x) for x in band.split(":")) if band else (0.0, float("inf"))
    # each --hold is "rawT:seconds"; fire once when the clock first passes rawT.
    holds = sorted(
        (float(t), float(d))
        for t, d in (h.split(":") for h in take_all("--hold"))
    )
    fasts = [tuple(float(x) for x in f.split(":")) for f in take_all("--fast")]
    src, dst = args[0], args[1]

    lines = open(src).read().splitlines()
    header = lines[0]
    events = [json.loads(l) for l in lines[1:] if l.strip()]

    out = [header]
    prev_raw = 0.0
    clock = 0.0
    hi = 0
    for ev in events:
        t, kind, data = ev[0], ev[1], ev[2]
        if t > end:
            break
        delta = min(t - prev_raw, max_gap) / speed
        for a, b, f in fasts:
            if a < t <= b:
                delta /= f
        prev_raw = t
        # Even out the cadence: keep tiny repaint bursts, clamp real pauses.
        if delta >= BURST:
            delta = max(band_lo, min(delta, band_hi))
        clock += max(delta, 0.0)
        # Insert any pending holds before emitting this event, so the dwell
        # lands on the frame already painted (the one at raw time <= T).
        while hi < len(holds) and t >= holds[hi][0]:
            clock += holds[hi][1]
            hi += 1
        out.append(json.dumps([round(clock, 6), kind, data]))

    with open(dst, "w") as f:
        f.write("\n".join(out) + "\n")

    print(f"{src} ({events[-1][0]:.1f}s) -> {dst} ({clock:.1f}s), "
          f"{len(events)} events, max-gap {max_gap}s, speed {speed}x")


if __name__ == "__main__":
    main()
