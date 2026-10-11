#!/usr/bin/env python3
"""Tests for the benchmark harness itself. Stdlib unittest, no model.

The live tests start a real taria-demo in a private tmux server and drive it
through either arm's MCP server over stdio, exactly as an agent would. They
need tmux and a fresh `cargo build --workspace`.

The subagent path is rehearsed end to end without a model (SubagentRehearsal):
plan a batch, write the agent files, `next` prepares a slot, the agent file's
own launcher command is started by hand and driven by the scripted solver,
and `next --agent-id` grades it from a hand-written transcript read by the
real bench/transcript.py.

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
import types
import unittest
from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timedelta, timezone
from unittest import mock

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
from harness import COLS, REPO, ROWS, DemoRun, McpStdio, mcp_config, require_fresh_build  # noqa: E402
import agents  # noqa: E402
import harness  # noqa: E402
import run as runner  # noqa: E402
import solvers  # noqa: E402
import summarize  # noqa: E402
import tasks  # noqa: E402
import tmux_mcp  # noqa: E402

try:
    import transcript  # noqa: E402
except ImportError:  # Lane B's module; the rehearsal needs it
    transcript = None

INPUT_FOCUSED = "[Enter] add task"
HAVE_TMUX = shutil.which("tmux") is not None
RUN_PY = os.path.join(BENCH, "run.py")
MCP_LAUNCH = os.path.join(BENCH, "mcp_launch.py")


def quiet_cli(*argv):
    """runner.cli(argv) with its output captured: (code, stdout, stderr)."""
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        code = runner.cli(list(argv))
    return code, out.getvalue(), err.getvalue()


def run_py(*argv, timeout=120):
    """bench/run.py in its own process, as the orchestrator runs it."""
    return subprocess.run(
        [sys.executable, RUN_PY, *argv], capture_output=True, text=True, timeout=timeout,
        stdin=subprocess.DEVNULL,
    )


def read_rows(results, batch):
    with open(os.path.join(results, batch, "runs.jsonl")) as f:
        return [json.loads(line) for line in f]


def _running(pid):
    """Whether `pid` is a live process (a zombie is not)."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            state = f.read().rsplit(")", 1)[1].split()[0]
    except OSError:
        return False
    return state != "Z"


def wait_gone(pid, timeout=3.0):
    deadline = time.monotonic() + timeout
    while _running(pid):
        if time.monotonic() > deadline:
            return False
        time.sleep(0.05)
    return True


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
        self.assertEqual(runner.classify(graded), ("failed", False))

    def test_a_harness_exception_is_a_harness_error_and_exits_2(self):
        def broken(*_args, **_kwargs):
            raise RuntimeError("the solver fell over")

        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        with mock.patch.object(runner, "solve", broken):
            code, out, _ = quiet_cli("dry-run", "--tasks", "T1", "--arms", "tmux",
                                     "--results-dir", tmp, "--batch", "broken")
        self.assertEqual(code, runner.EXIT_HARNESS)
        (row,) = read_rows(tmp, "broken")
        self.assertEqual(row["status"], "harness_error")
        self.assertIn("RuntimeError: the solver fell over", row["error"])
        self.assertEqual(json.loads(out), row, "the printed line is the recorded row")
        self.assertEqual(list(row), list(runner.ROW_KEYS))
        self.assertEqual((row["mode"], row["solver"], row["job"]), ("dry-run", "scripted", "j001"))


class Outcomes(unittest.TestCase):
    GOOD = {"check_passed": True, "demo_alive": True}
    BAD = {"check_passed": False, "demo_alive": True}

    def test_classify(self):
        c = runner.classify
        self.assertEqual(c(self.GOOD), ("passed", True))
        self.assertEqual(c(self.BAD), ("failed", False))
        self.assertEqual(c({"check_passed": True, "demo_alive": False}), ("failed", False))
        self.assertEqual(c(self.GOOD, error="x", cut_off=True), ("harness_error", None))
        # A cut-off run failed, whatever the dump says.
        for graded in (self.GOOD, self.BAD):
            self.assertEqual(c(graded, cut_off=True), ("max_turns", False))

    def test_rows_have_every_key(self):
        row = runner.new_row(task="T1")
        self.assertEqual(list(row), list(runner.ROW_KEYS))
        self.assertIsNone(row["tokens"])
        with self.assertRaises(KeyError):
            runner.new_row(cost_usd=1)

    def test_statuses(self):
        self.assertEqual(runner.STATUSES, ("passed", "failed", "max_turns", "harness_error"))


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
        for rel in ("target/debug/demo", "target/debug/mcp", "target/debug/demo.d", "target/debug/mcp.d"):
            self.touch(rel, 10)

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

    def test_a_build_that_relinked_nothing_still_clears_a_touched_lock(self):
        """A checkout rewrites Cargo.lock as it was: cargo builds nothing, so
        neither binary is relinked, but it rewrites each dep-info file, and
        that build must be accepted. Before, it was refused forever."""
        self.touch("Cargo.lock", 20)
        self.assertIn("Cargo.lock", self.refused())
        self.touch("target/debug/demo.d", 30)
        self.touch("target/debug/mcp.d", 30)
        self.check()

    def test_a_dep_info_file_does_not_excuse_stale_code(self):
        """The stamp dates manifests only: a source newer than its binary is
        still refused, however new the dep-info file is."""
        self.touch("crates/mcp/src/main.rs", 20)
        self.touch("target/debug/mcp.d", 30)
        self.assertIn(os.path.join("target", "debug", "mcp"), self.refused())

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


# --- Plans, claims and slots -----------------------------------------------------------


