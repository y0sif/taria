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
from contextlib import ExitStack, redirect_stderr, redirect_stdout
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
        self.assertEqual(a["arms"], ["taria", "tmux", "outline"], "every arm by default")
        self.assertEqual(len(a["jobs"]), 3 * 3 * 2 * 2)
        self.assertEqual(sorted(strip(a)), sorted(strip(c)), "the same jobs, in another order")
        self.assertEqual([j["id"] for j in a["jobs"]], [f"j{n:03d}" for n in range(1, 37)])
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

    def test_a_batch_with_rows_is_refused(self):
        """A dry run's job ids are a plan's: a plan over its rows would have
        claim skip those jobs as done, without a word."""
        paths = runner.Paths(self.results, "dry")
        runner.append_row(paths, runner.new_row(job="j001", mode="dry-run", status="passed"))
        code, text = self.plan("dry", "--tasks", "T1", "--seed", "1")
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn(f"batch dry already has rows ({paths.runs})", text)
        self.assertFalse(os.path.exists(paths.plan), "no plan written")
        # A line that is not a row still counts; an empty file does not.
        with open(paths.runs, "w") as f:
            f.write("{not a row\n")
        self.assertEqual(self.plan("dry", "--tasks", "T1")[0], runner.EXIT_HARNESS)
        open(paths.runs, "w").close()
        self.assertEqual(self.plan("dry", "--tasks", "T1")[0], 0)

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


