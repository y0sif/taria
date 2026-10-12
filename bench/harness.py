"""The benchmark's plumbing: the build check, the demo under test, each arm's
MCP config, and a small stdio MCP client.

Shared by run.py (the runner) and test_bench.py. Nothing here imports
scripts/: the CI gate's helpers are free to change without moving the
benchmark, and the benchmark never reaches into their internals.

Python 3 stdlib only.
"""

import json
import os
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

BENCH = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(BENCH)
DEMO_BIN = os.path.join(REPO, "target", "debug", "taria-demo")
MCP_BIN = os.path.join(REPO, "target", "debug", "taria-mcp")
TMUX_MCP = os.path.join(BENCH, "tmux_mcp.py")

SERVER_KEY = "tui"  # one name for every arm, so tool names do not name the arm
# taria: the bridge's JSON tree. tmux: the screen. outline: the same bridge
# as taria, showing the same tree as an indented outline (--tree-format).
ARMS = ("taria", "tmux", "outline")
# Exactly the tools each arm's server lists. The dry run checks the live
# servers against this, and grading holds the agent's tool calls to it
# (transcript.guard), so the two cannot drift apart unnoticed.
ARM_TOOLS = {
    "taria": ("read_tree", "act", "key", "type_text"),
    "tmux": ("screen", "send_keys"),
    "outline": ("read_tree", "act", "key", "type_text"),
}
# How the bridge arms show the tree. taria and outline list the same tools,
# so what tells them apart in a transcript is the read_tree description
# their server serves (transcript.guard checks it), and in the dry run the
# text the solver has to parse. taria runs the bridge's default (json), as
# every batch before the outline arm did.
ARM_TREE_FORMAT = {"taria": "json", "outline": "outline"}
SERVER_NAMES = {"taria": "taria-mcp", "tmux": "tmux-mcp", "outline": "taria-mcp"}

COLS, ROWS = 120, 40
READY_TIMEOUT = 5.0
RPC_TIMEOUT = 10.0

# What the binaries under test are built from, for the freshness check.
BINARIES = (DEMO_BIN, MCP_BIN)
SOURCE_ROOTS = ("crates", "examples", "Cargo.toml", "Cargo.lock")
CODE_SUFFIXES = (".rs",)
MANIFEST_SUFFIXES = (".toml", ".lock")


class HarnessError(Exception):
    """The run could not be carried out; nothing about the arm is measured."""


class ToolError(Exception):
    """A tools/call came back as a JSON-RPC error or an isError result."""

    def __init__(self, message, code=None):
        super().__init__(message)
        self.message = message
        self.code = code


# --- The build ------------------------------------------------------------------


def _newest(paths):
    """(path, mtime) of the newest file in `paths` that exists, or (None, 0.0)."""
    newest, newest_at = None, 0.0
    for path in paths:
        try:
            mtime = os.path.getmtime(path)
        except OSError:
            continue
        if mtime > newest_at:
            newest, newest_at = path, mtime
    return newest, newest_at


def _tree_files(repo, suffixes):
    """Every file under SOURCE_ROOTS in `repo` ending in one of `suffixes`."""
    for root in SOURCE_ROOTS:
        path = os.path.join(repo, root)
        if os.path.isfile(path):
            if path.endswith(suffixes):
                yield path
            continue
        for dirpath, _, names in os.walk(path):
            for name in names:
                if name.endswith(suffixes):
                    yield os.path.join(dirpath, name)


def dep_info_sources(binary):
    """The source files cargo's dep-info (`<binary>.d`) says `binary` was built
    from, or None when there is no readable dep-info for it.

    The file is Makefile syntax, `<binary>: <dep> <dep> ...` on its first
    line, with a space inside a path escaped as a backslash and a space.
    """
    try:
        with open(binary + ".d") as f:
            first = f.readline().rstrip("\n")
    except OSError:
        return None
    prefix = binary.replace(" ", "\\ ") + ":"
    if not first.startswith(prefix):
        return None
    deps = [d.replace("\\ ", " ") for d in re.split(r"(?<!\\) ", first[len(prefix):]) if d]
    return deps or None


