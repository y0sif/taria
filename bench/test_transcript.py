#!/usr/bin/env python3
"""Tests for bench/transcript.py and bench/summarize.py.

  python3 -m unittest discover -s bench -p 'test_transcript.py'

The fixtures are synthetic: bench/fixtures/transcripts/*.jsonl mirror the
shape of Claude Code subagent transcripts, bench/fixtures/summary_runs.jsonl
the rows bench/run.py writes. No real transcript is copied into them.
"""

import contextlib
import io
import json
import os
import shutil
import sys
import tempfile
import unittest

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)

import summarize  # noqa: E402
import transcript  # noqa: E402
from harness import ARM_TOOLS  # noqa: E402

FIXTURES = os.path.join(BENCH, "fixtures", "transcripts")
RUNS = os.path.join(BENCH, "fixtures", "summary_runs.jsonl")
CONTRACT = {
    "agent_id": (str,), "session_id": (str,), "models": (list,), "turns": (int,),
    "tool_calls": (dict,), "tool_calls_total": (int,), "toolsearch_calls": (int,),
    "tokens": (dict,), "wall_ms": (int, type(None)), "final_text": (str, type(None)),
    "ended_on_tool_use": (bool,), "tool_errors": (int,), "context": (dict,),
}
CONTEXT = {
    "claude_md_files": (list,), "deferred_tools": (list,), "mcp_instruction_servers": (list,),
    "environment_cwd": (str, type(None)), "skill_listing": (bool,), "attachment_types": (list,),
}


def fixture(name):
    return os.path.join(FIXTURES, name)


def read(name):
    return transcript.read_transcript(fixture(name))