class QuitDemo(unittest.TestCase):
    """How the demo ended, read from its dump before anything else."""

    def test_a_demo_the_agent_quit_is_a_clean_exit(self):
        run = FakeRun({**tasks.SEED_STATE, "quit": True}, alive=False)
        self.assertEqual(runner.quit_demo(run, False), "clean")
        self.assertEqual(run.quits, 0, "a demo already gone is not asked to quit")

    def test_a_demo_gone_without_quitting_crashed(self):
        run = FakeRun(tasks.SEED_STATE, alive=False)
        self.assertEqual(runner.quit_demo(run, False), "crashed")
        self.assertEqual(run.quits, 0)

    def test_a_running_demo_is_quit(self):
        run = FakeRun(tasks.SEED_STATE)
        self.assertEqual(runner.quit_demo(run, True), "clean")
        self.assertEqual(run.quits, 1)
        stuck = FakeRun(tasks.SEED_STATE)
        stuck.quit = lambda: False
        self.assertEqual(runner.quit_demo(stuck, True), "killed")


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

        def guard(stats, arm, family, arm_tools, effort=None, tree_format=None):
            seen.update(arm=arm, family=family, arm_tools=arm_tools, effort=effort, tree_format=tree_format)
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
                                "effort": "medium", "tree_format": "json"})
        self.assertEqual(list(row), list(runner.ROW_KEYS))
        self.assertEqual((row["mode"], row["solver"], row["model"], row["model_resolved"]),
                         ("subagent", "agent", "sonnet", "claude-sonnet-5-5"))
        self.assertEqual((row["effort"], row["efforts_resolved"], row["max_turns"]), ("medium", ["medium"], 30))
        self.assertEqual((row["claude_version"], row["claude_versions"]), ("2.1.294", ["2.1.294"]))
        self.assertEqual((row["job"], row["run"], row["seed"], row["agent_id"], row["session_id"]),
                         ("j004", 1, 5, "a1", "s1"))
        self.assertEqual((row["tool_calls_total"], row["toolsearch_calls"], row["turns"]), (3, 1, 4))
        self.assertEqual(row["guard_problems"], [])
        self.assertEqual((row["slot"], row["end_inferred"], row["notes"]), (2, False, []))
        self.assertEqual(row["demo_exit"], "clean")
        self.assertEqual(run.quits, 1)

    def test_failed(self):
        row, _, _ = self.grade(state=tasks.SEED_STATE)
        self.assertEqual((row["status"], row["passed"]), ("failed", False))
        self.assertIn("no new task", row["reason"])
        row, _, _ = self.grade(alive=False)
        self.assertEqual((row["status"], row["demo_exit"]), ("failed", "crashed"))
        # The agent quit the app: a failure (the check wants it running), but a clean exit.
        row, _, run = self.grade(state={**self.GOOD_STATE, "quit": True}, alive=False)
        self.assertEqual((row["status"], row["demo_exit"], run.quits), ("failed", "clean", 0))
        self.assertIn("the demo was quit", row["reason"])

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

    def test_a_final_text_reply_without_a_stop_reason_ended_on_its_own(self):
        unclosed = {**self.STATS, "last_stop_reason": None, "last_block_type": "text", "end_inferred": True}
        row, _, _ = self.grade(stats=unclosed)
        self.assertEqual((row["status"], row["passed"], row["error"]), ("passed", True, None))
        self.assertIs(row["end_inferred"], True)
        self.assertEqual(row["notes"], [transcript.INFERRED_END])

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

    def test_a_real_unclosed_final_reply_passes(self):
        """Job j108 of batch full1, which the old rule called a harness error."""
        row = self.real("real_taria_unclosed.jsonl", slot={**self.SLOT, "task": "T4", "model": "haiku"})
        self.assertEqual((row["status"], row["passed"], row["error"], row["guard_problems"]),
                         ("passed", True, None, []))
        self.assertIs(row["end_inferred"], True)
        self.assertEqual(row["notes"], [transcript.INFERRED_END])
        self.assertEqual(row["tokens"]["output"], 421)

    def test_a_real_mid_run_unclosed_message_flags_the_output_tokens(self):
        """Job j114 of batch full1: a message in the middle of the run never
        closed. The run passed and ended on end_turn; its output tokens are a
        lower bound, which the row says."""
        row = self.real("real_taria_midunclosed.jsonl", slot={**self.SLOT, "task": "T2", "model": "haiku"})
        self.assertEqual((row["status"], row["error"], row["guard_problems"]), ("passed", None, []))
        self.assertEqual((row["output_tokens_lower_bound"], row["unclosed_messages"]), (True, 1))
        self.assertEqual((row["end_inferred"], row["notes"]), (False, []))
        self.assertEqual(row["tokens"]["output"], 156)
        row = self.real("real_taria_unclosed.jsonl", slot={**self.SLOT, "task": "T4", "model": "haiku"})
        self.assertEqual((row["output_tokens_lower_bound"], row["unclosed_messages"], row["end_inferred"]),
                         (True, 1, True))
        row = self.real("real_taria_clean.jsonl")
        self.assertEqual((row["output_tokens_lower_bound"], row["unclosed_messages"]), (False, 0))

    def test_every_transcript_field_is_a_row_key_and_filled(self):
        path = os.path.join(BENCH, "fixtures", "transcripts", "real_taria_clean.jsonl")
        fields, _, _ = runner.transcript_verdict(transcript, path, "taria", "sonnet", "medium", 30)
        self.assertEqual(sorted(fields), sorted(runner.TRANSCRIPT_FIELDS))
        self.assertTrue(set(runner.TRANSCRIPT_FIELDS) <= set(runner.ROW_KEYS))

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

    def refused(self, agent_id="afixturetariaclean", transcript_file=None):
        code, out, err = quiet_cli("next", "--batch", "b", "--slot", "1", "--agent-id", agent_id,
                                   "--transcript-file", transcript_file or self.FIXTURE,
                                   "--results-dir", self.results)
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

    def test_the_first_prompt_is_compared_stripped(self):
        _, slot = self.hold()
        stats = transcript.read_transcript(self.FIXTURE)
        for padded in (stats["first_prompt"] + "\n", "  " + stats["first_prompt"] + " \n"):
            self.assertEqual(runner.identity_problems(slot, {**stats, "first_prompt": padded}), [])
        self.assertEqual(len(runner.identity_problems(slot, {**stats, "first_prompt": None})), 1)

    def copy_fixture(self, agent_type="tui-taria-1"):
        """The fixture under a temp dir, with a meta file naming `agent_type`
        (none when it is None)."""
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "agent-afixturetariaclean.jsonl")
        shutil.copy(self.FIXTURE, path)
        if agent_type is not None:
            with open(runner.meta_path(path), "w") as f:
                json.dump({"agentType": agent_type}, f)
        return path

    def test_no_transcript_is_refused(self):
        self.hold()
        missing = os.path.join(self.results, "agent-nobody.jsonl")
        error = self.refused(agent_id="nobody", transcript_file=missing)
        self.assertIn("no transcript for agent nobody", error)
        plan, slot = runner.load_plan(self.paths), runner.read_slot(self.paths, 1)
        nowhere = types.SimpleNamespace(find_transcript=lambda agent_id: None)
        with self.assertRaises(runner.CommandError) as caught:
            runner.check_agent_identity(self.paths, plan, 1, slot, "nobody", transcript_mod=nowhere)
        self.assertIn("no transcript for agent nobody (~/.claude/projects)", str(caught.exception))

    def test_a_transcript_without_a_meta_file_is_refused(self):
        self.hold()
        error = self.refused(transcript_file=self.copy_fixture(agent_type=None))
        self.assertIn("has no meta file", error)
        self.assertIn("agent type it was spawned as cannot be checked", error)

    def test_another_slots_agent_is_refused(self):
        self.hold()
        error = self.refused(transcript_file=self.copy_fixture(agent_type="tui-taria-2"))
        self.assertIn("was spawned as 'tui-taria-2', not this slot's agent 'tui-taria-1'", error)

    def finish(self, src, archive_error=None):
        """finish_slot on the held slot, grading stubbed: (row, the path
        graded, stderr)."""
        plan, slot = self.hold()
        seen = {}

        def grade(plan, slot, agent_id, transcript_file=None, **_):
            seen["path"] = transcript_file
            return runner.new_row(job=slot["job"], agent_id=agent_id, status="passed", passed=True)

        err = io.StringIO()
        with ExitStack() as stack:
            stack.enter_context(mock.patch.object(runner, "grade_agent_run", grade))
            stack.enter_context(mock.patch.object(runner, "teardown"))
            if archive_error is not None:
                stack.enter_context(mock.patch.object(runner, "archive_transcript", side_effect=archive_error))
            stack.enter_context(redirect_stderr(err))
            row = runner.finish_slot(self.paths, plan, 1, slot, "afixturetariaclean", src)
        return row, seen["path"], err.getvalue()

    def test_grading_archives_the_transcript_and_grades_the_copy(self):
        src = self.copy_fixture()
        row, graded, _ = self.finish(src)
        archived = os.path.join(self.results, "b", "transcripts", "agent-afixturetariaclean.jsonl")
        self.assertEqual(graded, archived, "the row is graded from the archived copy")
        for a, b in ((src, archived), (runner.meta_path(src), runner.meta_path(archived))):
            with open(a, "rb") as fa, open(b, "rb") as fb:
                self.assertEqual(fa.read(), fb.read(), b)
        self.assertEqual(runner.read_rows(self.paths), [row])
        self.assertIsNone(runner.read_slot(self.paths, 1), "the slot is freed")
        self.assertEqual([f for f in os.listdir(os.path.dirname(archived)) if f.endswith(".tmp")], [])

    def test_a_failed_archive_grades_the_live_transcript(self):
        src = self.copy_fixture()
        row, graded, err = self.finish(src, archive_error=OSError("disk full"))
        self.assertEqual(graded, src)
        self.assertIn("could not archive agent afixturetariaclean's transcript (disk full)", err)
        self.assertIn("run.py archive --batch b", err)
        self.assertEqual(row["status"], "passed")


