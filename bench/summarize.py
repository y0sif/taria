#!/usr/bin/env python3
"""Summarize benchmark results as markdown tables.

Reads the JSONL that bench/run.py writes. Which records are read is always
said, never assumed:

  summarize.py FILE [FILE ...]     exactly these files
  summarize.py --batch ID [...]    these batches, from bench/results/ (or
                                   from the files given)
  summarize.py --all               every batch in bench/results/, merged
  summarize.py                     the newest batch in bench/results/, and
                                   stderr names it

Headline rows are agent runs (--spend) only, one per (task, arm, model,
effort, limits), the limits being timeout_s, budget_usd and max_turns, so
runs held to different limits are never pooled. Dry runs are a scripted
solver, not an agent, so they get their own table, labelled as such, and
never feed a headline number; sabotage runs are labelled apart from both.

Per row: runs, then each outcome counted (passed of graded, failed, timeout,
budget, max turns, harness errors), how many cut-off runs (any limit) had the
right state when stopped, the pass rate with a Wilson 95% interval over the
graded runs (everything but harness errors; a cut-off run is a failure),
medians and IQRs of input+cache tokens, output tokens, tool calls, turns and
wall time, summed cost, cost per passed run, and the result subtypes seen.
Under each table, a two-sided Fisher exact test on pass counts between the
arms, per task, and a provenance section naming every batch, git SHA, claude
version and limits read.

Python 3 stdlib only.
"""

import argparse
import glob
import json
import math
import os
import statistics
import sys
from collections import Counter

BENCH = os.path.dirname(os.path.abspath(__file__))
RESULTS_DIR = os.path.join(BENCH, "results")
Z95 = 1.959964
SMALL_N = 5
CUTOFFS = ("timeout", "budget", "max_turns")  # the agent hit a limit: a failure
GRADED = ("passed", "failed", *CUTOFFS)
LIMIT_FIELDS = ("timeout_s", "budget_usd", "max_turns")
NO_BATCH = "(none)"


def wilson(passes, n, z=Z95):
    """Wilson score interval for `passes` out of `n`, as (low, high)."""
    if n == 0:
        return (0.0, 1.0)
    p = passes / n
    denom = 1 + z * z / n
    centre = (p + z * z / (2 * n)) / denom
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denom
    return (max(0.0, centre - half), min(1.0, centre + half))


def fisher_exact(a, b, c, d):
    """Two-sided Fisher exact p for the 2x2 table [[a, b], [c, d]].

    Sums the hypergeometric probability of every table with the same margins
    that is no more likely than the one observed (the usual two-sided rule,
    with a relative tolerance for float ties).
    """
    row1, row2, col1 = a + b, c + d, a + c
    n = row1 + row2
    if n == 0:
        return 1.0
    total = math.comb(n, col1)

    def prob(x):
        return math.comb(row1, x) * math.comb(row2, col1 - x) / total

    observed = prob(a)
    lo, hi = max(0, col1 - row2), min(row1, col1)
    p = sum(prob(x) for x in range(lo, hi + 1) if prob(x) <= observed * (1 + 1e-9))
    return min(1.0, p)


