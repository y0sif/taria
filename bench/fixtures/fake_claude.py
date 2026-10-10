#!/usr/bin/env python3
"""A stand-in for `claude -p --output-format stream-json`, for bench/test_bench.py.

Never calls a model or the network. The test puts a symlink named `claude`
to this file first on PATH, so run.py's spend path runs against it exactly as
it would against the real CLI: same argv, same process group, same stream.

FAKE_CLAUDE_MODE picks the behaviour:

  success     init, an assistant message, a success result
  hang        init, a streamed assistant message (no result), then ignore
              SIGTERM, spawn a child that ignores it too, and sleep
  noresult    init, an assistant message, then exit 3 with a stderr line
  extra_tool  init listing a built-in tool beside the arm's, then sleep
  plugins     init listing a plugin, then sleep
  denied      a success result carrying a permission denial
  api_error   a result with is_error set, as an API error reports it
  budget      a result with subtype error_max_budget_usd

FAKE_CLAUDE_SOLVE=1 first solves FAKE_CLAUDE_TASK (default T1) through the
MCP server named in --mcp-config, spawned the way claude spawns it, with the
scripted solver, and reports each call as a tool_use block.
FAKE_CLAUDE_LOG names a file that gets the argv, cwd, pids and the names of
the CLAUDE* environment variables this process saw.
"""

import json
import os
import signal
import subprocess
import sys
import time

BENCH = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
TOOLS = {"taria": ["read_tree", "act", "key", "type_text"], "tmux": ["screen", "send_keys"]}


def emit(event):
    sys.stdout.write(json.dumps(event) + "\n")
    sys.stdout.flush()


def flag(name, default=None):
    argv = sys.argv[1:]
    return argv[argv.index(name) + 1] if name in argv and argv.index(name) + 1 < len(argv) else default


def log(extra=None):
    path = os.environ.get("FAKE_CLAUDE_LOG")
    if not path:
        return
    data = {
        "argv": sys.argv[1:],
        "cwd": os.getcwd(),
        "pid": os.getpid(),
        "pgid": os.getpgid(0),
        "env_claude": sorted(k for k in os.environ if k == "CLAUDECODE" or k.startswith("CLAUDE_")),
        **(extra or {}),
    }
    with open(path, "w") as f:
        json.dump(data, f)


def usage(input_tokens, output_tokens):
    return {
        "input_tokens": input_tokens, "output_tokens": output_tokens,
        "cache_read_input_tokens": 100, "cache_creation_input_tokens": 1000,
    }


def solve_task(entry, arm):
    sys.path.insert(0, BENCH)
    from harness import McpStdio, wait_ready
    from solvers import CountingSession, solve

    client = McpStdio(entry, cwd=os.getcwd(), name=f"{arm} server")
    try:
        client.initialize()
        wait_ready(arm, client)
        session = CountingSession(client)
        solve(os.environ.get("FAKE_CLAUDE_TASK", "T1"), arm, session)
    finally:
        client.close()
    return session.calls


def main():
    if sys.argv[1:] == ["--version"]:
        print("0.0.0 (fake claude)")
        return 0
    mode = os.environ.get("FAKE_CLAUDE_MODE", "success")
    log()
    with open(flag("--mcp-config")) as f:
        entry = json.load(f)["mcpServers"]["tui"]
    arm = "taria" if os.path.basename(entry["command"]) == "taria-mcp" else "tmux"
    tools = [f"mcp__tui__{t}" for t in TOOLS[arm]]
    if mode == "extra_tool":
        tools.append("Bash")
    emit({
        "type": "system", "subtype": "init", "cwd": os.getcwd(), "session_id": "fake",
        "tools": tools, "mcp_servers": [{"name": "tui", "status": "connected"}],
        "model": flag("--model"), "permissionMode": flag("--permission-mode", "default"),
        "slash_commands": [], "apiKeySource": "none", "claude_code_version": "0.0.0",
        "output_style": "default", "agents": [], "skills": [],
        "plugins": [{"name": "fake-plugin", "path": "/nowhere"}] if mode == "plugins" else [],
    })
    if mode in ("extra_tool", "plugins"):
        time.sleep(30)  # the guard must stop this, not wait it out
        return 0
    calls = {}
    if os.environ.get("FAKE_CLAUDE_SOLVE") == "1":
        calls = solve_task(entry, arm)
    blocks = [
        {"type": "tool_use", "id": f"{name}-{i}", "name": f"mcp__tui__{name}", "input": {}}
        for name, count in calls.items() for i in range(count)
    ]
    # One API message streamed as two events, usage growing: counted once.
    emit({"type": "assistant", "message": {"id": "msg-1", "usage": usage(10, 5),
                                           "content": [{"type": "text", "text": "Looking."}]}})
    emit({"type": "assistant", "message": {"id": "msg-1", "usage": usage(10, 20), "content": blocks}})
    if mode == "hang":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        child = subprocess.Popen([
            sys.executable, "-c",
            "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(600)",
        ])
        log({"child": child.pid})
        time.sleep(600)
        return 0
    if mode == "noresult":
        sys.stderr.write("fake: lost the connection mid-turn\n")
        return 3
    result = {
        "type": "result", "subtype": "success", "is_error": False, "num_turns": 2,
        "duration_ms": 1200, "duration_api_ms": 900, "total_cost_usd": 0.01,
        "usage": usage(12, 34), "permission_denials": [], "result": "Done.",
    }
    if mode == "denied":
        result["permission_denials"] = [
            {"tool_name": "mcp__tui__act", "tool_use_id": "x", "tool_input": {}}
        ]
    elif mode == "api_error":
        result.update(is_error=True, result="API Error: 404 model: not-a-model", total_cost_usd=0)
    elif mode == "budget":
        result.update(subtype="error_max_budget_usd", is_error=True, total_cost_usd=1.03,
                      errors=["Reached maximum budget ($1)"])
    emit(result)
    return 0


if __name__ == "__main__":
    sys.exit(main())
