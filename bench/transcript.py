#!/usr/bin/env python3
"""Read a Claude Code subagent transcript into the numbers a bench row records.

Claude Code writes each subagent's transcript to
`~/.claude/projects/<project-slug>/<session-id>/subagents/agent-<agentId>.jsonl`,
one JSON object per line. The shapes relied on (checked against real
transcripts from Claude Code 2.1.294):

- every line: `type` (`user`, `assistant`, `attachment`, ...), `timestamp`
  (ISO 8601, `Z`), `agentId`, `sessionId`, `isSidechain`;
- `assistant` lines carry ONE content block each. A message streams over
  several lines that share `message.id` (`apiBlockIndex` numbers the block);
  `message.usage` repeats on each, and only the line with a non-null
  `message.stop_reason` has the final `output_tokens` (earlier lines carry a
  partial count, e.g. 8 then 286). Input and cache counts are the same on
  every line of a message. Blocks are `thinking`, `text` and `tool_use`.
  Claude Code's own `<synthetic>` messages (errors it writes itself, not a
  model reply) are left out of models and turns;
- `user` lines: `message.content` is the prompt string, or a list of
  `tool_result` blocks with `tool_use_id` and `is_error`;
- `attachment` lines carry `attachment.type`. Read here: `instructions`
  (`files[].path`: CLAUDE.md and its imports, memory), `deferred_tools_delta`
  (`addedNames`, `removedNames`, `readdedNames`, and `surfacedNames` for tools
  whose schema came loaded), `deferred_tools_record` (`entries[].name`: a
  schema loaded by a ToolSearch call), `mcp_instructions_delta`
  (`addedNames`, `removedNames`: MCP servers whose instructions reached the
  prompt), `environment` (`snapshot.workingDirectory`) and `skill_listing`.

What a transcript does not hold: the list of non-deferred tools the agent
was given (Bash, Read, ...). Only a call to one shows up, which is what
`guard` checks.

Everything here tolerates malformed lines, unknown types and missing
fields: they are skipped and counted, never raised.

Python 3 stdlib only.
"""

import glob
import json
import os
import re
from collections import Counter
from datetime import datetime

TOOLSEARCH = "ToolSearch"
SERVER_PREFIX = "mcp__tui__"
SYNTHETIC_MODEL = "<synthetic>"
FINAL_TEXT_MAX = 500
TOKEN_KEYS = (
    ("input", "input_tokens"),
    ("cache_creation", "cache_creation_input_tokens"),
    ("cache_read", "cache_read_input_tokens"),
    ("output", "output_tokens"),
)
AGENT_ID = re.compile(r"[0-9A-Za-z_-]+")


# --- Finding a transcript ---------------------------------------------------------------


def find_transcript(agent_id, projects_dir="~/.claude/projects"):
    """The one transcript for `agent_id` under `projects_dir`, or None.

    Raises ValueError for an id that is not [0-9a-zA-Z_-]+ (it is spliced
    into a glob) or when more than one transcript matches.
    """
    if not isinstance(agent_id, str) or not AGENT_ID.fullmatch(agent_id):
        raise ValueError(f"agent id {agent_id!r} is not [0-9a-zA-Z_-]+")
    root = glob.escape(os.path.expanduser(projects_dir))
    pattern = os.path.join(root, "*", "*", "subagents", f"agent-{agent_id}.jsonl")
    matches = sorted(glob.glob(pattern))
    if len(matches) > 1:
        raise ValueError(f"{len(matches)} transcripts for agent {agent_id}: {', '.join(matches)}")
    return matches[0] if matches else None


# --- Reading one ----------------------------------------------------------------------


def _int(value):
    return value if isinstance(value, int) and not isinstance(value, bool) else 0


def _names(value):
    """The strings in `value` when it is a list, else nothing."""
    return [v for v in value if isinstance(v, str)] if isinstance(value, list) else []


def _timestamp(value):
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _lines(path):
    """(parsed lines, malformed count) for the JSONL at `path`."""
    parsed, malformed = [], 0
    with open(path, encoding="utf-8", errors="replace") as f:
        for raw in f:
            raw = raw.strip()
            if not raw:
                continue
            try:
                line = json.loads(raw)
            except ValueError:
                malformed += 1
                continue
            if isinstance(line, dict):
                parsed.append(line)
            else:
                malformed += 1
    return parsed, malformed