def is_num(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def median_iqr(values):
    """(median, q1, q3) over the numbers in `values`, or None if there are none."""
    nums = sorted(v for v in values if is_num(v))
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


def tokens_in(record):
    """Input plus cache tokens; older records only have the usage dict."""
    if isinstance(record.get("tokens_in"), int):
        return record["tokens_in"]
    usage = record.get("usage") or {}
    parts = [usage.get(k) for k in ("input", "cache_read", "cache_creation")]
    parts = [p for p in parts if isinstance(p, int)]
    return sum(parts) if parts else None


def tokens_out(record):
    if isinstance(record.get("tokens_out"), int):
        return record["tokens_out"]
    value = (record.get("usage") or {}).get("output")
    return value if isinstance(value, int) else None


def load(paths):
    records = []
    for path in paths:
        with open(path) as f:
            for number, line in enumerate(f, 1):
                if not line.strip():
                    continue
                try:
                    record = json.loads(line)
                except ValueError:
                    print(f"summarize.py: {path}:{number} is not JSON; skipped", file=sys.stderr)
                    continue
                if isinstance(record, dict):
                    records.append(record)
    return records


# --- One table -----------------------------------------------------------------------

COLUMNS = (
    "task", "arm", "model", "effort", "limits", "runs", "passed", "failed", "timeout",
    "budget", "max turns", "cut off", "harness err", "pass rate (95% CI)",
    "in+cache tok med [IQR]", "out tok med [IQR]", "tool calls med [IQR]", "turns med [IQR]",
    "wall s med [IQR]", "cost $ (sum)", "$ per pass", "result subtypes",
)
HEADER = "| " + " | ".join(COLUMNS) + " |"
RULE = "|" + "|".join(["---"] * len(COLUMNS)) + "|"


def group_key(record):
    """(task, arm, model, effort, timeout_s, budget_usd, max_turns): runs held
    to different limits are different experiments, never one row."""
    return (
        record.get("task", "?"), record.get("arm", "?"),
        record.get("model") or "-", record.get("effort") or "-",
        *(record.get(field) for field in LIMIT_FIELDS),
    )


def _order(key):
    """Sort key for a group key, whose limits may be numbers or None."""
    def one(value):
        if value is None:
            return (2, 0, "")
        return (0, value, "") if is_num(value) else (1, 0, str(value))
    return tuple(one(v) for v in key)


def _g(value):
    return f"{value:g}" if is_num(value) else str(value)


def fmt_limits(timeout_s, budget_usd, max_turns):
    """`600s, $1, no turn cap`; `-` when none is recorded (a dry run, or a
    record from before limits were recorded)."""
    if timeout_s is None and budget_usd is None and max_turns is None:
        return "-"
    return ", ".join([
        "-" if timeout_s is None else f"{_g(timeout_s)}s",
        "-" if budget_usd is None else f"${_g(budget_usd)}",
        "no turn cap" if max_turns is None else f"{_g(max_turns)} turns",
    ])


def name(key):
    """How notes name a row: task/arm, plus whatever else sets it apart."""
    task, arm, model, effort, *lims = key
    extra = [part for part in (model, effort, fmt_limits(*lims)) if part != "-"]
    return f"{task}/{arm}" + (f" [{', '.join(extra)}]" if extra else "")


def counts(records):
    by_status = Counter(r.get("status") for r in records)
    graded = [r for r in records if r.get("status") in GRADED]
    passes = sum(1 for r in graded if r.get("passed") is True)
    return by_status, graded, passes


def row(key, records):
    """(markdown row, notes) for one group of records."""
    task, arm, model, effort, *lims = key
    who = name(key)
    by_status, graded, passes = counts(records)
    n = len(graded)
    if n:
        low, high = wilson(passes, n)
        rate = f"{passes / n:.0%} ({low:.0%}-{high:.0%})"
    else:
        rate = "-"
    notes = []
    # A scripted solver costs nothing and has no cost to be missing.
    costs = [r.get("cost_usd") for r in graded if r.get("mode") == "spend"]
    known = [c for c in costs if is_num(c)]
    unknown = len(costs) - len(known)
    mark = "*" if unknown and known else ""
    if unknown:
        notes.append(f"{who}: cost unknown for {unknown} graded run(s)")
    spend = [r for r in graded if r.get("mode") == "spend"]
    no_tokens = sum(1 for r in spend if tokens_in(r) is None)
    if no_tokens:
        notes.append(f"{who}: tokens unknown for {no_tokens} graded run(s), left out of the medians")
    no_turns = [r for r in spend if not is_num(r.get("turns"))]
    if no_turns:
        timeouts = sum(1 for r in no_turns if r.get("status") == "timeout")
        notes.append(
            f"{who}: turns unknown for {len(no_turns)} graded run(s), {timeouts} of them "
            "timeouts (only a result event carries num_turns), left out of the turns median"
        )
    cost = f"{sum(known):.2f}{mark}" if known else "-"
    if known and passes:
        per_pass = f"{sum(known) / passes:.3f}{mark}"
    else:
        per_pass = "n/a (0 passes)" if known else "-"
    cut = [r for r in graded if r.get("status") in CUTOFFS]
    # The state a passing run needs: the check and a live demo.
    correct = sum(1 for r in cut if r.get("check_passed") is True and r.get("demo_alive") is not False)
    subtypes = Counter(r.get("result_subtype") or "none" for r in records if r.get("mode") == "spend")
    others = sorted(s for s in by_status if s not in GRADED and s != "harness_error")
    if others:
        notes.append(f"{who}: {sum(by_status[s] for s in others)} record(s) with unknown status {others}")
    cells = [
        task, arm, model, effort, fmt_limits(*lims), str(len(records)), f"{passes}/{n}",
        str(by_status["failed"]), str(by_status["timeout"]), str(by_status["budget"]),
        str(by_status["max_turns"]),
        f"{len(cut)} ({correct} with correct state)" if cut else "0",
        str(by_status["harness_error"]), rate,
        fmt_spread([tokens_in(r) for r in graded]),
        fmt_spread([tokens_out(r) for r in graded]),
        fmt_spread([r.get("tool_calls_total") for r in graded]),
        fmt_spread([r.get("turns") for r in graded]),
        fmt_spread([r.get("wall_ms") for r in graded], scale=1000, digits=1),
        cost, per_pass,
        ", ".join(f"{s} {c}" for s, c in sorted(subtypes.items())) or "-",
    ]
    return "| " + " | ".join(cells) + " |", notes


def comparison(groups):
    """Per (task, model, effort, limits) with both arms: pass counts and Fisher's p."""
    cells = {}
    for (task, arm, *rest), records in groups.items():
        _, graded, passes = counts(records)
        cells.setdefault((task, *rest), {})[arm] = (passes, len(graded))
    lines = []
    for key in sorted(cells, key=_order):
        task, model, effort, *lims = key
        arms = cells[key]
        if "taria" not in arms or "tmux" not in arms:
            continue
        (tp, tn), (mp, mn) = arms["taria"], arms["tmux"]
        p = fisher_exact(tp, tn - tp, mp, mn - mp) if tn and mn else None
        lines.append(
            f"| {task} | {model} | {effort} | {fmt_limits(*lims)} | {tp}/{tn} | {mp}/{mn} | "
            f"{'-' if p is None else f'{p:.3f}'} |"
        )
    if not lines:
        return []
    return [
        "",
        "Arm comparison: two-sided Fisher exact test on pass counts (graded runs).",
        "",
        "| task | model | effort | limits | taria passed | tmux passed | Fisher p |",
        "|---|---|---|---|---|---|---|",
        *lines,
    ]


def table(records):
    groups = {}
    for r in records:
        groups.setdefault(group_key(r), []).append(r)
    ordered = sorted(groups, key=_order)
    lines, notes = [HEADER, RULE], []
    for key in ordered:
        line, row_notes = row(key, groups[key])
        lines.append(line)
        notes += row_notes
    small = [name(k) for k in ordered if len(counts(groups[k])[1]) < SMALL_N]
    lines += comparison(groups)
    footer = []
    if small:
        footer.append(
            f"n < {SMALL_N} graded runs in {'; '.join(small)}: at that size the IQR and the "
            "Fisher test are weak, so read them as descriptive."
        )
    if any(r.get("status") in CUTOFFS for r in records):
        footer.append(
            "A cut-off run (timeout, budget, max turns) hit a limit before the agent "
            "finished on its own, so it fails whatever its dump says; `cut off` counts "
            "how many of them had the right state when they were stopped."
        )
    if any("cost unknown" in n for n in notes):
        footer.append(
            "Cost is known only from a result event; a run cut off before one (a timeout) "
            "has none, so a sum marked * leaves those runs out and is a lower bound. "
            "Their tokens are what streamed before the cut (usage_source stream) or null."
        )
    footer += notes
    errors = [r for r in records if r.get("status") == "harness_error"]
    error_cost = [r.get("cost_usd") for r in errors if is_num(r.get("cost_usd"))]
    if error_cost:
        footer.append(f"Harness-error runs cost ${sum(error_cost):.2f}, outside every row.")
    unpriced = sum(1 for r in errors if r.get("mode") == "spend" and not is_num(r.get("cost_usd")))
    if unpriced:
        footer.append(
            f"{unpriced} harness-error agent run(s) ended before a result event (the guard "
            "stops a run at its init event), so what they spent is not known."
        )
    if footer:
        lines += [""] + [f"- {f}" for f in footer]
    return "\n".join(lines)


# --- Provenance -------------------------------------------------------------------------


def provenance(records):
    batches = {}
    for r in records:
        batches.setdefault(r.get("batch") or NO_BATCH, []).append(r)
    lines = [
        "| batch | kinds | records | git SHA | dirty | claude | models (agent) | efforts | "
        "timeout s | budget $ | max turns | seeds |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]

    def values(rs, field, fmt=str):
        got = sorted({fmt(r.get(field)) for r in rs if r.get(field) is not None})
        return ", ".join(got) or "-"

    def limit(rs, field):
        """An agent run's limit: `none` when not passed, `unrecorded` when the
        record predates the field. Dry runs have none to show."""
        got = {
            "unrecorded" if field not in r else "none" if r[field] is None else _g(r[field])
            for r in rs if label(r) == "agent"
        }
        return ", ".join(sorted(got)) or "-"

    for batch in sorted(batches):
        rs = batches[batch]
        lines.append(
            f"| {batch} | {', '.join(sorted({label(r) for r in rs}))} | {len(rs)} | "
            f"{values(rs, 'git_sha', lambda s: s[:12])} | {values(rs, 'git_dirty')} | "
            f"{values(rs, 'claude_version')} | {values(rs, 'agent_model')} | "
            f"{values(rs, 'effort')} | {limit(rs, 'timeout_s')} | {limit(rs, 'budget_usd')} | "
            f"{limit(rs, 'max_turns')} | {values(rs, 'seed')} |"
        )
    warnings = []
    for field, what in (("git_sha", "git SHAs"), ("claude_version", "claude versions")):
        seen = {r.get(field) for r in records if label(r) == "agent" and r.get(field)}
        if len(seen) > 1:
            warnings.append(f"the agent runs mix {len(seen)} {what}; rows pool them")
    if any(r.get("git_dirty") for r in records):
        warnings.append("some records come from a dirty tree; the SHA alone does not reproduce them")
    if any((r.get("batch") or NO_BATCH) == NO_BATCH for r in records):
        warnings.append("some records predate provenance (no batch, SHA or claude version)")
    if any(label(r) == "agent" and "timeout_s" not in r for r in records):
        warnings.append(
            "some agent records predate recorded limits; their rows show - for the limits, "
            "and runs held to different limits may be pooled in them"
        )
    groups = _agent_groups(records)
    for key in sorted(groups, key=_order):
        model = key[2]
        actual = {r.get("agent_model") for r in groups[key] if r.get("agent_model")}
        if actual and actual != {model}:
            warnings.append(f"{name(key)}: asked for {model}, the agent reported {sorted(actual)}")
    return lines + ([""] + [f"- {w}" for w in warnings] if warnings else [])


def _agent_groups(records):
    groups = {}
    for r in records:
        if label(r) == "agent":
            groups.setdefault(group_key(r), []).append(r)
    return groups


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
        ]
        for kind in ("dry-run", "dry-run (sabotage)"):
            subset = [r for r in scripted if label(r) == kind]
            if subset:
                out += ["", f"### {kind}", "", table(subset)]
    out += ["", "## Provenance", ""] + provenance(records)
    return "\n".join(out)


# --- Selecting records ---------------------------------------------------------------


def parse_args(argv):
    parser = argparse.ArgumentParser(description="Summarize taria benchmark results.")
    parser.add_argument("files", nargs="*", help="results files (default: bench/results/*.jsonl)")
    parser.add_argument("--batch", action="append", help="only this batch id (repeatable)")
    parser.add_argument("--all", action="store_true", help="every batch found, merged")
    args = parser.parse_args(argv)
    if args.all and args.batch:
        parser.error("--all and --batch exclude each other")
    return args


def select(args):
    """(records, a note on what was selected) or raises SystemExit with a reason."""
    paths = args.files or sorted(glob.glob(os.path.join(RESULTS_DIR, "*.jsonl")))
    if not paths:
        raise SystemExit(f"summarize.py: no results given and none in {RESULTS_DIR}")
    records = load(paths)
    batch_of = [(r.get("batch") or NO_BATCH) for r in records]
    found = sorted(set(batch_of))
    if args.batch:
        missing = [b for b in args.batch if b not in found]
        if missing:
            raise SystemExit(
                f"summarize.py: no records for batch {', '.join(missing)}; found {', '.join(found) or 'none'}"
            )
        chosen = set(args.batch)
        return [r for r, b in zip(records, batch_of) if b in chosen], f"batch {', '.join(args.batch)}"
    if args.files or args.all:
        return records, f"{len(found)} batch(es) from {len(paths)} file(s): {', '.join(found)}"
    newest = max(
        (b for b in found if b != NO_BATCH),
        key=lambda b: max(r.get("ts", "") for r, rb in zip(records, batch_of) if rb == b),
        default=None,
    )
    if newest is None:
        raise SystemExit(
            "summarize.py: no record in bench/results carries a batch id; name the files "
            "or pass --all"
        )
    note = f"no files or --batch given: the newest batch, {newest}"
    if len(found) > 1:
        note += f" ({len(found) - 1} other batch(es) left out; --all merges them)"
    return [r for r, b in zip(records, batch_of) if b == newest], note


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        records, note = select(args)
    except SystemExit as stop:
        print(stop.code, file=sys.stderr)
        return 1
    print(f"summarize.py: {note}; {len(records)} record(s)", file=sys.stderr)
    print(render(records))
    return 0


if __name__ == "__main__":
    sys.exit(main())
