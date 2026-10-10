#!/usr/bin/env python3
"""Tests for the benchmark harness itself. Stdlib unittest, no model.

The live tests start a real taria-demo in a private tmux server and drive it
through bench/tmux_mcp.py over stdio, exactly as the tmux arm's agent would.
They need tmux and a fresh `cargo build --workspace`.

The spend path is tested against bench/fixtures/fake_claude.py, put first on
PATH as `claude`: no network, no model, no API key. A test refuses to run if
`claude` on PATH is anything but the fake.

Usage:  python3 bench/test_bench.py [-v]
"""

import io
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
from harness import COLS, REPO, ROWS, DemoRun, McpStdio, mcp_config, require_fresh_build  # noqa: E402
import run as runner  # noqa: E402
import summarize  # noqa: E402
import tasks  # noqa: E402
import tmux_mcp  # noqa: E402

INPUT_FOCUSED = "[Enter] add task"
FAKE_CLAUDE = os.path.join(BENCH, "fixtures", "fake_claude.py")
HAVE_TMUX = shutil.which("tmux") is not None


@unittest.skipUnless(HAVE_TMUX, "tmux is not on PATH")
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

    def test_the_demo_gets_the_whole_terminal(self):
        """No status line in the private server: the pane is COLS x ROWS, and
        the screen either arm reads has ROWS lines."""
        self.assertEqual(self.demo.pane_size(), (COLS, ROWS))
        self.assertEqual((COLS, ROWS), (120, 40))


class SettleCap(unittest.TestCase):
    def test_the_tmux_cap_is_taria_mcps_update_wait(self):
        """Neither arm may buy accuracy with a longer wait: the tmux server's
        settle cap is read against UPDATE_WAIT in the bridge's source."""
        with open(os.path.join(REPO, "crates", "taria-mcp", "src", "server.rs")) as f:
            source = f.read()
        match = re.search(r"const UPDATE_WAIT: Duration = Duration::from_millis\((\d+)\);", source)
        self.assertIsNotNone(match, "UPDATE_WAIT is no longer a from_millis constant in server.rs")
        self.assertEqual(tmux_mcp.SETTLE_CAP, int(match.group(1)) / 1000)