class Regrade(unittest.TestCase):
    """`regrade` grades a batch's rows again with the current code, from the
    recorded state and demo_alive and the transcripts re-read (here the real
    fixtures, placed in a projects dir laid out as Claude Code's)."""

    FIXTURES = os.path.join(BENCH, "fixtures", "transcripts")

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)
        self.results = os.path.join(self.tmp, "results")
        self.projects = os.path.join(self.tmp, "projects")
        self.paths = runner.Paths(self.results, "b")
        code, _, err = quiet_cli("plan", "--batch", "b", "--tasks", "T1,T3,T4", "--arms", "taria",
                                 "--models", "sonnet,haiku", "--seed", "1", "--results-dir", self.results)
        self.assertEqual(code, 0, err)

    def place(self, fixture, agent_id, agent_type=None):
        """A fixture as agent `agent_id`'s transcript, meta file and all."""
        directory = os.path.join(self.projects, "-proj", "sess-1", "subagents")
        os.makedirs(directory, exist_ok=True)
        dst = os.path.join(directory, f"agent-{agent_id}.jsonl")
        shutil.copy(os.path.join(self.FIXTURES, fixture), dst)
        with open(os.path.join(self.FIXTURES, fixture[: -len(".jsonl")] + ".meta.json")) as f:
            meta = json.load(f)
        if agent_type is not None:
            meta["agentType"] = agent_type
        with open(runner.meta_path(dst), "w") as f:
            json.dump(meta, f)

    def row(self, job, task, model, agent_id, status, state=None, **extra):
        state = state if state is not None else tasks.self_test_cases()[task][0][0]
        fields = dict(ts="2026-10-11T06:00:00+00:00", batch="b", job=job, task=task, arm="taria", run=0, seed=1,
                      mode="subagent", solver="agent", model=model, effort="medium", max_turns=30,
                      agent_id=agent_id, status=status, passed={"passed": True, "failed": False}.get(status),
                      check_passed=True, demo_alive=True, demo_exit="clean", state=state,
                      error="old error" if status == "harness_error" else None)
        fields.update(extra)
        return fields  # an older row: no slot, end_inferred or notes keys unless given

    def write_rows(self, rows, junk=False):
        os.makedirs(self.paths.batch_dir, exist_ok=True)
        with open(self.paths.runs, "w") as f:
            for row in rows:
                f.write(json.dumps(row) + "\n")
            if junk:
                f.write("{not a row\n")

    def regrade(self):
        code, out, err = quiet_cli("regrade", "--batch", "b", "--results-dir", self.results,
                                   "--projects-dir", self.projects)
        self.assertEqual(code, 0, err)
        with open(self.paths.runs) as f:
            lines = f.read().splitlines()
        return out, {json.loads(l)["job"]: json.loads(l) for l in lines if l.startswith("{\"")}, lines

    def batch(self):
        self.place("real_taria_unclosed.jsonl", "aunclosed")
        self.place("real_taria_clean.jsonl", "aclean")
        self.place("real_taria_leaked.jsonl", "aleaked")
        self.place("real_taria_clean.jsonl", "awrongslot")
        rows = [
            # The old rule's false positive: an unclosed final reply.
            self.row("j001", "T4", "haiku", "aunclosed", "harness_error"),
            # Nothing to change.
            self.row("j002", "T3", "sonnet", "aclean", "passed"),
            # A leaked run once let through: the guard refuses it now.
            self.row("j003", "T1", "haiku", "aleaked", "passed"),
            # The recorded state is checked again: the seed is no deletion.
            self.row("j004", "T3", "sonnet", "aclean2", "passed", state=tasks.SEED_STATE),
            # A transcript that is gone keeps the row as it was.
            self.row("j005", "T1", "sonnet", "agone", "failed", check_passed=False),
            # An abort: no agent id.
            self.row("j006", "T1", "haiku", None, "harness_error", error="aborted: test"),
            # A row that names its slot is held to that slot's agent.
            self.row("j007", "T3", "sonnet", "awrongslot", "passed", slot=3),
            # A dry-run row keeps everything.
            dict(self.row("j008", "T1", "sonnet", None, "passed"), mode="dry-run", solver="scripted"),
        ]
        self.place("real_taria_clean.jsonl", "aclean2")
        self.write_rows(rows, junk=True)
        return rows

    def test_rows_are_regraded_from_what_persists(self):
        before = self.batch()
        with open(self.paths.runs, "rb") as f:
            original = f.read()
        out, rows, lines = self.regrade()
        self.assertEqual(lines[-1], "{not a row", "a line that is not a row is kept as it was")
        status = {job: r["status"] for job, r in rows.items()}
        self.assertEqual(status, {"j001": "passed", "j002": "passed", "j003": "harness_error", "j004": "failed",
                                  "j005": "failed", "j006": "harness_error", "j007": "harness_error",
                                  "j008": "passed"})
        self.assertIn("8 rows (1 dry-run kept, 2 kept as recorded); 4 changed status", out)
        self.assertIn("j001: harness_error -> passed (its last message is a text reply with no stop_reason", out)
        self.assertIn("j003: passed -> harness_error (guard: was given the instructions of 4 MCP server(s)", out)
        self.assertIn("j004: passed -> failed (", out)
        self.assertIn("j007: passed -> harness_error (guard: was spawned as 'tui-taria-1', not this slot's agent "
                      "'tui-taria-3')", out)
        unclosed = rows["j001"]
        self.assertEqual((unclosed["passed"], unclosed["error"], unclosed["end_inferred"]), (True, None, True))
        self.assertEqual(unclosed["notes"], [transcript.INFERRED_END])
        self.assertEqual(unclosed["tokens"]["output"], 421)
        self.assertEqual(rows["j004"]["reason"], tasks.check_t3(tasks.SEED_STATE)[1])
        # Every field kept, the regrade's added; the original status kept beside the new one.
        for old in before:
            new = rows[old["job"]]
            if old["mode"] == "dry-run":
                self.assertEqual(new, old)
                continue
            self.assertTrue(set(old) <= set(new))
            self.assertEqual(new["status_at_run"], old["status"])
            self.assertEqual(new["regrade_git_sha"], runner.git_provenance()["git_sha"])
            self.assertIn("regrade_git_dirty", new)
            self.assertIsNotNone(runner.parse_time(new["regraded_at"]))
            for key in ("ts", "state", "demo_exit", "git_sha", "agent_id"):
                self.assertEqual(new.get(key), old.get(key), key)
        self.assertEqual(rows["j005"]["status"], "failed")
        self.assertIn("kept as recorded: no transcript for agent agone", rows["j005"]["regrade_note"])
        self.assertNotIn("turns", {k for k, v in rows["j005"].items() if v is not None})
        self.assertIn("no agent id", rows["j006"]["regrade_note"])
        self.assertEqual(rows["j006"]["error"], "aborted: test")
        # The original, byte for byte, beside it.
        (backup,) = [f for f in os.listdir(self.paths.batch_dir) if f.startswith("runs.orig-")]
        self.assertRegex(backup, r"^runs\.orig-\d{8}T\d{6}Z\.jsonl$")
        with open(os.path.join(self.paths.batch_dir, backup), "rb") as f:
            self.assertEqual(f.read(), original)

    def test_a_second_regrade_changes_nothing(self):
        self.batch()
        _, first, _ = self.regrade()
        backups = sorted(f for f in os.listdir(self.paths.batch_dir) if f.startswith("runs.orig-"))
        with open(os.path.join(self.paths.batch_dir, backups[0]), "rb") as f:
            first_backup = f.read()
        out, second, _ = self.regrade()
        self.assertIn("0 changed status", out)
        strip = ("regraded_at", "regrade_git_sha", "regrade_git_dirty")
        self.assertEqual({j: {k: v for k, v in r.items() if k not in strip} for j, r in second.items()},
                         {j: {k: v for k, v in r.items() if k not in strip} for j, r in first.items()})
        self.assertEqual(second["j001"]["status_at_run"], "harness_error", "the status at run time stays")
        after = sorted(f for f in os.listdir(self.paths.batch_dir) if f.startswith("runs.orig-"))
        self.assertEqual(len(after), 2, "each regrade keeps its own backup")
        with open(os.path.join(self.paths.batch_dir, backups[0]), "rb") as f:
            self.assertEqual(f.read(), first_backup, "an existing backup is never overwritten")

    def test_a_backup_never_overwrites_one(self):
        os.makedirs(self.paths.batch_dir, exist_ok=True)
        first = runner.backup_path(self.paths, "20261011T000000Z")
        self.assertTrue(first.endswith("runs.orig-20261011T000000Z.jsonl"))
        open(first, "w").close()
        second = runner.backup_path(self.paths, "20261011T000000Z")
        self.assertTrue(second.endswith("runs.orig-20261011T000000Z-2.jsonl"))
        open(second, "w").close()
        self.assertTrue(runner.backup_path(self.paths, "20261011T000000Z").endswith("-3.jsonl"))

    def test_a_held_slot_is_refused(self):
        self.batch()
        with open(self.paths.runs, "rb") as f:
            original = f.read()
        plan = runner.load_plan(self.paths)
        os.makedirs(self.paths.slots, exist_ok=True)
        runner.create_slot(self.paths, 2, {"slot": 2, "batch": "b", "job": "j009", "state": "ready"})
        code, _, err = quiet_cli("regrade", "--batch", "b", "--results-dir", self.results,
                                 "--projects-dir", self.projects)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn(f"batch {plan['batch']} still holds slot(s) 2", err)
        with open(self.paths.runs, "rb") as f:
            self.assertEqual(f.read(), original)
        self.assertEqual([f for f in os.listdir(self.paths.batch_dir) if f.startswith("runs.orig-")], [])
        # Another batch's slot is not this batch's.
        runner.free_slot(self.paths, 2)
        runner.create_slot(self.paths, 2, {"slot": 2, "batch": "other", "job": "j001", "state": "ready"})
        self.assertEqual(self.regrade()[0].count("changed status"), 1)

    def test_a_batch_without_a_plan_is_refused(self):
        code, _, err = quiet_cli("regrade", "--batch", "nope", "--results-dir", self.results)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("no plan for batch nope", err)

    def archive_copy(self, fixture, agent_id):
        """A fixture as agent `agent_id`'s transcript in the batch's archive."""
        os.makedirs(self.paths.transcripts, exist_ok=True)
        dst = runner.archived_transcript(self.paths, agent_id)
        shutil.copy(os.path.join(self.FIXTURES, fixture), dst)
        shutil.copy(os.path.join(self.FIXTURES, fixture[: -len(".jsonl")] + ".meta.json"), runner.meta_path(dst))
        return dst

    def test_the_archived_transcript_is_preferred_to_the_live_one(self):
        """Claude Code deletes transcripts after cleanupPeriodDays: the
        archived copy grades a row whose live one is gone, and wins over a
        live one that differs."""
        self.archive_copy("real_taria_clean.jsonl", "aarchived")  # no live copy at all
        self.archive_copy("real_taria_clean.jsonl", "aboth")
        self.place("real_taria_leaked.jsonl", "aboth")  # the live one would fail the guard
        self.place("real_taria_unclosed.jsonl", "alive")  # live only: the fallback
        self.write_rows([
            self.row("j001", "T3", "sonnet", "aarchived", "harness_error"),
            self.row("j002", "T3", "sonnet", "aboth", "passed"),
            self.row("j003", "T4", "haiku", "alive", "passed"),
        ])
        out, rows, _ = self.regrade()
        self.assertEqual({j: r["status"] for j, r in rows.items()}, {"j001": "passed", "j002": "passed",
                                                                     "j003": "passed"})
        self.assertEqual({j: r["regrade_note"] for j, r in rows.items()}, {"j001": None, "j002": None, "j003": None})
        self.assertEqual(rows["j002"]["guard_problems"], [])
        self.assertIn("transcripts: 2 archived, 1 live", out)
        self.assertIn("1 transcript(s) were read live", out)
        self.assertEqual((rows["j003"]["output_tokens_lower_bound"], rows["j003"]["unclosed_messages"]), (True, 1))
        self.assertEqual((rows["j002"]["output_tokens_lower_bound"], rows["j002"]["unclosed_messages"]), (False, 0))

    def archive(self):
        return quiet_cli("archive", "--batch", "b", "--results-dir", self.results, "--projects-dir", self.projects)

    def test_archive_backfills_a_batch_from_the_live_transcripts(self):
        self.place("real_taria_clean.jsonl", "aone")
        self.place("real_taria_midunclosed.jsonl", "atwo")
        self.archive_copy("real_taria_unclosed.jsonl", "athree")  # already archived: kept
        self.place("real_taria_leaked.jsonl", "athree")
        self.write_rows([
            self.row("j001", "T3", "sonnet", "aone", "passed"),
            self.row("j002", "T2", "haiku", "atwo", "passed"),
            self.row("j003", "T4", "haiku", "athree", "passed"),
            self.row("j004", "T1", "sonnet", "agone", "passed"),  # deleted by Claude Code
            self.row("j005", "T1", "haiku", None, "harness_error", error="aborted: test"),
            dict(self.row("j006", "T1", "sonnet", None, "passed"), mode="dry-run", solver="scripted"),
        ])
        code, out, _ = self.archive()
        self.assertEqual(code, runner.EXIT_HARNESS, "a row whose transcript is gone everywhere")
        self.assertIn("2 copied, 1 already archived, 1 missing, 1 row(s) without an agent id", out)
        self.assertIn("j004: no transcript for agent agone", out)
        live = os.path.join(self.projects, "-proj", "sess-1", "subagents")
        for agent_id, src in (("aone", os.path.join(live, "agent-aone.jsonl")),
                              ("atwo", os.path.join(live, "agent-atwo.jsonl")),
                              # Already archived: kept, not replaced by the live one.
                              ("athree", os.path.join(self.FIXTURES, "real_taria_unclosed.jsonl"))):
            archived = runner.archived_transcript(self.paths, agent_id)
            for a, b in ((src, archived), (runner.meta_path(src), runner.meta_path(archived))):
                with open(a, "rb") as fa, open(b, "rb") as fb:
                    self.assertEqual(fa.read(), fb.read(), f"{agent_id}: {b}")
        # Again: nothing left to copy; the missing one is still missing.
        code, out, _ = self.archive()
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("0 copied, 3 already archived, 1 missing", out)
        # Every row has its copy now: exit 0, and regrade reads only the archive.
        shutil.rmtree(self.projects)
        self.write_rows([self.row("j001", "T3", "sonnet", "aone", "passed"),
                         self.row("j002", "T2", "haiku", "atwo", "passed")])
        code, out, _ = self.archive()
        self.assertEqual((code, "0 copied, 2 already archived, 0 missing" in out), (0, True), out)
        out, rows, _ = self.regrade()
        self.assertIn("transcripts: 2 archived, 0 live", out)
        self.assertEqual({j: r["status"] for j, r in rows.items()}, {"j001": "passed", "j002": "passed"})

    def test_archive_needs_rows(self):
        code, _, err = self.archive()
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("has no rows", err)