class Plan(unittest.TestCase):
    def setUp(self):
        self.results = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.results)

    def plan(self, batch, *argv):
        code, out, err = quiet_cli("plan", "--batch", batch, "--results-dir", self.results, *argv)
        return code, out + err

    def load(self, batch):
        with open(os.path.join(self.results, batch, "plan.json")) as f:
            return json.load(f)

    def test_the_shuffle_is_the_seeds(self):
        argv = ("--tasks", "T1,T2,T3", "--models", "sonnet,haiku", "--runs", "2", "--seed", "11")
        self.assertEqual(self.plan("a", *argv)[0], 0)
        self.assertEqual(self.plan("b", *argv)[0], 0)
        self.assertEqual(self.plan("c", *argv[:-1], "12")[0], 0)
        a, b, c = self.load("a"), self.load("b"), self.load("c")
        strip = lambda p: [(j["task"], j["arm"], j["model"], j["run"]) for j in p["jobs"]]  # noqa: E731
        self.assertEqual(strip(a), strip(b))
        self.assertNotEqual(strip(a), strip(c))
        self.assertEqual(len(a["jobs"]), 3 * 2 * 2 * 2)
        self.assertEqual(sorted(strip(a)), sorted(strip(c)), "the same jobs, in another order")
        self.assertEqual([j["id"] for j in a["jobs"]], [f"j{n:03d}" for n in range(1, 25)])
        self.assertEqual((a["seed"], a["effort"], a["max_turns"]), (11, "medium", 30))
        self.assertEqual(a["agent_body_sha256"], agents.body_sha256())
        self.assertEqual(a["models"], ["sonnet", "haiku"])

    def test_an_existing_plan_is_refused(self):
        self.assertEqual(self.plan("x", "--tasks", "T1", "--seed", "1")[0], 0)
        before = self.load("x")
        code, text = self.plan("x", "--tasks", "T2", "--seed", "2")
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("exists", text)
        self.assertEqual(self.load("x"), before)

    def test_bad_arguments_exit_2(self):
        self.assertEqual(self.plan("y", "--models", "gpt")[0], runner.EXIT_HARNESS)
        self.assertEqual(self.plan("../z")[0], runner.EXIT_HARNESS)
        self.assertEqual(self.plan("slots")[0], runner.EXIT_HARNESS)


class Claims(unittest.TestCase):
    """Two `next` commands never claim the same job: the batch lock covers
    reading rows and slots and writing the new slot file."""

    def setUp(self):
        self.results = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.results)
        code, _, err = quiet_cli("plan", "--batch", "b", "--results-dir", self.results,
                                 "--tasks", "T1,T2,T3,T4,T5", "--arms", "taria", "--seed", "3")
        self.assertEqual(code, 0, err)

    def test_concurrent_claims_never_share_a_job(self):
        script = (
            "import json, sys; sys.path.insert(0, sys.argv[1]); import run\n"
            "paths = run.Paths(sys.argv[2], 'b'); plan = run.load_plan(paths)\n"
            "job, slot = run.claim(paths, plan, int(sys.argv[3]))\n"
            "print(json.dumps(job and job['id']))\n"
        )
        procs = [
            subprocess.Popen([sys.executable, "-c", script, BENCH, self.results, str(k)],
                             stdout=subprocess.PIPE, text=True)
            for k in range(1, 9)
        ]
        claimed = [json.loads(p.communicate(timeout=60)[0]) for p in procs]
        jobs = [j for j in claimed if j is not None]
        self.assertEqual(len(jobs), 5, claimed)
        self.assertEqual(len(set(jobs)), 5, f"a job was claimed twice: {claimed}")
        self.assertEqual(claimed.count(None), 3)
        paths = runner.Paths(self.results, "b")
        held = runner.read_all_slots(paths)
        self.assertEqual(sorted(s["job"] for s in held.values()), sorted(jobs))

    def test_a_recorded_job_is_not_claimed_and_a_held_slot_is_refused(self):
        paths = runner.Paths(self.results, "b")
        plan = runner.load_plan(paths)
        first = plan["jobs"][0]["id"]
        runner.append_row(paths, runner.new_row(job=first, status="passed"))
        job, slot = runner.claim(paths, plan, 1)
        self.assertEqual(job["id"], plan["jobs"][1]["id"])
        self.assertEqual((slot["slot"], slot["state"], slot["arm"]), (1, "claimed", "taria"))
        with self.assertRaises(runner.CommandError):
            runner.claim(paths, plan, 1)

    def test_next_refuses_a_held_slot_without_an_agent_id(self):
        paths = runner.Paths(self.results, "b")
        plan = runner.load_plan(paths)
        _, slot = runner.claim(paths, plan, 2)
        slot["state"] = "ready"
        runner.write_slot(paths, 2, slot)
        code, out, err = quiet_cli("next", "--batch", "b", "--slot", "2", "--results-dir", self.results)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn(slot["job"], json.loads(out)["error"])
        self.assertIn("--agent-id", err)
        # And a slot of another batch is not this batch's to grade.
        code, out, _ = quiet_cli("plan", "--batch", "other", "--results-dir", self.results, "--tasks", "T1")
        code, out, _ = quiet_cli("next", "--batch", "other", "--slot", "2", "--agent-id", "a1",
                                 "--results-dir", self.results)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("of batch b", json.loads(out)["error"])

    def test_an_agent_id_for_an_empty_slot_is_refused(self):
        code, out, _ = quiet_cli("next", "--batch", "b", "--slot", "3", "--agent-id", "nobody",
                                 "--results-dir", self.results)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("holds no job", json.loads(out)["error"])
        code, out, _ = quiet_cli("next", "--batch", "b", "--slot", "3", "--agent-id", "../x",
                                 "--results-dir", self.results)
        self.assertEqual(code, runner.EXIT_HARNESS)


class SlotFields(unittest.TestCase):
    def fields(self, tmpdir="/tmp/tui-abc123"):
        return {
            "name": "taria-bench-1", "tmpdir": tmpdir, "workdir": f"{tmpdir}/work",
            "sock": f"{tmpdir}/s/app.sock", "tmux_sock": f"{tmpdir}/s/tmux.sock",
            "state_path": f"{tmpdir}/state.json", "tmux_conf": f"{tmpdir}/tmux.conf",
            "server_pid": 11, "demo_pid": 12,
        }

    def test_round_trip(self):
        run = DemoRun.from_slot(json.loads(json.dumps(self.fields())))
        self.assertEqual(run.to_slot(), self.fields())
        self.assertEqual(run.tmux, ["tmux", "-S", "/tmp/tui-abc123/s/tmux.sock", "-f", "/tmp/tui-abc123/tmux.conf"])

    def test_fields_that_are_not_ours_are_refused(self):
        """close() removes tmpdir, so a slot file cannot point it elsewhere."""
        bad = {
            "missing": {k: v for k, v in self.fields().items() if k != "sock"},
            "not tui-": self.fields("/home/user"),
            "relative": self.fields("tui-abc"),
            "outside": {**self.fields(), "sock": "/tmp/tui-abc1234/s/app.sock"},
            "pid": {**self.fields(), "demo_pid": "12"},
        }
        for name, fields in bad.items():
            with self.subTest(case=name), self.assertRaises(harness.HarnessError):
                DemoRun.from_slot(fields)


