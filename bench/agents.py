"""The subagent definitions an orchestrating Claude Code session spawns, one
per arm per slot: `tui-<arm>-<slot>.md`.

Each file gives its agent exactly one tool server, `tui`, defined inline, so
Claude Code starts it when the agent starts and stops it when the agent
finishes. The server is bench/mcp_launch.py, which reads that slot's state
file (written by `run.py next`) and execs the arm's real server pointed at
that slot's demo. `tools: mcp__tui` grants every tool of that server and
nothing else.

The arms' files (taria, tmux, outline) differ only in `name`, `description`
and the launcher's `--arm`/`--slot`; the body, which is the agent's system
prompt, is the same bytes in all of them, and its sha256 is recorded in the
batch's plan.json. FILE_RE anchors the arm between `tui-` and `-<slot>.md`,
so no arm's name can be read as another's.

The frontmatter follows https://code.claude.com/docs/en/sub-agents.md, whose
inline example is

    mcpServers:
      # Inline definition: scoped to this subagent only
      - playwright:
          type: stdio
          command: npx
          args: ["-y", "@playwright/mcp@latest"]

Strings are written as JSON, which YAML reads as double-quoted scalars, so
`parse` here can read back exactly what `render` writes without a YAML
library.

Python 3 stdlib only.
"""

import hashlib
import json
import os
import re
import sys

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
from harness import ARMS, REPO, SERVER_KEY  # noqa: E402

MCP_LAUNCH = os.path.join(BENCH, "mcp_launch.py")
# The repo this bench is in: a Claude Code session started at its root loads
# these agents. Pass --dir (agents) and --agents-dir (next) to put them elsewhere.
DEFAULT_DIR = os.path.join(REPO, ".claude", "agents")
DEFAULT_EFFORT = "medium"
DEFAULT_MAX_TURNS = 30
EFFORTS = ("low", "medium", "high", "xhigh", "max")
# The `model` field is a default: the orchestrator's Agent call names the
# model for each job, which overrides it.
DEFAULT_MODEL = "sonnet"

BODY = (
    "You are operating a terminal application on the user's behalf. The application is "
    "already running, and the tools you have are your only way to see it and control it. "
    "Do what the user asks. When you are done, reply with one short sentence saying what "
    "you did."
)
MARKER = "Spawned only by bench/run.py's orchestrator."
FILE_RE = re.compile(r"^tui-(%s)-([1-9][0-9]*)\.md$" % "|".join(ARMS))


def agent_name(arm, slot):
    return f"tui-{arm}-{slot}"


def body_sha256(body=BODY):
    """The sha256 of a system prompt, as its file holds it (stripped)."""
    return hashlib.sha256(body.strip().encode()).hexdigest()


def launcher_args(arm, slot, slots_dir):
    return [MCP_LAUNCH, "--arm", arm, "--slot", str(slot), "--slots-dir", os.path.abspath(slots_dir)]


def render(arm, slot, slots_dir, effort=DEFAULT_EFFORT, max_turns=DEFAULT_MAX_TURNS,
           python=sys.executable):
    """The agent file for `arm` in `slot`, whose launcher reads `slots_dir`."""
    if arm not in ARMS:
        raise ValueError(f"unknown arm {arm!r}")
    if effort not in EFFORTS:
        raise ValueError(f"unknown effort {effort!r}")
    args = ", ".join(json.dumps(a) for a in launcher_args(arm, slot, slots_dir))
    return (
        "---\n"
        f"name: {agent_name(arm, slot)}\n"
        f"description: Benchmark runner, {arm} arm, slot {slot}. {MARKER}\n"
        f"tools: mcp__{SERVER_KEY}\n"
        f"model: {DEFAULT_MODEL}\n"
        f"effort: {effort}\n"
        f"maxTurns: {int(max_turns)}\n"
        "omitClaudeMd: true\n"
        "mcpServers:\n"
        f"  - {SERVER_KEY}:\n"
        "      type: stdio\n"
        f"      command: {json.dumps(os.path.abspath(python))}\n"
        f"      args: [{args}]\n"
        "---\n"
        "\n"
        f"{BODY}\n"
    )