def _final_usage(lines):
    """The usage of a streamed message: the line that ended it (non-null
    stop_reason), else the one with the most output tokens."""
    def output(line):
        return _int(line["usage"].get("output_tokens"))

    with_usage = [l for l in lines if isinstance(l["usage"], dict)]
    if not with_usage:
        return {}
    stopped = [l for l in with_usage if l["stop_reason"] is not None]
    return max(stopped or with_usage, key=output)["usage"]


def ended_on_tool_use(blocks):
    """Whether the run stopped right after asking for a tool.

    `blocks` are the assistant content blocks in transcript order. A run that
    ends on its own ends on text; one cut off by maxTurns ends on a tool_use
    with no assistant text after it. Thinking blocks are not an ending.
    """
    for block in reversed(blocks):
        kind = block.get("type")
        if kind == "thinking" or kind == "redacted_thinking":
            continue
        return kind == "tool_use"
    return False


def _context(attachments):
    claude_md, deferred, loaded, servers, types = [], set(), set(), set(), set()
    cwd, skills = None, False
    for att in attachments:
        kind = att.get("type")
        if not isinstance(kind, str):
            continue
        types.add(kind)
        if kind == "instructions":
            for item in att.get("files") or []:
                path = item.get("path") if isinstance(item, dict) else None
                if isinstance(path, str) and path not in claude_md:
                    claude_md.append(path)
        elif kind == "deferred_tools_delta":
            for key in ("addedNames", "readdedNames", "surfacedNames"):
                deferred.update(_names(att.get(key)))
            deferred.difference_update(_names(att.get("removedNames")))
        elif kind == "deferred_tools_record":
            # A schema loaded by ToolSearch: the tool was on offer.
            for entry in att.get("entries") or []:
                name = entry.get("name") if isinstance(entry, dict) else None
                if isinstance(name, str):
                    loaded.add(name)
        elif kind == "mcp_instructions_delta":
            servers.update(_names(att.get("addedNames")))
            servers.difference_update(_names(att.get("removedNames")))
        elif kind == "environment":
            snapshot = att.get("snapshot")
            if isinstance(snapshot, dict) and isinstance(snapshot.get("workingDirectory"), str):
                cwd = snapshot["workingDirectory"]
        elif kind == "skill_listing":
            skills = True
    return {
        "claude_md_files": claude_md,
        "deferred_tools": sorted(deferred | loaded),
        "loaded_tools": sorted(loaded),
        "mcp_instruction_servers": sorted(servers),
        "environment_cwd": cwd,
        "skill_listing": skills,
        "attachment_types": sorted(types),
    }