@unittest.skipUnless(HAVE_TMUX, "tmux is not on PATH")
class Grading(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        require_fresh_build()

    def test_a_dead_demo_fails_whatever_its_dump_says(self):
        demo = DemoRun()
        self.addCleanup(demo.close)
        demo.start()
        self.assertTrue(demo.demo_alive())
        with open(f"/proc/{demo.demo_pid}/cmdline", "rb") as f:
            line = f.read()
        self.assertIn(b"taria-demo", line)
        os.kill(demo.demo_pid, signal.SIGKILL)  # this run's demo, checked above
        deadline = time.monotonic() + 2
        while demo.demo_alive():
            self.assertLess(time.monotonic(), deadline, "the demo outlived SIGKILL")
            time.sleep(0.02)
        graded = runner.grade(tasks.TASKS["T1"], demo)
        self.assertFalse(graded["demo_alive"])
        self.assertFalse(graded["state"]["quit"], "a crash leaves quit false in the dump")
        self.assertTrue(graded["reason"].startswith("demo exited"), graded["reason"])
        self.assertEqual(runner.classify({}, graded), ("failed", False, None))

    def test_a_harness_exception_is_a_harness_error_and_exits_2(self):
        def broken(*_args, **_kwargs):
            raise RuntimeError("the solver fell over")

        original = runner.solve
        runner.solve = broken
        self.addCleanup(setattr, runner, "solve", original)
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        results = os.path.join(tmp, "r.jsonl")
        args = runner.parse_args(["--dry-run", "--tasks", "T1", "--arms", "tmux", "--results", results])
        with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            code = runner.run_all(args)
        self.assertEqual(code, runner.EXIT_HARNESS)
        with open(results) as f:
            record = json.loads(f.read())
        self.assertEqual(record["status"], "harness_error")
        self.assertIn("RuntimeError: the solver fell over", record["error"])


class Outcomes(unittest.TestCase):
    GOOD = {"check_passed": True, "demo_alive": True}
    BAD = {"check_passed": False, "demo_alive": True}

    def test_classify(self):
        c = runner.classify
        self.assertEqual(c({}, self.GOOD), ("passed", True, None))
        self.assertEqual(c({}, self.BAD), ("failed", False, None))
        self.assertEqual(c({}, {"check_passed": True, "demo_alive": False}), ("failed", False, None))
        self.assertEqual(c({"timed_out": True}, self.GOOD), ("timeout", False, None))
        self.assertEqual(c({"result_subtype": "error_max_budget_usd"}, self.GOOD), ("budget", True, None))
        self.assertEqual(c({"result_subtype": "error_max_budget_usd"}, self.BAD), ("budget", False, None))
        self.assertEqual(c({"harness_error": "x", "timed_out": True}, self.GOOD), ("harness_error", None, "x"))

    def test_exit_codes(self):
        self.assertEqual(runner.exit_code(["passed", "budget"], [True, True]), 0)
        self.assertEqual(runner.exit_code(["passed", "timeout"], [True, False]), 1)
        self.assertEqual(runner.exit_code(["budget"], [False]), 1)
        self.assertEqual(runner.exit_code(["failed", "harness_error"], [False, False]), 2)

    def test_session_markers_never_reach_the_agent(self):
        saved = dict(os.environ)
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(saved)))
        os.environ.update(CLAUDECODE="1", CLAUDE_CODE_ENTRYPOINT="cli", CLAUDE_EFFORT="xhigh",
                          CLAUDE_CODE_EFFORT_LEVEL="max", CLAUDE_CONFIG_DIR="/c", KEEP_ME="y")
        env, stripped = runner.spend_env()
        for name in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_EFFORT", "CLAUDE_CODE_EFFORT_LEVEL"):
            self.assertNotIn(name, env)
            self.assertIn(name, stripped)
        self.assertEqual(env["CLAUDE_CONFIG_DIR"], "/c")
        self.assertEqual(env["KEEP_ME"], "y")


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
        "mcp_servers": [{"name": "tui", "status": "connected"}],
        "permissionMode": "dontAsk", "plugins": [], "skills": [], "output_style": "default",
    }

    def test_the_guard_refuses_anything_but_the_arms_setup(self):
        bad = {
            "extra tool": {"tools": self.INIT_TARIA["tools"] + ["Bash"]},
            "plugin": {"plugins": [{"name": "p", "path": "/p"}]},
            "skill": {"skills": ["review"]},
            "second server": {"mcp_servers": [{"name": "tui", "status": "connected"},
                                              {"name": "x", "status": "connected"}]},
            "server down": {"mcp_servers": [{"name": "tui", "status": "failed"}]},
            "permission mode": {"permissionMode": "bypassPermissions"},
            "output style": {"output_style": "Explanatory"},
        }
        runner.StreamStats("taria").feed(self.INIT_TARIA)  # the good one passes
        for name, change in bad.items():
            with self.subTest(case=name):
                stats = runner.StreamStats("taria")
                with self.assertRaises(runner.GuardError):
                    stats.feed({**self.INIT_TARIA, **change})
                self.assertIsNotNone(stats.agent_env(), "a refused init is still recorded")
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
        self.assertEqual((got["tokens_in"], got["tokens_out"]), (4310, 20))
        self.assertEqual(
            (got["input_tokens"], got["output_tokens"], got["cache_read_tokens"], got["cache_creation_tokens"]),
            (10, 20, 300, 4000),
        )
        self.assertEqual(got["usage_source"], "result")
        self.assertEqual(got["turns"], 3)
        self.assertEqual(got["cost_usd"], 0.05)
        self.assertEqual(got["result_subtype"], "success")
        self.assertEqual(got["agent_env"]["permissionMode"], "dontAsk")

    def test_a_cut_off_stream_keeps_its_usage(self):
        stats = runner.StreamStats("taria")
        stats.feed(self.INIT_TARIA)

        def message(mid, out):
            usage = {"input_tokens": 5, "output_tokens": out, "cache_read_input_tokens": 50,
                     "cache_creation_input_tokens": 500}
            return {"type": "assistant", "message": {"id": mid, "usage": usage, "content": []}}

        for event in (message("m1", 1), message("m1", 9), message("m2", 3)):
            stats.feed(event)
        got = stats.summary()
        self.assertEqual(got["usage_source"], "stream")
        self.assertEqual(got["usage"], {"input": 10, "output": 12, "cache_read": 100, "cache_creation": 1000})
        self.assertIsNone(got["cost_usd"])

    def test_the_command_carries_every_flag(self):
        cmd = runner.spend_command("Do it.", "/x/mcp.json", 1.0, "claude-sonnet-5-5")
        self.assertEqual(cmd[:3], ["claude", "-p", "Do it."])
        for flag in ("--bare", "--strict-mcp-config", "--disable-slash-commands",
                     "--no-session-persistence", "--verbose", "--setting-sources="):
            self.assertIn(flag, cmd)
        self.assertEqual(cmd[cmd.index("--tools") + 1], "")
        self.assertEqual(cmd[cmd.index("--allowedTools") + 1], "mcp__tui")
        self.assertEqual(cmd[cmd.index("--max-budget-usd") + 1], "1")
        self.assertEqual(cmd[cmd.index("--effort") + 1], "medium")
        self.assertEqual(cmd[cmd.index("--permission-mode") + 1], "dontAsk")
        cmd = runner.spend_command("Do it.", "/x/mcp.json", 1.0, "m", effort="high")
        self.assertEqual(cmd[cmd.index("--effort") + 1], "high")