@unittest.skipUnless(HAVE_TMUX, "tmux is not on PATH")
class SlotLive(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        require_fresh_build()

    def test_another_process_can_reach_and_close_a_demo(self):
        demo = DemoRun()
        self.addCleanup(demo.close)
        demo.start()
        demo.wait_ready_for_agent()
        self.assertTrue(demo.socket_accepts())
        fields = json.loads(json.dumps(demo.to_slot()))
        script = (
            "import json, sys; sys.path.insert(0, sys.argv[1]); from harness import DemoRun\n"
            "run = DemoRun.from_slot(json.loads(sys.argv[2]))\n"
            "print(json.dumps([run.demo_alive(), run.read_state()['tab'], 'Active' in run.capture()]))\n"
            "run.close()\n"
        )
        out = subprocess.run([sys.executable, "-c", script, BENCH, json.dumps(fields)],
                             capture_output=True, text=True, timeout=30)
        self.assertEqual(json.loads(out.stdout), [True, "active", True], out.stderr)
        self.assertTrue(wait_gone(demo.demo_pid), "the demo outlived close() from another process")
        self.assertFalse(_running(demo.server_pid))
        self.assertFalse(os.path.exists(demo.tmpdir))

    def test_a_reused_pid_is_not_ours(self):
        """The demo is proved by its socket in its environment, not its name alone."""
        demo = DemoRun()
        self.addCleanup(demo.close)
        demo.start()
        self.assertTrue(harness._is_ours(demo.demo_pid, harness.DEMO_BIN, f"TARIA_SOCK={demo.sock}"))
        self.assertFalse(harness._is_ours(demo.demo_pid, harness.DEMO_BIN, "TARIA_SOCK=/elsewhere"))
        self.assertFalse(harness._is_ours(os.getpid(), harness.DEMO_BIN))


# --- Grading an agent run -----------------------------------------------------------------


class FakeRun:
    """Stands in for a DemoRun: a dump, a liveness, a quit."""

    def __init__(self, state, alive=True):
        self.state, self.alive, self.quits = state, alive, 0

    def settled_state(self):
        return self.state

    def demo_alive(self):
        return self.alive

    def quit(self):
        self.quits += 1
        self.state = {**self.state, "quit": True}
        return True

    def read_state(self):
        return self.state


class GradeAgentRun(unittest.TestCase):
    PLAN = {"batch": "b", "seed": 5, "effort": "medium", "max_turns": 30}
    SLOT = {"slot": 2, "batch": "b", "job": "j004", "task": "T1", "arm": "taria", "model": "sonnet",
            "run": 1, "demo": {}}
    PROV = {"git_sha": "abc", "git_dirty": False}
    GOOD_STATE = tasks.self_test_cases()["T1"][0][0]
    STATS = {
        "agent_id": "a1", "session_id": "s1", "models": ["claude-sonnet-5-5"], "turns": 4,
        "tool_calls": {"mcp__tui__read_tree": 1, "mcp__tui__act": 2}, "tool_calls_total": 3,
        "toolsearch_calls": 1, "tokens": {"input": 1, "cache_creation": 2, "cache_read": 3, "output": 4},
        "wall_ms": 9000, "final_text": "Added it.", "ended_on_tool_use": False, "tool_errors": 0,
        "context": {"claude_md_files": []}, "efforts": ["medium"], "claude_versions": ["2.1.294"],
        "last_model": "claude-sonnet-5-5", "last_stop_reason": "end_turn", "ended_synthetic": False,
        "synthetic_text": None, "synthetic_error": None, "max_turns_reached": None,
    }

    def module(self, stats=None, problems=(), path="/x/agent-a1.jsonl"):
        seen = {}

        def guard(stats, arm, family, arm_tools, effort=None):
            seen.update(arm=arm, family=family, arm_tools=arm_tools, effort=effort)
            return list(problems)

        return types.SimpleNamespace(
            find_transcript=lambda agent_id: path,
            read_transcript=lambda p: dict(stats or self.STATS),
            guard=guard,
            ending=transcript.ending,
        ), seen

    def grade(self, state=None, alive=True, **module_args):
        mod, seen = self.module(**module_args)
        run = FakeRun(state if state is not None else self.GOOD_STATE, alive)
        row = runner.grade_agent_run(self.PLAN, self.SLOT, "a1", run=run, transcript_mod=mod, prov=self.PROV)
        return row, seen, run

    def test_passed(self):
        row, seen, run = self.grade()
        self.assertEqual((row["status"], row["passed"], row["check_passed"]), ("passed", True, True))
        self.assertEqual(seen, {"arm": "taria", "family": "sonnet", "arm_tools": harness.ARM_TOOLS["taria"],
                                "effort": "medium"})
        self.assertEqual(list(row), list(runner.ROW_KEYS))
        self.assertEqual((row["mode"], row["solver"], row["model"], row["model_resolved"]),
                         ("subagent", "agent", "sonnet", "claude-sonnet-5-5"))
        self.assertEqual((row["effort"], row["efforts_resolved"], row["max_turns"]), ("medium", ["medium"], 30))
        self.assertEqual((row["claude_version"], row["claude_versions"]), ("2.1.294", ["2.1.294"]))
        self.assertEqual((row["job"], row["run"], row["seed"], row["agent_id"], row["session_id"]),
                         ("j004", 1, 5, "a1", "s1"))
        self.assertEqual((row["tool_calls_total"], row["toolsearch_calls"], row["turns"]), (3, 1, 4))
        self.assertEqual(row["guard_problems"], [])
        self.assertEqual(row["demo_exit"], "clean")
        self.assertEqual(run.quits, 1)

    def test_failed(self):
        row, _, _ = self.grade(state=tasks.SEED_STATE)
        self.assertEqual((row["status"], row["passed"]), ("failed", False))
        self.assertIn("no new task", row["reason"])
        row, _, _ = self.grade(alive=False)
        self.assertEqual((row["status"], row["demo_exit"]), ("failed", "crashed"))

    def test_max_turns(self):
        cut = {**self.STATS, "ended_on_tool_use": True, "last_stop_reason": "tool_use", "turns": 30}
        row, _, _ = self.grade(stats=cut)
        self.assertEqual((row["status"], row["passed"], row["check_passed"]), ("max_turns", False, True))
        self.assertIsNone(row["error"])
        # Under the cap, a run that stopped on a tool call did not end on its own.
        row, _, _ = self.grade(stats={**cut, "turns": 29})
        self.assertEqual((row["status"], row["passed"]), ("harness_error", None))
        self.assertIn("did not end on its own: its last message stopped on 'tool_use'", row["error"])
        self.assertIs(row["check_passed"], True, "the dump is still graded")

    def test_a_run_that_died_on_claude_codes_error_is_a_harness_error(self):
        died = {**self.STATS, "last_model": "<synthetic>", "last_stop_reason": "stop_sequence",
                "ended_synthetic": True, "synthetic_text": "You've hit your session limit",
                "synthetic_error": "rate_limit"}
        row, _, _ = self.grade(stats=died)
        self.assertEqual((row["status"], row["passed"]), ("harness_error", None))
        self.assertIn("not a model reply (rate_limit): \"You've hit your session limit\"", row["error"])
        row, _, _ = self.grade(stats={**self.STATS, "last_stop_reason": None})
        self.assertEqual(row["status"], "harness_error")
        self.assertIn("never finished streaming", row["error"])

    def test_guard_problems_are_a_harness_error(self):
        row, _, _ = self.grade(problems=["called Bash (1x), which is not a taria arm tool"])
        self.assertEqual((row["status"], row["passed"]), ("harness_error", None))
        self.assertEqual(row["guard_problems"], ["called Bash (1x), which is not a taria arm tool"])
        self.assertIn("guard: called Bash", row["error"])
        self.assertIs(row["check_passed"], True, "the dump is still graded")

    def test_a_missing_transcript_is_a_harness_error(self):
        row, _, _ = self.grade(path=None)
        self.assertEqual(row["status"], "harness_error")
        self.assertIn("no transcript for agent a1", row["error"])
        self.assertIsNone(row["turns"])

    def test_an_unreadable_transcript_is_a_harness_error(self):
        mod, _ = self.module()
        mod.read_transcript = lambda p: (_ for _ in ()).throw(OSError("gone"))
        row = runner.grade_agent_run(self.PLAN, self.SLOT, "a1", run=FakeRun(self.GOOD_STATE),
                                     transcript_mod=mod, prov=self.PROV)
        self.assertEqual(row["status"], "harness_error")
        self.assertIn("OSError: gone", row["error"])

    def test_another_slots_agent_is_a_harness_error(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "agent-a1.jsonl")
        open(path, "w").close()
        with open(os.path.join(tmp, "agent-a1.meta.json"), "w") as f:
            json.dump({"agentType": "tui-taria-1"}, f)
        row, _, _ = self.grade(path=path)
        self.assertEqual(row["status"], "harness_error")
        self.assertIn("'tui-taria-1', not this slot's agent 'tui-taria-2'", row["error"])

    def real(self, name, slot=None, state=None, **plan):
        """Grade a real (scrubbed) fixture through the real transcript module."""
        path = os.path.join(BENCH, "fixtures", "transcripts", name)
        slot = slot or {**self.SLOT, "slot": 1, "task": "T3", "model": "sonnet"}
        state = state if state is not None else tasks.self_test_cases()[slot["task"]][0][0]
        return runner.grade_agent_run({**self.PLAN, **plan}, slot, "a1", run=FakeRun(state),
                                      transcript_mod=transcript, transcript_file=path, prov=self.PROV)

    def test_a_real_clean_run_passes(self):
        row = self.real("real_taria_clean.jsonl")
        self.assertEqual((row["status"], row["guard_problems"], row["error"]), ("passed", [], None))
        self.assertEqual(row["context"]["offered_tools"], sorted("mcp__tui__" + t for t in harness.ARM_TOOLS["taria"]))
        row = self.real("real_tmux_clean.jsonl",
                        slot={**self.SLOT, "slot": 1, "task": "T1", "arm": "tmux", "model": "haiku"})
        self.assertEqual((row["status"], row["guard_problems"]), ("passed", []))

    def test_a_real_leaked_run_is_a_harness_error(self):
        row = self.real("real_taria_leaked.jsonl", slot={**self.SLOT, "slot": 1, "task": "T1", "model": "haiku"})
        self.assertEqual(row["status"], "harness_error")
        self.assertIn("instructions of 4 MCP server(s) besides tui: furcule, plugin:context7:context7, "
                      "plugin:posthog:posthog, taria", row["error"])

    def test_the_plans_effort_is_held_against_a_real_run(self):
        row = self.real("real_taria_clean.jsonl", effort="high")
        self.assertEqual(row["status"], "harness_error")
        self.assertEqual(row["guard_problems"], ["ran at effort medium, not the plan's high"])

    def test_an_agent_mcp_launch_refused_is_a_harness_error(self):
        """mcp_launch refusing (a second launch of a job) leaves the agent
        without tools: the first snapshot only, no tui instructions."""
        with open(os.path.join(BENCH, "fixtures", "transcripts", "real_taria_clean.jsonl")) as f:
            lines = [json.loads(line) for line in f]
        first = next(i for i, l in enumerate(lines)
                     if l["type"] == "attachment" and l["attachment"]["type"] == "prompt_snapshot")
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "agent-a1.jsonl")
        with open(path, "w") as f:
            f.writelines(json.dumps(l) + "\n" for l in lines[: first + 1] + [lines[-1]])
        row = runner.grade_agent_run(self.PLAN, {**self.SLOT, "slot": 1, "task": "T3"}, "a1",
                                     run=FakeRun(tasks.SEED_STATE), transcript_mod=transcript,
                                     transcript_file=path, prov=self.PROV)
        self.assertEqual(row["status"], "harness_error")
        self.assertIn("was not offered mcp__tui__act, mcp__tui__key, mcp__tui__read_tree, mcp__tui__type_text",
                      row["error"])

    def test_the_stub_route_through_load_transcript_module(self):
        mod, _ = self.module()
        with mock.patch.object(runner, "load_transcript_module", return_value=mod):
            row = runner.grade_agent_run(self.PLAN, self.SLOT, "a1", run=FakeRun(self.GOOD_STATE),
                                         prov=self.PROV)
        self.assertEqual(row["status"], "passed")