class AgentsDir(unittest.TestCase):
    def test_the_default_is_this_repos_agents_dir(self):
        self.assertEqual(agents.DEFAULT_DIR, os.path.join(REPO, ".claude", "agents"))
        self.assertEqual(os.path.dirname(os.path.dirname(agents.DEFAULT_DIR)), os.path.dirname(BENCH))
        args = runner.parse_args(["next", "--batch", "b", "--slot", "1"])
        self.assertEqual(args.agents_dir, agents.DEFAULT_DIR)
        args = runner.parse_args(["next", "--batch", "b", "--slot", "1", "--agents-dir", "/elsewhere"])
        self.assertEqual(args.agents_dir, "/elsewhere")
        self.assertEqual(runner.parse_args(["agents", "--slots", "1"]).dir, agents.DEFAULT_DIR)


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
        self.assertEqual(ours, ["tui-outline-1.md", "tui-outline-2.md", "tui-taria-1.md", "tui-taria-2.md",
                                "tui-tmux-1.md", "tui-tmux-2.md"])
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


# The opening of each bridge arm's read_tree description, as taria-mcp
# serves it (what tells the taria and outline arms apart in a transcript).
TOOL_DESCRIPTIONS = {
    ("taria", "read_tree"): "Read the connected TUI app's current semantic tree as JSON: every node ...",
    ("outline", "read_tree"): "Read the connected TUI app's current semantic tree as an outline: every node ...",
}


