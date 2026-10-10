#!/usr/bin/env python3
"""Summarize benchmark results as markdown tables, one row per task x arm.

Reads the JSONL that bench/run.py appends (every file in bench/results/ when
no path is given). The headline table holds agent runs (--spend) only. Dry
runs are a scripted solver, not an agent, so they get their own table,
labelled as such, and never feed a headline number; sabotage runs are
labelled apart from both.

Per row: runs, harness errors (excluded from every other column), pass rate
with a Wilson 95% interval, median and IQR of total tokens, tool calls,
turns and wall time, and summed cost.

Python 3 stdlib only.

Usage:  python3 bench/summarize.py [results.jsonl ...]
"""

import glob
import json
import math
import os
import statistics
import sys

BENCH = os.path.dirname(os.path.abspath(__file__))
RESULTS_DIR = os.path.join(BENCH, "results")
Z95 = 1.959964


def wilson(passes, n, z=Z95):
    """Wilson score interval for `passes` out of `n`, as (low, high)."""
    if n == 0:
        return (0.0, 1.0)
    p = passes / n
    denom = 1 + z * z / n
    centre = (p + z * z / (2 * n)) / denom
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denom
    return (max(0.0, centre - half), min(1.0, centre + half))


def median_iqr(values):
    """(median, q1, q3) over the numbers in `values`, or None if there are none."""
    nums = sorted(v for v in values if isinstance(v, (int, float)) and not isinstance(v, bool))
    if not nums:
        return None
    if len(nums) == 1:
        return (nums[0], nums[0], nums[0])
    q1, q2, q3 = statistics.quantiles(nums, n=4, method="inclusive")
    return (q2, q1, q3)


def _fmt_num(value, scale=1.0, digits=0):
    value = value / scale
    return f"{value:,.{digits}f}"


def fmt_spread(values, scale=1.0, digits=0):
    stats = median_iqr(values)
    if stats is None:
        return "-"
    med, q1, q3 = stats
    if q1 == q3:
        return _fmt_num(med, scale, digits)
    return f"{_fmt_num(med, scale, digits)} [{_fmt_num(q1, scale, digits)}-{_fmt_num(q3, scale, digits)}]"


def label(record):
    if record.get("mode") == "spend":
        return "agent"
    if record.get("solver") == "scripted-sabotage":
        return "dry-run (sabotage)"
    return "dry-run"


def load(paths):
    records = []
    for path in paths:
        with open(path) as f:
            for number, line in enumerate(f, 1):
                if not line.strip():
                    continue
                try:
                    records.append(json.loads(line))
                except ValueError:
                    print(f"summarize.py: {path}:{number} is not JSON; skipped", file=sys.stderr)
    return records


HEADER = (
    "| task | arm | kind | runs | errors | passed | pass rate (95% CI) | tokens med [IQR] "
    "| tool calls med [IQR] | turns med [IQR] | wall s med [IQR] | cost $ (sum) |"
)
RULE = "|" + "|".join(["---"] * (HEADER.count("|") - 1)) + "|"


def row(task, arm, kind, records):
    graded = [r for r in records if r.get("status") != "harness_error"]
    errors = len(records) - len(graded)
    passes = sum(1 for r in graded if r.get("passed") is True)
    n = len(graded)
    if n:
        low, high = wilson(passes, n)
        rate = f"{passes / n:.0%} ({low:.0%}-{high:.0%})"
    else:
        rate = "-"
    costs = [r["cost_usd"] for r in graded if isinstance(r.get("cost_usd"), (int, float))]
    cost = f"{sum(costs):.2f}" if costs else "-"
    cells = [
        task, arm, kind, str(len(records)), str(errors), f"{passes}/{n}", rate,
        fmt_spread([r.get("tokens") for r in graded]),
        fmt_spread([r.get("tool_calls_total") for r in graded]),
        fmt_spread([r.get("turns") for r in graded]),
        fmt_spread([r.get("wall_ms") for r in graded], scale=1000, digits=1),
        cost,
    ]
    return "| " + " | ".join(cells) + " |"


def table(records):
    groups = {}
    for r in records:
        groups.setdefault((r.get("task", "?"), r.get("arm", "?"), label(r)), []).append(r)
    lines = [HEADER, RULE]
    for (task, arm, kind) in sorted(groups):
        lines.append(row(task, arm, kind, groups[(task, arm, kind)]))
    return "\n".join(lines)


def render(records):
    agent = [r for r in records if label(r) == "agent"]
    scripted = [r for r in records if label(r) != "agent"]
    out = ["## Agent runs (headline)", ""]
    out.append(table(agent) if agent else "No agent runs (`run.py --spend`) in these results.")
    if scripted:
        out += [
            "",
            "## Dry runs (scripted solver, no model; not headline numbers)",
            "",
            "Tokens, turns and cost do not exist for a scripted solver; wall time is "
            "the solver's, measured once its server is up.",
            "",
            table(scripted),
        ]
    return "\n".join(out)


def main(argv=None):
    paths = (argv if argv is not None else sys.argv[1:]) or sorted(
        glob.glob(os.path.join(RESULTS_DIR, "*.jsonl"))
    )
    if not paths:
        print(f"summarize.py: no results given and none in {RESULTS_DIR}", file=sys.stderr)
        return 1
    print(render(load(paths)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