class AgentIdentity(unittest.TestCase):
    """`next --agent-id` refuses an agent that cannot have run the slot's
    job, before recording anything, and keeps the slot for the right one."""

    FIXTURE = os.path.join(BENCH, "fixtures", "transcripts", "real_taria_clean.jsonl")  # T3, started 05:20:26.399Z

    def setUp(self):
        self.results = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.results)
        self.paths = runner.Paths(self.results, "b")

    def hold(self, task="T3", prepared="2026-10-11T05:20:20.000+00:00"):
        code, _, err = quiet_cli("plan", "--batch", "b", "--tasks", task, "--arms", "taria", "--seed", "1",
                                 "--results-dir", self.results)
        self.assertEqual(code, 0, err)
        plan = runner.load_plan(self.paths)
        _, slot = runner.claim(self.paths, plan, 1)
        slot.update(state="ready", demo=SlotFields().fields(), prepared=prepared)
        runner.write_slot(self.paths, 1, slot)
        return plan, slot

    def refused(self, agent_id="afixturetariaclean"):
        code, out, err = quiet_cli("next", "--batch", "b", "--slot", "1", "--agent-id", agent_id,
                                   "--transcript-file", self.FIXTURE, "--results-dir", self.results)
        self.assertEqual(code, runner.EXIT_HARNESS, err)
        self.assertIsNotNone(runner.read_slot(self.paths, 1), "the slot is kept for the right agent")
        self.assertFalse(any(r.get("job") == "j001" for r in runner.read_rows(self.paths)), "nothing recorded")
        error = json.loads(out)["error"]
        self.assertIn("nothing recorded", error)
        self.assertIn("run.py abort --batch b --slot 1", error)
        return error

    def test_the_right_transcript_is_this_slots_run(self):
        _, slot = self.hold()
        stats = transcript.read_transcript(self.FIXTURE)
        self.assertEqual(runner.identity_problems(slot, stats), [])
        # The same instant as the prepare is not before it.
        self.assertEqual(runner.identity_problems({**slot, "prepared": "2026-10-11T05:20:26.399+00:00"}, stats), [])

    def test_an_agent_id_the_batch_recorded_is_refused(self):
        self.hold()
        runner.append_row(self.paths, runner.new_row(job="j999", agent_id="afixturetariaclean", status="passed"))
        error = self.refused()
        self.assertIn("agent afixturetariaclean was already recorded for j999", error)

    def test_another_tasks_transcript_is_refused(self):
        self.hold(task="T1")
        error = self.refused()
        self.assertIn(f"its first prompt is {tasks.TASKS['T3'].prompt!r}, not the task's "
                      f"{tasks.TASKS['T1'].prompt!r}", error)

    def test_a_transcript_older_than_the_slots_demo_is_refused(self):
        self.hold(prepared="2026-10-11T05:20:27.000+00:00")
        error = self.refused()
        self.assertIn("it started at 2026-10-11T05:20:26.399Z, before the slot was prepared at "
                      "2026-10-11T05:20:27.000+00:00", error)

    def test_the_prepared_time_is_recorded_to_the_millisecond(self):
        self.assertRegex(runner.now("milliseconds"), r"T\d\d:\d\d:\d\d\.\d{3}\+00:00$")


