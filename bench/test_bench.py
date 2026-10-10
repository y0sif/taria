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
import harness  # noqa: E402
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
        self.assertEqual(c({"harness_error": "x", "timed_out": True}, self.GOOD), ("harness_error", None, "x"))
        # Every limit is one rule: a failure, whatever the dump says.
        for outcome, status in (
            ({"timed_out": True}, "timeout"),
            ({"result_subtype": "error_max_budget_usd"}, "budget"),
            ({"result_subtype": "error_max_turns"}, "max_turns"),
        ):
            for graded in (self.GOOD, self.BAD):
                with self.subTest(status=status, check=graded["check_passed"]):
                    self.assertEqual(c(outcome, graded), (status, False, None))

    def test_cut_off_results_are_not_harness_errors(self):
        for subtype in ("error_max_budget_usd", "error_max_turns"):
            self.assertIsNone(runner._result_problem({"subtype": subtype, "is_error": True}))
        self.assertIn("error_during_execution", runner._result_problem(
            {"subtype": "error_during_execution", "is_error": True}))

    def test_exit_codes(self):
        self.assertEqual(runner.exit_code(["passed", "passed"], [True, True]), 0)
        self.assertEqual(runner.exit_code(["passed", "timeout"], [True, False]), 1)
        self.assertEqual(runner.exit_code(["budget"], [False]), 1)
        self.assertEqual(runner.exit_code(["max_turns"], [False]), 1)
        self.assertEqual(runner.exit_code(["failed", "harness_error"], [False, False]), 2)

    def test_the_agent_env_is_an_allowlist(self):
        saved = dict(os.environ)
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(saved)))
        os.environ.update(CLAUDECODE="1", CLAUDE_CODE_ENTRYPOINT="cli", CLAUDE_EFFORT="xhigh",
                          CLAUDE_CODE_EFFORT_LEVEL="max", CLAUDE_CONFIG_DIR="/c", KEEP_ME="y",
                          ANTHROPIC_MODEL="m", NODE_OPTIONS="--x", LANG="C.UTF-8")
        env, dropped = runner.spend_env()
        self.assertLessEqual(set(env), runner.ENV_ALLOW)
        for name in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_EFFORT", "CLAUDE_CODE_EFFORT_LEVEL",
                     "CLAUDE_CONFIG_DIR", "KEEP_ME", "ANTHROPIC_MODEL", "NODE_OPTIONS"):
            self.assertNotIn(name, env)
            self.assertIn(name, dropped)
        self.assertEqual((env["PATH"], env["LANG"]), (os.environ["PATH"], "C.UTF-8"))
        self.assertEqual(sorted(dropped), dropped)
        self.assertNotIn("PATH", dropped)
        self.assertIn("ANTHROPIC_API_KEY", runner.ENV_ALLOW)  # the only credential --bare reads
        # What the harness sets itself goes on top, and is not "dropped".
        original = runner.HARNESS_ENV
        runner.HARNESS_ENV = {"BENCH_HARNESS_SET": "1", "KEEP_ME": "harness"}
        self.addCleanup(setattr, runner, "HARNESS_ENV", original)
        env, dropped = runner.spend_env()
        self.assertEqual((env["BENCH_HARNESS_SET"], env["KEEP_ME"]), ("1", "harness"))
        self.assertNotIn("KEEP_ME", dropped)

    def test_limits_are_recorded_and_null_for_a_dry_run(self):
        spend = runner.parse_args(["--spend", "--timeout", "30", "--budget-usd", "0.5", "--max-turns", "9"])
        self.assertEqual(runner.limits(spend), {"timeout_s": 30.0, "budget_usd": 0.5, "max_turns": 9})
        default = runner.limits(runner.parse_args(["--spend"]))
        self.assertEqual(default, {"timeout_s": runner.DEFAULT_TIMEOUT_S,
                                   "budget_usd": runner.DEFAULT_BUDGET_USD, "max_turns": None})
        dry = runner.limits(runner.parse_args(["--dry-run", "--timeout", "30"]))
        self.assertEqual(dry, {"timeout_s": None, "budget_usd": None, "max_turns": None})
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            runner.parse_args(["--spend", "--max-turns", "0"])