@unittest.skipUnless(HAVE_TMUX, "tmux is not on PATH")
class FakeClaude(unittest.TestCase):
    """run.py's spend path end to end against a fake `claude`, live demo included."""

    @classmethod
    def setUpClass(cls):
        require_fresh_build()
        cls.tmp = tempfile.mkdtemp(prefix="fake-claude-")
        os.symlink(FAKE_CLAUDE, os.path.join(cls.tmp, "claude"))
        cls.saved_env = dict(os.environ)
        os.environ.pop("ANTHROPIC_API_KEY", None)  # never needed, never sent
        os.environ["PATH"] = cls.tmp + os.pathsep + os.environ["PATH"]
        # Markers a parent Claude Code session sets; none may reach the agent.
        os.environ.update(CLAUDECODE="1", CLAUDE_CODE_FAKE_MARKER="1")
        cls.log = os.path.join(cls.tmp, "argv.json")
        os.environ["FAKE_CLAUDE_LOG"] = cls.log

    @classmethod
    def tearDownClass(cls):
        os.environ.clear()
        os.environ.update(cls.saved_env)
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self):
        found = shutil.which("claude")
        if found is None or os.path.realpath(found) != os.path.realpath(FAKE_CLAUDE):
            self.fail(f"claude on PATH is {found}, not the fake; refusing to spend")
        if os.path.exists(self.log):
            os.remove(self.log)

    def spend(self, mode, arm="taria", task="T1", solve=False, timeout=20, argv=()):
        os.environ["FAKE_CLAUDE_MODE"] = mode
        os.environ["FAKE_CLAUDE_SOLVE"] = "1" if solve else "0"
        os.environ["FAKE_CLAUDE_TASK"] = task
        args = runner.parse_args(["--spend", "--tasks", task, "--arms", arm, "--timeout",
                                  str(timeout), "--batch", "test-batch", *argv])
        started = time.monotonic()
        record = runner.run_one(tasks.TASKS[task], arm, args, 0)
        return record, time.monotonic() - started

    def fake_log(self):
        with open(self.log) as f:
            return json.load(f)

    def test_a_solved_run_passes_with_its_stream_recorded(self):
        record, _ = self.spend("success", arm="tmux", solve=True)
        self.assertEqual(record["status"], "passed", record.get("error") or record.get("reason"))
        self.assertTrue(record["passed"])
        self.assertEqual(record["result_subtype"], "success")
        self.assertEqual(record["usage_source"], "result")
        self.assertEqual((record["tokens_in"], record["tokens_out"]), (12 + 100 + 1000, 34))
        self.assertEqual(record["cost_usd"], 0.01)
        self.assertEqual(record["tool_calls"], {"screen": 1, "send_keys": 3})
        env = record["agent_env"]
        self.assertEqual((env["plugins"], env["skills"], env["permissionMode"]), ([], [], "dontAsk"))
        self.assertEqual(env["tools"], ["mcp__tui__screen", "mcp__tui__send_keys"])
        # Provenance on the record.
        self.assertEqual(record["batch"], "test-batch")
        self.assertEqual(record["claude_version"], "0.0.0 (fake claude)")
        self.assertRegex(record["git_sha"] or "", r"^[0-9a-f]{40}$")
        self.assertIn(record["git_dirty"], (True, False))
        self.assertEqual((record["model"], record["effort"]), (runner.DEFAULT_MODEL, "medium"))
        # What the agent was started with.
        seen = self.fake_log()
        self.assertEqual(seen["pid"], seen["pgid"], "the agent leads its own process group")
        self.assertTrue(seen["cwd"].endswith("/work"))
        self.assertIn("--setting-sources=", seen["argv"])
        self.assertEqual(seen["argv"][seen["argv"].index("--effort") + 1], "medium")
        self.assertEqual(seen["env_claude"], [], "session markers reached the agent")
        self.assertIn("CLAUDECODE", record["env_stripped"])

    def test_an_unsolved_run_fails_on_the_dump(self):
        record, _ = self.spend("success", argv=("--effort", "low"))
        self.assertEqual(record["status"], "failed")
        self.assertIs(record["passed"], False)
        self.assertIn("no new task", record["reason"])
        self.assertEqual(record["effort"], "low")

    def test_a_timeout_kills_the_whole_group_and_keeps_the_streamed_usage(self):
        original = runner.KILL_GRACE_S
        runner.KILL_GRACE_S = 0.5
        self.addCleanup(setattr, runner, "KILL_GRACE_S", original)
        record, elapsed = self.spend("hang", timeout=2)
        self.assertEqual(record["status"], "timeout")
        self.assertIs(record["passed"], False)
        self.assertIsNone(record["error"])
        self.assertTrue(record["timed_out"])
        self.assertEqual(record["usage_source"], "stream")
        self.assertEqual(record["usage"], {"input": 10, "output": 20, "cache_read": 100, "cache_creation": 1000})
        self.assertIsNone(record["cost_usd"])
        self.assertLess(elapsed, 2 + 0.5 + 5 + 3)
        seen = self.fake_log()
        for pid in (seen["pid"], seen["child"]):
            self.assertFalse(_running(pid), f"pid {pid} survived the group kill")

    def test_no_result_event_is_a_harness_error(self):
        record, _ = self.spend("noresult")
        self.assertEqual(record["status"], "harness_error")
        self.assertIsNone(record["passed"])
        self.assertIn("without a result event", record["error"])
        self.assertIn("lost the connection", record["error"])
        self.assertEqual(record["usage_source"], "stream")

    def test_the_guard_stops_an_extra_tool_at_once(self):
        record, elapsed = self.spend("extra_tool")
        self.assertEqual(record["status"], "harness_error")
        self.assertTrue(record["guard_failed"])
        self.assertIn("'Bash'", record["error"])
        self.assertIn("Bash", record["agent_env"]["tools"])
        self.assertLess(elapsed, 10, "the guard waited for the agent instead of stopping it")

    def test_the_guard_stops_a_plugin(self):
        record, elapsed = self.spend("plugins")
        self.assertEqual(record["status"], "harness_error")
        self.assertTrue(record["guard_failed"])
        self.assertIn("plugins are loaded: ['fake-plugin']", record["error"])
        self.assertLess(elapsed, 10)

    def test_a_permission_denial_is_a_harness_error(self):
        record, _ = self.spend("denied")
        self.assertEqual(record["status"], "harness_error")
        self.assertIn("permission denial", record["error"])
        self.assertIn("mcp__tui__act", record["error"])
        self.assertEqual(record["permission_denials"], 1)

    def test_an_error_result_is_a_harness_error(self):
        record, _ = self.spend("api_error")
        self.assertEqual(record["status"], "harness_error")
        self.assertIn("API Error: 404", record["error"])

    def test_a_budget_stop_is_graded_and_counted_apart(self):
        record, _ = self.spend("budget", arm="tmux", solve=True)
        self.assertEqual(record["status"], "budget")
        self.assertIs(record["passed"], True)
        self.assertEqual(record["cost_usd"], 1.03)
        record, _ = self.spend("budget")
        self.assertEqual(record["status"], "budget")
        self.assertIs(record["passed"], False)

    def test_a_guard_failure_stops_the_batch_with_its_record_written(self):
        original = runner.has_api_key
        runner.has_api_key = lambda: True  # the fake needs no key; none is set
        self.addCleanup(setattr, runner, "has_api_key", original)
        os.environ["FAKE_CLAUDE_MODE"] = "plugins"
        results = os.path.join(self.tmp, "guard.jsonl")
        args = runner.parse_args(["--spend", "--tasks", "T1,T2", "--arms", "taria", "--results", results])
        err = io.StringIO()
        with redirect_stdout(io.StringIO()), redirect_stderr(err):
            code = runner.run_all(args)
        self.assertEqual(code, runner.EXIT_HARNESS)
        with open(results) as f:
            records = [json.loads(line) for line in f]
        self.assertEqual(len(records), 1, "the batch went on after the guard fired")
        self.assertTrue(records[0]["guard_failed"])
        self.assertIn("1 run(s) not started", err.getvalue())

    def test_spend_without_a_key_exits_2(self):
        env = {k: v for k, v in os.environ.items() if k != "ANTHROPIC_API_KEY"}
        proc = subprocess.run(
            [sys.executable, os.path.join(BENCH, "run.py"), "--spend", "--tasks", "T1",
             "--arms", "taria", "--results", os.path.join(self.tmp, "nokey.jsonl")],
            capture_output=True, text=True, env=env, timeout=60,
        )
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("ANTHROPIC_API_KEY", proc.stderr)
        self.assertFalse(os.path.exists(self.log), "claude was started without a key")