def read_transcript(path):
    """The bench's numbers for the transcript at `path` (see the module doc)."""
    lines, malformed = _lines(path)
    agent_id = session_id = None
    stamps, attachments, versions, efforts = [], [], set(), set()
    messages = {}  # message id -> {"model", "lines"}, in first-seen order
    blocks = {}  # (message id, block index) -> block, in first-seen order
    errors = set()
    synthetic = set()
    for number, line in enumerate(lines):
        agent_id = agent_id or (line.get("agentId") if isinstance(line.get("agentId"), str) else None)
        session_id = session_id or (line.get("sessionId") if isinstance(line.get("sessionId"), str) else None)
        stamp = _timestamp(line.get("timestamp"))
        if stamp is not None:
            stamps.append(stamp)
        if isinstance(line.get("version"), str):
            versions.add(line["version"])
        kind = line.get("type")
        if kind == "attachment" and isinstance(line.get("attachment"), dict):
            attachments.append(line["attachment"])
        elif kind == "user" and isinstance(line.get("message"), dict):
            content = line["message"].get("content")
            for pos, block in enumerate(content if isinstance(content, list) else []):
                if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("is_error") is True:
                    errors.add(block.get("tool_use_id") or (number, pos))
        elif kind == "assistant" and isinstance(line.get("message"), dict):
            msg = line["message"]
            mid = msg.get("id") if isinstance(msg.get("id"), str) else f"line-{number}"
            model = msg.get("model") if isinstance(msg.get("model"), str) else None
            if model == SYNTHETIC_MODEL:
                synthetic.add(mid)
                continue
            if isinstance(line.get("effort"), str):
                efforts.add(line["effort"])
            entry = messages.setdefault(mid, {"model": None, "lines": []})
            entry["model"] = entry["model"] or model
            entry["lines"].append({"usage": msg.get("usage"), "stop_reason": msg.get("stop_reason")})
            index = line.get("apiBlockIndex")
            content = msg.get("content") if isinstance(msg.get("content"), list) else []
            for pos, block in enumerate(content):
                if not isinstance(block, dict):
                    continue
                key = (mid, index + pos) if isinstance(index, int) else (mid, f"{number}.{pos}")
                blocks[key] = block  # a later line of the same block is the fuller one

    ordered = list(blocks.values())
    tool_ids, tool_calls = set(), Counter()
    for pos, block in enumerate(ordered):
        if block.get("type") != "tool_use":
            continue
        uid = block.get("id") or f"block-{pos}"
        if uid in tool_ids:
            continue
        tool_ids.add(uid)
        tool_calls[block.get("name") if isinstance(block.get("name"), str) else "?"] += 1

    tokens = {name: 0 for name, _ in TOKEN_KEYS}
    for entry in messages.values():
        usage = _final_usage(entry["lines"])
        for name, key in TOKEN_KEYS:
            tokens[name] += _int(usage.get(key))

    texts = [b.get("text") for b in ordered if b.get("type") == "text" and isinstance(b.get("text"), str)]
    final_text = texts[-1].strip()[:FINAL_TEXT_MAX] if texts else None
    if not agent_id:
        base = os.path.basename(path)
        if base.startswith("agent-") and base.endswith(".jsonl"):
            agent_id = base[len("agent-"):-len(".jsonl")]

    return {
        "agent_id": agent_id,
        "session_id": session_id,
        "models": sorted({e["model"] for e in messages.values() if e["model"]}),
        "turns": len(messages),
        "tool_calls": dict(sorted(tool_calls.items())),
        "tool_calls_total": sum(n for name, n in tool_calls.items() if name != TOOLSEARCH),
        "toolsearch_calls": tool_calls.get(TOOLSEARCH, 0),
        "tokens": tokens,
        "wall_ms": int((max(stamps) - min(stamps)).total_seconds() * 1000) if len(stamps) >= 2 else None,
        "final_text": final_text or None,
        "ended_on_tool_use": ended_on_tool_use(ordered),
        "tool_errors": len(errors),
        "context": _context(attachments),
        # Beyond the contract, for provenance and diagnosis.
        "efforts": sorted(efforts),
        "claude_versions": sorted(versions),
        "synthetic_messages": len(synthetic),
        "malformed_lines": malformed,
    }


# --- Was the run set up as its arm requires? ----------------------------------------------


def guard(stats, arm, model_family, arm_tools):
    """Problems with how the run was set up, one sentence each; empty when
    the agent ran as its arm requires.

    Allowed: the arm's `mcp__tui__<tool>` tools and ToolSearch (which loads
    them). Every tool called outside that, every model not
    `claude-<family>-*`, any instruction file (agents run with omitClaudeMd),
    any deferred tool on offer outside the arm's own (this also catches the
    other arm's `mcp__tui__` tools), and a run with no assistant turn.
    """
    allowed = {SERVER_PREFIX + t for t in arm_tools}
    problems = []
    for name, count in sorted((stats.get("tool_calls") or {}).items()):
        if name != TOOLSEARCH and name not in allowed:
            problems.append(f"called {name} ({count}x), which is not a {arm} arm tool")
    prefix = f"claude-{model_family}-"
    for model in stats.get("models") or []:
        if not model.startswith(prefix):
            problems.append(f"ran on {model}, not the requested {prefix}*")
    context = stats.get("context") or {}
    files = context.get("claude_md_files") or []
    if files:
        problems.append(f"was given {len(files)} instruction file(s) despite omitClaudeMd: {', '.join(files)}")
    extra = [t for t in context.get("deferred_tools") or [] if t not in allowed]
    if extra:
        problems.append(f"was offered {len(extra)} deferred tool(s) beyond the {arm} arm: {', '.join(extra)}")
    if not stats.get("turns"):
        problems.append("never ran: the transcript has no assistant turn")
    return problems


if __name__ == "__main__":
    import sys

    for arg in sys.argv[1:]:
        print(json.dumps(read_transcript(arg), indent=2))