def _build_stamp(binary):
    """When a build last considered `binary`: the later of the binary and its
    dep-info file. Cargo rewrites `<binary>.d` on every build that includes
    the binary, relinked or not, so a manifest touched without a change that
    matters (a checkout rewriting Cargo.lock as it was) is behind the stamp
    after the next `cargo build --workspace`, though the binary is not."""
    stamp = os.path.getmtime(binary)
    try:
        stamp = max(stamp, os.path.getmtime(binary + ".d"))
    except OSError:
        pass
    return stamp


def require_fresh_build(repo=REPO, binaries=BINARIES):
    """Refuse a target/debug where any required binary is missing or stale.

    The benchmark never builds, so a stale binary would measure a build
    nobody asked for. Each binary is held to the sources it is built from,
    which cargo lists in its dep-info file, so rebuilding one binary cannot
    hide a stale other, and a binary a build had no reason to relink (an
    edit to the other one's crate) is not refused. Without a readable
    dep-info, a binary is held to every `.rs` file in the tree. Manifests
    and the lock file are workspace-wide and dated against the last build,
    the newest binary or dep-info file (see _build_stamp): a build after them
    relinks whatever they changed, and rewrites the dep-info even when they
    changed nothing cargo acts on.
    """
    missing = [b for b in binaries if not os.path.exists(b)]
    if missing:
        raise HarnessError(
            f"{', '.join(os.path.relpath(b, repo) for b in missing)} not built; "
            "run cargo build --workspace, then rerun"
        )
    all_code = None
    stale = []  # (lag in seconds, what is stale, the newer source)
    for binary in binaries:
        sources = dep_info_sources(binary)
        if sources is None:
            if all_code is None:
                all_code = list(_tree_files(repo, CODE_SUFFIXES))
            sources = all_code
        source, source_at = _newest(sources)
        if source is None:
            raise HarnessError(f"found no sources to date {os.path.relpath(binary, repo)} against")
        built_at = os.path.getmtime(binary)
        if built_at < source_at:
            stale.append((source_at - built_at, os.path.relpath(binary, repo), source))
    manifest, manifest_at = _newest(_tree_files(repo, MANIFEST_SUFFIXES))
    last_build = max(_build_stamp(b) for b in binaries)
    if manifest is not None and last_build < manifest_at:
        stale.append((manifest_at - last_build, "the last build in target/debug", manifest))
    if stale:
        lag, what, source = max(stale)
        raise HarnessError(
            f"{what} is {lag:.0f}s older than {os.path.relpath(source, repo)}, so it is "
            "not this source tree; run cargo build --workspace, then rerun"
        )


# --- The demo under test --------------------------------------------------------


