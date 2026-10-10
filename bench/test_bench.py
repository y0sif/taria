#!/usr/bin/env python3
"""Tests for the benchmark harness itself. Stdlib unittest, no model.

The live tests start a real taria-demo in a private tmux server and drive it
through bench/tmux_mcp.py over stdio, exactly as the tmux arm's agent would.
They need tmux and a fresh `cargo build --workspace`.

Usage:  python3 bench/test_bench.py [-v]
"""

import json
import os
import shutil
import sys
import time
import unittest

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
from harness import DemoRun, McpStdio, mcp_config, require_fresh_build  # noqa: E402
import run as runner  # noqa: E402
import summarize  # noqa: E402
import tasks  # noqa: E402
import tmux_mcp  # noqa: E402

INPUT_FOCUSED = "[Enter] add task"


@unittest.skipIf(shutil.which("tmux") is None, "tmux is not on PATH")
class TmuxMcpLive(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        require_fresh_build()

    def setUp(self):
        self.demo = DemoRun()
        self.addCleanup(self.demo.close)
        self.demo.start()
        entry = mcp_config("tmux", self.demo)["mcpServers"]["tui"]
        self.client = McpStdio(entry, cwd=self.demo.workdir, name="tmux-mcp")
        self.addCleanup(self.client.close)
        self.client.initialize()

    def focus_input(self):
        self.demo.send("i")
        deadline = time.monotonic() + 2
        while INPUT_FOCUSED not in self.demo.capture():
            self.assertLess(time.monotonic(), deadline, "the input never took the keyboard")
            time.sleep(0.02)

    def test_escape_then_d_is_two_presses(self):
        """One call, "Escape d", from the focused input: Escape hands the
        keyboard back to the list and d opens the delete dialog on the
        selected task. Sent as one tmux command it was alt+d, which the input
        ignores, and nothing happened."""
        self.focus_input()
        screen = self.client.call("send_keys", {"keys": "Escape d"})
        state = self.demo.settled_state()
        self.assertEqual(state["dialog"], {"task": "task-1"}, state)
        self.assertEqual(state["draft"], "")
        self.assertIn("Delete task 'Write the taria README'?", screen,
                      "send_keys returned a screen from before its keys landed")

    def test_settle_returns_the_screen_after_the_keys(self):
        screen = self.client.call("send_keys", {"keys": "Down"})
        self.assertIn("  > [ ] Wire up the MCP bridge", screen.splitlines())

    def test_a_key_that_changes_nothing_costs_the_full_budget(self):
        before = self.client.call("screen")
        started = time.monotonic()
        screen = self.client.call("send_keys", {"keys": "F5"})
        elapsed = time.monotonic() - started
        self.assertEqual(screen, before)
        self.assertGreaterEqual(elapsed, tmux_mcp.SETTLE_CAP)
        self.assertLess(elapsed, tmux_mcp.SETTLE_CAP + 0.5)

    def test_literal_text_reaches_the_input_verbatim(self):
        self.focus_input()
        self.client.call("send_keys", {"keys": "Escape d y", "literal": True})
        self.assertEqual(self.demo.settled_state()["draft"], "Escape d y")


class Checkers(unittest.TestCase):
    def test_every_checker_passes_good_states_and_fails_bad_ones(self):
        for task_id, (good, bad) in tasks.self_test_cases().items():
            check = tasks.TASKS[task_id].check
            for state in good:
                with self.subTest(task=task_id, case="good"):
                    self.assertTrue(check(state)[0], check(state)[1])
            for name, (state, needle) in bad.items():
                with self.subTest(task=task_id, case=name):
                    passed, reason = check(state)
                    self.assertFalse(passed)
                    self.assertIn(needle, reason)

    def test_a_field_the_dump_grows_cannot_slip_through(self):
        state = json.loads(json.dumps(tasks.SEED_STATE))
        state["tasks"] = [t for t in state["tasks"] if t["id"] != "task-3"]
        state["tasks"][0]["priority"] = "high"
        passed, reason = tasks.check_t3(state)
        self.assertFalse(passed)
        self.assertIn("task-1 priority", reason)

    def test_prompts_name_no_tool_node_or_key(self):
        banned = ("taria", "tmux", "node", "task-", "key", "press", "tree", "screen")
        for task in tasks.TASKS.values():
            for word in banned:
                with self.subTest(task=task.id, word=word):
                    self.assertNotIn(word, task.prompt.lower())


class SpendStream(unittest.TestCase):
    INIT_TARIA = {
        "type": "system", "subtype": "init", "model": "m",
        "tools": ["mcp__tui__read_tree", "mcp__tui__act", "mcp__tui__key", "mcp__tui__type_text"],
    }

    def test_the_guard_refuses_any_other_tool_list(self):
        stats = runner.StreamStats("taria")
        with self.assertRaises(runner.GuardError):
            stats.feed({**self.INIT_TARIA, "tools": self.INIT_TARIA["tools"] + ["Bash"]})
        with self.assertRaises(runner.GuardError):
            runner.StreamStats("tmux").feed(self.INIT_TARIA)
        with self.assertRaises(runner.GuardError):
            runner.StreamStats("taria").feed({"type": "assistant", "message": {"content": []}})

    def test_the_stream_is_parsed_into_the_record_fields(self):
        stats = runner.StreamStats("taria")
        use = {"type": "tool_use", "id": "a", "name": "mcp__tui__act", "input": {}}
        events = [
            self.INIT_TARIA,
            {"type": "assistant", "message": {"content": [{"type": "text", "text": "hi"}, use]}},
            {"type": "assistant", "message": {"content": [use]}},  # same block, re-emitted
            {"type": "assistant", "message": {"content": [{**use, "id": "b", "name": "mcp__tui__read_tree"}]}},
            {
                "type": "result", "subtype": "success", "is_error": False, "num_turns": 3,
                "duration_ms": 9000, "duration_api_ms": 7000, "total_cost_usd": 0.05,
                "usage": {"input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 300,
                          "cache_creation_input_tokens": 4000},
            },
        ]
        for event in events:
            stats.feed(event)
        got = stats.summary()
        self.assertEqual(got["tool_calls"], {"act": 1, "read_tree": 1})
        self.assertEqual(got["tokens"], 4330)
        self.assertEqual(got["turns"], 3)
        self.assertEqual(got["cost_usd"], 0.05)
        self.assertEqual(got["result_subtype"], "success")
        self.assertEqual(got["usage"]["cache_creation"], 4000)

    def test_the_command_carries_every_flag(self):
        cmd = runner.spend_command("Do it.", "/x/mcp.json", 1.0, "claude-sonnet-5-5")
        self.assertEqual(cmd[:3], ["claude", "-p", "Do it."])
        for flag in ("--bare", "--strict-mcp-config", "--disable-slash-commands",
                     "--no-session-persistence", "--verbose"):
            self.assertIn(flag, cmd)
        self.assertEqual(cmd[cmd.index("--tools") + 1], "")
        self.assertEqual(cmd[cmd.index("--allowedTools") + 1], "mcp__tui")
        self.assertEqual(cmd[cmd.index("--max-budget-usd") + 1], "1")


class Summary(unittest.TestCase):
    def test_wilson_interval(self):
        low, high = summarize.wilson(1, 1)
        self.assertAlmostEqual(low, 0.2065, places=3)
        self.assertEqual(high, 1.0)
        low, high = summarize.wilson(5, 10)
        self.assertAlmostEqual(low, 0.2366, places=3)
        self.assertAlmostEqual(high, 0.7634, places=3)

    def test_dry_runs_stay_out_of_the_headline(self):
        dry = {"task": "T1", "arm": "taria", "mode": "dry-run", "solver": "scripted",
               "status": "passed", "passed": True, "tool_calls_total": 2, "wall_ms": 80}
        out = summarize.render([dry])
        headline, _, rest = out.partition("## Dry runs")
        self.assertIn("No agent runs", headline)
        self.assertIn("| T1 | taria | dry-run | 1 | 0 | 1/1 |", rest)


if __name__ == "__main__":
    unittest.main()