def _running(pid):
    """Whether `pid` is a live process (a zombie is not)."""
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        try:
            with open(f"/proc/{pid}/stat") as f:
                state = f.read().rsplit(")", 1)[1].split()[0]
        except OSError:
            return False
        if state == "Z":
            return False
        time.sleep(0.05)
    return True


class Summary(unittest.TestCase):
    def rec(self, status, passed=None, **extra):
        base = {"task": "T1", "arm": "taria", "mode": "spend", "solver": "agent",
                "model": "m", "effort": "medium", "batch": "b1", "ts": "2026-10-10T00:00:00+00:00",
                "status": status, "passed": passed, "tool_calls_total": 3, "wall_ms": 1000,
                "turns": 2, "cost_usd": 0.1, "tokens_in": 1000, "tokens_out": 50,
                "result_subtype": "success"}
        return {**base, **extra}

    def test_wilson_interval(self):
        low, high = summarize.wilson(1, 1)
        self.assertAlmostEqual(low, 0.2065, places=3)
        self.assertEqual(high, 1.0)
        low, high = summarize.wilson(5, 10)
        self.assertAlmostEqual(low, 0.2366, places=3)
        self.assertAlmostEqual(high, 0.7634, places=3)

    def test_fisher_exact(self):
        self.assertAlmostEqual(summarize.fisher_exact(3, 0, 0, 3), 0.1)
        self.assertAlmostEqual(summarize.fisher_exact(4, 0, 0, 4), 2 / 70)
        self.assertAlmostEqual(summarize.fisher_exact(1, 0, 1, 0), 1.0)
        self.assertAlmostEqual(summarize.fisher_exact(0, 0, 0, 0), 1.0)
        # Symmetric in the arms.
        self.assertAlmostEqual(summarize.fisher_exact(7, 3, 2, 8), summarize.fisher_exact(2, 8, 7, 3))

    def test_outcomes_counted_and_timeouts_fail(self):
        records = [
            self.rec("passed", True),
            self.rec("failed", False),
            self.rec("timeout", False, cost_usd=None, result_subtype=None, tokens_in=None, tokens_out=None),
            self.rec("budget", True, cost_usd=1.0, result_subtype="error_max_budget_usd"),
            self.rec("harness_error", None, cost_usd=0.2, result_subtype="success"),
        ]
        out = summarize.table(records)
        line = next(l for l in out.splitlines() if l.startswith("| T1 |"))
        cells = [c.strip() for c in line.strip("|").split("|")]
        col = dict(zip(summarize.COLUMNS, cells))
        self.assertEqual(col["runs"], "5")
        self.assertEqual(col["passed"], "2/4")  # the timeout is a graded failure
        self.assertEqual((col["failed"], col["timeout"], col["budget"], col["harness err"]), ("1", "1", "1", "1"))
        self.assertTrue(col["pass rate (95% CI)"].startswith("50%"))
        self.assertEqual(col["cost $ (sum)"], "1.20*")
        self.assertEqual(col["$ per pass"], "0.600*")
        self.assertEqual(col["result subtypes"], "error_max_budget_usd 1, none 1, success 3")
        self.assertIn("cost unknown for 1 graded run(s)", out)
        self.assertIn("tokens unknown for 1 graded run(s)", out)
        self.assertIn("Harness-error runs cost $0.20", out)

    def test_rows_split_by_model_and_effort_and_arms_are_compared(self):
        records = [self.rec("passed", True)] * 6 + [self.rec("failed", False, arm="tmux")] * 6
        records += [self.rec("passed", True, effort="high")]
        out = summarize.table(records)
        self.assertEqual(sum(1 for l in out.splitlines() if l.startswith("| T1 | taria |")), 2)
        self.assertIn("| T1 | m | medium | 6/6 | 0/6 | 0.002 |", out)
        self.assertIn("n < 5 graded runs in T1/taria", out)  # the effort=high row has n=1

    def test_dry_runs_stay_out_of_the_headline(self):
        dry = {"task": "T1", "arm": "taria", "mode": "dry-run", "solver": "scripted",
               "status": "passed", "passed": True, "tool_calls_total": 2, "wall_ms": 80, "batch": "b"}
        out = summarize.render([dry])
        headline, _, rest = out.partition("## Dry runs")
        self.assertIn("No agent runs", headline)
        self.assertIn("| T1 | taria | - | - | 1 | 1/1 | 0 | 0 | 0 | 0 |", rest)
        self.assertNotIn("cost unknown", rest)

    def test_records_are_selected_explicitly(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        old, new = os.path.join(tmp, "a.jsonl"), os.path.join(tmp, "b.jsonl")
        with open(old, "w") as f:
            f.write(json.dumps(self.rec("passed", True, batch="old", ts="2026-01-01T00:00:00+00:00")) + "\n")
        with open(new, "w") as f:
            f.write(json.dumps(self.rec("failed", False, batch="new", ts="2026-02-01T00:00:00+00:00")) + "\n")
        original = summarize.RESULTS_DIR
        summarize.RESULTS_DIR = tmp
        self.addCleanup(setattr, summarize, "RESULTS_DIR", original)
        pick = lambda *argv: summarize.select(summarize.parse_args(list(argv)))  # noqa: E731
        records, note = pick()
        self.assertEqual([r["batch"] for r in records], ["new"])
        self.assertIn("1 other batch(es) left out", note)
        self.assertEqual([r["batch"] for r in pick("--batch", "old")[0]], ["old"])
        self.assertEqual(sorted(r["batch"] for r in pick("--all")[0]), ["new", "old"])
        self.assertEqual([r["batch"] for r in pick(old)[0]], ["old"])
        with self.assertRaises(SystemExit):
            pick("--batch", "missing")


if __name__ == "__main__":
    unittest.main()