class ReadTranscript(unittest.TestCase):
    def test_every_fixture_has_every_contract_key(self):
        names = sorted(f for f in os.listdir(FIXTURES) if f.endswith(".jsonl"))
        self.assertGreaterEqual(len(names), 6)
        for name in names:
            with self.subTest(fixture=name):
                stats = read(name)
                for key, types in CONTRACT.items():
                    self.assertIn(key, stats)
                    self.assertIsInstance(stats[key], types, key)
                for key, types in CONTEXT.items():
                    self.assertIn(key, stats["context"])
                    self.assertIsInstance(stats["context"][key], types, key)
                self.assertEqual(sorted(stats["tokens"]), ["cache_creation", "cache_read", "input", "output"])
                self.assertEqual(stats["models"], sorted(set(stats["models"])))
                self.assertEqual(stats["context"]["attachment_types"],
                                 sorted(set(stats["context"]["attachment_types"])))
                json.dumps(stats)  # a row field: it has to serialize

    def test_a_clean_taria_run(self):
        stats = read("clean_taria.jsonl")
        self.assertEqual(stats["agent_id"], "afixtureclean01")
        self.assertEqual(stats["session_id"], "00000000-0000-4000-8000-0000000000b1")
        self.assertEqual(stats["models"], ["claude-sonnet-5-5"])
        self.assertEqual(stats["turns"], 4)
        self.assertEqual(stats["tool_calls"], {"ToolSearch": 1, "mcp__tui__act": 1, "mcp__tui__read_tree": 1})
        self.assertEqual((stats["tool_calls_total"], stats["toolsearch_calls"]), (2, 1))
        self.assertEqual(stats["tokens"], {"input": 9, "cache_creation": 5800, "cache_read": 14100, "output": 385})
        self.assertEqual(stats["wall_ms"], 6180)
        self.assertEqual(stats["final_text"], "Done: 'Write docs' is marked done.")
        self.assertIs(stats["ended_on_tool_use"], False)
        self.assertEqual(stats["tool_errors"], 0)
        ctx = stats["context"]
        self.assertEqual(ctx["claude_md_files"], [])
        self.assertEqual(ctx["deferred_tools"], sorted("mcp__tui__" + t for t in ARM_TOOLS["taria"]))
        self.assertEqual(ctx["loaded_tools"], ["mcp__tui__act", "mcp__tui__read_tree"])
        self.assertEqual(ctx["mcp_instruction_servers"], ["tui"])
        self.assertEqual(ctx["environment_cwd"], "/tmp/bench-work")
        self.assertIs(ctx["skill_listing"], False)
        self.assertEqual(ctx["attachment_types"], ["date", "deferred_tools_delta", "deferred_tools_record",
                                                   "environment", "mcp_instructions_delta", "model"])
        self.assertEqual(stats["efforts"], ["medium"])
        self.assertEqual(stats["claude_versions"], ["2.1.294"])

    def test_streamed_usage_counts_each_message_once_at_its_final_count(self):
        stats = read("tmux_streaming.jsonl")
        # Five messages: final outputs 316 (after 6, 6), 90 (after 30, and a
        # line written twice), 70 (no stop_reason: the max), 25 (two stopped
        # lines), 40 (after 9).
        self.assertEqual(stats["turns"], 5)
        self.assertEqual(stats["tokens"], {"input": 12, "cache_creation": 3850, "cache_read": 14000, "output": 541})
        naive = {"input": 0, "output": 0}
        with open(fixture("tmux_streaming.jsonl")) as f:
            for line in f:
                d = json.loads(line)
                if d["type"] == "assistant":
                    naive["input"] += d["message"]["usage"]["input_tokens"]
                    naive["output"] += d["message"]["usage"]["output_tokens"]
        self.assertGreater(naive["output"], stats["tokens"]["output"], "the fixture streams nothing")
        self.assertGreater(naive["input"], stats["tokens"]["input"])
        # The tool_use written on two lines is one call.
        self.assertEqual(stats["tool_calls"], {"ToolSearch": 1, "mcp__tui__screen": 2, "mcp__tui__send_keys": 2})
        self.assertEqual(stats["tool_errors"], 1)
        self.assertEqual(stats["models"], ["claude-haiku-5-5"])
        self.assertEqual(stats["final_text"], "Deleted 'Old idea'.")

    def test_toolsearch_is_counted_apart_from_the_total(self):
        for name in ("clean_taria.jsonl", "tmux_streaming.jsonl", "cutoff.jsonl", "wrong_model.jsonl"):
            with self.subTest(fixture=name):
                stats = read(name)
                self.assertEqual(stats["toolsearch_calls"], 1)
                self.assertEqual(stats["tool_calls_total"], sum(stats["tool_calls"].values()) - 1)
                self.assertEqual(stats["tool_calls"]["ToolSearch"], 1)

    def test_a_run_cut_off_on_a_tool_use(self):
        stats = read("cutoff.jsonl")
        self.assertIs(stats["ended_on_tool_use"], True)
        self.assertEqual(stats["final_text"], "Reading the tree first.")  # earlier text survives
        self.assertEqual(stats["turns"], 3)
        for name in ("clean_taria.jsonl", "tmux_streaming.jsonl", "forbidden.jsonl", "wrong_model.jsonl",
                     "malformed.jsonl", "never_ran.jsonl"):
            self.assertIs(read(name)["ended_on_tool_use"], False, name)

    def test_ended_on_tool_use_looks_past_thinking_only(self):
        tool, text, think = {"type": "tool_use"}, {"type": "text", "text": "x"}, {"type": "thinking"}
        self.assertFalse(transcript.ended_on_tool_use([]))
        self.assertFalse(transcript.ended_on_tool_use([think]))
        self.assertFalse(transcript.ended_on_tool_use([tool, text]))
        self.assertFalse(transcript.ended_on_tool_use([tool, text, think]))
        self.assertTrue(transcript.ended_on_tool_use([text, tool]))
        self.assertTrue(transcript.ended_on_tool_use([tool, think]))

    def test_context_follows_the_deltas(self):
        ctx = read("forbidden.jsonl")["context"]
        self.assertEqual(ctx["claude_md_files"], ["/home/someone/project/CLAUDE.md"])
        # NotebookEdit was added, then removed; the other server likewise.
        self.assertEqual(ctx["deferred_tools"], ["WebFetch", "mcp__other__lookup", "mcp__tui__act",
                                                 "mcp__tui__key", "mcp__tui__read_tree", "mcp__tui__type_text"])
        self.assertEqual(ctx["mcp_instruction_servers"], ["tui"])
        self.assertIs(ctx["skill_listing"], True)
        self.assertIn("instructions", ctx["attachment_types"])

    def test_malformed_and_unknown_lines_are_skipped(self):
        stats = read("malformed.jsonl")
        self.assertEqual(stats["malformed_lines"], 3)
        self.assertEqual(stats["synthetic_messages"], 1)
        self.assertEqual(stats["models"], ["claude-sonnet-5-5"], "<synthetic> is not a model")
        self.assertEqual(stats["turns"], 2)
        self.assertEqual(stats["tool_calls"], {"mcp__tui__act": 1, "mcp__tui__read_tree": 1})
        self.assertEqual(stats["tokens"]["output"], 57)
        self.assertEqual(stats["final_text"], "All set.")
        self.assertEqual(stats["context"]["deferred_tools"], sorted("mcp__tui__" + t for t in ARM_TOOLS["taria"]))
        self.assertNotIn("progress", stats["context"]["attachment_types"])

    def test_an_agent_that_never_ran(self):
        stats = read("never_ran.jsonl")
        self.assertEqual((stats["turns"], stats["models"], stats["final_text"]), (0, [], None))
        self.assertEqual(stats["tool_calls"], {})
        self.assertEqual(stats["tokens"], {"input": 0, "cache_creation": 0, "cache_read": 0, "output": 0})

    def test_an_empty_file(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "agent-aempty.jsonl")
        with open(path, "w") as f:
            f.write("\n{not json\n")
        stats = transcript.read_transcript(path)
        self.assertEqual(stats["agent_id"], "aempty")  # from the file name
        self.assertIsNone(stats["session_id"])
        self.assertIsNone(stats["wall_ms"])
        self.assertEqual((stats["turns"], stats["malformed_lines"]), (0, 1))

    def test_final_text_is_truncated(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "agent-along.jsonl")
        line = {"type": "assistant", "timestamp": "2026-10-11T12:00:00.000Z",
                "message": {"id": "m1", "model": "claude-sonnet-5-5", "stop_reason": "end_turn",
                            "content": [{"type": "text", "text": "x" * 900}], "usage": {"output_tokens": 3}}}
        with open(path, "w") as f:
            f.write(json.dumps(line) + "\n")
        stats = transcript.read_transcript(path)
        self.assertEqual(len(stats["final_text"]), transcript.FINAL_TEXT_MAX)
        self.assertEqual(stats["tokens"]["output"], 3)