def write_transcript(directory, agent_id, prompt, arm, calls, agent_type,
                     model="claude-sonnet-5-5", effort="medium", served=None):
    """A minimal subagent transcript in the shape real ones have (see
    bench/transcript.py and fixtures/transcripts/real_*.jsonl), starting now:
    the prompt, a first prompt_snapshot without tools, one assistant message
    per tool call with its result, the snapshot listing the arm's tools and
    the tui server's instructions after the first, and a closing sentence.
    Plus the meta file naming the agent type. `served` is the server's own
    tools/list entries, whose descriptions the snapshot then records (else
    TOOL_DESCRIPTIONS). Returns the transcript's path."""
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
    tools = ([{"name": f"mcp__tui__{t['name']}", "description": t.get("description", "")} for t in served]
             if served is not None else
             [{"name": f"mcp__tui__{t}", "description": TOOL_DESCRIPTIONS.get((arm, t), t)}
              for t in harness.ARM_TOOLS[arm]])
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
                                          job["agent"], served=client.tools)

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
            self.assertEqual((row["output_tokens_lower_bound"], row["unclosed_messages"]), (False, 0))
            archived = os.path.join(self.results, "reh", "transcripts", f"agent-{row['agent_id']}.jsonl")
            self.assertTrue(os.path.isfile(archived) and os.path.isfile(runner.meta_path(archived)), archived)
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
        # No transcript for this id anywhere: the orchestrator's mistake, refused, the slot kept.
        empty = os.path.join(self.tmp, "agent-nobody.jsonl")
        code, refused, err = self.next("err", 1, "--agent-id", "nobody", "--transcript-file", empty)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("no transcript for agent nobody", refused["error"])
        self.assertIsNotNone(runner.read_slot(runner.Paths(self.results), 1), "the slot is kept")
        self.assertFalse(os.path.exists(os.path.join(self.results, "err", "runs.jsonl")), "nothing recorded")
        # Its agent, on the wrong model: a harness error, and the slot claims nothing.
        path = write_transcript(self.tmp, "wrongmodel", job["prompt"], "taria", {"read_tree": 1}, job["agent"],
                                model="claude-haiku-5-5")
        code, refused, err = self.next("err", 1, "--agent-id", "wrongmodel", "--transcript-file", path)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("harness error", refused["error"])
        self.assertIn("ran on claude-haiku-5-5", refused["error"])
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

    def test_an_outline_job_through_one_slot(self):
        """The outline arm end to end: its agent file's launcher execs the
        bridge with --tree-format outline, the outline solver drives it, and
        grading holds the run to the outline (the transcript records the
        description the server served). The taria agent, whose tools have
        the same names, gets none for this slot."""
        run_py("agents", "--slots", "1", "--dir", self.agents_dir, *self.common)
        out = run_py("plan", "--batch", "outl", "--tasks", "T5", "--arms", "outline", "--seed", "3",
                     *self.common)
        self.assertEqual(out.returncode, 0, out.stderr)
        code, job, err = self.next("outl", 1)
        self.assertEqual(code, 0, err)
        self.assertEqual(job["agent"], "tui-outline-1")
        slot = runner.read_slot(runner.Paths(self.results), 1)
        self.assertEqual(slot["arm"], "outline")
        taria = self.launcher("tui-taria-1")
        wrong = subprocess.run([taria["command"], *taria["args"]], capture_output=True, text=True,
                               timeout=30, stdin=subprocess.DEVNULL)
        self.assertEqual(wrong.returncode, 1)
        self.assertIn("for the outline arm, not taria", wrong.stderr)
        agent_id, path = self.act_as_agent("outl", job)
        code, done, err = self.next("outl", 1, "--agent-id", agent_id, "--transcript-file", path)
        self.assertEqual((code, done), (0, {"done": True}), err)
        (row,) = read_rows(self.results, "outl")
        self.assertEqual((row["arm"], row["status"], row["guard_problems"]), ("outline", "passed", []),
                         row["error"] or row["reason"])
        self.assertEqual(row["context"]["tree_formats"], ["outline"])
        # The same run, held to the taria arm, is refused by the guard.
        fields, errors, _ = runner.transcript_verdict(transcript, path, "taria", "sonnet", "medium", 30)
        self.assertEqual(fields["guard_problems"],
                         ["was offered a read_tree that shows the tree as outline, not as the taria arm's json"])

    def test_next_refuses_agent_files_that_do_not_match_the_plan(self):
        run_py("agents", "--slots", "1", "--dir", self.agents_dir, "--effort", "high", *self.common)
        run_py("plan", "--batch", "mismatch", "--tasks", "T1", "--arms", "taria", *self.common)
        code, refused, _ = self.next("mismatch", 1)
        self.assertEqual(code, runner.EXIT_HARNESS)
        self.assertIn("effort is 'high', the plan needs 'medium'", refused["error"])
        self.assertEqual(runner.read_all_slots(runner.Paths(self.results)), {})