# --- The launcher and the agent files ------------------------------------------------------


class McpLaunch(unittest.TestCase):
    def setUp(self):
        self.slots = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.slots)

    def launch(self, arm, slot=1):
        return subprocess.run(
            [sys.executable, MCP_LAUNCH, "--arm", arm, "--slot", str(slot), "--slots-dir", self.slots],
            capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL,
        )

    def write(self, slot, **fields):
        with open(os.path.join(self.slots, f"{slot}.json"), "w") as f:
            json.dump(fields, f)

    def test_a_missing_slot_is_refused(self):
        out = self.launch("taria")
        self.assertEqual(out.returncode, 1)
        self.assertEqual(out.stdout, "", "nothing may reach the MCP stream")
        self.assertIn("slot 1 holds no job", out.stderr)
        self.assertEqual(len(out.stderr.strip().splitlines()), 1)

    def test_the_wrong_arm_is_refused(self):
        self.write(1, job="j002", arm="tmux", state="ready", demo=SlotFields().fields())
        out = self.launch("taria")
        self.assertEqual(out.returncode, 1)
        self.assertEqual(out.stdout, "")
        self.assertIn("holds job j002 for the tmux arm, not taria", out.stderr)
        self.assertIn("wrong one for the slot", out.stderr)

    def test_a_slot_not_ready_is_refused(self):
        self.write(1, job="j002", arm="taria", state="starting", demo=SlotFields().fields())
        out = self.launch("taria")
        self.assertEqual(out.returncode, 1)
        self.assertIn("not ready", out.stderr)

    def markers(self):
        return sorted(f for f in os.listdir(self.slots) if f.endswith(".launched"))

    def test_refusals_leave_no_launch_marker(self):
        self.launch("taria")
        self.write(1, job="j002", arm="tmux", state="ready", demo=SlotFields().fields())
        self.launch("taria")
        self.write(1, job="j002", arm="taria", state="starting", demo=SlotFields().fields())
        self.launch("taria")
        self.assertEqual(self.markers(), [])

    def test_one_launch_per_job(self):
        """The first agent's launcher marks the job and execs the server (the
        tmux arm's exits at the end of its stdin); a second is refused before
        anything reaches the MCP stream."""
        self.write(1, job="j003", arm="tmux", state="ready", demo=SlotFields().fields())
        first = self.launch("tmux")
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(self.markers(), ["1.j003.launched"])
        with open(os.path.join(self.slots, "1.j003.launched")) as f:
            self.assertIsInstance(json.load(f)["pid"], int)
        second = self.launch("tmux")
        self.assertEqual(second.returncode, 1)
        self.assertEqual(second.stdout, "", "nothing may reach the MCP stream")
        self.assertIn("slot 1 (job j003) was already launched", second.stderr)
        self.assertIn("one agent per job", second.stderr)
        self.assertEqual(len(second.stderr.strip().splitlines()), 1)

    def test_a_job_id_not_fit_for_a_file_name_is_refused(self):
        self.write(1, job="../j003", arm="tmux", state="ready", demo=SlotFields().fields())
        out = self.launch("tmux")
        self.assertEqual(out.returncode, 1)
        self.assertIn("not [A-Za-z0-9_-]+", out.stderr)
        self.assertEqual(self.markers(), [])

    def test_freeing_a_slot_removes_its_markers_only(self):
        paths = runner.Paths(os.path.dirname(self.slots))
        paths.slots = self.slots
        for name in ("1.j003.launched", "1.j001.launched", "11.j003.launched", "2.j003.launched"):
            open(os.path.join(self.slots, name), "w").close()
        self.write(1, job="j003", arm="tmux", state="ready")
        runner.free_slot(paths, 1)
        self.assertEqual(self.markers(), ["11.j003.launched", "2.j003.launched"])
        self.assertFalse(os.path.exists(os.path.join(self.slots, "1.json")))

    def test_the_launcher_execs_what_the_dry_run_spawns(self):
        """One definition of each arm's server: mcp_launch execs server_entry."""
        fields = SlotFields().fields()
        run = DemoRun.from_slot(fields)
        self.assertEqual(harness.server_entry("taria", run)["env"], {"TARIA_SOCK": fields["sock"]})
        self.assertEqual(harness.server_entry("tmux", run)["args"][-4:],
                         ["--session", fields["name"], "--socket-path", fields["tmux_sock"]])
        self.assertEqual(mcp_config("taria", run)["mcpServers"]["tui"], harness.server_entry("taria", run))


