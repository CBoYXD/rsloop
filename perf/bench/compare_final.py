#!/usr/bin/env python3
"""rsloop-perf final table: median rps per row, fork vs rsloop 0.1.64 vs uvloop vs asyncio.

usage: compare_final.py [suffix=_final] [apis...]

Rows measured under the quiet gate (harness.py) carry "quiet" and "contaminated": only clean attempts
count; a row with no clean attempt is reported and left out of the medians.

A run with only the uvloop and fork arms (BENCH_ARMS=uvloop,fork) gets a two-arm table instead: each
arm's median and its min–max over the rounds, and a noise reading per row. A row is "within noise"
when the two arms' round ranges overlap, "fork ahead" or "fork behind" when they do not. The per-round
column pairs the two arms measured in the same round (minutes apart).
"""

import json
import math
import statistics
import sys
from collections import defaultdict

OUT = "/home/cboyxd/.cache/rsloop-perf/run/bench"
SUFFIX = sys.argv[1] if len(sys.argv) > 1 else "_final"
APIS = sys.argv[2:] or ["admin_api", "investor_api"]
ARMS = ["uvloop", "asyncio", "rsloop", "fork"]


def med(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def f0(value):
    return "—" if value is None else f"{value:,.0f}"


def f1(value):
    return "—" if value is None else f"{value:.1f}"


def pct(value, base):
    return f"{(value / base - 1) * 100:+.1f}%" if value and base else "—"


def geomean(ratios):
    ratios = [r for r in ratios if r]
    return math.exp(sum(math.log(r) for r in ratios) / len(ratios)) if ratios else None


def spread(values):
    values = [v for v in values if v]
    return (max(values) / min(values) - 1) * 100 if len(values) > 1 else None


def foreign(r):
    return (r.get("quiet") or {}).get("foreign_cores")


def load_rows(api):
    """Clean measurements per (path, c, arm, round), plus counts of discarded attempts."""
    clean, dirty = {}, defaultdict(int)
    with open(f"{OUT}/{api}{SUFFIX}.jsonl") as f:
        for line in f:
            r = json.loads(line)
            if r["kind"] != "measure":
                continue
            key = (r["path"], r["concurrency"], r["arm"], r["round"])
            if r.get("contaminated"):
                dirty[key] += 1
            else:
                clean[key] = r
    return clean, dirty


def present_arms(clean):
    return [arm for arm in ARMS if any(k[2] == arm for k in clean)]


def fork_builds(api):
    """Native-module sha256 the fork arm imported, per round (harness start rows)."""
    builds = defaultdict(set)
    with open(f"{OUT}/{api}{SUFFIX}.jsonl") as f:
        for line in f:
            r = json.loads(line)
            if r["kind"] == "start" and r["arm"].startswith("fork"):
                builds[r.get("fork_build_sha256")].add(r["round"])
    return builds


def noise(fork, uvloop):
    if min(fork) > max(uvloop):
        return "fork ahead"
    if max(fork) < min(uvloop):
        return "fork behind"
    return "within noise"


def pair_table(api, clean, dirty):
    rows = defaultdict(list)
    order = []
    for (path, c, arm, _round), r in sorted(clean.items(), key=lambda kv: kv[0][3]):
        if (path, c) not in order:
            order.append((path, c))
        rows[(path, c, arm)].append(r)
    paths = list(dict.fromkeys(p for p, _ in order))
    order.sort(key=lambda k: (paths.index(k[0]), k[1]))
    rounds = sorted({k[3] for k in clean})
    out = [
        f"#### {api}: fork vs uvloop, median rps of {len(rounds)} interleaved rounds (60 s each)",
        "",
        (
            "| endpoint | c | uvloop (min–max) | **fork** (min–max) | fork vs uvloop | per round (min…max) | reading | "
            "CPU-ms/1k uvloop / fork | new PG sessions/1k uvloop / fork | load1 at rows (min–max) | "
            "foreign CPU cores (max) |"
        ),
        "|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    vs_uv, no_health, faster, readings = [], [], 0, defaultdict(int)
    for path, c in order:
        uv = [r["rps"] for r in rows[(path, c, "uvloop")]]
        fk = [r["rps"] for r in rows[(path, c, "fork")]]
        both = rows[(path, c, "uvloop")] + rows[(path, c, "fork")]
        loads = [r["load1"] for r in both] + [
            r["load1_end"] for r in both if "load1_end" in r
        ]
        foreigns = [foreign(r) for r in both if foreign(r) is not None]
        cpu = {
            arm: med([r["cpu_ms_per_1k"] for r in rows[(path, c, arm)]])
            for arm in ("uvloop", "fork")
        }
        pg = {
            arm: med([r.get("pg_sessions_per_1k") for r in rows[(path, c, arm)]])
            for arm in ("uvloop", "fork")
        }
        reading = noise(fk, uv) if uv and fk else "—"
        readings[reading] += 1
        paired = [
            clean[(path, c, "fork", n)]["rps"] / clean[(path, c, "uvloop", n)]["rps"]
            for n in rounds
            if (path, c, "fork", n) in clean and (path, c, "uvloop", n) in clean
        ]
        if uv and fk:
            vs_uv.append(med(fk) / med(uv))
            if not path.startswith("/health"):
                no_health.append(med(fk) / med(uv))
            faster += med(fk) >= med(uv)
        out.append(
            f"| `{path}` | {c} | {f0(med(uv))} ({f0(min(uv, default=None))}–{f0(max(uv, default=None))}) | "
            f"**{f0(med(fk))}** ({f0(min(fk, default=None))}–{f0(max(fk, default=None))}) | "
            f"{pct(med(fk), med(uv))} | "
            f"{pct(min(paired), 1) if paired else '—'}…{pct(max(paired), 1) if paired else '—'} | "
            f"{reading} | {f0(cpu['uvloop'])} / {f0(cpu['fork'])} | "
            f"{f1(pg['uvloop'])} / {f1(pg['fork'])} | {min(loads):.1f}–{max(loads):.1f} | "
            f"{f'{max(foreigns):.2f}' if foreigns else '—'} |"
        )
    builds = fork_builds(api)
    out += [
        "",
        (
            f"fork ≥ uvloop on {faster} of {len(vs_uv)} rows; geometric mean fork vs uvloop {pct(geomean(vs_uv), 1)} "
            f"(without `/health` rows {pct(geomean(no_health), 1)}); "
            + ", ".join(f"{n} {k}" for k, n in sorted(readings.items()))
            + "."
        ),
        "",
        "Rounds per arm: "
        + ", ".join(
            f"{arm} {len({k[3] for k in clean if k[2] == arm})}"
            for arm in ("uvloop", "fork")
        )
        + ". Fork native module (sha256) per round: "
        + "; ".join(
            f"`{sha}` r{','.join(map(str, sorted(r)))}" for sha, r in builds.items()
        )
        + ".",
        "",
        f"Clean rows: {len(clean)}; contaminated attempts discarded and re-measured: {sum(dirty.values())}"
        + (
            f"; rows without a clean attempt: {sorted(k for k in dirty if k not in clean)}"
            if any(k not in clean for k in dirty)
            else ""
        )
        + ".",
    ]
    return "\n".join(out) + "\n"


def table(api):
    clean, dirty = load_rows(api)
    if present_arms(clean) == ["uvloop", "fork"]:
        return pair_table(api, clean, dirty)
    rows = defaultdict(list)
    order = []
    for (path, c, arm, _round), r in clean.items():
        if (path, c) not in order:
            order.append((path, c))
        rows[(path, c, arm)].append(r)
    paths = list(dict.fromkeys(p for p, _ in order))
    order.sort(key=lambda k: (paths.index(k[0]), k[1]))
    out = [
        f"#### {api}: median rps of 3 interleaved rounds (60 s each)",
        "",
        (
            "| endpoint | c | uvloop | asyncio | rsloop 0.1.64 | **fork** | fork vs uvloop | fork vs 0.1.64 | uvloop round spread | "
            "CPU-ms/1k uvloop / 0.1.64 / fork | new PG sessions/1k uvloop / 0.1.64 / fork | load1 at rows (min–max) | "
            "foreign CPU cores (max) |"
        ),
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    vs_uv, vs_old, faster = [], [], 0
    for path, c in order:
        m = {arm: med([r["rps"] for r in rows[(path, c, arm)]]) for arm in ARMS}
        cpu = {
            arm: med([r["cpu_ms_per_1k"] for r in rows[(path, c, arm)]]) for arm in ARMS
        }
        pg = {
            arm: med([r.get("pg_sessions_per_1k") for r in rows[(path, c, arm)]])
            for arm in ARMS
        }
        loads = [r["load1"] for arm in ARMS for r in rows[(path, c, arm)]]
        loads += [
            r["load1_end"]
            for arm in ARMS
            for r in rows[(path, c, arm)]
            if "load1_end" in r
        ]
        foreigns = [
            foreign(r)
            for arm in ARMS
            for r in rows[(path, c, arm)]
            if foreign(r) is not None
        ]
        sp = spread([r["rps"] for r in rows[(path, c, "uvloop")]])
        if m["fork"] and m["uvloop"]:
            vs_uv.append(m["fork"] / m["uvloop"])
            faster += m["fork"] >= m["uvloop"]
        if m["fork"] and m["rsloop"]:
            vs_old.append(m["fork"] / m["rsloop"])
        out.append(
            f"| `{path}` | {c} | {f0(m['uvloop'])} | {f0(m['asyncio'])} | {f0(m['rsloop'])} | **{f0(m['fork'])}** | "
            f"{pct(m['fork'], m['uvloop'])} | {pct(m['fork'], m['rsloop'])} | {f1(sp)}% | "
            f"{f0(cpu['uvloop'])} / {f0(cpu['rsloop'])} / {f0(cpu['fork'])} | "
            f"{f1(pg['uvloop'])} / {f1(pg['rsloop'])} / {f1(pg['fork'])} | "
            f"{min(loads):.1f}–{max(loads):.1f} | {f'{max(foreigns):.2f}' if foreigns else '—'} |"
        )
    out += [
        "",
        (
            f"fork ≥ uvloop on {faster} of {len(vs_uv)} rows; geometric mean fork vs uvloop {pct(geomean(vs_uv), 1)}, "
            f"fork vs rsloop 0.1.64 {pct(geomean(vs_old), 1)}."
        ),
        "",
        "Rounds per arm: "
        + ", ".join(
            f"{arm} {len({k[3] for k in clean if k[2] == arm})}"
            for arm in ARMS
            if any(k[2] == arm for k in clean)
        )
        + ".",
        "",
        f"Clean rows: {len(clean)}; contaminated attempts discarded and re-measured: {sum(dirty.values())}"
        + (
            f"; rows without a clean attempt: {sorted(k for k in dirty if k not in clean)}"
            if any(k not in clean for k in dirty)
            else ""
        )
        + ".",
    ]
    return "\n".join(out) + "\n"


def per_round(api):
    out = [
        f"<details><summary>{api}: every row, every round (rps; load1 at start–end; foreign CPU cores)</summary>",
        "",
        "| endpoint | c | arm | r1 | r2 | r3 |",
        "|---|---|---|---|---|---|",
    ]
    rows = defaultdict(dict)
    clean, dirty = load_rows(api)
    for (path, c, arm, rnd), r in clean.items():
        rows[(path, c, arm)][rnd] = r

    def cell(r, key):
        load = f"{r['load1']:.1f}" + (
            f"–{r['load1_end']:.1f}" if "load1_end" in r else ""
        )
        f = foreign(r)
        retry = f", {dirty[key]} discarded" if dirty.get(key) else ""
        return (
            f"{r['rps']:,.0f} ({load}"
            + (f"; {f:.2f}" if f is not None else "")
            + f"{retry})"
        )

    paths = list(dict.fromkeys(k[0] for k in clean))
    for (path, c, arm), by_round in sorted(
        rows.items(),
        key=lambda kv: (ARMS.index(kv[0][2]), paths.index(kv[0][0]), kv[0][1]),
    ):
        cells = [
            cell(by_round[i], (path, c, arm, i)) if i in by_round else "—"
            for i in (1, 2, 3)
        ]
        out.append(f"| `{path}` | {c} | {arm} | " + " | ".join(cells) + " |")
    out += ["", "</details>", ""]
    return "\n".join(out)


for api in APIS:
    try:
        print(table(api))
        print(per_round(api))
    except FileNotFoundError:
        print(f"({api}: no data yet)\n")