# --- The outline arm --------------------------------------------------------------------------------

OUTLINE_EXAMPLE = """protocol_version=1 seq=1
app app "taria-demo"
  tabs tabs "Tabs" = "Active"
    tab-active tab "Active" = "selected" [select]
    tab-done tab "Done" [select]
  tasks list "Active tasks" = "task-1"
    task-1 list_item "Write the taria README" = "todo" [select toggle delete] *focused
    task-3 list_item "Wire up the MCP bridge" = "todo" [select toggle delete]
    task-4 list_item "Record the killer demo" = "todo" [select toggle delete]
  input text_input "New task" = "" [set_value focus]
  quit button "Quit" [activate]"""


class OutlineArm(unittest.TestCase):
    """The third arm: taria's binary and tools, the tree as an outline."""

    def test_the_server_entry_is_tarias_plus_the_tree_format(self):
        run = DemoRun.from_slot(SlotFields().fields())
        taria, outline = harness.server_entry("taria", run), harness.server_entry("outline", run)
        self.assertEqual(outline["command"], harness.MCP_BIN)
        self.assertEqual(outline["command"], taria["command"])
        self.assertEqual(outline["env"], taria["env"])
        self.assertEqual(outline["args"], [*taria["args"], "--tree-format", "outline"])
        self.assertNotIn("--tree-format", taria["args"], "taria keeps the bridge's default, as full1 ran")
        self.assertEqual(mcp_config("outline", run)["mcpServers"]["tui"], outline)
        with self.assertRaises(harness.HarnessError):
            harness.server_entry("outlines", run)

    def test_its_tools_names_and_formats(self):
        self.assertIn("outline", harness.ARMS)
        self.assertEqual(harness.ARM_TOOLS["outline"], harness.ARM_TOOLS["taria"])
        self.assertEqual(harness.SERVER_NAMES["outline"], "taria-mcp")
        self.assertEqual(harness.ARM_TREE_FORMAT, {"taria": "json", "outline": "outline"})
        self.assertEqual(set(harness.ARMS), set(harness.ARM_TOOLS))
        self.assertEqual(set(harness.ARMS), set(solvers.SOLVERS))
        self.assertEqual(runner.parse_args(["dry-run"]).arms, list(harness.ARMS))
        self.assertEqual(runner.parse_args(["plan", "--batch", "b", "--arms", "outline"]).arms, ["outline"])

    def test_the_ready_wait_runs_for_both_bridge_arms(self):
        class Client:
            def __init__(self):
                self.calls = 0

            def call(self, tool):
                self.calls += 1
                if self.calls < 3:
                    raise harness.ToolError("not connected yet")
                return "tree"

        for arm, want in (("taria", 3), ("outline", 3), ("tmux", 0)):
            client = Client()
            harness.wait_ready(arm, client)
            self.assertEqual(client.calls, want, arm)

    def test_the_dry_run_checks_the_served_tree_format(self):
        def tools(description):
            return [{"name": "act", "description": "Invoke ..."}, {"name": "read_tree", "description": description}]

        json_desc = TOOL_DESCRIPTIONS[("taria", "read_tree")]
        outline_desc = TOOL_DESCRIPTIONS[("outline", "read_tree")]
        runner.check_tree_format("taria", "taria-mcp", tools(json_desc))
        runner.check_tree_format("outline", "taria-mcp", tools(outline_desc))
        runner.check_tree_format("tmux", "tmux-mcp", [])
        for arm, desc in (("outline", json_desc), ("taria", outline_desc), ("outline", "Read the tree.")):
            with self.subTest(arm=arm, desc=desc), self.assertRaises(harness.HarnessError) as caught:
                runner.check_tree_format(arm, "taria-mcp", tools(desc))
            self.assertIn(f"the {arm} arm needs {harness.ARM_TREE_FORMAT[arm]}", str(caught.exception))

    def test_its_agent_file_differs_only_in_name_description_and_arm(self):
        slots = "/tmp/slots"
        texts = {arm: agents.render(arm, 2, slots) for arm in harness.ARMS}
        bodies = {arm: text.split("\n---\n", 1)[1].encode() for arm, text in texts.items()}
        self.assertEqual(len(set(bodies.values())), 1, "one body, byte for byte")
        outline = agents.parse(texts["outline"])
        self.assertEqual(agents.body_sha256(outline["body"]), agents.body_sha256())
        self.assertEqual(outline["fields"]["name"], "tui-outline-2")
        self.assertEqual(outline["fields"]["description"],
                         "Benchmark runner, outline arm, slot 2. Spawned only by bench/run.py's orchestrator.")
        self.assertEqual(outline["server"]["args"][1:5], ["--arm", "outline", "--slot", "2"])
        diff = [x for x, y in zip(texts["taria"].splitlines(), texts["outline"].splitlines()) if x != y]
        self.assertEqual([x.split(":")[0].strip() for x in diff], ["name", "description", "args"])

    def test_agents_writes_the_outline_files_and_the_plan_checks_them(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        slots = os.path.join(tmp, "results", "slots")
        written = agents.write(2, tmp, slots)
        names = sorted(os.path.basename(p) for p in written)
        self.assertIn("tui-outline-1.md", names)
        self.assertIn("tui-outline-2.md", names)
        path = os.path.join(tmp, "tui-outline-1.md")
        self.assertTrue(agents.is_ours(path))
        sha = agents.body_sha256()
        self.assertEqual(agents.problems(path, "outline", 1, slots, "medium", 30, sha), [])
        # Read as the taria arm's file, it is the wrong one: name and launcher.
        found = agents.problems(path, "taria", 1, slots, "medium", 30, sha)
        self.assertEqual(len(found), 2, found)
        self.assertIn("name is 'tui-outline-1', the plan needs 'tui-taria-1'", found[0])
        self.assertEqual(len(agents.remove(tmp)), 3 * 2)

    def test_arm_names_cannot_be_read_as_each_other(self):
        self.assertEqual(agents.FILE_RE.match("tui-outline-1.md").groups(), ("outline", "1"))
        self.assertEqual(agents.FILE_RE.match("tui-taria-12.md").groups(), ("taria", "12"))
        for name in ("tui-outline-0.md", "tui-outlinetaria-1.md", "tui-taria-outline-1.md", "xtui-taria-1.md",
                     "tui-outline-1.md.bak"):
            self.assertIsNone(agents.FILE_RE.match(name), name)
        res = runner.ARM_AGENT_RE
        self.assertIsNotNone(res["outline"].fullmatch("tui-outline-1"))
        self.assertIsNotNone(res["taria"].fullmatch("tui-taria-10"))
        for arm, other in (("taria", "tui-outline-1"), ("outline", "tui-taria-1"), ("tmux", "tui-outline-1"),
                           ("outline", "tui-outline-1x"), ("taria", "tui-taria-1-outline")):
            self.assertIsNone(res[arm].fullmatch(other), (arm, other))

    def test_the_meta_files_agent_type_tells_the_arms_apart(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "agent-a1.jsonl")
        with open(runner.meta_path(path), "w") as f:
            json.dump({"agentType": "tui-outline-1"}, f)
        self.assertEqual(runner.agent_type_problems(path, "tui-outline-1"), [])
        self.assertEqual(runner.agent_type_problems(path, runner.ARM_AGENT_RE["outline"]), [])
        self.assertEqual(runner.agent_type_problems(path, "tui-taria-1"),
                         ["was spawned as 'tui-outline-1', not this slot's agent 'tui-taria-1'"])
        self.assertTrue(runner.agent_type_problems(path, runner.ARM_AGENT_RE["taria"]))
        with open(runner.meta_path(path), "w") as f:
            json.dump({"agentType": "tui-taria-1"}, f)
        self.assertTrue(runner.agent_type_problems(path, runner.ARM_AGENT_RE["outline"]))
        self.assertTrue(runner.agent_type_problems(path, "tui-outline-1"))

    def test_the_launcher_refuses_the_other_bridge_arm(self):
        slots = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, slots)
        for held, launched in (("taria", "outline"), ("outline", "taria")):
            with open(os.path.join(slots, "1.json"), "w") as f:
                json.dump({"job": "j001", "arm": held, "state": "ready", "demo": SlotFields().fields()}, f)
            out = subprocess.run(
                [sys.executable, MCP_LAUNCH, "--arm", launched, "--slot", "1", "--slots-dir", slots],
                capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL,
            )
            self.assertEqual(out.returncode, 1)
            self.assertEqual(out.stdout, "")
            self.assertIn(f"for the {held} arm, not {launched}", out.stderr)
            self.assertEqual([f for f in os.listdir(slots) if f.endswith(".launched")], [])