class AgentFiles(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir)
        self.slots = os.path.join(self.dir, "results", "slots")

    def test_bodies_are_identical_and_the_frontmatter_complete(self):
        taria = agents.render("taria", 1, self.slots)
        tmux = agents.render("tmux", 1, self.slots)
        a, b = agents.parse(taria), agents.parse(tmux)
        self.assertEqual(taria.split("\n---\n", 1)[1].encode(), tmux.split("\n---\n", 1)[1].encode())
        self.assertEqual(a["body"], agents.BODY)
        self.assertEqual(agents.body_sha256(a["body"]), agents.body_sha256(b["body"]))
        self.assertEqual(a["fields"], {
            "name": "tui-taria-1",
            "description": "Benchmark runner, taria arm, slot 1. Spawned only by bench/run.py's orchestrator.",
            "tools": "mcp__tui", "model": "sonnet", "effort": "medium", "maxTurns": "30",
            "omitClaudeMd": "true",
        })
        self.assertEqual(a["server"]["_name"], "tui")
        self.assertEqual(a["server"]["type"], "stdio")
        self.assertEqual(a["server"]["command"], sys.executable)
        self.assertTrue(os.path.isabs(a["server"]["command"]))
        self.assertEqual(a["server"]["args"], [agents.MCP_LAUNCH, "--arm", "taria", "--slot", "1",
                                               "--slots-dir", os.path.abspath(self.slots)])
        self.assertEqual(b["server"]["args"][1:5], ["--arm", "tmux", "--slot", "1"])
        # Only name, description and the launcher's arm and slot differ.
        diff = [(x, y) for x, y in zip(taria.splitlines(), tmux.splitlines()) if x != y]
        self.assertEqual([x.split(":")[0].strip() for x, _ in diff], ["name", "description", "args"])

    def test_the_documented_inline_server_shape(self):
        """The YAML matches the sub-agents doc's inline example: a list item
        keyed by the server name, holding type, command and args."""
        text = agents.render("tmux", 3, self.slots, effort="high", max_turns=12)
        self.assertIn("\nmcpServers:\n  - tui:\n      type: stdio\n      command: ", text)
        self.assertIn('\n      args: ["', text)
        self.assertIn("\neffort: high\nmaxTurns: 12\n", text)
        self.assertTrue(text.startswith("---\nname: tui-tmux-3\n"))

    def test_problems_name_what_differs_from_the_plan(self):
        (path,) = [p for p in agents.write(1, self.dir, self.slots) if p.endswith("tui-taria-1.md")]
        sha = agents.body_sha256()
        self.assertEqual(agents.problems(path, "taria", 1, self.slots, "medium", 30, sha), [])
        found = agents.problems(path, "taria", 1, self.slots, "high", 20, "0" * 64)
        self.assertEqual(len(found), 3, found)
        self.assertTrue(agents.problems(path, "taria", 1, "/other/slots", "medium", 30, sha))
        self.assertIn("does not exist", agents.problems(path + "x", "taria", 1, self.slots, "medium", 30, sha)[0])

    def test_remove_deletes_only_its_own_files(self):
        code, out, err = quiet_cli("agents", "--slots", "2", "--dir", self.dir,
                                   "--results-dir", os.path.join(self.dir, "results"))
        self.assertEqual(code, 0, err)
        ours = sorted(f for f in os.listdir(self.dir) if f.endswith(".md"))
        self.assertEqual(ours, ["tui-taria-1.md", "tui-taria-2.md", "tui-tmux-1.md", "tui-tmux-2.md"])
        others = {
            "reviewer.md": "---\nname: reviewer\n---\n",
            "tui-other-1.md": "---\nname: x\n---\n",
            "tui-taria-9.md": "---\nname: tui-taria-9\ndescription: a person's own agent\n---\n",
        }
        for name, text in others.items():
            with open(os.path.join(self.dir, name), "w") as f:
                f.write(text)
        code, _, _ = quiet_cli("agents", "--remove", "--dir", self.dir)
        self.assertEqual(code, 0)
        self.assertEqual(sorted(f for f in os.listdir(self.dir) if f.endswith(".md")), sorted(others))

    def test_a_file_not_ours_is_never_overwritten(self):
        path = os.path.join(self.dir, "tui-tmux-1.md")
        with open(path, "w") as f:
            f.write("---\nname: tui-tmux-1\ndescription: mine\n---\n")
        code, _, err = quiet_cli("agents", "--slots", "1", "--dir", self.dir)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("not written by bench/agents.py", err)
        with open(path) as f:
            self.assertIn("description: mine", f.read())


