#!/usr/bin/env python3
"""tps-bench verdict: parse the two sampler outputs into a comparison
report. Numbers are averages over the sampled `tps` / `mspt` responses;
flush = timed `save-all flush` (full save cycle), rotate = timed
forceload window switch (256-chunk load + unload = disk read + write).

Gates (exit 1 = red):
  - both phases must have produced samples
  - the module must be attached (csaves_lines non-empty)
  - c-saves flush time must beat Anvil (the whole point of the engine)
Soft (warn only): TPS not lower, MSPT p95 not higher.
"""
import re
import sys


def fnum(xs):
    xs = [x for x in xs if x is not None]
    return sum(xs) / len(xs) if xs else float("nan")


def parse(path):
    tps, mspt_avg, p95, p99, flushes, rotates = [], [], [], [], [], []
    for line in open(path, encoding="utf-8", errors="replace"):
        m = re.search(r"TPS[^:\d]*1m[^:\d]*5m[^:\d]*15m[^:\d]*:\s*([\d.]+)", line)
        if not m:
            m = re.search(r"TPS[^:]*:\s*([\d.]+)", line)
        if m:
            tps.append(float(m.group(1)))
        for pat, sink in (
            (r"(?:avg|mean)\s*[:=]\s*([\d.]+)", mspt_avg),
            (r"p95\s*[:=]\s*([\d.]+)", p95),
            (r"p99\s*[:=]\s*([\d.]+)", p99),
        ):
            m = re.search(pat, line, re.IGNORECASE)
            if m:
                sink.append(float(m.group(1)))
        m = re.search(r"flush\s+([\d.]+)s", line)
        if m:
            flushes.append(float(m.group(1)))
        m = re.search(r"rotate\s+\S+\s+([\d.]+)s", line)
        if m:
            rotates.append(float(m.group(1)))
    return {
        "tps": fnum(tps), "mspt": fnum(mspt_avg), "p95": fnum(p95), "p99": fnum(p99),
        "flush": fnum(flushes), "rotate": fnum(rotates),
        "n_tps": len(tps), "n_flush": len(flushes), "n_rotate": len(rotates),
    }


def spikes_of(path):
    try:
        return sum(1 for _ in open(path, encoding="utf-8", errors="replace"))
    except FileNotFoundError:
        return -1


def main():
    a_path, b_path, spikes_a, spikes_b, csaves_lines = sys.argv[1:6]
    A, B = parse(a_path), parse(b_path)
    mod_attached = False
    try:
        mod_attached = any(
            "attached store" in l or "import region" in l
            for l in open(csaves_lines, encoding="utf-8", errors="replace")
        )
    except FileNotFoundError:
        pass

    print("# TPS bench — Paper 26.3 Anvil (A) vs c-saves injected (B)")
    print()
    print(f"| metric | A (Anvil) | B (c-saves) | B vs A |")
    print(f"|---|---|---|---|")

    def row(name, key, fmt="{:.2f}", lower_is_better=False):
        va, vb = A[key], B[key]
        if va == va and vb == vb and va != 0:
            cmp = f"{vb / va:.2f}x"
        else:
            cmp = "n/a"
        fa = fmt.format(va) if va == va else "n/a"
        fb = fmt.format(vb) if vb == vb else "n/a"
        print(f"| {name} | {fa} | {fb} | {cmp} |")

    row("TPS (avg of samples)", "tps")
    row("MSPT avg (ms)", "mspt", lower_is_better=True)
    row("MSPT p95 (ms)", "p95", lower_is_better=True)
    row("MSPT p99 (ms)", "p99", lower_is_better=True)
    row("save-all flush (s)", "flush", "{:.2f}", lower_is_better=True)
    row("forceload rotate (s)", "rotate", "{:.2f}", lower_is_better=True)
    print(f"| samples (tps/flush/rotate) | {A['n_tps']}/{A['n_flush']}/{A['n_rotate']} | "
          f"{B['n_tps']}/{B['n_flush']}/{B['n_rotate']} | |")
    print(f"| lag-spike log lines | {spikes_of(spikes_a)} | {spikes_of(spikes_b)} | |")
    print(f"| module attached | n/a | {'YES' if mod_attached else 'NO'} | |")
    print()

    fails = []
    a_no_metrics = A["n_tps"] == 0 and A["mspt"] != A["mspt"]
    b_no_metrics = B["n_tps"] == 0 and B["mspt"] != B["mspt"]
    if a_no_metrics or b_no_metrics:
        fails.append("no TPS and no MSPT samples in one of the phases")
    if not mod_attached:
        fails.append("c-saves module did not attach (no '[c-saves] attached store' line)")
    if A["n_flush"] and B["n_flush"] and B["flush"] >= A["flush"]:
        fails.append(
            f"save flush: c-saves {B['flush']:.2f}s >= Anvil {A['flush']:.2f}s")
    warns = []
    if A["tps"] == A["tps"] and B["tps"] == B["tps"] and B["tps"] < A["tps"] * 0.995:
        warns.append(f"TPS B {B['tps']:.2f} < A {A['tps']:.2f}")
    if A["p95"] == A["p95"] and B["p95"] == B["p95"] and B["p95"] > A["p95"] * 1.05:
        warns.append(f"MSPT p95 B {B['p95']:.2f} > A {A['p95']:.2f}")

    for w in warns:
        print(f"::warning::{w}")
    if fails:
        for f in fails:
            print(f"::error::{f}")
        print("VERDICT: FAIL")
        sys.exit(1)
    print("VERDICT: PASS")


if __name__ == "__main__":
    main()