class Freshness(unittest.TestCase):
    """require_fresh_build against a made-up tree: each binary is held to the
    sources its cargo dep-info names, so a fresh binary cannot hide a stale one."""

    def setUp(self):
        # A space in the path: dep-info escapes it, and the parser must not split there.
        self.repo = tempfile.mkdtemp(prefix="fresh check ")
        self.addCleanup(shutil.rmtree, self.repo)
        files = {
            "Cargo.toml": "", "Cargo.lock": "",
            "crates/core/Cargo.toml": "", "crates/core/src/lib.rs": "",
            "crates/mcp/Cargo.toml": "", "crates/mcp/src/main.rs": "",
            "examples/demo/Cargo.toml": "", "examples/demo/src/main.rs": "",
            "target/debug/demo": "", "target/debug/mcp": "",
        }
        for rel, text in files.items():
            path = self.path(rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                f.write(text)
        self.demo, self.mcp = self.path("target/debug/demo"), self.path("target/debug/mcp")
        self.write_dep_info(self.demo, ["crates/core/src/lib.rs", "examples/demo/src/main.rs"])
        self.write_dep_info(self.mcp, ["crates/core/src/lib.rs", "crates/mcp/src/main.rs"])
        self.t0 = time.time() - 1000
        for rel in files:
            self.touch(rel, 0)
        self.touch("target/debug/demo", 10)
        self.touch("target/debug/mcp", 10)

    def path(self, rel):
        return os.path.join(self.repo, rel)

    def write_dep_info(self, binary, deps):
        esc = lambda p: p.replace(" ", "\\ ")  # noqa: E731
        line = f"{esc(binary)}: " + " ".join(esc(self.path(d)) for d in deps)
        with open(binary + ".d", "w") as f:
            f.write(line + "\n\n" + "".join(f"{esc(self.path(d))}:\n" for d in deps))

    def touch(self, rel, offset):
        os.utime(self.path(rel), (self.t0 + offset, self.t0 + offset))

    def check(self):
        harness.require_fresh_build(self.repo, (self.demo, self.mcp))

    def refused(self):
        with self.assertRaises(harness.HarnessError) as caught:
            self.check()
        return str(caught.exception)

    def test_a_fresh_tree_passes(self):
        self.check()
        self.assertEqual(harness.dep_info_sources(self.demo),
                         [self.path("crates/core/src/lib.rs"), self.path("examples/demo/src/main.rs")])

    def test_a_rebuilt_demo_cannot_hide_a_stale_mcp(self):
        self.touch("crates/mcp/src/main.rs", 20)
        self.touch("target/debug/demo", 30)  # newest binary, newer than every source
        message = self.refused()
        self.assertIn(os.path.join("target", "debug", "mcp"), message)
        self.assertIn(os.path.join("crates", "mcp", "src", "main.rs"), message)

    def test_a_shared_source_needs_both_binaries_relinked(self):
        self.touch("crates/core/src/lib.rs", 20)
        self.touch("target/debug/demo", 30)
        self.assertIn(os.path.join("target", "debug", "mcp"), self.refused())

    def test_a_binary_the_build_had_no_reason_to_relink_passes(self):
        """An edit to the demo's crate relinks only the demo; the older mcp is not stale."""
        self.touch("examples/demo/src/main.rs", 20)
        self.touch("target/debug/demo", 30)
        self.check()

    def test_manifests_are_dated_against_the_last_build(self):
        self.touch("Cargo.lock", 20)
        self.assertIn("Cargo.lock", self.refused())
        self.touch("target/debug/demo", 30)  # a build after the lock change
        self.check()

    def test_without_dep_info_a_binary_is_held_to_every_source(self):
        os.remove(self.mcp + ".d")
        self.assertIsNone(harness.dep_info_sources(self.mcp))
        self.touch("examples/demo/src/main.rs", 20)
        self.touch("target/debug/demo", 30)
        self.assertIn(os.path.join("target", "debug", "mcp"), self.refused())

    def test_a_missing_binary_is_refused(self):
        os.remove(self.mcp)
        self.assertIn("not built", self.refused())

    def test_the_real_dep_info_names_each_binarys_sources(self):
        for binary, own in ((harness.DEMO_BIN, "examples/demo-app/src/main.rs"),
                            (harness.MCP_BIN, "crates/taria-mcp/src/server.rs")):
            if not os.path.exists(binary + ".d"):
                self.skipTest(f"{binary}.d not built")
            sources = harness.dep_info_sources(binary)
            self.assertIn(os.path.join(REPO, own), sources)
            self.assertIn(os.path.join(REPO, "crates/taria/src/lib.rs"), sources)


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
        self.assertNotIn("--max-turns", cmd)  # not passed unless asked for
        cmd = runner.spend_command("Do it.", "/x/mcp.json", 1.0, "m", effort="high", max_turns=12)
        self.assertEqual(cmd[cmd.index("--effort") + 1], "high")
        self.assertEqual(cmd[cmd.index("--max-turns") + 1], "12")

    def test_an_init_the_guard_cannot_read_is_refused(self):
        with self.assertRaises(runner.GuardError):
            runner.StreamStats("taria").feed({**self.INIT_TARIA, "tools": 5})


@unittest.skipUnless(HAVE_TMUX, "tmux is not on PATH")
class FakeClaude(unittest.TestCase):
    """run.py's spend path end to end against a fake `claude`, live demo included."""

    SECRET = "s3cr3t-value-never-recorded"

    @classmethod
    def setUpClass(cls):
        require_fresh_build()
        cls.tmp = tempfile.mkdtemp(prefix="fake-claude-")
        os.symlink(FAKE_CLAUDE, os.path.join(cls.tmp, "claude"))
        cls.control = os.path.join(cls.tmp, "fake_claude.json")
        cls.saved_env = dict(os.environ)
        os.environ.pop("ANTHROPIC_API_KEY", None)  # never needed, never sent
        os.environ["PATH"] = cls.tmp + os.pathsep + os.environ["PATH"]
        # Markers a parent Claude Code session sets, and anything else a shell
        # exports: none may reach the agent.
        os.environ.update(CLAUDECODE="1", CLAUDE_CODE_FAKE_MARKER="1", BENCH_SECRET_MARKER=cls.SECRET)
        cls.log = os.path.join(cls.tmp, "argv.json")

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
        self.steer("success")

    def steer(self, mode, task="T1", solve=False):
        """The fake reads its orders from a file: no variable of ours reaches it."""
        with open(self.control, "w") as f:
            json.dump({"mode": mode, "task": task, "solve": solve, "log": self.log}, f)

    def spend(self, mode, arm="taria", task="T1", solve=False, timeout=20, argv=()):
        self.steer(mode, task, solve)
        args = runner.parse_args(["--spend", "--tasks", task, "--arms", arm, "--timeout",
                                  str(timeout), "--batch", "test-batch", *argv])
        started = time.monotonic()
        record = runner.run_one(tasks.TASKS[task], arm, args, 0)
        return record, time.monotonic() - started

    def spend_batch(self, mode, name, task_ids="T1,T2"):
        """run_all over two jobs: (exit code, records written, stderr)."""
        original = runner.has_api_key
        runner.has_api_key = lambda: True  # the fake needs no key; none is set
        self.addCleanup(setattr, runner, "has_api_key", original)
        self.steer(mode)
        results = os.path.join(self.tmp, f"{name}.jsonl")
        args = runner.parse_args(["--spend", "--tasks", task_ids, "--arms", "taria", "--results", results])
        err = io.StringIO()
        with redirect_stdout(io.StringIO()), redirect_stderr(err):
            code = runner.run_all(args)
        with open(results) as f:
            records = [json.loads(line) for line in f]
        return code, records, err.getvalue()

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
        self.assertEqual((record["timeout_s"], record["budget_usd"], record["max_turns"]),
                         (20.0, runner.DEFAULT_BUDGET_USD, None))
        # What the agent was started with.
        seen = self.fake_log()
        self.assertEqual(seen["pid"], seen["pgid"], "the agent leads its own process group")
        self.assertTrue(seen["cwd"].endswith("/work"))
        self.assertIn("--setting-sources=", seen["argv"])
        self.assertEqual(seen["argv"][seen["argv"].index("--effort") + 1], "medium")
        self.assertNotIn("--max-turns", seen["argv"])

    def test_the_agent_gets_only_the_allowlisted_environment(self):
        record, _ = self.spend("success")
        seen = set(self.fake_log()["env"])
        allowed = runner.ENV_ALLOW | set(runner.HARNESS_ENV)
        self.assertLessEqual(seen, allowed, f"reached the agent: {sorted(seen - allowed)}")
        self.assertIn("PATH", seen)
        dropped = record["env_dropped"]
        for name in ("CLAUDECODE", "CLAUDE_CODE_FAKE_MARKER", "BENCH_SECRET_MARKER"):
            self.assertIn(name, dropped)
        self.assertEqual(sorted(seen | set(dropped)), sorted(os.environ), "a name went unaccounted for")
        self.assertNotIn(self.SECRET, json.dumps(record), "a dropped value was recorded")

    def test_limits_are_recorded_and_passed(self):
        record, _ = self.spend("success", argv=("--max-turns", "7", "--budget-usd", "0.5"))
        self.assertEqual((record["timeout_s"], record["budget_usd"], record["max_turns"]), (20.0, 0.5, 7))
        argv = self.fake_log()["argv"]
        self.assertEqual(argv[argv.index("--max-turns") + 1], "7")
        self.assertEqual(argv[argv.index("--max-budget-usd") + 1], "0.5")
        # And the summary never pools runs held to different limits.
        other, _ = self.spend("success", argv=("--budget-usd", "0.5"))
        table = summarize.table([record, other])
        rows = [line for line in table.splitlines() if line.startswith("| T1 | taria |")]
        self.assertEqual(len(rows), 2, table)
        self.assertTrue(any("| 20s, $0.5, 7 turns |" in r for r in rows), rows)
        self.assertTrue(any("| 20s, $0.5, no turn cap |" in r for r in rows), rows)

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
        self.assertIs(record["check_passed"], False)  # graded all the same
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
        self.assertEqual(record["cost_usd"], 0.01, "the denied run's spend is kept")

    def test_an_error_result_is_a_harness_error(self):
        record, _ = self.spend("api_error")
        self.assertEqual(record["status"], "harness_error")
        self.assertIn("API Error: 404", record["error"])

    def test_a_budget_stop_fails_with_its_dump_graded(self):
        record, _ = self.spend("budget", arm="tmux", solve=True)
        self.assertEqual(record["status"], "budget")
        self.assertIs(record["passed"], False, "a cut-off run never passes")
        self.assertIs(record["check_passed"], True, "the dump is still graded")
        self.assertIsNone(record["error"])
        self.assertEqual(record["cost_usd"], 1.03)
        record, _ = self.spend("budget")
        self.assertEqual(record["status"], "budget")
        self.assertIs(record["check_passed"], False)

    def test_a_max_turns_stop_fails_with_its_dump_graded(self):
        """error_max_turns carries is_error, as the budget stop does; it is the
        agent hitting a limit, not a harness error."""
        record, _ = self.spend("max_turns", arm="tmux", solve=True, argv=("--max-turns", "3"))
        self.assertEqual(record["status"], "max_turns", record.get("error"))
        self.assertIs(record["passed"], False)
        self.assertIs(record["check_passed"], True)
        self.assertIsNone(record["error"])
        self.assertEqual((record["result_subtype"], record["max_turns"], record["cost_usd"]),
                         ("error_max_turns", 3, 0.2))
        unsolved, _ = self.spend("max_turns", arm="tmux", argv=("--max-turns", "3"))
        self.assertEqual((unsolved["status"], unsolved["check_passed"]), ("max_turns", False))
        line = next(l for l in summarize.table([record, unsolved]).splitlines() if l.startswith("| T1 |"))
        col = dict(zip(summarize.COLUMNS, (c.strip() for c in line.strip("|").split("|"))))
        self.assertEqual((col["passed"], col["max turns"]), ("0/2", "2"))
        self.assertEqual(col["cut off"], "2 (1 with correct state)")

    def test_a_grade_or_quit_failure_keeps_the_runs_cost(self):
        def broken(*_args, **_kwargs):
            raise RuntimeError("grading fell over")

        for target, attr in ((runner, "grade"), (runner.DemoRun, "quit")):
            with self.subTest(failing=attr):
                original = getattr(target, attr)
                setattr(target, attr, broken)
                try:
                    record, _ = self.spend("success")
                finally:
                    setattr(target, attr, original)
                self.assertEqual(record["status"], "harness_error")
                self.assertIsNone(record["passed"])
                self.assertIn("RuntimeError: grading fell over", record["error"])
                self.assertEqual(record["cost_usd"], 0.01)
                self.assertEqual(record["usage"], {"input": 12, "output": 34, "cache_read": 100,
                                                   "cache_creation": 1000})
                self.assertEqual(record["result_subtype"], "success")
                self.assertIn("tool_calls_total", record)

    def test_a_guard_failure_stops_the_batch_with_its_record_written(self):
        code, records, err = self.spend_batch("plugins", "guard")
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertEqual(len(records), 1, "the batch went on after the guard fired")
        self.assertTrue(records[0]["guard_failed"])
        self.assertIn("at its init event", err)
        self.assertIn("1 run(s) not started", err)

    def test_a_permission_denial_stops_the_batch(self):
        """A misconfigured --allowedTools would refuse every run alike: one paid
        run says so, the rest are never started."""
        code, records, err = self.spend_batch("denied", "denied")
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertEqual(len(records), 1, "the batch went on after a permission denial")
        self.assertEqual(records[0]["permission_denials"], 1)
        self.assertIn("--allowedTools", err)
        self.assertIn("1 run(s) not started", err)

    def test_a_cut_off_batch_runs_to_the_end(self):
        code, records, _ = self.spend_batch("max_turns", "turns")
        self.assertEqual(code, runner.EXIT_FAILED)
        self.assertEqual([r["status"] for r in records], ["max_turns", "max_turns"])

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

    @staticmethod
    def columns(out, prefix="| T1 |"):
        line = next(l for l in out.splitlines() if l.startswith(prefix))
        return dict(zip(summarize.COLUMNS, (c.strip() for c in line.strip("|").split("|"))))

    def test_outcomes_counted_and_every_cut_off_fails(self):
        records = [
            self.rec("passed", True, check_passed=True, demo_alive=True),
            self.rec("failed", False, check_passed=False, demo_alive=True),
            self.rec("timeout", False, check_passed=False, demo_alive=True, cost_usd=None,
                     result_subtype=None, tokens_in=None, tokens_out=None, turns=None),
            self.rec("budget", False, check_passed=True, demo_alive=True, cost_usd=1.0,
                     result_subtype="error_max_budget_usd"),
            self.rec("max_turns", False, check_passed=True, demo_alive=False,
                     result_subtype="error_max_turns"),
            self.rec("harness_error", None, cost_usd=0.2, result_subtype="success"),
        ]
        out = summarize.table(records)
        col = self.columns(out)
        self.assertEqual(col["runs"], "6")
        self.assertEqual(col["passed"], "1/5")  # every cut-off run is a graded failure
        self.assertEqual(
            (col["failed"], col["timeout"], col["budget"], col["max turns"], col["harness err"]),
            ("1", "1", "1", "1", "1"),
        )
        # The budget run had the right state; the max-turns run's demo had died.
        self.assertEqual(col["cut off"], "3 (1 with correct state)")
        self.assertTrue(col["pass rate (95% CI)"].startswith("20%"))
        self.assertEqual(col["cost $ (sum)"], "1.30*")
        self.assertEqual(col["$ per pass"], "1.300*")
        self.assertEqual(col["result subtypes"],
                         "error_max_budget_usd 1, error_max_turns 1, none 1, success 3")
        self.assertIn("cost unknown for 1 graded run(s)", out)
        self.assertIn("tokens unknown for 1 graded run(s)", out)
        self.assertIn("turns unknown for 1 graded run(s), 1 of them timeouts", out)
        self.assertIn("Harness-error runs cost $0.20", out)
        self.assertIn("fails whatever its dump says", out)

    def test_cost_per_pass_with_no_passes(self):
        col = self.columns(summarize.table([self.rec("failed", False)] * 2))
        self.assertEqual((col["cost $ (sum)"], col["$ per pass"]), ("0.20", "n/a (0 passes)"))
        col = self.columns(summarize.table([self.rec("failed", False, cost_usd=None)]))
        self.assertEqual(col["$ per pass"], "-")

    def test_an_unpriced_harness_error_is_said(self):
        out = summarize.table([self.rec("harness_error", None, cost_usd=None, guard_failed=True)])
        self.assertIn("1 harness-error agent run(s) ended before a result event", out)

    def test_rows_split_by_model_and_effort_and_arms_are_compared(self):
        records = [self.rec("passed", True)] * 6 + [self.rec("failed", False, arm="tmux")] * 6
        records += [self.rec("passed", True, effort="high")]
        out = summarize.table(records)
        self.assertEqual(sum(1 for l in out.splitlines() if l.startswith("| T1 | taria |")), 2)
        self.assertIn("| T1 | m | medium | - | 6/6 | 0/6 | 0.002 |", out)
        self.assertIn("n < 5 graded runs in T1/taria [m, high]", out)  # n=1 there

    def test_rows_and_comparisons_split_by_limits(self):
        lim = {"timeout_s": 600.0, "budget_usd": 1.0, "max_turns": None}
        tight = {**lim, "max_turns": 10}
        records = [self.rec("passed", True, **lim)] * 5 + [self.rec("failed", False, arm="tmux", **lim)] * 5
        records += [self.rec("failed", False, **tight)] * 5 + [self.rec("passed", True, arm="tmux", **tight)] * 5
        records += [self.rec("passed", True, **{**lim, "budget_usd": 2.0})]
        records += [self.rec("passed", True, **{**lim, "timeout_s": 300.0})]
        out = summarize.table(records)
        taria = [l for l in out.splitlines() if l.startswith("| T1 | taria |")]
        self.assertEqual(len(taria), 4, out)
        self.assertEqual(sorted(self.columns(l, "|")["limits"] for l in taria),
                         ["300s, $1, no turn cap", "600s, $1, 10 turns", "600s, $1, no turn cap",
                          "600s, $2, no turn cap"])
        self.assertIn("| T1 | m | medium | 600s, $1, no turn cap | 5/5 | 0/5 | 0.008 |", out)
        self.assertIn("| T1 | m | medium | 600s, $1, 10 turns | 0/5 | 5/5 | 0.008 |", out)
        self.assertIn("T1/taria [m, medium, 600s, $2, no turn cap]", out)  # n < 5

    def test_provenance_names_the_limits(self):
        records = [
            self.rec("passed", True, timeout_s=600.0, budget_usd=1.0, max_turns=None),
            self.rec("passed", True, batch="b2", timeout_s=300.0, budget_usd=0.5, max_turns=20),
            self.rec("passed", True, batch="old"),  # from before limits were recorded
        ]
        lines = summarize.provenance(records)
        row = lambda b: next(l for l in lines if l.startswith(f"| {b} |"))  # noqa: E731
        self.assertIn("| 600 | 1 | none |", row("b1"))
        self.assertIn("| 300 | 0.5 | 20 |", row("b2"))
        self.assertIn("| unrecorded | unrecorded | unrecorded |", row("old"))
        self.assertTrue(any("predate recorded limits" in l for l in lines))
        self.assertIn("timeout s | budget $ | max turns", lines[0])

    def test_dry_runs_stay_out_of_the_headline(self):
        dry = {"task": "T1", "arm": "taria", "mode": "dry-run", "solver": "scripted",
               "status": "passed", "passed": True, "tool_calls_total": 2, "wall_ms": 80, "batch": "b",
               "timeout_s": None, "budget_usd": None, "max_turns": None}
        out = summarize.render([dry])
        headline, _, rest = out.partition("## Dry runs")
        self.assertIn("No agent runs", headline)
        self.assertIn("| T1 | taria | - | - | - | 1 | 1/1 | 0 | 0 | 0 | 0 | 0 | 0 |", rest)
        self.assertNotIn("cost unknown", rest)
        self.assertNotIn("turns unknown", rest)

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
