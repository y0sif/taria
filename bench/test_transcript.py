#!/usr/bin/env python3
"""Tests for bench/transcript.py and bench/summarize.py.

  python3 -m unittest discover -s bench -p 'test_transcript.py'

Two kinds of fixture in bench/fixtures/transcripts/:

- real_*.jsonl: real subagent transcripts of this exact setup (tui-<arm>-<K>
  agents, Claude Code 2.1.294), scrubbed by bench/fixtures/scrub_transcript.py.
  Messages, models, usage, stop_reasons, effort, tool calls, results and
  timestamps are as Claude Code wrote them.
    real_taria_clean   a taria run (sonnet, T3). Every real taria run so far
                       came from a session whose other MCP servers leaked
                       their instructions in, so this one's
                       mcp_instructions_delta addedNames was edited to
                       ["tui"] (the attachment's `_fixture_note` says so);
                       nothing else was changed.
    real_tmux_clean    a tmux run (haiku, T1), clean as recorded.
    real_taria_leaked  a taria run (haiku, T1) as recorded, leaked servers
                       and all: the guard must refuse it.
    real_taria_unclosed  a taria run (haiku, T4, job j108 of batch full1),
                       clean as recorded, whose final text reply Claude Code
                       never closed: its lines carry stop_reason null and a
                       placeholder output_tokens of 1. It ended on its own.
  The guard and ending tests use these, and variants made from them in the
  test (a line dropped, a field changed) to stand for runs not yet seen.
- the others are synthetic and test parsing edge cases: streamed usage
  (tmux_streaming), deltas and removals (forbidden), malformed lines
  (malformed), an agent with no assistant turn (never_ran).

bench/fixtures/summary_runs.jsonl holds synthetic rows as bench/run.py
writes them.
"""

import contextlib
import copy
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
from tasks import TASKS  # noqa: E402

FIXTURES = os.path.join(BENCH, "fixtures", "transcripts")
RUNS = os.path.join(BENCH, "fixtures", "summary_runs.jsonl")
TARIA_CLEAN, TMUX_CLEAN, TARIA_LEAKED = "real_taria_clean.jsonl", "real_tmux_clean.jsonl", "real_taria_leaked.jsonl"
TARIA_UNCLOSED = "real_taria_unclosed.jsonl"
CONTRACT = {
    "agent_id": (str,), "session_id": (str,), "models": (list,), "turns": (int,),
    "tool_calls": (dict,), "tool_calls_total": (int,), "toolsearch_calls": (int,),
    "tokens": (dict,), "wall_ms": (int, type(None)), "final_text": (str, type(None)),
    "ended_on_tool_use": (bool,), "tool_errors": (int,), "context": (dict,),
    "last_model": (str, type(None)), "last_stop_reason": (str, type(None)), "ended_synthetic": (bool,),
    "last_block_type": (str, type(None)), "end_inferred": (bool,), "output_tokens_lower_bound": (bool,),
    "first_prompt": (str, type(None)), "started": (str, type(None)),
}
CONTEXT = {
    "offered_tools": (list,), "offered_tools_snapshots": (list,),
    "claude_md_files": (list,), "deferred_tools": (list,), "mcp_instruction_servers": (list,),
    "deferred_tools_added": (list,), "mcp_instruction_servers_added": (list,),
    "environment_cwd": (str, type(None)), "skill_listing": (bool,), "attachment_types": (list,),
}
TUI = {arm: sorted("mcp__tui__" + t for t in tools) for arm, tools in ARM_TOOLS.items()}


def fixture(name):
    return os.path.join(FIXTURES, name)


def read(name):
    return transcript.read_transcript(fixture(name))


def lines_of(name):
    with open(fixture(name)) as f:
        return [json.loads(line) for line in f if line.strip()]


def is_att(line, kind):
    return line.get("type") == "attachment" and line.get("attachment", {}).get("type") == kind


def assistant_ids(lines):
    """Message ids of the real assistant messages, in order."""
    seen = []
    for line in lines:
        if line.get("type") == "assistant" and line["message"]["id"] not in seen:
            seen.append(line["message"]["id"])
    return seen


def cut_after_message(lines, n):
    """The lines up to and including the tool results of the n-th assistant
    message (1-based): a run that stopped after its n-th turn's tools ran."""
    ids = assistant_ids(lines)
    keep, last_index = ids[:n], 0
    for i, line in enumerate(lines):
        if line.get("type") == "assistant" and line["message"]["id"] in keep:
            last_index = i
    end = last_index + 1
    while end < len(lines) and lines[end].get("type") != "assistant":
        end += 1
    return lines[:end]