class OutlineParser(unittest.TestCase):
    """The outline solver's reading of taria-mcp's outline (docs/cli.md)."""

    def test_the_documented_example(self):
        tree = solvers.parse_outline(OUTLINE_EXAMPLE)
        self.assertEqual((tree["protocol_version"], tree["seq"]), (1, 1))
        root = tree["root"]
        self.assertEqual((root["id"], root["role"], root["label"]), ("app", "app", "taria-demo"))
        self.assertEqual([c["id"] for c in root["children"]], ["tabs", "tasks", "input", "quit"])
        task1 = solvers.find(tree, "task-1")
        self.assertEqual(task1, {"id": "task-1", "role": "list_item", "label": "Write the taria README",
                                 "value": "todo", "actions": ["select", "toggle", "delete"], "focused": True})
        self.assertNotIn("focused", solvers.find(tree, "task-3"))
        self.assertEqual(solvers.find(tree, "input")["value"], "")
        self.assertNotIn("value", solvers.find(tree, "tab-done"))
        self.assertEqual(solvers.find(tree, "tabs")["value"], "Active")
        self.assertEqual([n["id"] for n in solvers.TariaSolver.items(tree)], ["task-1", "task-3", "task-4"])

    def test_quoted_names_escapes_and_extra_fields(self):
        text = "\n".join([
            'protocol_version=1 seq=7 "odd key"={"a":[1,2]}',
            '"root node" {"weird":{"x":1}} "A \\"quoted\\" ] label" = "line\\nbreak" '
            '["do it" go] *focused hint="a b" score=3',
            '  child "role with space" [x]',
            '    grandchild leaf',
            '  sibling sparkline "S" = "" "*focusedish"=true',
        ])
        tree = solvers.parse_outline(text)
        self.assertEqual(tree["odd key"], {"a": [1, 2]})
        root = tree["root"]
        self.assertEqual(root["id"], "root node")
        self.assertEqual(root["role"], {"weird": {"x": 1}})
        self.assertEqual(root["label"], 'A "quoted" ] label')
        self.assertEqual(root["value"], "line\nbreak")
        self.assertEqual(root["actions"], ["do it", "go"])
        self.assertIs(root["focused"], True)
        self.assertEqual((root["hint"], root["score"]), ("a b", 3))
        child, sibling = root["children"]
        self.assertEqual((child["id"], child["role"], child["actions"]), ("child", "role with space", ["x"]))
        self.assertEqual(child["children"], [{"id": "grandchild", "role": "leaf"}])
        self.assertEqual((sibling["role"], sibling["value"], sibling["*focusedish"]), ("sparkline", "", True))
        self.assertNotIn("focused", sibling)

    def test_a_quoted_key_right_after_the_role_is_a_field_not_a_label(self):
        """docs/cli.md: a quoted token followed directly by `=` is a field
        key; only one followed by a space or the line's end is the label."""
        def root(line):
            return solvers.parse_outline("protocol_version=1\n" + line)["root"]

        self.assertEqual(root('app app "odd key"=1'), {"id": "app", "role": "app", "odd key": 1})
        self.assertEqual(root('app app "odd key"={"a":"b c"} score=2'),
                         {"id": "app", "role": "app", "odd key": {"a": "b c"}, "score": 2})
        self.assertEqual(root('app app "L" "odd key"=1'), {"id": "app", "role": "app", "label": "L", "odd key": 1})
        self.assertEqual(root('app app "L" = "V" "k"=[1]'),
                         {"id": "app", "role": "app", "label": "L", "value": "V", "k": [1]})
        self.assertEqual(root('app app "a=b"'), {"id": "app", "role": "app", "label": "a=b"})
        # Once a field has come, a quoted token is no label any more.
        for line in ('app app "k"=1 "late label"', 'app app "L" "second label"'):
            with self.subTest(line=line), self.assertRaises(solvers.SolverError):
                root(line)

    def test_what_is_not_an_outline_is_refused(self):
        for text in ("app app", "protocol_version=1\n  app app", "protocol_version=1\napp app\n   odd x",
                     "protocol_version=1\napp app\n    skipped x", "protocol_version=1\napp app\nsecond root",
                     "The app received this input.\napp app", 'protocol_version=1\napp app "unclosed'):
            with self.subTest(text=text), self.assertRaises(solvers.SolverError):
                solvers.parse_outline(text)

    def test_results_carry_an_outline_a_note_and_an_outline_or_neither(self):
        self.assertEqual(solvers.outline_in(OUTLINE_EXAMPLE), solvers.parse_outline(OUTLINE_EXAMPLE))
        note = "The app received this input and deliberately did nothing with it. Re-plan from the current tree below."
        self.assertEqual(solvers.outline_in(note + "\n" + OUTLINE_EXAMPLE), solvers.parse_outline(OUTLINE_EXAMPLE))
        self.assertIsNone(solvers.outline_in("The app received this input, and its tree did not change within 500ms."))
        # A tree the outline cannot express is served as JSON, and read so.
        as_json = {"protocol_version": 1, "seq": 2, "root": {"id": "app", "role": "app"}}
        self.assertEqual(solvers.outline_in(json.dumps(as_json)), as_json)

    def test_the_sabotage_and_plans_cover_every_arm(self):
        for arm in harness.ARMS:
            solver = solvers.SOLVERS[arm](None)
            for task_id, (plan, wrong) in solvers.PLANS.items():
                for method, *_ in plan + wrong:
                    self.assertTrue(callable(getattr(solver, method, None)), (arm, task_id, method))