# --- The subagent path, end to end, without a model ------------------------------------------


def write_transcript(directory, agent_id, prompt, arm, calls, agent_type,
                     model="claude-sonnet-5-5", effort="medium"):
    """A minimal subagent transcript in the shape real ones have (see
    bench/transcript.py and fixtures/transcripts/real_*.jsonl), starting now:
    the prompt, a first prompt_snapshot without tools, one assistant message
    per tool call with its result, the snapshot listing the arm's tools and
    the tui server's instructions after the first, and a closing sentence.
    Plus the meta file naming the agent type. Returns the transcript's path."""
    t0 = datetime.now(timezone.utc)
    session = "00000000-0000-4000-8000-00000000beef"
    lines, n = [], 0

    def line(kind, **extra):
        nonlocal n
        n += 1
        stamp = (t0 + timedelta(milliseconds=500 * n)).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"
        lines.append({"parentUuid": None, "isSidechain": True, "agentId": agent_id, "type": kind,
                      "uuid": f"{agent_id}-{n}", "timestamp": stamp, "cwd": "/tmp/work",
                      "sessionId": session, "version": "2.1.294", **extra})

    usage = {"input_tokens": 10, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 1000,
             "output_tokens": 20}
    line("user", message={"role": "user", "content": prompt})
    system = [agents.BODY]
    line("attachment", attachment={"type": "prompt_snapshot", "systemPrompt": system})
    tools = [{"name": f"mcp__tui__{t}", "description": t} for t in harness.ARM_TOOLS[arm]]
    for i, name in enumerate(name for name, count in sorted(calls.items()) for _ in range(count)):
        line("assistant", apiBlockIndex=0, effort=effort, message={
            "model": model, "id": f"msg_{i}", "type": "message", "role": "assistant",
            "content": [{"type": "tool_use", "id": f"toolu_{i}", "name": f"mcp__tui__{name}", "input": {}}],
            "stop_reason": "tool_use", "usage": usage})
        line("user", message={"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": f"toolu_{i}", "content": "ok", "is_error": False}]})
        if i == 0:
            line("attachment", attachment={"type": "prompt_snapshot", "systemPrompt": system, "tools": tools})
            line("attachment", attachment={"type": "mcp_instructions_delta", "addedNames": ["tui"],
                                           "addedBlocks": ["## tui\n..."], "removedNames": []})
    line("assistant", apiBlockIndex=0, effort=effort, message={
        "model": model, "id": "msg_end", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "I did what you asked."}], "stop_reason": "end_turn",
        "usage": usage})
    path = os.path.join(directory, f"agent-{agent_id}.jsonl")
    with open(path, "w") as f:
        f.writelines(json.dumps(entry) + "\n" for entry in lines)
    with open(os.path.join(directory, f"agent-{agent_id}.meta.json"), "w") as f:
        json.dump({"agentType": agent_type, "model": "sonnet"}, f)
    return path