def parse(text):
    """{"fields": top-level scalars, "server": the inline server, "body": str}
    from an agent file `render` wrote. Raises ValueError on anything else."""
    if not text.startswith("---\n"):
        raise ValueError("no frontmatter")
    front, sep, body = text[4:].partition("\n---\n")
    if not sep:
        raise ValueError("frontmatter is not closed")
    fields, server, in_servers = {}, {}, False
    for line in front.split("\n"):
        if not line.strip():
            continue
        if not line.startswith(" "):
            key, _, value = line.partition(":")
            value = value.strip()
            in_servers = key == "mcpServers"
            if not in_servers:
                fields[key] = value
            continue
        if not in_servers:
            raise ValueError(f"unexpected indented line {line!r}")
        stripped = line.strip()
        if stripped.startswith("- "):
            server["_name"] = stripped[2:].rstrip(":")
            continue
        key, _, value = stripped.partition(":")
        value = value.strip()
        if key in ("command", "args"):
            value = json.loads(value)
        server[key] = value
    return {"fields": fields, "server": server, "body": body.strip()}


def read(path):
    with open(path) as f:
        return parse(f.read())


def problems(path, arm, slot, slots_dir, effort, max_turns, body_sha):
    """Every way the agent file at `path` is not the one a plan expects."""
    if not os.path.exists(path):
        return [f"{path} does not exist; run `run.py agents --slots N --dir {os.path.dirname(path)}`"]
    try:
        agent = read(path)
    except (OSError, ValueError) as err:
        return [f"{path} cannot be read as an agent file: {err}"]
    fields, server = agent["fields"], agent["server"]
    want = {
        "name": agent_name(arm, slot),
        "tools": f"mcp__{SERVER_KEY}",
        "effort": effort,
        "maxTurns": str(max_turns),
        "omitClaudeMd": "true",
    }
    found = [
        f"{path}: {key} is {fields.get(key)!r}, the plan needs {value!r}"
        for key, value in want.items()
        if fields.get(key) != value
    ]
    if body_sha256(agent["body"]) != body_sha:
        found.append(f"{path}: its system prompt is not the one the plan recorded (sha256 differs)")
    if server.get("_name") != SERVER_KEY or server.get("type") != "stdio":
        found.append(f"{path}: its MCP server is not a stdio server named {SERVER_KEY!r}")
    if server.get("args") != launcher_args(arm, slot, slots_dir):
        found.append(f"{path}: its launcher args {server.get('args')} do not read {slots_dir} for slot {slot}")
    return found


def is_ours(path):
    """Whether `path` is an agent file this module wrote (name and marker)."""
    if not FILE_RE.match(os.path.basename(path)):
        return False
    try:
        return MARKER in read(path)["fields"].get("description", "")
    except (OSError, ValueError):
        return False


def write(slots, directory, slots_dir, effort=DEFAULT_EFFORT, max_turns=DEFAULT_MAX_TURNS):
    """Write tui-<arm>-<slot>.md for every arm and slots 1..`slots`.

    Refuses to overwrite a file of that name this module did not write.
    Returns the paths written.
    """
    os.makedirs(directory, exist_ok=True)
    paths = []
    for slot in range(1, slots + 1):
        for arm in ARMS:
            path = os.path.join(directory, f"{agent_name(arm, slot)}.md")
            if os.path.exists(path) and not is_ours(path):
                raise FileExistsError(f"{path} exists and was not written by bench/agents.py")
            paths.append((path, render(arm, slot, slots_dir, effort, max_turns)))
    for path, text in paths:
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            f.write(text)
        os.replace(tmp, path)
    return [path for path, _ in paths]


def remove(directory):
    """Delete this module's tui-<arm>-<slot>.md files from `directory`, and
    nothing else. Returns the paths removed."""
    removed = []
    try:
        names = sorted(os.listdir(directory))
    except FileNotFoundError:
        return removed
    for name in names:
        path = os.path.join(directory, name)
        if os.path.isfile(path) and is_ours(path):
            os.remove(path)
            removed.append(path)
    return removed