def _normalized(node):
    """A JSON tree node as the outline shows it: a custom action by its
    inner name, focused only when true, no empty lists or null strings."""
    out = {"id": node["id"], "role": node["role"] if not isinstance(node["role"], dict) else next(iter(node["role"]))}
    for key in ("label", "value"):
        if node.get(key) is not None:
            out[key] = node[key]
    actions = [a["custom"] if isinstance(a, dict) and "custom" in a else a for a in node.get("actions") or []]
    if actions:
        out["actions"] = actions
    if node.get("focused") is True:
        out["focused"] = True
    children = [_normalized(c) for c in node.get("children") or []]
    if children:
        out["children"] = children
    for key, value in node.items():
        if key not in ("id", "role", "label", "value", "actions", "focused", "children"):
            out[key] = value
    return out


@unittest.skipUnless(HAVE_TMUX, "tmux is not on PATH")
class OutlineLive(unittest.TestCase):
    """The outline the real bridge serves parses to the tree its JSON mode
    serves for the same demo, and its read_tree says it is an outline."""

    @classmethod
    def setUpClass(cls):
        require_fresh_build()

    def test_the_parsed_outline_is_the_json_tree(self):
        demo = runner.prepare_demo(tasks.TASKS["T6"])  # the dialog open: the deepest tree
        self.addCleanup(demo.close)
        trees, descriptions = {}, {}
        for arm in ("taria", "outline"):  # one after the other: the app serves one bridge at a time
            client = McpStdio(harness.server_entry(arm, demo), cwd=demo.workdir, name=arm)
            try:
                client.initialize()
                client.tools_list()
                descriptions[arm] = next(t["description"] for t in client.tools if t["name"] == "read_tree")
                harness.wait_ready(arm, client)
                trees[arm] = client.call("read_tree")
            finally:
                client.close()
        self.assertTrue(trees["taria"].startswith("{"), trees["taria"][:80])
        self.assertTrue(trees["outline"].startswith("protocol_version="), trees["outline"][:80])
        as_json = json.loads(trees["taria"])
        parsed = solvers.parse_outline(trees["outline"])
        self.assertEqual(parsed["root"], _normalized(as_json["root"]))
        # The header holds the same top-level fields (seq may move between two connections).
        self.assertEqual(sorted(parsed), sorted(as_json))
        self.assertEqual(parsed["protocol_version"], as_json["protocol_version"])
        self.assertIsNotNone(solvers.find(parsed, "dialog-confirm"))
        self.assertEqual(transcript.tree_format_of(descriptions["taria"]), "json")
        self.assertEqual(transcript.tree_format_of(descriptions["outline"]), "outline")
        self.assertLess(len(trees["outline"]), len(trees["taria"]))


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
