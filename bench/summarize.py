#!/usr/bin/env python3
"""Summarize benchmark results as Markdown, on stdout.

Reads the rows bench/run.py writes, one per run, to
bench/results/<batch>/runs.jsonl. Which rows are read is always said, never
assumed:

  summarize.py FILE [FILE ...]     exactly these files
  summarize.py --batch ID [...]    only this batch (repeatable, or
                                   comma-separated), from the files given or
                                   else bench/results/; several are merged
  summarize.py --all               every batch found, merged
  summarize.py                     the newest batch in bench/results/, and
                                   stderr names it

Without files, bench/results/*/runs.jsonl and bench/results/*.jsonl (the
older flat layout) are searched.

Sections:

- Not a clean batch, first, when any batch's plan tolerated an attachment
  (run.py plan --tolerate-attachment; its rows' context.tolerated_attachments):
  per such batch what its plan tolerated and how many runs got it (that it
  carries it in every call only when every run got it), and, merged with
  clean batches, that the conditions differ. Every agent
  section repeats the marker under its heading, and every table row built
  from such runs has `[not clean]` beside the arm, task or pair it is about
  (`[not clean: X only]` for a comparison where only X's side carries it).
- Run conditions, only when the rows come from more than one batch: per
  batch the git SHA, claude versions, the agents' identity line (the first
  line of their system prompt, prompt_snapshot.cliPrefix, which Claude Code
  chooses), models, effort, max turns, seed, arms, n and the attachments its
  plan tolerated, and which of them differ between the batches with agent
  runs, since merged batches are compared as if they ran together. A batch
  of dry runs only is listed, and named as left out of that comparison.
- Agent runs (headline): subagent rows only, one row per (task, arm, model,
  effort, max_turns), so runs held to different settings are never pooled.
  Per row: graded runs (n), passes and the pass rate with a Wilson 95%
  interval, harness errors (counted, but outside n and the pass rate), runs
  cut off at max_turns (failures; how many had the right state when cut),
  and medians: tool calls [IQR] (ToolSearch excluded), ToolSearch calls,
  turns, tokens processed [IQR] (input + cache_creation + cache_read),
  output tokens and wall time. A run with an assistant message Claude Code
  never closed (output_tokens_lower_bound, or end_inferred on a row from
  before that field) has a placeholder output count, so it is left out of
  the output-token median, and the cell says how many were left out. A
  subagent row from before that field that is not end_inferred cannot be
  checked for such a message mid-run: it stays in the median, the cell
  counts it as unchecked, and a footer warns its output may be low.
- Arms per model, across all tasks: per model and arm the pass rate (Wilson
  95%), and median [IQR] tokens processed, tool calls and output tokens;
  then each pair of arms (outline vs taria, and every arm vs tmux) with the
  pass-rate difference, a Fisher exact p and the ratios of those medians.
- Per model, each pair of arms per task (the pass-rate difference with a
  two-sided Fisher exact p, and the baseline/subject ratios of median tokens
  processed and median tool calls; with only taria and tmux this is the
  "taria vs tmux" section). Then each arm across all tasks: pass rate,
  median tokens processed and tool calls per run, total tokens processed.
- Model comparison: each pair of models per arm, overall pass rate (Fisher
  p) and median tokens processed.
- Dry runs (a scripted solver, no model) in their own table, never headline.
- Provenance: batch, git SHA and dirty flag, claude version, models
  requested and resolved, effort, max turns, runs; guard problems; and the
  context each arm's agents were given (attachment types, every deferred
  tool and MCP instruction server that ever reached them, as the guard
  checks, the tree formats their read_tree descriptions named, and how many
  runs got each attachment their plan tolerated), so a reader can see the
  arms got equivalent context.

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
from itertools import combinations

BENCH = os.path.dirname(os.path.abspath(__file__))
RESULTS_DIR = os.path.join(BENCH, "results")
Z95 = 1.959964
SMALL_N = 5
CUTOFF = "max_turns"  # the agent ran out of turns: a failure
GRADED = ("passed", "failed", CUTOFF)
KNOWN = (*GRADED, "harness_error")
AGENT_MODES = ("subagent", "spend")  # "spend": rows from before subagent runs
# Which arms are compared, and which way round: each pair is (subject,
# baseline), its difference is subject minus baseline and its ratios are
# baseline/subject (above 1x: the baseline used more). Every arm is held
# against tmux, the screen scraper, and outline against taria, the same
# bridge showing JSON. bench/harness.py's ARMS names them; this module
# reads only rows, so it keeps its own copy of the order.
ARM_ORDER = ("taria", "tmux", "outline")
BASELINE = "tmux"
EXTRA_PAIRS = (("outline", "taria"),)
NO_BATCH = "(none)"
# Attachment types a batch's plan tolerated (run.py plan
# --tolerate-attachment), which each of its rows records in
# context.tolerated_attachments, and how the marker names what such an
# attachment put in front of the agent. A batch that tolerated one is not a
# clean batch: where every arm got the same note its arms compare with each
# other, but its absolute token counts include the note.
TOLERATED_WHAT = {"remote_session_change": "remote control's note"}
NOT_CLEAN = "[not clean]"


# --- Statistics ---------------------------------------------------------------------------


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


def median(values):
    stats = median_iqr(values)
    return None if stats is None else stats[0]


# --- Formatting ---------------------------------------------------------------------------


def fmt_tokens(value):
    return f"{value:,.0f}"


def fmt_count(value):
    """1, 12, 1.5: a median of counts keeps its half."""
    return f"{value:,.0f}" if float(value).is_integer() else f"{value:,.1f}"


def fmt_seconds(ms):
    return f"{ms / 1000:,.1f}"


def fmt_spread(values, fmt=fmt_count):
    """`median [q1-q3]`, or the median alone when the quartiles meet; `-` for none."""
    stats = median_iqr(values)
    if stats is None:
        return "-"
    med, q1, q3 = stats
    return fmt(med) if q1 == q3 else f"{fmt(med)} [{fmt(q1)}-{fmt(q3)}]"


def fmt_median(values, fmt=fmt_count):
    med = median(values)
    return "-" if med is None else fmt(med)


def fmt_rate(passes, n):
    if not n:
        return "-"
    low, high = wilson(passes, n)
    return f"{passes / n:.0%} ({low:.0%}-{high:.0%})"


def fmt_p(p):
    return "-" if p is None else f"{p:.3f}"


def fmt_ratio(num, den):
    if num is None or den is None or den == 0:
        return "-"
    return f"{num / den:.2f}x"


def _show(value):
    return "-" if value is None else f"{value:g}" if is_num(value) else str(value)


# --- Reading rows ---------------------------------------------------------------------------


def label(record):
    if record.get("mode") in AGENT_MODES:
        return "agent"
    if record.get("solver") == "scripted-sabotage":
        return "dry-run (sabotage)"
    return "dry-run"


def _tokens(record):
    tokens = record.get("tokens")
    return tokens if isinstance(tokens, dict) else {}


def tokens_processed(record):
    """input + cache_creation + cache_read; None when none of them is known."""
    parts = [_tokens(record).get(k) for k in ("input", "cache_creation", "cache_read")]
    parts = [p for p in parts if is_num(p)]
    if parts:
        return sum(parts)
    # Rows from before subagent runs.
    if is_num(record.get("tokens_in")):
        return record["tokens_in"]
    usage = record.get("usage") or {}
    legacy = [usage.get(k) for k in ("input", "cache_read", "cache_creation") if is_num(usage.get(k))]
    return sum(legacy) if legacy else None


def output_lower_bound(record):
    """Whether the row's output tokens are only a lower bound: an assistant
    message Claude Code never closed kept a placeholder count. Rows from
    before `output_tokens_lower_bound` was recorded flag it by end_inferred,
    which needs such a message (a mid-run one goes unseen there)."""
    return record.get("output_tokens_lower_bound") is True or record.get("end_inferred") is True


def output_unchecked(record):
    """Whether a subagent row's output tokens may be low without the row
    saying so: graded before `output_tokens_lower_bound` existed, and not
    end_inferred, so an assistant message Claude Code never closed in the
    middle of the run cannot be ruled out. A regrade records the field."""
    return (
        record.get("mode") == "subagent"
        and not isinstance(record.get("output_tokens_lower_bound"), bool)
        and record.get("end_inferred") is not True
        and tokens_output(record) is not None
    )


def output_cell(graded, fmt=None):
    """The output-token median of `graded` (fmt_median, or `fmt` such as
    fmt_spread), leaving out the rows whose count is a lower bound, with
    `(N left out, M unchecked)` when there are any."""
    fmt = fmt or fmt_median
    bounded = sum(1 for r in graded if output_lower_bound(r))
    unchecked = sum(1 for r in graded if output_unchecked(r))
    cell = fmt([tokens_output(r) for r in graded if not output_lower_bound(r)], fmt_tokens)
    notes = ([f"{bounded} left out"] if bounded else []) + ([f"{unchecked} unchecked"] if unchecked else [])
    return cell + (f" ({', '.join(notes)})" if notes else "")


def tokens_output(record):
    value = _tokens(record).get("output")
    if is_num(value):
        return value
    if is_num(record.get("tokens_out")):
        return record["tokens_out"]
    value = (record.get("usage") or {}).get("output")
    return value if is_num(value) else None


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


# --- Batches that tolerated an attachment ------------------------------------------------------


def _context_of(record):
    return record.get("context") if isinstance(record.get("context"), dict) else {}


def tolerated_types(record):
    """The attachment types the row's plan tolerated, from its context
    (none for a row of a clean batch, or graded before the field existed)."""
    tolerated = _context_of(record).get("tolerated_attachments")
    return sorted(str(t) for t in tolerated) if isinstance(tolerated, dict) else []


def not_clean(records):
    """Whether any of `records` comes from a batch that tolerated an attachment."""
    return any(tolerated_types(r) for r in records)


def _marked(cell, records):
    """`cell`, with NOT_CLEAN after it when `records` include a run of a
    batch that tolerated an attachment, so the row cannot pass for clean."""
    return f"{cell} {NOT_CLEAN}" if not_clean(records) else cell


def _marked_pair(cell, sides):
    """`cell` for a comparison of two `sides`, (name, records) each: NOT_CLEAN
    after it when both include runs of a batch that is not clean, and
    `[not clean: <name> only]` when one does, since then the two were run
    under different conditions."""
    dirty = [name for name, records in sides if not_clean(records)]
    if not dirty:
        return cell
    return f"{cell} {NOT_CLEAN}" if len(dirty) == len(sides) else f"{cell} [not clean: {dirty[0]} only]"


def _what(types):
    return " and ".join(TOLERATED_WHAT.get(t, f"a {t} attachment") for t in types)


def _carried(rows, kind):
    """(runs that got a `kind` attachment, each one's rendered characters)."""
    chars = []
    for r in rows:
        tolerated = _context_of(r).get("tolerated_attachments")
        tally = tolerated.get(kind) if isinstance(tolerated, dict) else None
        if isinstance(tally, dict) and is_num(tally.get("count")) and tally["count"] > 0:
            chars.append(tally.get("rendered_chars") if is_num(tally.get("rendered_chars")) else 0)
    return len(chars), chars


def _tolerance_head(batch, types, rows):
    """What a batch that tolerated `types` carries, true to how many of its
    `rows` got each: "B carries X in every call" when every run got every
    type, else "B tolerated X; N of M runs got it"."""
    counts = [(kind, _carried(rows, kind)[0]) for kind in types]
    if all(n == len(rows) for _, n in counts):
        return f"{batch} carries {_what(types)} in every call"
    if len(counts) == 1:
        return f"{batch} tolerated {_what(types)}; {counts[0][1]} of {len(rows)} runs got it"
    return f"{batch} tolerated " + "; ".join(f"{_what([kind])}, which {n} of {len(rows)} runs got"
                                             for kind, n in counts)


def tolerance_banner(records):
    """The marker for results with runs of a batch that tolerated an
    attachment: per such batch what it carries and how many of its runs got
    it, then, when clean batches are merged in, that the conditions differ,
    and what NOT_CLEAN marks. Empty when every batch is clean. A batch is
    said to carry the note in every call only when every run got it."""
    agent = [r for r in records if label(r) == "agent"]
    batches = group(agent, lambda r: r.get("batch") or NO_BATCH)
    tolerating = {b: sorted({t for r in rs for t in tolerated_types(r)}) for b, rs in batches.items()}
    tolerating = {b: types for b, types in tolerating.items() if types}
    if not tolerating:
        return []
    lines = []
    for batch in sorted(tolerating):
        types, rows = tolerating[batch], batches[batch]
        got, counts = [], []
        for kind in types:
            n, chars = _carried(rows, kind)
            counts.append(n)
            size = "" if not chars else f", {min(chars)}" + (f"-{max(chars)}" if max(chars) != min(chars) else "")
            got.append(f"{kind}: {n} of {len(rows)} runs got it{size}{' characters each' if size else ''}")
        if all(n == len(rows) for n in counts):
            what = ("Where every arm got the same note, its arms compare with each other, but its absolute token "
                    "counts include the note.")
        elif not any(counts):
            what = ("No run got it, so no token count includes it, but its plan would have let it through, so it "
                    "is not a clean batch.")
        else:
            what = ("Only some runs got it, so its arms may not have carried it alike (the agent context table "
                    "counts them per arm and model), and the token counts of the runs that got it include it.")
        lines.append(f"> **{_tolerance_head(batch, types, rows)}: not a clean batch.** Its plan tolerated "
                     f"{'; '.join(got)}. {what}")
    clean = sorted(b for b in batches if b not in tolerating)
    if clean:
        lines.append(f"> Mixed conditions: {', '.join(clean)} tolerated nothing. An arm from "
                     f"{', '.join(sorted(tolerating))} held against one from {', '.join(clean)} differs in this "
                     "too: the note's tokens count on one side only.")
    lines.append(f"> Rows marked `{NOT_CLEAN}` include runs of a batch that is not clean, and a comparison "
                 "marked `[not clean: X only]` holds X's such runs against runs that are clean.")
    out = []
    for line in lines:
        out += [line, ">"]  # one quote, its paragraphs apart
    return out[:-1]


def tolerance_marker(records):
    """One line under each agent section's heading when `records` include
    a batch that is not clean (the banner at the top says which)."""
    if not not_clean(records):
        return []
    batches = group([r for r in records if tolerated_types(r)], lambda r: r.get("batch") or NO_BATCH)
    heads = [_tolerance_head(b, sorted({t for r in rs for t in tolerated_types(r)}), rs)
             for b, rs in sorted(batches.items())]
    one = len(heads) == 1
    where = "" if all(h.endswith(" in every call") for h in heads) else " where a run got it"
    return [f"> {'; '.join(heads)}: not {'a clean batch' if one else 'clean batches'}. Rows marked `{NOT_CLEAN}` "
            f"include {'its' if one else 'their'} runs, whose absolute token counts include the note{where}.", ""]


# --- Grouping -----------------------------------------------------------------------------


def group_key(record):
    """(task, arm, model, effort, max_turns): runs held to different settings
    are different experiments, never one row."""
    return (
        record.get("task", "?"), record.get("arm", "?"),
        record.get("model") or "-", record.get("effort") or "-", record.get("max_turns"),
    )


def _order(key):
    """Sort key for a tuple whose parts may be numbers, strings or None."""
    def one(value):
        if value is None:
            return (2, 0, "")
        return (0, value, "") if is_num(value) else (1, 0, str(value))
    return tuple(one(v) for v in key)


def group(records, key):
    groups = {}
    for r in records:
        groups.setdefault(key(r), []).append(r)
    return groups


def name(key):
    """How notes name a row: task/arm, plus whatever else sets it apart."""
    task, arm, model, effort, max_turns = key
    extra = [part for part in (model, effort) if part != "-"]
    if max_turns is not None:
        extra.append(f"{_show(max_turns)} turns")
    return f"{task}/{arm}" + (f" [{', '.join(extra)}]" if extra else "")


def counts(records):
    """(statuses, graded rows, passes): harness errors and unknown statuses
    are outside the graded rows; a cut-off run is a graded failure."""
    by_status = Counter(r.get("status") for r in records)
    graded = [r for r in records if r.get("status") in GRADED]
    passes = sum(1 for r in graded if r.get("passed") is True)
    return by_status, graded, passes


def cut_off(graded):
    """`N (M with correct state)` for the max_turns runs among `graded`."""
    cut = [r for r in graded if r.get("status") == CUTOFF]
    if not cut:
        return "0"
    # The state a passing run needs: the check and a live demo.
    correct = sum(1 for r in cut if r.get("check_passed") is True and r.get("demo_alive") is not False)
    return f"{len(cut)} ({correct} with correct state)"


# --- The per-group table ----------------------------------------------------------------------

UNCHECKED_WARNING = (
    "Warning: {n} graded run(s) were graded before `output_tokens_lower_bound` existed and are not "
    "end_inferred, so an assistant message Claude Code never closed mid-run cannot be ruled out: "
    "their output tokens may be low. They stay in the output-token median, and `(N unchecked)` in "
    "a cell counts them; `run.py regrade` on their batch records the field."
)

COLUMNS = (
    "task", "arm", "model", "effort", "max turns", "n", "passed", "pass rate (95% CI)",
    "harness err", "cut off", "tool calls med [IQR]", "ToolSearch med", "turns med",
    "tokens processed med [IQR]", "output tok med", "wall s med",
)


def _header(columns):
    return ["| " + " | ".join(columns) + " |", "|" + "|".join(["---"] * len(columns)) + "|"]


def _row(cells):
    return "| " + " | ".join(str(c) for c in cells) + " |"


def row(key, records):
    """(markdown row, notes) for one group of records."""
    task, arm, model, effort, max_turns = key
    who = name(key)
    by_status, graded, passes = counts(records)
    notes = []
    agent = [r for r in graded if label(r) == "agent"]
    no_tokens = sum(1 for r in agent if tokens_processed(r) is None)
    if no_tokens:
        notes.append(f"{who}: tokens unknown for {no_tokens} graded run(s), left out of the medians")
    others = sorted(str(s) for s in by_status if s not in KNOWN)
    if others:
        count = sum(n for s, n in by_status.items() if s not in KNOWN)
        notes.append(f"{who}: {count} row(s) with unknown status {others}, outside n and the pass rate")
    cells = [
        task, _marked(arm, records), model, effort, _show(max_turns), len(graded), f"{passes}/{len(graded)}",
        fmt_rate(passes, len(graded)), by_status["harness_error"], cut_off(graded),
        fmt_spread([r.get("tool_calls_total") for r in graded]),
        fmt_median([r.get("toolsearch_calls") for r in graded]),
        fmt_median([r.get("turns") for r in graded]),
        fmt_spread([tokens_processed(r) for r in graded], fmt_tokens),
        output_cell(graded),
        fmt_median([r.get("wall_ms") for r in graded], fmt_seconds),
    ]
    return _row(cells), notes


def table(records, agent=True):
    groups = group(records, group_key)
    ordered = sorted(groups, key=_order)
    lines, notes = _header(COLUMNS), []
    for key in ordered:
        line, row_notes = row(key, groups[key])
        lines.append(line)
        notes += row_notes
    footer = []
    small = [name(k) for k in ordered if len(counts(groups[k])[1]) < SMALL_N]
    if small:
        footer.append(
            f"n < {SMALL_N} graded runs in {'; '.join(small)}: at that size the IQR and the "
            "Fisher test are weak, so read them as descriptive."
        )
    errors = [r for r in records if r.get("status") == "harness_error"]
    if errors:
        spent = [tokens_processed(r) for r in errors if tokens_processed(r) is not None]
        footer.append(
            f"{len(errors)} harness-error run(s) are counted in `harness err` and left out of n, "
            "the pass rate and every median"
            + (f"; they processed {fmt_tokens(sum(spent))} tokens." if spent else ".")
        )
    if any(r.get("status") == CUTOFF for r in records):
        footer.append(
            "A cut-off run hit max_turns before the agent finished on its own, so it fails "
            "whatever the demo's state; `cut off` counts how many had the right state when cut."
        )
    bounded = [r for r in records if r.get("status") in GRADED and output_lower_bound(r)]
    if bounded:
        footer.append(
            f"{len(bounded)} graded run(s) had an assistant message Claude Code never closed, whose "
            "output count is a placeholder: their output tokens are a lower bound, so they are left "
            "out of the output-token median, and `(N left out)` in a cell counts them. Their input "
            "and cache counts are exact, so tokens processed keep every run."
        )
    unchecked = [r for r in records if r.get("status") in GRADED and output_unchecked(r)]
    if unchecked:
        footer.append(UNCHECKED_WARNING.format(n=len(unchecked)))
    if agent and records:
        footer.append(
            "Tokens processed = input + cache_creation + cache_read. Tool calls exclude ToolSearch, "
            "which loads the arm's deferred tools. Slots ran in parallel, so wall time is "
            "indicative only."
        )
    footer += notes
    if footer:
        lines += [""] + [f"- {f}" for f in footer]
    return "\n".join(lines)


# --- Arm and model comparisons -----------------------------------------------------------------


def _arm_stats(records):
    _, graded, passes = counts(records)
    return {
        "passes": passes, "n": len(graded),
        "tokens": median([tokens_processed(r) for r in graded]),
        "calls": median([r.get("tool_calls_total") for r in graded]),
        "output": median([tokens_output(r) for r in graded if not output_lower_bound(r)]),
        "total": sum(t for t in (tokens_processed(r) for r in graded) if t is not None),
        "tasks": len({r.get("task") for r in graded}),
    }


def _arm_key(arm):
    return (ARM_ORDER.index(arm), "") if arm in ARM_ORDER else (len(ARM_ORDER), str(arm))


def arm_pairs(arms):
    """The (subject, baseline) pairs to compare among `arms`: outline vs
    taria, then every other arm vs tmux, each only when both are present."""
    present = sorted({a for a in arms if a is not None}, key=_arm_key)
    pairs = [(a, b) for a, b in EXTRA_PAIRS if a in present and b in present]
    if BASELINE in present:
        pairs += [(a, BASELINE) for a in present if a != BASELINE and (a, BASELINE) not in pairs]
    return pairs


def _difference(s, b):
    """(pass-rate difference subject minus baseline, Fisher p), or ("-", None)."""
    if not (s["n"] and b["n"]):
        return "-", None
    diff = f"{(s['passes'] / s['n'] - b['passes'] / b['n']) * 100:+.0f} pp"
    return diff, fisher_exact(s["passes"], s["n"] - s["passes"], b["passes"], b["n"] - b["passes"])


def arm_comparison(records, subject="taria", baseline=BASELINE):
    """Per (task, effort, max_turns) with both arms: `subject` vs `baseline`."""
    cells = {}
    for r in records:
        cells.setdefault((r.get("task", "?"), r.get("effort") or "-", r.get("max_turns")), {}) \
            .setdefault(r.get("arm"), []).append(r)
    lines = []
    for key in sorted(cells, key=_order):
        arms = cells[key]
        if subject not in arms or baseline not in arms:
            continue
        t, m = _arm_stats(arms[subject]), _arm_stats(arms[baseline])
        diff, p = _difference(t, m)
        task, effort, max_turns = key
        sides = ((subject, arms[subject]), (baseline, arms[baseline]))
        lines.append(_row([
            _marked_pair(task, sides), effort, _show(max_turns), f"{t['passes']}/{t['n']}", f"{m['passes']}/{m['n']}",
            diff, fmt_p(p), fmt_ratio(m["tokens"], t["tokens"]), fmt_ratio(m["calls"], t["calls"]),
        ]))
    if not lines:
        return [f"No task has graded runs of both {subject} and {baseline} for this model."]
    s, b = subject, baseline
    return [
        f"{s} vs {b} per task: pass-rate difference ({s} minus {b}, percentage points) "
        f"with a two-sided Fisher exact p, and {b}/{s} ratios of the medians (above 1x: {b} "
        "used more).",
        "",
        *_header(("task", "effort", "max turns", f"{s} passed", f"{b} passed", "difference",
                  "Fisher p", f"tokens processed {b}/{s}", f"tool calls {b}/{s}")),
        *lines,
    ]


def pairs_title(records):
    """`taria vs tmux`, or every pair present, for the per-model heading."""
    pairs = arm_pairs({r.get("arm") for r in records})
    return ", ".join(f"{a} vs {b}" for a, b in pairs) if pairs else "Arms"


def arms_per_model(records):
    """Every (model, arm) across all tasks, then every pair of arms per model.

    One table for the question a batch is run for: does an arm change the
    pass rate or the cost on each model. Medians are per run, with the IQR;
    output tokens leave out the rows whose count is a lower bound."""
    cells = group(records, lambda r: (r.get("model") or "-", r.get("arm", "?"),
                                      r.get("effort") or "-", r.get("max_turns")))
    order = lambda key: (_order(key[:1]), _arm_key(key[1]), _order(key[2:]))  # noqa: E731
    lines = []
    for key in sorted(cells, key=order):
        by_status, graded, passes = counts(cells[key])
        model, arm, effort, max_turns = key
        lines.append(_row([
            model, _marked(arm, cells[key]), effort, _show(max_turns), len({r.get("task") for r in graded}),
            len(graded),
            f"{passes}/{len(graded)}", fmt_rate(passes, len(graded)), by_status["harness_error"],
            fmt_spread([tokens_processed(r) for r in graded], fmt_tokens),
            fmt_spread([r.get("tool_calls_total") for r in graded]),
            output_cell(graded, fmt_spread),
        ]))
    out = [
        "Each arm per model, across all tasks (graded runs; medians per run with the IQR):",
        "",
        *_header(("model", "arm", "effort", "max turns", "tasks", "n", "passed", "pass rate (95% CI)",
                  "harness err", "tokens processed med [IQR]", "tool calls med [IQR]",
                  "output tok med [IQR]")),
        *lines,
    ]
    pair_lines = []
    settings = group(records, lambda r: (r.get("model") or "-", r.get("effort") or "-", r.get("max_turns")))
    for key in sorted(settings, key=_order):
        arms = group(settings[key], lambda r: r.get("arm"))
        for subject, baseline in arm_pairs(arms):
            s, b = _arm_stats(arms[subject]), _arm_stats(arms[baseline])
            diff, p = _difference(s, b)
            model, effort, max_turns = key
            sides = ((subject, arms[subject]), (baseline, arms[baseline]))
            pair_lines.append(_row([
                model, effort, _show(max_turns), _marked_pair(f"{subject} vs {baseline}", sides),
                f"{s['passes']}/{s['n']}", f"{b['passes']}/{b['n']}", diff, fmt_p(p),
                fmt_ratio(b["tokens"], s["tokens"]), fmt_ratio(b["calls"], s["calls"]),
                fmt_ratio(b["output"], s["output"]),
            ]))
    if pair_lines:
        out += [
            "",
            "Each pair of arms per model, across all tasks: pass-rate difference (subject minus "
            "baseline, percentage points) with a two-sided Fisher exact p, and baseline/subject "
            "ratios of the per-run medians (above 1x: the baseline used more).",
            "",
            *_header(("model", "effort", "max turns", "subject vs baseline", "subject passed",
                      "baseline passed", "difference", "Fisher p", "tokens processed baseline/subject",
                      "tool calls baseline/subject", "output tokens baseline/subject")),
            *pair_lines,
        ]
    return out


def arm_overall(records):
    """Each arm across all tasks, per (effort, max_turns)."""
    cells = group(records, lambda r: (r.get("arm", "?"), r.get("effort") or "-", r.get("max_turns")))
    lines = []
    for key in sorted(cells, key=_order):
        s = _arm_stats(cells[key])
        arm, effort, max_turns = key
        lines.append(_row([
            _marked(arm, cells[key]), effort, _show(max_turns), s["tasks"], s["n"], f"{s['passes']}/{s['n']}",
            fmt_rate(s["passes"], s["n"]),
            "-" if s["tokens"] is None else fmt_tokens(s["tokens"]),
            "-" if s["calls"] is None else fmt_count(s["calls"]),
            fmt_tokens(s["total"]) if s["n"] else "-",
        ]))
    return [
        "Each arm across all tasks (graded runs):",
        "",
        *_header(("arm", "effort", "max turns", "tasks", "n", "passed", "pass rate (95% CI)",
                  "tokens processed med/run", "tool calls med/run", "tokens processed total")),
        *lines,
    ]


def per_model(records):
    pairs = arm_pairs({r.get("arm") for r in records})
    out = []
    for model in sorted({r.get("model") or "-" for r in records}):
        subset = [r for r in records if (r.get("model") or "-") == model]
        out += ["", f"### {model}", ""]
        for subject, baseline in pairs:
            out += [*arm_comparison(subset, subject, baseline), ""]
        if not pairs:
            arms = sorted({str(r.get("arm", "?")) for r in records}, key=_arm_key)
            out += [f"Only one arm in these results ({', '.join(arms) or 'none'}); no pair to compare.", ""]
        out += arm_overall(subset)
    return out


def model_comparison(records):
    models = sorted({r.get("model") for r in records if r.get("model")})
    if len(models) < 2:
        return [f"Only one model in these results ({', '.join(models) or 'none'}); nothing to compare."]
    out = []
    for a, b in combinations(models, 2):
        cells = {}
        for r in records:
            if r.get("model") in (a, b):
                key = (r.get("arm", "?"), r.get("effort") or "-", r.get("max_turns"))
                cells.setdefault(key, {}).setdefault(r["model"], []).append(r)
        lines = []
        for key in sorted(cells, key=_order):
            if a not in cells[key] or b not in cells[key]:
                continue
            sa, sb = _arm_stats(cells[key][a]), _arm_stats(cells[key][b])
            p = None
            if sa["n"] and sb["n"]:
                p = fisher_exact(sa["passes"], sa["n"] - sa["passes"], sb["passes"], sb["n"] - sb["passes"])
            arm, effort, max_turns = key
            lines.append(_row([
                _marked_pair(arm, ((a, cells[key][a]), (b, cells[key][b]))), effort, _show(max_turns),
                f"{sa['passes']}/{sa['n']} {fmt_rate(sa['passes'], sa['n'])}",
                f"{sb['passes']}/{sb['n']} {fmt_rate(sb['passes'], sb['n'])}", fmt_p(p),
                "-" if sa["tokens"] is None else fmt_tokens(sa["tokens"]),
                "-" if sb["tokens"] is None else fmt_tokens(sb["tokens"]),
            ]))
        if not lines:
            out += [f"No arm has runs of both {a} and {b}.", ""]
            continue
        out += [
            *_header(("arm", "effort", "max turns", f"{a} passed", f"{b} passed", "Fisher p",
                      f"{a} tokens processed med", f"{b} tokens processed med")),
            *lines, "",
        ]
    return out[:-1] if out and out[-1] == "" else out


# --- Provenance --------------------------------------------------------------------------------


def _values(rows, field, fmt=str):
    got = set()
    for r in rows:
        value = r.get(field)
        for v in value if isinstance(value, list) else [value]:
            if v is not None and v != "":
                got.add(fmt(v))
    return ", ".join(sorted(got)) or "-"


def provenance(records):
    batches = group(records, lambda r: r.get("batch") or NO_BATCH)
    lines = _header(("batch", "kinds", "runs", "git SHA", "dirty", "claude", "models requested",
                     "models resolved", "efforts", "max turns", "seeds"))
    for batch in sorted(batches):
        rs = batches[batch]
        agent = [r for r in rs if label(r) == "agent"]
        lines.append(_row([
            batch, ", ".join(sorted({label(r) for r in rs})), len(rs),
            _values(rs, "git_sha", lambda s: str(s)[:12]), _values(rs, "git_dirty"),
            _values(rs, "claude_version"), _values(agent, "model"), _values(agent, "model_resolved"),
            _values(agent, "effort"), _values(agent, "max_turns", _show), _values(rs, "seed"),
        ]))
    warnings = []
    agent = [r for r in records if label(r) == "agent"]
    for field, what in (("git_sha", "git SHAs"), ("claude_version", "claude versions")):
        seen = {r.get(field) for r in agent if r.get(field)}
        if len(seen) > 1:
            warnings.append(f"the agent runs mix {len(seen)} {what}; rows pool them")
    if any(r.get("git_dirty") for r in records):
        warnings.append("some rows come from a dirty tree; the SHA alone does not reproduce them")
    if any((r.get("batch") or NO_BATCH) == NO_BATCH for r in records):
        warnings.append("some rows predate provenance (no batch, SHA or claude version)")
    for (model,), rs in sorted(group(agent, lambda r: (r.get("model"),)).items(), key=lambda kv: _order(kv[0])):
        off = Counter()
        for r in rs:
            value = r.get("model_resolved")
            for v in value if isinstance(value, list) else [value]:
                if v and model and not str(v).startswith(f"claude-{model}-"):
                    off[str(v)] += 1
        if off:
            ran = ", ".join(f"{v} in {n} run(s)" for v, n in sorted(off.items()))
            warnings.append(f"asked for {model}, the agents ran on {ran}")
    return lines + ([""] + [f"- {w}" for w in warnings] if warnings else [])


def _sha(row):
    sha = row.get("git_sha")
    return None if not sha else str(sha)[:12] + (" (dirty)" if row.get("git_dirty") else "")


NOT_RECORDED = "not recorded"


def _identity_lines(rows):
    """The agents' identity lines (context.cli_prefixes) over `rows`, each
    distinct one quoted, plus `not recorded` when a row predates the field
    (or has no transcript)."""
    seen = set()
    for r in rows:
        context = r.get("context") if isinstance(r.get("context"), dict) else {}
        prefixes = context.get("cli_prefixes")
        if isinstance(prefixes, list):
            seen.update(json.dumps(str(p)) for p in prefixes)
        else:
            seen.add(NOT_RECORDED)
    return "; ".join(sorted(seen)) or "-"


def _batch_conditions(rows):
    """What one batch ran under, as display strings: the settings an agent
    run depends on, read from its rows (agent rows where there are any)."""
    agent = [r for r in rows if label(r) == "agent"] or rows
    _, graded, _ = counts(agent)
    claude = set()
    for r in agent:
        versions = r.get("claude_versions")
        claude.update(v for v in (versions if isinstance(versions, list) else [r.get("claude_version")]) if v)
    return {
        "kinds": ", ".join(sorted({label(r) for r in rows})),
        "git SHA": ", ".join(sorted({_sha(r) for r in agent if _sha(r)})) or "-",
        "claude": ", ".join(sorted(claude)) or "-",
        "identity line": _identity_lines([r for r in agent if label(r) == "agent"]),
        "models": _values(agent, "model"),
        "models resolved": _values(agent, "model_resolved"),
        "effort": _values(agent, "effort"),
        "max turns": _values(agent, "max_turns", _show),
        "seed": _values(agent, "seed"),
        "arms": ", ".join(sorted({str(r.get("arm", "?")) for r in agent}, key=_arm_key)),
        "n": len(graded),
        "harness err": sum(1 for r in agent if r.get("status") == "harness_error"),
        "tolerated attachments": ", ".join(sorted({t for r in agent for t in tolerated_types(r)})) or "none",
    }


CONDITION_COLUMNS = ("kinds", "git SHA", "claude", "identity line", "models", "models resolved", "effort",
                     "max turns", "seed", "arms", "n", "harness err", "tolerated attachments")
# The conditions that can change a result; arms and n differ by design. The
# identity line is the first line of the agent's system prompt
# (prompt_snapshot.cliPrefix), which Claude Code chooses, so two batches can
# differ in it with nothing in the bench changed. The tolerated attachments
# are what the batch's plan let through (run.py plan --tolerate-attachment):
# a batch with one is not clean, and its token counts include it.
COMPARED_CONDITIONS = ("git SHA", "claude", "identity line", "models resolved", "effort", "max turns", "seed",
                       "tolerated attachments")


def conditions(records):
    """Per batch, the conditions its runs had, and which differ between
    batches: merged batches are compared as if they ran together. Only
    batches with agent runs are compared: a dry run has no claude version,
    model, effort or max turns, so it would differ from any agent batch."""
    batches = group(records, lambda r: r.get("batch") or NO_BATCH)
    rows = {batch: _batch_conditions(rs) for batch, rs in batches.items()}
    lines = [
        f"These results merge {len(rows)} batches. Each arm's numbers come from the batches that "
        "ran it, so arms from different batches are compared across these conditions:",
        "",
        *_header(("batch", *CONDITION_COLUMNS)),
        *(_row([batch, *(rows[batch][c] for c in CONDITION_COLUMNS)]) for batch in sorted(rows)),
    ]
    notes = []
    compared = sorted(b for b, rs in batches.items() if any(label(r) == "agent" for r in rs))
    scripted = sorted(b for b in rows if b not in compared)
    which = "The batches" if not scripted else f"The batches with agent runs ({', '.join(compared)})"
    if len(compared) < 2:
        notes.append(f"{'No batch has' if not compared else f'Only {compared[0]} has'} agent runs, so no "
                     "run conditions are compared between batches.")
    else:
        differ = [c for c in COMPARED_CONDITIONS if len({rows[b][c] for b in compared}) > 1]
        if differ:
            notes.append(f"{which} differ in {', '.join(differ)}: a difference between arms run in "
                         "different batches may come from these as well as from the arms.")
        else:
            notes.append(f"{which} agree on " + ", ".join(COMPARED_CONDITIONS) + ".")
    unrecorded = [b for b in compared if NOT_RECORDED in rows[b]["identity line"]]
    if unrecorded:
        notes.append(f"The identity line is {NOT_RECORDED} for some agent rows of {', '.join(unrecorded)} "
                     "(graded before the field existed, or without a transcript), so it cannot be compared "
                     "there; `run.py regrade` records it.")
    if scripted:
        notes.append(f"Left out of that comparison, as dry runs only (a scripted solver, no model, so no "
                     f"claude version, model, effort or max turns): {', '.join(scripted)}.")
    per_arm = group([r for r in records if label(r) == "agent"], lambda r: r.get("arm", "?"))
    for arm in sorted(per_arm, key=_arm_key):
        from_batches = sorted({r.get("batch") or NO_BATCH for r in per_arm[arm]})
        if len(from_batches) > 1:
            notes.append(f"arm {arm} has runs in {len(from_batches)} batches ({', '.join(from_batches)}), "
                         "pooled in every table")
    return lines + [""] + [f"- {n}" for n in notes]


def guard_section(records):
    flagged = [r for r in records if r.get("guard_problems")]
    if not flagged:
        return ["No run had a guard problem."]
    problems = Counter()
    for r in flagged:
        for p in r["guard_problems"] if isinstance(r["guard_problems"], list) else [r["guard_problems"]]:
            text = str(p)
            problems[text if len(text) <= 160 else text[:157] + "..."] += 1
    statuses = Counter(r.get("status") for r in flagged)
    lines = [
        f"{len(flagged)} run(s) had guard problems ("
        + ", ".join(f"{s} {n}" for s, n in sorted(statuses.items(), key=lambda kv: str(kv[0]))) + "):",
        "",
    ]
    return lines + [f"- {p} ({n}x)" for p, n in problems.most_common()]


def _sets(values):
    """`a, b (4); c (2)`: each distinct list and how many runs had it."""
    seen = Counter(tuple(v) for v in values)
    return "; ".join(
        f"{', '.join(k) or 'none'} ({n})" for k, n in sorted(seen.items(), key=lambda kv: (-kv[1], kv[0]))
    )


def _ever(context, field):
    """Every name in `field` that ever reached the agent: `<field>_added`
    (removals ignored, which is what the guard holds a run to) with the
    net list, for rows from before the added lists were recorded."""
    return sorted(set(context.get(f"{field}_added") or []) | set(context.get(field) or []))


def _tree_formats(context):
    """The tree formats a row's read_tree descriptions named (none for an
    arm without read_tree), or `not recorded` for a row from before
    `tree_formats` existed."""
    formats = context.get("tree_formats")
    return [str(f) for f in formats] if isinstance(formats, list) else ["not recorded"]


def _tolerated_cell(rows):
    """`remote_session_change 10/10`: per type the rows' plans tolerated, how
    many of the rows got one; `-` for none."""
    types = sorted({t for r in rows for t in tolerated_types(r)})
    return "; ".join(f"{t} {_carried(rows, t)[0]}/{len(rows)}" for t in types) or "-"


def context_section(records):
    """What each arm's agents were given, so the arms can be seen to match."""
    rows = []
    for key, rs in sorted(group([r for r in records if label(r) == "agent"],
                                lambda r: (r.get("arm", "?"), r.get("model") or "-")).items(),
                          key=lambda kv: _order(kv[0])):
        contexts = [r["context"] for r in rs if isinstance(r.get("context"), dict)]
        if not contexts:
            rows.append(_row([*key, f"0/{len(rs)}", "-", "-", "-", "-", "-", "-", "-", "-"]))
            continue
        types = Counter(t for c in contexts for t in set(c.get("attachment_types") or []))
        every = sorted(t for t, n in types.items() if n == len(contexts))
        some = sorted(f"{t} ({n})" for t, n in types.items() if n < len(contexts))
        rows.append(_row([
            *key, f"{len(contexts)}/{len(rs)}", ", ".join(every) or "-", ", ".join(some) or "-",
            _sets(_ever(c, "deferred_tools") for c in contexts),
            _sets(_ever(c, "mcp_instruction_servers") for c in contexts),
            _sets(_tree_formats(c) for c in contexts),
            f"{sum(1 for c in contexts if c.get('skill_listing'))}/{len(contexts)}",
            f"{sum(1 for c in contexts if c.get('claude_md_files'))}/{len(contexts)}",
            _tolerated_cell(rs),
        ]))
    if not rows:
        return ["No agent runs, so no agent context to show."]
    return [
        "Context the agents were given, from their transcripts (runs with a transcript / runs). "
        "Deferred tools and MCP instruction servers list every one that ever reached a run, even "
        "one later removed (what the guard checks), as each distinct set with its run count; "
        "the arms differ there by design (each arm's own tools), and should match elsewhere. "
        "Tree formats are the formats the read_tree descriptions each run was offered named, which "
        "the guard holds each bridge arm to (none for an arm without read_tree; `not recorded` "
        "for rows graded before the field existed). Tolerated attachments are those the batch's plan "
        "let through, with how many runs got one: every arm of a batch should get the same.",
        "",
        *_header(("arm", "model", "runs with context", "attachment types (every run)",
                  "attachment types (some runs)", "deferred tools (ever)", "MCP instruction servers (ever)",
                  "tree formats", "skill listing", "instruction files", "tolerated attachments (runs that got one)")),
        *rows,
    ]


# --- The whole report --------------------------------------------------------------------------


def render(records):
    agent = [r for r in records if label(r) == "agent"]
    scripted = [r for r in records if label(r) != "agent"]
    # A batch that tolerated an attachment is marked first, then under each
    # agent section's heading, and in every table row built from its runs.
    banner, marker = tolerance_banner(records), tolerance_marker(agent)
    out = [*banner, ""] if banner else []
    if len({r.get("batch") or NO_BATCH for r in records}) > 1:
        out += ["## Run conditions", "", *conditions(records), ""]
    out += ["## Agent runs (headline)", "", *marker]
    if agent:
        out.append(table(agent))
        out += ["", "## Arms per model, across all tasks", "", *marker] + arms_per_model(agent)
        out += ["", f"## {pairs_title(agent)}, per model", *(["", marker[0]] if marker else [])] + per_model(agent)
        out += ["", "## Model comparison", "", *marker] + model_comparison(agent)
    else:
        out.append("No agent runs in these results.")
    if scripted:
        out += [
            "",
            "## Dry runs (scripted solver, no model; not headline numbers)",
            "",
            "Tokens and turns do not exist for a scripted solver; wall time is the solver's, "
            "measured once its server is up.",
        ]
        for kind in ("dry-run", "dry-run (sabotage)"):
            subset = [r for r in scripted if label(r) == kind]
            if subset:
                out += ["", f"### {kind}", "", table(subset, agent=False)]
    out += ["", "## Provenance", ""] + provenance(records)
    out += ["", "### Guard", ""] + guard_section(agent)
    out += ["", "### Agent context", ""] + context_section(agent)
    return "\n".join(out)


# --- Selecting records ---------------------------------------------------------------------


def parse_args(argv):
    parser = argparse.ArgumentParser(description="Summarize taria benchmark results as Markdown.")
    parser.add_argument("files", nargs="*",
                        help="results files (default: bench/results/*/runs.jsonl and bench/results/*.jsonl)")
    parser.add_argument("--batch", action="append",
                        help="only this batch id (repeatable, or comma-separated); several are merged")
    parser.add_argument("--all", action="store_true", help="every batch found, merged")
    args = parser.parse_args(argv)
    if args.all and args.batch:
        parser.error("--all and --batch exclude each other")
    if args.batch:
        args.batch = list(dict.fromkeys(b.strip() for v in args.batch for b in v.split(",") if b.strip()))
        if not args.batch:
            parser.error("--batch needs a batch id")
    return args


def default_paths():
    return sorted(
        glob.glob(os.path.join(RESULTS_DIR, "*", "runs.jsonl"))
        + glob.glob(os.path.join(RESULTS_DIR, "*.jsonl"))
    )


def select(args):
    """(records, a note on what was selected) or raises SystemExit with a reason."""
    paths = args.files or default_paths()
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
        key=lambda b: max(str(r.get("ts", "")) for r, rb in zip(records, batch_of) if rb == b),
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
    unchecked = sum(1 for r in records if r.get("status") in GRADED and output_unchecked(r))
    if unchecked:
        print(f"summarize.py: warning: {unchecked} graded run(s) predate output_tokens_lower_bound and "
              "are not end_inferred, so their output tokens may be low (see the headline footer)",
              file=sys.stderr)
    print(render(records))
    return 0


if __name__ == "__main__":
    sys.exit(main())