def _cmdline(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return f.read().replace(b"\0", b" ").decode(errors="replace")
    except OSError:
        return None


def _environ(pid):
    """The environment `pid` was started with, as a list of NAME=value, or None."""
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            return f.read().decode(errors="replace").split("\0")
    except OSError:
        return None


def _is_ours(pid, needle, env_entry=None):
    """Whether /proc shows `pid` as the process this run started: its command
    line names `needle` and, when `env_entry` is given, it was started with
    that NAME=value. A pid can be reused once its process exits, and a slot is
    torn down by a later process than the one that started it, so the command
    line alone (every demo's names DEMO_BIN) is not proof enough."""
    line = _cmdline(pid)
    if line is None or needle not in line:
        return False
    if env_entry is None:
        return True
    env = _environ(pid)
    return env is not None and env_entry in env


def _kill_if_ours(pid, needle, env_entry=None):
    """SIGKILL `pid` only while /proc still shows it as ours (see _is_ours).

    Without /proc (not Linux) nothing is killed here; tmux kill-server has
    already done the work there.
    """
    deadline = time.monotonic() + 1.0
    while time.monotonic() < deadline:
        if not _is_ours(pid, needle, env_entry):
            return
        time.sleep(0.05)
    if _is_ours(pid, needle, env_entry):
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass


# The longest AF_UNIX path taria binds (crates/taria/src/socket.rs).
SOCKET_PATH_MAX = 107 if sys.platform.startswith("linux") else 103
# The demo draws its socket path on its header line, which the tmux arm
# reads: a temp dir naming either arm or the benchmark would tell it which.
TELLTALE = ("taria", "tmux", "bench")
# What a slot file must carry to reach and tear down its demo.
SLOT_FIELDS = (
    "name", "tmpdir", "workdir", "sock", "tmux_sock", "state_path", "tmux_conf",
    "server_pid", "demo_pid",
)


class DemoRun:
    """One taria-demo in its own tmux server and temp dir.

    Layout, chosen so the agent sees nothing that names the benchmark:

        <tmp>/work/        the agent's working directory, empty
        <tmp>/s/app.sock   the demo's taria socket (the tmux arm sees this
                           path on the demo's header line)
        <tmp>/s/tmux.sock  this run's private tmux server
        <tmp>/state.json   the demo's state dump, graded after the run

        <tmp>/tmux.conf    the private server's only config: status off

    `<tmp>` has a neutral `tui-` prefix, and a temp dir whose path names an
    arm or the benchmark is refused (the tmux arm reads the socket path off
    the screen). The tmux session is named `taria-bench-<hex>`, which never
    reaches the agent: the status line is off, so capture-pane returns the
    whole COLS x ROWS pane and nothing else.

    A run started by one process can be graded and torn down by another:
    to_slot() is everything needed to reach it, and from_slot() rebuilds it.
    The demo lives in the tmux server, which detaches into its own session,
    so it outlives the process that started it.
    """

    def __init__(self):
        base = tempfile.gettempdir()
        told = [word for word in TELLTALE if word in base.lower()]
        if told:
            raise HarnessError(
                f"the temp dir {base} names {', '.join(told)}, and the demo draws its socket "
                "path on screen, where the tmux arm reads it; set TMPDIR=/tmp, then rerun"
            )
        self.name = f"taria-bench-{uuid.uuid4().hex[:8]}"
        self.tmpdir = tempfile.mkdtemp(prefix="tui-")
        self.workdir = os.path.join(self.tmpdir, "work")
        sockdir = os.path.join(self.tmpdir, "s")
        self.sock = os.path.join(sockdir, "app.sock")
        self.tmux_sock = os.path.join(sockdir, "tmux.sock")
        self.state_path = os.path.join(self.tmpdir, "state.json")
        if len(os.fsencode(self.sock)) > SOCKET_PATH_MAX:
            shutil.rmtree(self.tmpdir, ignore_errors=True)
            raise HarnessError(
                f"the demo's socket path {self.sock} is over {SOCKET_PATH_MAX} bytes, so taria "
                "would refuse to bind it; set TMPDIR=/tmp, then rerun"
            )
        # A private server on a socket inside the temp dir, with a config of
        # its own: the user's tmux sessions and settings stay out of the
        # measurement, teardown cannot touch anything but this run, and
        # removing the temp dir removes the socket whatever else failed. The
        # config turns the status line off before the session exists, so the
        # demo starts at the full COLS x ROWS (with it on, a row goes to the
        # status line and the demo is resized under both arms).
        self.tmux_conf = os.path.join(self.tmpdir, "tmux.conf")
        self.tmux = ["tmux", "-S", self.tmux_sock, "-f", self.tmux_conf]
        self.server_pid = None
        self.demo_pid = None
        try:
            os.mkdir(self.workdir, 0o700)
            os.mkdir(sockdir, 0o700)
            with open(self.tmux_conf, "w") as f:
                f.write("set-option -g status off\n")
        except BaseException:  # a signal included: nothing else holds tmpdir yet
            shutil.rmtree(self.tmpdir, ignore_errors=True)
            raise

    def to_slot(self):
        """What a later process needs to talk to this demo and tear it down."""
        return {field: getattr(self, field) for field in SLOT_FIELDS}

    @classmethod
    def from_slot(cls, fields):
        """The run to_slot() described, for grading and close() from another process.

        Refuses fields that do not describe one of ours: close() removes
        `tmpdir`, so it must be a `tui-` temp dir holding every other path.
        """
        missing = [f for f in SLOT_FIELDS if f not in fields]
        if missing:
            raise HarnessError(f"slot file lacks {', '.join(missing)}")
        tmpdir = fields["tmpdir"]
        if not (isinstance(tmpdir, str) and os.path.isabs(tmpdir)
                and os.path.basename(tmpdir.rstrip(os.sep)).startswith("tui-")):
            raise HarnessError(f"slot file's tmpdir {tmpdir!r} is not a tui- temp dir")
        for field in ("workdir", "sock", "tmux_sock", "state_path", "tmux_conf"):
            path = fields[field]
            if not (isinstance(path, str) and os.path.isabs(path) and path != tmpdir
                    and os.path.commonpath([tmpdir, path]) == tmpdir):
                raise HarnessError(f"slot file's {field} {path!r} is outside {tmpdir}")
        for field in ("server_pid", "demo_pid"):
            if fields[field] is not None and not isinstance(fields[field], int):
                raise HarnessError(f"slot file's {field} {fields[field]!r} is not a pid")
        run = cls.__new__(cls)
        for field in SLOT_FIELDS:
            setattr(run, field, fields[field])
        run.tmux = ["tmux", "-S", run.tmux_sock, "-f", run.tmux_conf]
        return run

    def _tmux(self, *args, check=True):
        env = {k: v for k, v in os.environ.items() if k != "TMUX"}
        try:
            result = subprocess.run(
                self.tmux + list(args), capture_output=True, text=True, env=env, timeout=10
            )
        except (OSError, subprocess.SubprocessError) as err:
            raise HarnessError(f"tmux {args[0]} could not run: {err}")
        if check and result.returncode != 0:
            raise HarnessError(f"tmux {args[0]} failed: {result.stderr.strip()}")
        return result

    def start(self):
        self._tmux(
            "new-session", "-d", "-s", self.name, "-x", str(COLS), "-y", str(ROWS),
            "env", f"TARIA_SOCK={self.sock}", f"TARIA_DEMO_STATE={self.state_path}",
            DEMO_BIN,
        )
        # Only a fallback for close(): kill-server through the socket path is
        # the real teardown, so a failure here costs nothing.
        info = self._tmux("display-message", "-p", "-t", self.name, "#{pid} #{pane_pid}", check=False)
        try:
            self.server_pid, self.demo_pid = (int(x) for x in info.stdout.split())
        except ValueError:
            pass
        deadline = time.monotonic() + READY_TIMEOUT
        while time.monotonic() < deadline:
            if os.path.exists(self.sock) and os.path.exists(self.state_path):
                break
            time.sleep(0.02)
        else:
            raise HarnessError(
                f"the demo never came up (socket {os.path.exists(self.sock)}, "
                f"state dump {os.path.exists(self.state_path)}); pane:\n{self.capture()}"
            )
        # The first frame is drawn right after the first dump; wait for it to
        # hold still so neither arm's first look races the first draw.
        self.wait_screen_settled(deadline - time.monotonic())

    def socket_accepts(self):
        """Whether the demo's taria socket takes a connection right now."""
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        probe.settimeout(1.0)
        try:
            probe.connect(self.sock)
            return True
        except OSError:
            return False
        finally:
            probe.close()

    def wait_ready_for_agent(self, timeout=READY_TIMEOUT):
        """Hold until either arm's server, started later, finds the demo ready:
        the socket takes a connection, the state dump exists, and the pane
        holds a drawn, settled screen. No MCP client is started here."""
        deadline = time.monotonic() + timeout
        while not (os.path.exists(self.state_path) and self.socket_accepts()):
            if time.monotonic() > deadline:
                raise HarnessError(
                    f"the demo is not ready for an agent (socket accepts {self.socket_accepts()}, "
                    f"state dump {os.path.exists(self.state_path)})"
                )
            time.sleep(0.05)
        return self.wait_screen_settled(max(deadline - time.monotonic(), 1.0))

    def wait_screen_settled(self, timeout=READY_TIMEOUT):
        deadline = time.monotonic() + max(timeout, 0.2)
        last = None
        while time.monotonic() < deadline:
            screen = self.capture()
            if screen.strip() and screen == last:
                return screen
            last = screen
            time.sleep(0.05)
        raise HarnessError("the demo's screen never settled")

    def alive(self):
        return self._tmux("has-session", "-t", self.name, check=False).returncode == 0

    def demo_alive(self):
        """Whether the demo process itself still runs, not just its dump.

        A crashed demo leaves a dump that says `quit: false`, so grading asks
        the process. The pane's pid is the demo (tmux execs the command, env
        execs the binary); /proc must still show it, which a zombie does not.
        Without /proc or a pid, the tmux session's life stands in: it ends
        when the demo does.
        """
        if self.demo_pid and os.path.isdir("/proc"):
            return _is_ours(self.demo_pid, DEMO_BIN, f"TARIA_SOCK={self.sock}")
        return self.alive()

    def pane_size(self):
        """(columns, rows) of the demo's pane, as the demo sees it."""
        out = self._tmux("display-message", "-p", "-t", self.name, "#{pane_width} #{pane_height}").stdout
        cols, rows = (int(x) for x in out.split())
        return cols, rows

    def capture(self):
        return self._tmux("capture-pane", "-p", "-t", self.name, check=False).stdout

    def send(self, *keys):
        """One tmux send-keys per key, apart: an Esc glued to the next key reads as alt+key."""
        for i, key in enumerate(keys):
            if i:
                time.sleep(0.03)
            self._tmux("send-keys", "-t", self.name, "--", key)

    def read_state(self):
        with open(self.state_path) as f:
            return json.load(f)

    def wait_state(self, predicate, timeout=2.0, what="the expected state"):
        deadline = time.monotonic() + timeout
        state = self.read_state()
        while not predicate(state):
            if time.monotonic() > deadline:
                raise HarnessError(f"the demo never reached {what}; state: {state}")
            time.sleep(0.02)
            state = self.read_state()
        return state

    def settled_state(self, cap=1.0):
        """The dump once two reads 100 ms apart agree (or `cap` runs out)."""
        deadline = time.monotonic() + cap
        last = self.read_state()
        while time.monotonic() < deadline:
            time.sleep(0.1)
            current = self.read_state()
            if current == last:
                return current
            last = current
        return last

    def quit(self):
        """Quit the demo the way a person would, whatever state it was left in.

        Esc closes a dialog, Esc leaves the input, q quits from the list.
        Returns True if the demo exited on its own.
        """
        for key in ("Escape", "Escape", "q"):
            if not self.alive():
                break
            self.send(key)
            time.sleep(0.15)
        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            if not self.alive():
                return True
            time.sleep(0.05)
        return False

    def close(self):
        """Tear everything down. Safe after any failure, twice, or mid-signal.

        No step depends on an earlier one having worked: kill-server goes
        through the socket path this run chose, the pid fallbacks check /proc
        before killing, and the temp dir (socket included) goes regardless.
        """
        if os.path.exists(self.tmux_sock):
            env = {k: v for k, v in os.environ.items() if k != "TMUX"}
            try:
                subprocess.run(
                    self.tmux + ["kill-server"], capture_output=True, env=env, timeout=5
                )
            except (OSError, subprocess.SubprocessError):
                pass
        if self.demo_pid:
            _kill_if_ours(self.demo_pid, DEMO_BIN, f"TARIA_SOCK={self.sock}")
        if self.server_pid:
            _kill_if_ours(self.server_pid, self.tmux_sock)
        shutil.rmtree(self.tmpdir, ignore_errors=True)


def server_entry(arm, run):
    """The stdio server `arm` runs against `run`'s demo, as an MCP config entry.

    The one definition of each arm's server: the dry run spawns it from this
    entry, and bench/mcp_launch.py execs it for an agent.
    """
    if arm == "taria":
        return {
            "type": "stdio",
            "command": MCP_BIN,
            "args": ["--app", "taria-demo"],
            "env": {"TARIA_SOCK": run.sock},
        }
    if arm == "outline":
        return {
            "type": "stdio",
            "command": MCP_BIN,
            "args": ["--app", "taria-demo", "--tree-format", "outline"],
            "env": {"TARIA_SOCK": run.sock},
        }
    if arm == "tmux":
        return {
            "type": "stdio",
            "command": sys.executable,
            "args": [TMUX_MCP, "--session", run.name, "--socket-path", run.tmux_sock],
        }
    raise HarnessError(f"unknown arm {arm!r}; choose from {', '.join(ARMS)}")


def mcp_config(arm, run):
    """The `mcpServers` config for `arm` against `run`'s demo."""
    return {"mcpServers": {SERVER_KEY: server_entry(arm, run)}}


# --- A stdio MCP client -----------------------------------------------------------


class McpStdio:
    """One MCP server spawned from a config entry, spoken to over stdio.

    Newline-delimited JSON-RPC 2.0: initialize, tools/list and tools/call,
    each with a timeout. A transport failure (the server exiting, closing
    stdout, sending something that is not JSON, or not answering in time) is
    a HarnessError naming the server and quoting its stderr; a tool that
    answers with an error is a ToolError, which a solver may expect.
    """

    def __init__(self, entry, cwd, name):
        self.name = name
        env = dict(os.environ)
        env.update(entry.get("env", {}))
        try:
            self.proc = subprocess.Popen(
                [entry["command"], *entry.get("args", [])],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=env,
                cwd=cwd,
            )
        except OSError as err:
            raise HarnessError(f"{name} could not start: {err}")
        self._next_id = 0
        self.tools = []
        self._buf = b""
        self._stderr = bytearray()
        self._errdrain = threading.Thread(target=self._drain_stderr, daemon=True)
        self._errdrain.start()

    def _drain_stderr(self):
        for line in self.proc.stderr:
            self._stderr += line
            del self._stderr[:-4096]

    def _broken(self, what):
        code = self.proc.poll()
        status = f"exited with code {code}" if code is not None else "is still running"
        tail = self._stderr.decode(errors="replace").strip()
        return HarnessError(
            f"{self.name} {what} ({status}){'; stderr: ' + tail[-800:] if tail else ''}"
        )

    def _send(self, obj):
        try:
            self.proc.stdin.write((json.dumps(obj) + "\n").encode())
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError, ValueError):
            raise self._broken("stopped reading its stdin")

    def _read_line(self, deadline):
        fd = self.proc.stdout.fileno()
        while True:
            nl = self._buf.find(b"\n")
            if nl >= 0:
                line, self._buf = self._buf[:nl], self._buf[nl + 1 :]
                return line
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise self._broken("sent no complete line in time")
            ready, _, _ = select.select([fd], [], [], min(remaining, 0.25))
            if ready:
                chunk = os.read(fd, 65536)
                if not chunk:
                    raise self._broken("closed its stdout")
                self._buf += chunk

    def request(self, method, params=None, timeout=RPC_TIMEOUT):
        self._next_id += 1
        req_id = self._next_id
        msg = {"jsonrpc": "2.0", "id": req_id, "method": method}
        if params is not None:
            msg["params"] = params
        self._send(msg)
        deadline = time.monotonic() + timeout
        while True:
            line = self._read_line(deadline)
            if not line.strip():
                continue
            try:
                resp = json.loads(line)
            except ValueError:
                raise self._broken(f"answered {method} with a line that is not JSON: {line[:200]!r}")
            if isinstance(resp, dict) and resp.get("id") == req_id:
                return resp
            # A notification or a request from the server: not ours to answer.

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self._send(msg)

    def initialize(self):
        resp = self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "taria-bench", "version": "0.1.0"},
            },
        )
        if "error" in resp:
            raise HarnessError(f"{self.name} refused initialize: {resp['error']}")
        self.notify("notifications/initialized")
        return resp["result"]

    def tools_list(self):
        """The tool names; `self.tools` keeps the full entries, descriptions included."""
        resp = self.request("tools/list", {})
        if "error" in resp:
            raise HarnessError(f"{self.name} refused tools/list: {resp['error']}")
        self.tools = resp["result"]["tools"]
        return [tool["name"] for tool in self.tools]

    def call(self, tool, arguments=None, timeout=RPC_TIMEOUT):
        """tools/call; returns the result's text. Raises ToolError on an error result."""
        resp = self.request("tools/call", {"name": tool, "arguments": arguments or {}}, timeout)
        if "error" in resp:
            err = resp["error"]
            raise ToolError(f"{self.name} {tool}: {err.get('message', err)}", err.get("code"))
        result = resp.get("result") or {}
        text = "\n".join(
            c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"
        )
        if result.get("isError"):
            raise ToolError(f"{self.name} {tool}: {text or 'isError with no content'}")
        return text

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        if self.proc.poll() is None:
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                try:
                    self.proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    pass
        self._errdrain.join(1)
        for pipe in (self.proc.stdout, self.proc.stderr):
            try:
                pipe.close()
            except (OSError, ValueError):
                pass


def wait_ready(arm, client, timeout=READY_TIMEOUT):
    """Hold the solver until the server can answer, as a real client's startup does.

    Claude Code starts its MCP servers before the model's first turn, so an
    agent never meets a bridge that has not connected yet. These calls are
    the harness's, not the solver's, and are not counted.
    """
    if arm not in ARM_TREE_FORMAT:
        return
    deadline = time.monotonic() + timeout
    while True:
        try:
            client.call("read_tree")
            return
        except ToolError as err:
            if time.monotonic() > deadline:
                raise HarnessError(f"taria-mcp ({arm} arm) never reached the demo: {err.message}")
            time.sleep(0.1)