class Guard(unittest.TestCase):
    def guard(self, name, arm, family):
        return transcript.guard(read(name), arm, family, ARM_TOOLS[arm])

    def test_clean_runs_pass(self):
        self.assertEqual(self.guard("clean_taria.jsonl", "taria", "sonnet"), [])
        self.assertEqual(self.guard("tmux_streaming.jsonl", "tmux", "haiku"), [])
        self.assertEqual(self.guard("cutoff.jsonl", "taria", "sonnet"), [])
        self.assertEqual(self.guard("malformed.jsonl", "taria", "sonnet"), [])

    def test_a_forbidden_tool_a_claude_md_and_extra_deferred_tools(self):
        problems = self.guard("forbidden.jsonl", "taria", "sonnet")
        self.assertEqual(len(problems), 3, problems)
        called, files, offered = problems
        self.assertIn("called Read (1x)", called)
        self.assertIn("/home/someone/project/CLAUDE.md", files)
        self.assertIn("2 deferred tool(s) beyond the taria arm: WebFetch, mcp__other__lookup", offered)

    def test_the_wrong_model(self):
        problems = self.guard("wrong_model.jsonl", "tmux", "haiku")
        self.assertEqual(problems, ["ran on claude-sonnet-5-5, not the requested claude-haiku-*"])
        self.assertEqual(self.guard("wrong_model.jsonl", "tmux", "sonnet"), [])

    def test_the_other_arms_tools(self):
        problems = self.guard("clean_taria.jsonl", "tmux", "sonnet")
        self.assertTrue(any(p.startswith("called mcp__tui__act (1x)") for p in problems), problems)
        self.assertTrue(any(p.startswith("called mcp__tui__read_tree (1x)") for p in problems), problems)
        self.assertTrue(any("4 deferred tool(s) beyond the tmux arm" in p for p in problems), problems)
        self.assertFalse(any("ToolSearch" in p for p in problems), "ToolSearch is always allowed")

    def test_an_agent_that_never_ran(self):
        problems = self.guard("never_ran.jsonl", "taria", "sonnet")
        self.assertEqual(problems, ["never ran: the transcript has no assistant turn"])

    def test_guard_tolerates_a_sparse_stats_dict(self):
        self.assertEqual(transcript.guard({}, "taria", "sonnet", ARM_TOOLS["taria"]),
                         ["never ran: the transcript has no assistant turn"])