def synthetic_line(template, text, error):
    """A trailing Claude Code error message, in the shape real transcripts
    hold (model <synthetic>, stop_sequence, isApiErrorMessage, error)."""
    line = copy.deepcopy(template)
    line.pop("apiBlockIndex", None)
    line.pop("requestId", None)
    line.update(isApiErrorMessage=True, error=error, timestamp="2026-10-11T05:30:00.000Z")
    line["message"] = {"id": "msg_synthetic", "model": transcript.SYNTHETIC_MODEL, "role": "assistant",
                       "type": "message", "stop_reason": "stop_sequence", "stop_sequence": "",
                       "content": [{"type": "text", "text": text}],
                       "usage": {"input_tokens": 0, "output_tokens": 0}}
    return line


class Variant:
    """Write fixture variants to a temp dir and read them back."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)

    def write(self, lines, name="agent-avariant.jsonl"):
        path = os.path.join(self.tmp, name)
        with open(path, "w") as f:
            f.writelines(json.dumps(line) + "\n" for line in lines)
        return path

    def variant(self, lines):
        return transcript.read_transcript(self.write(lines))


class ReadTranscript(Variant, unittest.TestCase):
    def test_every_fixture_has_every_contract_key(self):
        names = sorted(f for f in os.listdir(FIXTURES) if f.endswith(".jsonl"))
        self.assertGreaterEqual(len(names), 8)
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

    def test_a_real_taria_run(self):
        stats = read(TARIA_CLEAN)
        self.assertEqual(stats["agent_id"], "afixturetariaclean")
        self.assertEqual(stats["session_id"], "00000000-0000-4000-8000-0000000000c1")
        self.assertEqual(stats["models"], ["claude-sonnet-5-5"])
        self.assertEqual(stats["turns"], 4)
        self.assertEqual(stats["tool_calls"], {"mcp__tui__act": 2, "mcp__tui__read_tree": 1})
        self.assertEqual((stats["tool_calls_total"], stats["toolsearch_calls"]), (3, 0))
        self.assertEqual(stats["tokens"], {"input": 8, "cache_creation": 7281, "cache_read": 16036, "output": 248})
        self.assertEqual(stats["wall_ms"], 6497)
        self.assertTrue(stats["final_text"].startswith('I deleted the task "Wire up the MCP bridge"'))
        self.assertIs(stats["ended_on_tool_use"], False)
        self.assertEqual(stats["tool_errors"], 0)
        self.assertEqual((stats["last_model"], stats["last_stop_reason"], stats["ended_synthetic"]),
                         ("claude-sonnet-5-5", "end_turn", False))
        self.assertIsNone(stats["max_turns_reached"])
        self.assertEqual(stats["first_prompt"], TASKS["T3"].prompt)
        self.assertEqual(stats["started"], "2026-10-11T05:20:26.399Z")
        ctx = stats["context"]
        # Two snapshots: the first written before any tool was known.
        self.assertEqual(ctx["offered_tools_snapshots"], [None, TUI["taria"]])
        self.assertEqual(ctx["offered_tools"], TUI["taria"])
        self.assertEqual(ctx["deferred_tools"], [])
        self.assertEqual(ctx["claude_md_files"], [])
        self.assertEqual(ctx["mcp_instruction_servers"], ["tui"])
        self.assertEqual(ctx["environment_cwd"], "/home/user/project")
        self.assertIs(ctx["skill_listing"], False)
        self.assertEqual(ctx["attachment_types"], ["credential_org", "environment", "mcp_instructions_delta",
                                                   "prompt_snapshot", "session_context"])
        self.assertEqual(stats["efforts"], ["medium"])
        self.assertEqual(stats["claude_versions"], ["2.1.294"])

    def test_a_real_tmux_run(self):
        stats = read(TMUX_CLEAN)
        self.assertEqual((stats["models"], stats["turns"]), (["claude-haiku-5-5"], 10))
        self.assertEqual(stats["tool_calls"], {"mcp__tui__screen": 2, "mcp__tui__send_keys": 9})
        self.assertEqual(stats["context"]["offered_tools_snapshots"], [None, TUI["tmux"]])
        self.assertEqual(stats["context"]["mcp_instruction_servers"], ["tui"])
        self.assertEqual(stats["first_prompt"], TASKS["T1"].prompt)
        self.assertEqual((stats["last_stop_reason"], stats["ended_synthetic"]), ("end_turn", False))
        self.assertEqual(stats["tokens"]["output"], 2169)

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

    def test_a_real_message_streamed_over_lines_counts_once(self):
        # The leaked run's second and third messages each stream over two
        # lines (thinking then tool_use, text then tool_use).
        lines = lines_of(TARIA_LEAKED)
        self.assertEqual(sum(1 for l in lines if l["type"] == "assistant"), 6)
        stats = read(TARIA_LEAKED)
        self.assertEqual(stats["turns"], 4)
        self.assertEqual(stats["tool_calls"], {"mcp__tui__act": 1, "mcp__tui__key": 1, "mcp__tui__read_tree": 1})

    def test_toolsearch_is_counted_apart_from_the_total(self):
        stats = read("tmux_streaming.jsonl")
        self.assertEqual((stats["toolsearch_calls"], stats["tool_calls"]["ToolSearch"]), (1, 1))
        self.assertEqual(stats["tool_calls_total"], sum(stats["tool_calls"].values()) - 1)
        for name in (TARIA_CLEAN, TMUX_CLEAN, TARIA_LEAKED):  # real runs load nothing
            with self.subTest(fixture=name):
                stats = read(name)
                self.assertEqual(stats["toolsearch_calls"], 0)
                self.assertEqual(stats["tool_calls_total"], sum(stats["tool_calls"].values()))

    def test_a_run_cut_off_on_a_tool_use(self):
        # The leaked run, stopped after its third turn's tool ran: it said
        # something, then asked for a key.
        stats = self.variant(cut_after_message(lines_of(TARIA_LEAKED), 3))
        self.assertIs(stats["ended_on_tool_use"], True)
        self.assertEqual(stats["last_stop_reason"], "tool_use")
        self.assertEqual(stats["final_text"], "The text is in the input and it is focused. Submitting with Enter now.")
        self.assertEqual(stats["turns"], 3)
        for name in (TARIA_CLEAN, TMUX_CLEAN, TARIA_LEAKED, "tmux_streaming.jsonl", "forbidden.jsonl",
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

    def test_a_trailing_synthetic_message_is_recorded(self):
        lines = cut_after_message(lines_of(TARIA_CLEAN), 3)
        lines.append(synthetic_line(lines[-2], "You've hit your session limit", "rate_limit"))
        stats = self.variant(lines)
        self.assertEqual((stats["last_model"], stats["last_stop_reason"]), ("<synthetic>", "stop_sequence"))
        self.assertIs(stats["ended_synthetic"], True)
        self.assertEqual((stats["synthetic_text"], stats["synthetic_error"]),
                         ("You've hit your session limit", "rate_limit"))
        self.assertEqual((stats["synthetic_messages"], stats["turns"]), (1, 3))
        self.assertEqual(stats["models"], ["claude-sonnet-5-5"], "<synthetic> is not a model")

    def test_a_synthetic_message_the_run_recovered_from_is_not_its_end(self):
        lines = lines_of(TARIA_CLEAN)
        cut = len(cut_after_message(lines, 2))
        lines.insert(cut, synthetic_line(lines[cut - 2], "Request timed out", "server_error"))
        stats = self.variant(lines)
        self.assertEqual((stats["synthetic_messages"], stats["ended_synthetic"]), (1, False))
        self.assertEqual(stats["last_stop_reason"], "end_turn")

    def test_snapshots_are_unioned_and_each_kept(self):
        lines = lines_of(TARIA_CLEAN)
        later = copy.deepcopy(next(l for l in lines if is_att(l, "prompt_snapshot") and "tools" in l["attachment"]))
        later["attachment"]["tools"] = [{"name": "Bash", "description": "x"}]
        lines.append(later)
        ctx = self.variant(lines)["context"]
        self.assertEqual(ctx["offered_tools_snapshots"], [None, TUI["taria"], ["Bash"]])
        self.assertEqual(ctx["offered_tools"], sorted(TUI["taria"] + ["Bash"]))

    def test_context_follows_the_deltas(self):
        ctx = read("forbidden.jsonl")["context"]
        self.assertEqual(ctx["claude_md_files"], ["/home/someone/project/CLAUDE.md"])
        # NotebookEdit was added, then removed; the other server likewise.
        self.assertEqual(ctx["deferred_tools"], ["WebFetch", "mcp__other__lookup", "mcp__tui__act",
                                                 "mcp__tui__key", "mcp__tui__read_tree", "mcp__tui__type_text"])
        self.assertEqual(ctx["mcp_instruction_servers"], ["tui"])
        # Every name ever added, removals ignored.
        self.assertEqual(ctx["deferred_tools_added"], ["NotebookEdit", "WebFetch", "mcp__other__lookup",
                                                       "mcp__tui__act", "mcp__tui__key", "mcp__tui__read_tree",
                                                       "mcp__tui__type_text"])
        self.assertEqual(ctx["mcp_instruction_servers_added"], ["other", "tui"])
        self.assertIs(ctx["skill_listing"], True)
        self.assertIn("instructions", ctx["attachment_types"])
        self.assertEqual((ctx["offered_tools"], ctx["offered_tools_snapshots"]), ([], []))

    def test_malformed_and_unknown_lines_are_skipped(self):
        stats = read("malformed.jsonl")
        self.assertEqual(stats["malformed_lines"], 3)
        self.assertEqual(stats["synthetic_messages"], 1)
        self.assertEqual(stats["models"], ["claude-sonnet-5-5"], "<synthetic> is not a model")
        self.assertEqual(stats["turns"], 2)
        self.assertEqual(stats["tool_calls"], {"mcp__tui__act": 1, "mcp__tui__read_tree": 1})
        self.assertEqual(stats["tokens"]["output"], 57)
        self.assertEqual(stats["final_text"], "All set.")
        self.assertEqual(stats["context"]["deferred_tools"], TUI["taria"])
        self.assertNotIn("progress", stats["context"]["attachment_types"])

    def test_an_agent_that_never_ran(self):
        stats = read("never_ran.jsonl")
        self.assertEqual((stats["turns"], stats["models"], stats["final_text"]), (0, [], None))
        self.assertEqual(stats["tool_calls"], {})
        self.assertEqual(stats["tokens"], {"input": 0, "cache_creation": 0, "cache_read": 0, "output": 0})
        self.assertEqual((stats["last_model"], stats["last_stop_reason"]), (None, None))

    def test_an_empty_file(self):
        path = os.path.join(self.tmp, "agent-aempty.jsonl")
        with open(path, "w") as f:
            f.write("\n{not json\n")
        stats = transcript.read_transcript(path)
        self.assertEqual(stats["agent_id"], "aempty")  # from the file name
        self.assertIsNone(stats["session_id"])
        self.assertIsNone(stats["wall_ms"])
        self.assertEqual((stats["turns"], stats["malformed_lines"]), (0, 1))
        self.assertEqual((stats["first_prompt"], stats["started"]), (None, None))

    def test_final_text_is_truncated(self):
        path = os.path.join(self.tmp, "agent-along.jsonl")
        line = {"type": "assistant", "timestamp": "2026-10-11T12:00:00.000Z",
                "message": {"id": "m1", "model": "claude-sonnet-5-5", "stop_reason": "end_turn",
                            "content": [{"type": "text", "text": "x" * 900}], "usage": {"output_tokens": 3}}}
        with open(path, "w") as f:
            f.write(json.dumps(line) + "\n")
        stats = transcript.read_transcript(path)
        self.assertEqual(len(stats["final_text"]), transcript.FINAL_TEXT_MAX)
        self.assertEqual(stats["tokens"]["output"], 3)


class Guard(Variant, unittest.TestCase):
    """The guard over real runs, and variants of them."""

    def guard(self, stats, arm, family, effort="medium"):
        return transcript.guard(stats, arm, family, ARM_TOOLS[arm], effort=effort)

    def test_clean_real_runs_pass(self):
        self.assertEqual(self.guard(read(TARIA_CLEAN), "taria", "sonnet"), [])
        self.assertEqual(self.guard(read(TMUX_CLEAN), "tmux", "haiku"), [])

    def test_other_servers_instructions_are_named(self):
        problems = self.guard(read(TARIA_LEAKED), "taria", "haiku")
        self.assertEqual(problems, ["was given the instructions of 4 MCP server(s) besides tui: furcule, "
                                    "plugin:context7:context7, plugin:posthog:posthog, taria"])

    def test_a_server_that_left_still_counts(self):
        """Removals do not excuse a server: its instructions reached the
        agent, whether or not it later disconnected."""
        lines = lines_of(TARIA_LEAKED)
        delta = copy.deepcopy(next(l for l in lines if is_att(l, "mcp_instructions_delta")))
        delta["attachment"].update(addedNames=[], addedBlocks=[], removedNames=[
            "furcule", "plugin:context7:context7", "plugin:posthog:posthog", "taria"])
        stats = self.variant(lines + [delta])
        self.assertEqual(stats["context"]["mcp_instruction_servers"], ["tui"], "net of removals")
        self.assertEqual(self.guard(stats, "taria", "haiku"), [
            "was given the instructions of 4 MCP server(s) besides tui: furcule, "
            "plugin:context7:context7, plugin:posthog:posthog, taria"])

    def test_a_deferred_tool_that_left_still_counts(self):
        lines = lines_of(TARIA_CLEAN)
        added = copy.deepcopy(next(l for l in lines if is_att(l, "environment")))
        added["attachment"] = {"type": "deferred_tools_delta", "addedNames": ["Bash"], "removedNames": []}
        removed = copy.deepcopy(added)
        removed["attachment"] = {"type": "deferred_tools_delta", "addedNames": [], "removedNames": ["Bash"]}
        stats = self.variant(lines + [added, removed])
        self.assertEqual(stats["context"]["deferred_tools"], [])
        self.assertEqual(self.guard(stats, "taria", "sonnet"),
                         ["was offered 1 deferred tool(s) beyond the taria arm: Bash"])

    def test_removals_matter_for_tui_itself(self):
        lines = lines_of(TARIA_CLEAN)
        delta = copy.deepcopy(next(l for l in lines if is_att(l, "mcp_instructions_delta")))
        delta["attachment"].update(addedNames=[], addedBlocks=[], removedNames=["tui"])
        self.assertEqual(self.guard(self.variant(lines + [delta]), "taria", "sonnet"),
                         ["lost the tui server's instructions mid-run: the server was removed (it disconnected)"])

    def test_no_tui_instructions(self):
        lines = [l for l in lines_of(TARIA_CLEAN) if not is_att(l, "mcp_instructions_delta")]
        problems = self.guard(self.variant(lines), "taria", "sonnet")
        self.assertEqual(problems, ["never got the tui server's instructions: it did not start, or was refused"])

    def test_an_agent_offered_no_tools(self):
        """What an agent whose server never started (or mcp_launch refused)
        leaves: the first snapshot only, no tui instructions, and a reply."""
        lines = lines_of(TARIA_CLEAN)
        first_snapshot = next(i for i, l in enumerate(lines) if is_att(l, "prompt_snapshot"))
        lines = lines[: first_snapshot + 1] + [lines[-1]]
        stats = self.variant(lines)
        self.assertEqual((stats["context"]["offered_tools"], stats["tool_calls"]), ([], {}))
        problems = self.guard(stats, "taria", "sonnet")
        self.assertEqual(problems, [
            "was not offered mcp__tui__act, mcp__tui__key, mcp__tui__read_tree, mcp__tui__type_text: "
            "the tui server did not start, or mcp_launch refused it",
            "never got the tui server's instructions: it did not start, or was refused",
        ])

    def test_no_prompt_snapshot(self):
        lines = [l for l in lines_of(TARIA_CLEAN) if not is_att(l, "prompt_snapshot")]
        self.assertEqual(self.guard(self.variant(lines), "taria", "sonnet"),
                         ["has no prompt_snapshot, so the tools it was offered cannot be checked"])

    def test_a_tool_beyond_the_arm_on_offer(self):
        lines = lines_of(TARIA_CLEAN)
        snapshot = next(l for l in lines if is_att(l, "prompt_snapshot") and "tools" in l["attachment"])
        snapshot["attachment"]["tools"].append({"name": "Bash", "description": "Run a command"})
        self.assertEqual(self.guard(self.variant(lines), "taria", "sonnet"),
                         ["was offered 1 tool(s) beyond the taria arm: Bash"])

    def test_the_other_arms_run(self):
        problems = self.guard(read(TMUX_CLEAN), "taria", "haiku")
        self.assertEqual(problems, [
            "called mcp__tui__screen (2x), which is not a taria arm tool",
            "called mcp__tui__send_keys (9x), which is not a taria arm tool",
            "was not offered mcp__tui__act, mcp__tui__key, mcp__tui__read_tree, mcp__tui__type_text: "
            "the tui server did not start, or mcp_launch refused it",
            "was offered 2 tool(s) beyond the taria arm: mcp__tui__screen, mcp__tui__send_keys",
        ])

    def test_the_wrong_model(self):
        self.assertEqual(self.guard(read(TARIA_CLEAN), "taria", "haiku"),
                         ["ran on claude-sonnet-5-5, not the requested claude-haiku-*"])

    def test_the_effort_must_be_the_plans(self):
        self.assertEqual(self.guard(read(TARIA_CLEAN), "taria", "sonnet", effort="high"),
                         ["ran at effort medium, not the plan's high"])
        lines = lines_of(TARIA_CLEAN)
        next(l for l in lines if l["type"] == "assistant")["effort"] = "high"
        self.assertEqual(self.guard(self.variant(lines), "taria", "sonnet"),
                         ["ran at effort high, medium, not the plan's medium"])
        for line in lines:
            line.pop("effort", None)
        self.assertEqual(self.guard(self.variant(lines), "taria", "sonnet"),
                         ["ran at effort (none recorded), not the plan's medium"])
        self.assertEqual(self.guard(read(TARIA_CLEAN), "taria", "sonnet", effort=None), [])

    def test_a_forbidden_tool_and_an_instruction_file(self):
        lines = lines_of(TARIA_CLEAN)
        call = next(l for l in lines if l["type"] == "assistant" and l["message"]["content"][0]["type"] == "tool_use")
        call["message"]["content"][0]["name"] = "Read"
        instructions = copy.deepcopy(next(l for l in lines if is_att(l, "environment")))
        instructions["attachment"] = {"type": "instructions", "files": [{"path": "/home/user/project/CLAUDE.md"}]}
        lines.insert(2, instructions)
        problems = self.guard(self.variant(lines), "taria", "sonnet")
        self.assertEqual(problems, [
            "called Read (1x), which is not a taria arm tool",
            "was given 1 instruction file(s) despite omitClaudeMd: /home/user/project/CLAUDE.md",
        ])

    def test_deferred_tools_beyond_the_arm(self):
        problems = self.guard(read("forbidden.jsonl"), "taria", "sonnet")
        # NotebookEdit and the other server were removed later: they still count.
        self.assertIn("was offered 3 deferred tool(s) beyond the taria arm: NotebookEdit, WebFetch, "
                      "mcp__other__lookup", problems)
        self.assertIn("was given the instructions of 1 MCP server(s) besides tui: other", problems)
        self.assertNotIn("ToolSearch", " ".join(problems))

    def test_an_agent_that_never_ran(self):
        problems = self.guard(read("never_ran.jsonl"), "taria", "sonnet")
        self.assertEqual(problems[-1], "never ran: the transcript has no assistant turn")

    def test_guard_tolerates_a_sparse_stats_dict(self):
        self.assertEqual(transcript.guard({}, "taria", "sonnet", ARM_TOOLS["taria"]), [
            "has no prompt_snapshot, so the tools it was offered cannot be checked",
            "never got the tui server's instructions: it did not start, or was refused",
            "never ran: the transcript has no assistant turn",
        ])


class Ending(Variant, unittest.TestCase):
    """How a run ended, from real runs and variants of them."""

    def test_real_runs_ended_on_their_own(self):
        for name in (TARIA_CLEAN, TMUX_CLEAN, TARIA_LEAKED):
            with self.subTest(fixture=name):
                self.assertEqual(transcript.ending(read(name), 30), ("end_turn", None))

    def test_a_usage_limit_is_a_harness_error(self):
        lines = cut_after_message(lines_of(TARIA_CLEAN), 3)
        # As in real transcripts: the reply cut off mid-stream, then Claude
        # Code's own message.
        partial = copy.deepcopy(lines[-2])
        partial["message"].update(id="msg_partial", stop_reason=None,
                                  content=[{"type": "text", "text": "Now I will"}])
        lines += [partial, synthetic_line(partial, "You've hit your session limit", "rate_limit")]
        kind, detail = transcript.ending(self.variant(lines), 30)
        self.assertEqual(kind, "harness_error")
        self.assertEqual(detail, "the run ended on Claude Code's own message, not a model reply (rate_limit): "
                                 "\"You've hit your session limit\"")

    def test_stopping_on_a_tool_call_under_the_cap_is_a_harness_error(self):
        stats = self.variant(cut_after_message(lines_of(TARIA_CLEAN), 3))
        kind, detail = transcript.ending(stats, 30)
        self.assertEqual(kind, "harness_error")
        self.assertIn("its last message stopped on 'tool_use'", detail)
        self.assertIn("at turn 3, under maxTurns 30", detail)

    def test_stopping_on_a_tool_call_at_the_cap_is_max_turns(self):
        stats = self.variant(cut_after_message(lines_of(TARIA_CLEAN), 3))
        self.assertTrue(transcript.cut_off_at_max_turns(stats, 3))
        self.assertEqual(transcript.ending(stats, 3), ("max_turns", "stopped on a tool call at turn 3 of maxTurns 3"))
        self.assertEqual(transcript.ending(stats, 2)[0], "max_turns")
        # A run that finished on its own at the cap is not cut off.
        self.assertEqual(transcript.ending(read(TARIA_CLEAN), 4), ("end_turn", None))

    def test_an_unclosed_tool_call_at_the_cap_is_max_turns(self):
        """The final tool_use message's closing line was never written
        (stop_reason null on every line): at the cap it is still the cut-off."""
        lines = cut_after_message(lines_of(TARIA_CLEAN), 3)
        last = assistant_ids(lines)[-1]
        for line in lines:
            if line.get("type") == "assistant" and line["message"]["id"] == last:
                line["message"]["stop_reason"] = None
        stats = self.variant(lines)
        self.assertEqual((stats["last_stop_reason"], stats["ended_on_tool_use"]), (None, True))
        self.assertTrue(transcript.cut_off_at_max_turns(stats, 3))
        self.assertEqual(transcript.ending(stats, 3)[0], "max_turns")
        kind, detail = transcript.ending(stats, 30)
        self.assertEqual(kind, "harness_error", "under the cap it did not end on its own")
        self.assertIn("stopped on None", detail)

    def test_a_recorded_max_turns_reached_is_proof(self):
        lines = cut_after_message(lines_of(TARIA_CLEAN), 3)
        reached = copy.deepcopy(next(l for l in lines if is_att(l, "environment")))
        reached["attachment"] = {"type": "max_turns_reached", "maxTurns": 30, "turnCount": 31}
        stats = self.variant(lines + [reached])
        self.assertEqual(stats["max_turns_reached"], {"max_turns": 30, "turn_count": 31})
        self.assertEqual(transcript.ending(stats, 30),
                         ("max_turns", "Claude Code recorded max_turns_reached (turn 31 of 30)"))

    def test_a_final_text_reply_without_a_stop_reason_ended_on_its_own(self):
        lines = lines_of(TARIA_CLEAN)
        lines[-1]["message"]["stop_reason"] = None
        stats = self.variant(lines)
        self.assertEqual((stats["last_block_type"], stats["end_inferred"]), ("text", True))
        self.assertEqual(transcript.ending(stats, 30), ("end_turn", transcript.INFERRED_END))

    def test_a_final_message_without_a_stop_reason_that_is_not_text(self):
        for block in ({"type": "thinking", "thinking": "", "signature": "x"}, {"type": "text", "text": "  "}):
            with self.subTest(block=block["type"]):
                lines = lines_of(TARIA_CLEAN)
                lines[-1]["message"].update(stop_reason=None, content=[block])
                stats = self.variant(lines)
                self.assertIs(stats["end_inferred"], False)
                kind, detail = transcript.ending(stats, 30)
                self.assertEqual(kind, "harness_error")
                self.assertIn("stopped on None (it never finished streaming", detail)

    def test_any_other_stop_reason(self):
        for reason in ("max_tokens", "refusal", "pause_turn", "stop_sequence"):
            with self.subTest(reason=reason):
                lines = lines_of(TARIA_CLEAN)
                lines[-1]["message"]["stop_reason"] = reason
                stats = self.variant(lines)
                self.assertIs(stats["end_inferred"], False)
                kind, detail = transcript.ending(stats, 30)
                self.assertEqual(kind, "harness_error")
                self.assertIn(repr(reason), detail)

    def test_never_ran(self):
        self.assertEqual(transcript.ending(read("never_ran.jsonl"), 30)[0], "never_ran")
        self.assertEqual(transcript.ending({}, 30)[0], "never_ran")


class UnclosedReply(Variant, unittest.TestCase):
    """A real run whose final text reply Claude Code never closed (job j108
    of batch full1): its only line carries stop_reason null and an
    output_tokens of 1. The reply arrived, so the run ended on its own."""

    def test_the_fixture_as_read(self):
        stats = read(TARIA_UNCLOSED)
        self.assertEqual((stats["models"], stats["turns"]), (["claude-haiku-5-5"], 4))
        self.assertEqual(stats["tool_calls"], {"mcp__tui__act": 2, "mcp__tui__read_tree": 1})
        self.assertEqual(stats["first_prompt"], TASKS["T4"].prompt)
        self.assertEqual((stats["last_model"], stats["last_stop_reason"], stats["last_block_type"]),
                         ("claude-haiku-5-5", None, "text"))
        self.assertEqual((stats["ended_on_tool_use"], stats["ended_synthetic"]), (False, False))
        self.assertIs(stats["end_inferred"], True)
        self.assertIs(stats["output_tokens_lower_bound"], True)
        self.assertEqual(stats["tokens"]["output"], 421)  # the final message counts its placeholder 1
        self.assertTrue(stats["final_text"].startswith('I added a new task titled "Record the killer demo (follow-up)"'))
        self.assertEqual(stats["context"]["mcp_instruction_servers"], ["tui"])
        for name in (TARIA_CLEAN, TMUX_CLEAN, TARIA_LEAKED):
            with self.subTest(fixture=name):
                self.assertEqual((read(name)["end_inferred"], read(name)["output_tokens_lower_bound"]),
                                 (False, False))

    def test_it_passes_the_guard_and_ended_on_its_own(self):
        stats = read(TARIA_UNCLOSED)
        self.assertEqual(transcript.guard(stats, "taria", "haiku", ARM_TOOLS["taria"], effort="medium"), [])
        self.assertEqual(transcript.ending(stats, 30), ("end_turn", transcript.INFERRED_END))
        self.assertIn("output tokens are a lower bound", transcript.INFERRED_END)

    def final(self, lines):
        last = assistant_ids(lines)[-1]
        return [l for l in lines if l.get("type") == "assistant" and l["message"]["id"] == last]

    def test_a_tool_call_in_its_place_did_not_end_on_its_own(self):
        lines = lines_of(TARIA_UNCLOSED)
        (line,) = self.final(lines)
        line["message"]["content"] = [{"type": "tool_use", "id": "toolu_x", "name": "mcp__tui__read_tree",
                                       "input": {}}]
        stats = self.variant(lines)
        self.assertEqual((stats["end_inferred"], stats["ended_on_tool_use"]), (False, True))
        kind, detail = transcript.ending(stats, 30)
        self.assertEqual(kind, "harness_error")
        self.assertIn("stopped on None", detail)
        self.assertEqual(transcript.ending(stats, 4)[0], "max_turns", "at the cap: the cut-off")

    def test_an_explicit_stop_other_than_end_turn_is_still_a_harness_error(self):
        lines = lines_of(TARIA_UNCLOSED)
        self.final(lines)[0]["message"]["stop_reason"] = "max_tokens"
        stats = self.variant(lines)
        self.assertIs(stats["end_inferred"], False)
        self.assertEqual(transcript.ending(stats, 30)[0], "harness_error")

    def test_a_trailing_synthetic_message_is_still_a_harness_error(self):
        lines = lines_of(TARIA_UNCLOSED)
        lines.append(synthetic_line(self.final(lines)[0], "API Error: overloaded", "server_error"))
        kind, detail = transcript.ending(self.variant(lines), 30)
        self.assertEqual(kind, "harness_error")
        self.assertIn("Claude Code's own message", detail)


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
        stats = read(TARIA_CLEAN)
        row = {"task": "T3", "arm": "taria", "mode": "subagent", "model": "sonnet", "effort": "medium",
               "max_turns": 30, "batch": "b", "status": "passed", "passed": True,
               **{k: stats[k] for k in ("turns", "tool_calls", "tool_calls_total", "toolsearch_calls",
                                        "tokens", "wall_ms", "context")}}
        col = Summary.columns(next(l for l in summarize.table([row]).splitlines() if l.startswith("| T3 |")))
        self.assertEqual(col["tokens processed med [IQR]"], "23,325")  # 8 + 7,281 + 16,036
        self.assertEqual((col["tool calls med [IQR]"], col["ToolSearch med"], col["turns med"]), ("3", "0", "4"))


if __name__ == "__main__":
    unittest.main()