@unittest.skipUnless(HAVE_TMUX, "tmux is not on PATH")
@unittest.skipIf(transcript is None, "bench/transcript.py is missing")
class SubagentRehearsal(unittest.TestCase):
    """What the orchestrator does, with the scripted solver in the agent's
    place: every command a separate process, the server started from the
    agent file's own command line."""

    @classmethod
    def setUpClass(cls):
        require_fresh_build()

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="rehearsal-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.results = os.path.join(self.tmp, "results")
        self.agents_dir = os.path.join(self.tmp, "agents")
        self.common = ("--results-dir", self.results)
        self.addCleanup(self.abort_leftovers)

    def abort_leftovers(self):
        for k, slot in runner.read_all_slots(runner.Paths(self.results)).items():
            runner.teardown(slot)

    def next(self, batch, slot, *argv):
        out = run_py("next", "--batch", batch, "--slot", str(slot), "--agents-dir", self.agents_dir,
                     *self.common, *argv)
        lines = out.stdout.splitlines()
        self.assertEqual(len(lines), 1, f"next printed {lines!r}; stderr: {out.stderr}")
        return out.returncode, json.loads(lines[0]), out.stderr

    def launcher(self, agent):
        entry = agents.read(os.path.join(self.agents_dir, f"{agent}.md"))["server"]
        return {"command": entry["command"], "args": entry["args"]}

    def act_as_agent(self, batch, job):
        """Start the agent file's server by hand, drive the job with the
        scripted solver, and write the transcript it would have left."""
        slot = runner.read_slot(runner.Paths(self.results), job["slot"])
        arm, task = slot["arm"], slot["task"]
        self.assertTrue(DemoRun.from_slot(slot["demo"]).demo_alive(), "the demo died with `next`")
        client = McpStdio(self.launcher(job["agent"]), cwd=self.tmp, name=job["agent"])
        try:
            client.initialize()
            self.assertEqual(sorted(client.tools_list()), sorted(harness.ARM_TOOLS[arm]))
            harness.wait_ready(arm, client)
            session = solvers.CountingSession(client)
            solvers.solve(task, arm, session)
        finally:
            client.close()
        agent_id = f"rehearsal{job['job']}"
        return agent_id, write_transcript(self.tmp, agent_id, job["prompt"], arm, dict(session.calls),
                                          job["agent"])

    def test_two_jobs_through_one_slot(self):
        out = run_py("agents", "--slots", "1", "--dir", self.agents_dir, *self.common)
        self.assertEqual(out.returncode, 0, out.stderr)
        out = run_py("plan", "--batch", "reh", "--tasks", "T6", "--arms", "taria,tmux", "--seed", "4",
                     *self.common)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("2 jobs", out.stdout)

        seen_arms, demos = [], []
        code, job, err = self.next("reh", 1)
        for _ in range(2):
            self.assertEqual(code, 0, err)
            self.assertEqual(set(job), {"job", "slot", "agent", "model", "prompt"})
            self.assertEqual((job["slot"], job["model"]), (1, "sonnet"))
            self.assertEqual(job["prompt"], tasks.TASKS["T6"].prompt)
            arm = job["agent"].split("-")[1]
            seen_arms.append(arm)
            slot = runner.read_slot(runner.Paths(self.results), 1)
            demos.append(slot["demo"])
            # The other arm's agent file gets no tools for this slot.
            other = "tmux" if arm == "taria" else "taria"
            wrong = subprocess.run([*[self.launcher(f"tui-{other}-1")["command"]],
                                    *self.launcher(f"tui-{other}-1")["args"]],
                                   capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL)
            self.assertEqual(wrong.returncode, 1)
            self.assertIn("wrong one for the slot", wrong.stderr)
            # A second `next` without an agent id is slot misuse.
            code, refused, _ = self.next("reh", 1)
            self.assertEqual(code, runner.EXIT_HARNESS)
            self.assertIn(job["job"], refused["error"])
            status = run_py("status", "--batch", "reh", *self.common)
            self.assertIn(f"slot 1: {job['job']} (T6/{arm}/sonnet run 0), ready", status.stdout)

            agent_id, path = self.act_as_agent("reh", job)
            # The job was launched once; a second agent for it gets no tools.
            again = subprocess.run([self.launcher(job["agent"])["command"], *self.launcher(job["agent"])["args"]],
                                   capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL)
            self.assertEqual(again.returncode, 1)
            self.assertIn(f"(job {job['job']}) was already launched", again.stderr)
            code, job, err = self.next("reh", 1, "--agent-id", agent_id, "--transcript-file", path)
            self.assertIn("passed", err)
            self.assertEqual([f for f in os.listdir(os.path.join(self.results, "slots")) if f.endswith(".launched")],
                             [], "grading leaves no launch marker behind")

        self.assertEqual(code, 0, err)
        self.assertEqual(job, {"done": True})
        self.assertEqual(sorted(seen_arms), ["taria", "tmux"])
        rows = read_rows(self.results, "reh")
        self.assertEqual([r["status"] for r in rows], ["passed", "passed"], [r["error"] or r["reason"] for r in rows])
        for row in rows:
            self.assertEqual(list(row), list(runner.ROW_KEYS))
            self.assertEqual((row["mode"], row["solver"], row["demo_exit"]), ("subagent", "agent", "clean"))
            self.assertEqual((row["model_resolved"], row["efforts_resolved"], row["guard_problems"]),
                             ("claude-sonnet-5-5", ["medium"], []))
            self.assertEqual(row["claude_version"], "2.1.294")
            self.assertGreater(row["tool_calls_total"], 0)
            self.assertTrue(row["agent_id"].startswith("rehearsalj00"))
        status = run_py("status", "--batch", "reh", *self.common)
        self.assertIn("2 recorded (passed 2, failed 0, max_turns 0, harness_error 0); 0 held; 0 pending",
                      status.stdout)
        for demo in demos:
            self.assertFalse(os.path.exists(demo["tmpdir"]))
            self.assertTrue(wait_gone(demo["demo_pid"]))
            self.assertFalse(_running(demo["server_pid"]))

    def test_a_harness_error_claims_nothing_and_abort_frees_a_slot(self):
        run_py("agents", "--slots", "2", "--dir", self.agents_dir, *self.common)
        out = run_py("plan", "--batch", "err", "--tasks", "T1,T2", "--arms", "taria", "--seed", "1",
                     *self.common)
        self.assertEqual(out.returncode, 0, out.stderr)
        code, job, err = self.next("err", 1)
        self.assertEqual(code, 0, err)
        demo = runner.read_slot(runner.Paths(self.results), 1)["demo"]
        # No transcript for this id anywhere: a harness error, and the slot claims nothing.
        empty = os.path.join(self.tmp, "agent-nobody.jsonl")
        code, refused, err = self.next("err", 1, "--agent-id", "nobody", "--transcript-file", empty)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("harness error", refused["error"])
        self.assertIsNone(runner.read_slot(runner.Paths(self.results), 1))
        self.assertTrue(wait_gone(demo["demo_pid"]))
        (row,) = read_rows(self.results, "err")
        self.assertEqual((row["status"], row["job"]), ("harness_error", job["job"]))
        # Resuming claims the other job; abort records it and tears it down.
        code, job2, err = self.next("err", 2)
        self.assertEqual(code, 0, err)
        self.assertNotEqual(job2["job"], job["job"])
        demo = runner.read_slot(runner.Paths(self.results), 2)["demo"]
        out = run_py("abort", "--batch", "err", "--slot", "2", "--reason", "rehearsal", *self.common)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertTrue(wait_gone(demo["demo_pid"]))
        rows = read_rows(self.results, "err")
        self.assertEqual(rows[-1]["error"], "aborted: rehearsal")
        self.assertEqual(rows[-1]["state"]["tasks"], [dict(t) for t in tasks.SEED_TASKS])
        code, done, _ = self.next("err", 1)
        self.assertEqual((code, done), (0, {"done": True}))

    def test_next_refuses_agent_files_that_do_not_match_the_plan(self):
        run_py("agents", "--slots", "1", "--dir", self.agents_dir, "--effort", "high", *self.common)
        run_py("plan", "--batch", "mismatch", "--tasks", "T1", "--arms", "taria", *self.common)
        code, refused, _ = self.next("mismatch", 1)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("effort is 'high', the plan needs 'medium'", refused["error"])
        self.assertEqual(runner.read_all_slots(runner.Paths(self.results)), {})


# --- Summary statistics (the rest of summarize.py is tested in test_transcript.py) ---------------


class Summary(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