class FindTranscript(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp(suffix="[glob]*")  # glob characters in the path itself
        self.addCleanup(shutil.rmtree, self.root)

    def place(self, project, session, agent_id):
        directory = os.path.join(self.root, project, session, "subagents")
        os.makedirs(directory, exist_ok=True)
        path = os.path.join(directory, f"agent-{agent_id}.jsonl")
        with open(path, "w") as f:
            f.write("{}\n")
        return path

    def test_none_one_and_two_matches(self):
        self.assertIsNone(transcript.find_transcript("a123", self.root))
        path = self.place("-proj", "sess-1", "a123")
        self.place("-proj", "sess-1", "a1234")  # a longer id is another agent
        self.assertEqual(transcript.find_transcript("a123", self.root), path)
        self.place("-other", "sess-2", "a123")
        with self.assertRaises(ValueError):
            transcript.find_transcript("a123", self.root)

    def test_only_the_subagents_layout_matches(self):
        os.makedirs(os.path.join(self.root, "-proj", "sess-1"))
        with open(os.path.join(self.root, "-proj", "sess-1", "agent-a9.jsonl"), "w") as f:
            f.write("{}\n")
        self.assertIsNone(transcript.find_transcript("a9", self.root))

    def test_bad_ids_are_refused(self):
        for bad in ("", "../a123", "a/b", "a*", "a?", "[a]", "a.b", " a", None, 7):
            with self.subTest(agent_id=bad), self.assertRaises(ValueError):
                transcript.find_transcript(bad, self.root)


class Summary(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.records = summarize.load([RUNS])
        cls.out = summarize.render(cls.records)

    @staticmethod
    def columns(line):
        return dict(zip(summarize.COLUMNS, (c.strip() for c in line.strip().strip("|").split("|"))))

    def headline_row(self, task, arm, model):
        headline = self.out.partition("## taria vs tmux")[0]
        line = next(l for l in headline.splitlines() if l.startswith(f"| {task} | {arm} | {model} |"))
        return self.columns(line)

    def test_the_fixture_covers_both_modes_models_and_every_status(self):
        self.assertEqual({r["mode"] for r in self.records}, {"subagent", "dry-run"})
        self.assertEqual({r["model"] for r in self.records if r["model"]}, {"sonnet", "haiku"})
        self.assertEqual({r["status"] for r in self.records}, {"passed", "failed", "max_turns", "harness_error"})

    def test_headline_rows(self):
        col = self.headline_row("T1", "taria", "sonnet")
        self.assertEqual((col["effort"], col["max turns"], col["n"], col["passed"]), ("medium", "30", "5", "4/5"))
        self.assertEqual(col["pass rate (95% CI)"], "80% (38%-96%)")
        self.assertEqual(col["cut off"], "1 (1 with correct state)")
        self.assertEqual(col["tool calls med [IQR]"], "4 [3-4]")
        self.assertEqual(col["ToolSearch med"], "1")
        self.assertEqual(col["turns med"], "6")
        self.assertEqual(col["tokens processed med [IQR]"], "22,000 [21,000-23,000]")
        self.assertEqual(col["output tok med"], "400")
        self.assertEqual(col["wall s med"], "14.0")
        col = self.headline_row("T1", "tmux", "sonnet")
        self.assertEqual((col["n"], col["passed"], col["harness err"], col["cut off"]), ("4", "1/4", "1", "0"))
        # The demo died in the haiku tmux cut-off: not the right state.
        col = self.headline_row("T1", "tmux", "haiku")
        self.assertEqual((col["n"], col["harness err"], col["cut off"]), ("3", "1", "1 (0 with correct state)"))

    def test_no_cost_budget_or_timeout_left(self):
        for gone in ("cost", "budget", "timeout", "$"):
            self.assertFalse(any(gone in c for c in summarize.COLUMNS), gone)
            self.assertNotIn(gone, self.out.lower() if gone != "$" else self.out)

    def test_the_arm_comparison_per_model(self):
        sonnet = self.out.partition("### sonnet")[2].partition("## Model comparison")[0]
        p = summarize.fisher_exact(4, 1, 1, 3)
        self.assertIn(f"| T1 | medium | 30 | 4/5 | 1/4 | +55 pp | {p:.3f} | 2.14x | 2.50x |", sonnet)
        self.assertIn("| T2 | medium | 30 | 3/3 | 2/3 | +33 pp | 1.000 | 2.00x | 3.00x |", sonnet)
        self.assertIn("| taria | medium | 30 | 2 | 8 | 7/8 | 88% (53%-98%) | 20,500 | 3 | 164,000 |", sonnet)
        self.assertIn("| tmux | medium | 30 | 2 | 7 | 3/7 | 43% (16%-75%) | 44,000 | 9 | 326,000 |", sonnet)
        haiku = self.out.partition("### haiku")[2].partition("### sonnet")[0]
        self.assertIn(f"| T1 | medium | 30 | 2/3 | 0/3 | +67 pp | {summarize.fisher_exact(2, 1, 0, 3):.3f} |", haiku)

    def test_the_model_comparison(self):
        section = self.out.partition("## Model comparison")[2].partition("## Dry runs")[0]
        self.assertIn("haiku passed | sonnet passed | Fisher p", section)
        p = summarize.fisher_exact(2, 1, 7, 1)
        self.assertIn(f"| taria | medium | 30 | 2/3 67% (21%-94%) | 7/8 88% (53%-98%) | {p:.3f} | 27,000 | 20,500 |",
                      section)
        one = [r for r in self.records if r.get("model") == "sonnet"]
        self.assertIn("Only one model", "\n".join(summarize.model_comparison(one)))

    def test_dry_runs_stay_out_of_the_headline(self):
        headline, _, rest = self.out.partition("## Dry runs")
        self.assertNotIn("| - | - | - |", headline)
        self.assertIn("### dry-run (sabotage)", rest)
        self.assertIn("| T1 | taria | - | - | - | 2 | 2/2 |", rest)

    def test_footer_notes(self):
        self.assertIn("2 harness-error run(s) are counted in `harness err`", self.out)
        self.assertIn("processed 8,000 tokens", self.out)
        self.assertIn("wall time is indicative only", self.out)
        self.assertIn("hit max_turns", self.out)
        self.assertIn("n < 5 graded runs in T1/taria [haiku, medium, 30 turns]", self.out)

    def test_provenance_guard_and_context(self):
        prov = self.out.partition("## Provenance")[2]
        self.assertIn("| fixture-b1 | agent, dry-run, dry-run (sabotage) | 27 | abc123def456 | False | 2.1.294 | "
                      "haiku, sonnet | claude-haiku-5-5, claude-sonnet-5-5 | medium | 30 | 42 |", prov)
        self.assertIn("asked for haiku, the agents ran on claude-sonnet-5-5 in 1 run(s)", prov)
        self.assertIn("2 run(s) had guard problems (harness_error 2)", prov)
        self.assertIn("- called Bash (1x), which is not a tmux arm tool (1x)", prov)
        ctx = prov.partition("### Agent context")[2]
        taria = next(l for l in ctx.splitlines() if l.startswith("| taria | sonnet |"))
        tmux = next(l for l in ctx.splitlines() if l.startswith("| tmux | sonnet |"))
        self.assertIn("| 8/8 |", taria)
        self.assertIn("mcp_instructions_delta", taria)
        self.assertIn("deferred_tools_record (1)", taria)
        self.assertIn("tui (8)", taria)
        self.assertIn("| 7/8 |", tmux)  # the harness error has no transcript
        self.assertNotIn("mcp_instructions_delta", tmux)
        self.assertIn("mcp__tui__screen, mcp__tui__send_keys (7)", tmux)
        self.assertIn("none (7)", tmux)

    def test_wilson_and_fisher(self):
        low, high = summarize.wilson(1, 1)
        self.assertAlmostEqual(low, 0.2065, places=3)
        self.assertEqual(high, 1.0)
        low, high = summarize.wilson(5, 10)
        self.assertAlmostEqual(low, 0.2366, places=3)
        self.assertAlmostEqual(high, 0.7634, places=3)
        self.assertEqual(summarize.wilson(0, 0), (0.0, 1.0))
        self.assertAlmostEqual(summarize.fisher_exact(3, 0, 0, 3), 0.1)
        self.assertAlmostEqual(summarize.fisher_exact(4, 0, 0, 4), 2 / 70)
        self.assertAlmostEqual(summarize.fisher_exact(0, 0, 0, 0), 1.0)
        self.assertAlmostEqual(summarize.fisher_exact(7, 3, 2, 8), summarize.fisher_exact(2, 8, 7, 3))
        self.assertAlmostEqual(summarize.fisher_exact(4, 1, 1, 3), 0.2063, places=4)

    def test_empty_input(self):
        out = summarize.render([])
        self.assertIn("No agent runs in these results.", out)
        self.assertIn("## Provenance", out)
        self.assertIn("No agent runs, so no agent context to show.", out)
        summarize.table([])

    def test_all_harness_errors(self):
        errors = [dict(r, status="harness_error", passed=None) for r in self.records if r["mode"] == "subagent"]
        out = summarize.render(errors)
        col = self.headline_row_in(out, "T1", "taria", "sonnet")
        self.assertEqual((col["n"], col["passed"], col["pass rate (95% CI)"]), ("0", "0/0", "-"))
        self.assertEqual((col["harness err"], col["tool calls med [IQR]"], col["tokens processed med [IQR]"]),
                         ("5", "-", "-"))
        self.assertIn("| T1 | medium | 30 | 0/0 | 0/0 | - | - | - | - |", out)
        self.assertIn("23 harness-error run(s)", out)

    def headline_row_in(self, out, task, arm, model):
        line = next(l for l in out.splitlines() if l.startswith(f"| {task} | {arm} | {model} |"))
        return self.columns(line)

    def test_an_unknown_status_is_said_and_not_graded(self):
        base = next(r for r in self.records if r["mode"] == "subagent" and r["status"] == "passed")
        out = summarize.table([base, dict(base, status="timeout", passed=False)])
        col = self.headline_row_in(out, base["task"], base["arm"], base["model"])
        self.assertEqual((col["n"], col["passed"]), ("1", "1/1"))
        self.assertIn("1 row(s) with unknown status ['timeout']", out)

    def test_rows_split_by_effort_and_max_turns(self):
        base = next(r for r in self.records if r["mode"] == "subagent" and r["status"] == "passed")
        out = summarize.table([base, dict(base, effort="high"), dict(base, max_turns=10)])
        prefix = f"| {base['task']} | {base['arm']} | {base['model']} |"
        self.assertEqual(sum(1 for l in out.splitlines() if l.startswith(prefix)), 3)

    def test_records_are_selected_explicitly(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        base = next(r for r in self.records if r["mode"] == "subagent")
        for batch, ts in (("old", "2026-01-01T00:00:00+00:00"), ("new", "2026-02-01T00:00:00+00:00")):
            os.makedirs(os.path.join(tmp, batch))
            with open(os.path.join(tmp, batch, "runs.jsonl"), "w") as f:
                f.write(json.dumps(dict(base, batch=batch, ts=ts)) + "\n")
        with open(os.path.join(tmp, "flat.jsonl"), "w") as f:  # the older layout
            f.write(json.dumps(dict(base, batch="flat", ts="2025-01-01T00:00:00+00:00")) + "\n")
        original = summarize.RESULTS_DIR
        summarize.RESULTS_DIR = tmp
        self.addCleanup(setattr, summarize, "RESULTS_DIR", original)
        pick = lambda *argv: summarize.select(summarize.parse_args(list(argv)))  # noqa: E731
        records, note = pick()
        self.assertEqual([r["batch"] for r in records], ["new"])
        self.assertIn("2 other batch(es) left out", note)
        self.assertEqual([r["batch"] for r in pick("--batch", "old")[0]], ["old"])
        self.assertEqual(sorted(r["batch"] for r in pick("--all")[0]), ["flat", "new", "old"])
        with self.assertRaises(SystemExit):
            pick("--batch", "missing")

    def test_main_prints_markdown(self):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = summarize.main([RUNS])
        self.assertEqual(code, 0)
        self.assertTrue(out.getvalue().startswith("## Agent runs (headline)"))
        self.assertIn("27 record(s)", err.getvalue())


class RealTranscriptShape(unittest.TestCase):
    """The reader agrees with what read_transcript is fed by run.py: a row
    built from a fixture's stats feeds the summary without loss."""

    def test_stats_feed_a_summary_row(self):
        stats = read("clean_taria.jsonl")
        row = {"task": "T1", "arm": "taria", "mode": "subagent", "model": "sonnet", "effort": "medium",
               "max_turns": 30, "batch": "b", "status": "passed", "passed": True,
               **{k: stats[k] for k in ("turns", "tool_calls", "tool_calls_total", "toolsearch_calls",
                                        "tokens", "wall_ms", "context")}}
        col = Summary.columns(next(l for l in summarize.table([row]).splitlines() if l.startswith("| T1 |")))
        self.assertEqual(col["tokens processed med [IQR]"], "19,909")
        self.assertEqual((col["tool calls med [IQR]"], col["ToolSearch med"], col["turns med"]), ("2", "1", "4"))


if __name__ == "__main__":
    unittest.main()
