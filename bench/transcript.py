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
- `user` lines: `message.content` is the prompt string (the first user line
  is the task the agent was given), or a list of `tool_result` blocks with
  `tool_use_id` and `is_error`;
- `attachment` lines carry `attachment.type`. Read here: `prompt_snapshot`
  (`tools[].name`: the tools the agent was offered; see below),
  `mcp_instructions_delta` (`addedNames`, `removedNames`: MCP servers whose
  instructions reached the prompt), `instructions` (`files[].path`: CLAUDE.md
  and its imports, memory), `deferred_tools_delta` (`addedNames`,
  `removedNames`, `readdedNames`, and `surfacedNames` for tools whose schema
  came loaded), `deferred_tools_record` (`entries[].name`: a schema loaded by
  a ToolSearch call), `environment` (`snapshot.workingDirectory`),
  `skill_listing` and `max_turns_reached` (`maxTurns`, `turnCount`).

The tools the agent was offered are in its `prompt_snapshot` attachments.
Claude Code writes the first before any tool is known, without a `tools`
key, and a new one whenever the tool list changes; each repeats the tools
already recorded, so their union is what the agent was offered. A real run
of either arm has two, the second listing exactly the arm's `mcp__tui__`
tools inline (no deferral, no ToolSearch). A run whose server never started
has no tools in any snapshot, which is what `guard` checks.

How a run ended is read off its final assistant message: `end_turn` when
the agent finished on its own. Claude Code 2.1.294 sometimes never writes the
closing line of a final text reply, so every line of that message carries
stop_reason null and a placeholder output_tokens (1 or 5, say). A final
message whose last block is a non-empty text, with no tool_use after it,
ended on its own all the same: `end_inferred` says the end was inferred, and
its output tokens are a lower bound (`output_tokens_lower_bound`). Claude
Code's own `<synthetic>` messages
(`isApiErrorMessage`, `error` such as `rate_limit` or `server_error`, text
such as "You've hit your session limit") are left out of models and turns,
but one written last means the run died on that error. A run stopped at
maxTurns leaves no trace of its own in the subagent's transcript: Claude
Code 2.1.294 yields `max_turns_reached` to the parent and stops before
recording it, after running the last turn's tools, so the transcript ends on
a `tool_use` message and its result. `ending` infers the cut-off from that
shape and the turn count, and takes a `max_turns_reached` attachment as
proof if a later version records one.

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
SERVER = "tui"
SERVER_PREFIX = f"mcp__{SERVER}__"
SYNTHETIC_MODEL = "<synthetic>"
END_TURN = "end_turn"
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


def _block_text(content):
    """The text of a message's content: the string, or its text blocks joined."""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return None
    texts = [b.get("text") for b in content if isinstance(b, dict) and b.get("type") == "text"]
    texts = [t for t in texts if isinstance(t, str)]
    return "".join(texts) if texts else None


def _prompt_text(content):
    """The prompt a user line carries, or None for a tool result."""
    if isinstance(content, list) and any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
        return None
    return _block_text(content)


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


def _snapshot_tools(att):
    """The sorted tool names a prompt_snapshot lists, or None when it has no
    `tools` key (Claude Code writes the first one before any tool is known)."""
    tools = att.get("tools")
    if not isinstance(tools, list):
        return None
    return sorted({t["name"] for t in tools if isinstance(t, dict) and isinstance(t.get("name"), str)})


def _context(attachments):
    claude_md, deferred, loaded, servers, types = [], set(), set(), set(), set()
    deferred_added, servers_added = set(), set()  # every name ever added, removals ignored
    snapshots, offered = [], set()
    cwd, skills = None, False
    for att in attachments:
        kind = att.get("type")
        if not isinstance(kind, str):
            continue
        types.add(kind)
        if kind == "prompt_snapshot":
            names = _snapshot_tools(att)
            snapshots.append(names)
            offered.update(names or [])
        elif kind == "instructions":
            for item in att.get("files") or []:
                path = item.get("path") if isinstance(item, dict) else None
                if isinstance(path, str) and path not in claude_md:
                    claude_md.append(path)
        elif kind == "deferred_tools_delta":
            for key in ("addedNames", "readdedNames", "surfacedNames"):
                deferred.update(_names(att.get(key)))
                deferred_added.update(_names(att.get(key)))
            deferred.difference_update(_names(att.get("removedNames")))
        elif kind == "deferred_tools_record":
            # A schema loaded by ToolSearch: the tool was on offer.
            for entry in att.get("entries") or []:
                name = entry.get("name") if isinstance(entry, dict) else None
                if isinstance(name, str):
                    loaded.add(name)
        elif kind == "mcp_instructions_delta":
            servers.update(_names(att.get("addedNames")))
            servers_added.update(_names(att.get("addedNames")))
            servers.difference_update(_names(att.get("removedNames")))
        elif kind == "environment":
            snapshot = att.get("snapshot")
            if isinstance(snapshot, dict) and isinstance(snapshot.get("workingDirectory"), str):
                cwd = snapshot["workingDirectory"]
        elif kind == "skill_listing":
            skills = True
    return {
        # The union of every prompt_snapshot's tools, and each snapshot's own
        # list (None for one without a `tools` key), in transcript order.
        "offered_tools": sorted(offered),
        "offered_tools_snapshots": snapshots,
        "claude_md_files": claude_md,
        # Net of removals: what was on offer at the end.
        "deferred_tools": sorted(deferred | loaded),
        "loaded_tools": sorted(loaded),
        "mcp_instruction_servers": sorted(servers),
        # Every name ever added, removals ignored: a server or tool that
        # reached the agent and later left still reached it.
        "deferred_tools_added": sorted(deferred_added | loaded),
        "mcp_instruction_servers_added": sorted(servers_added),
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
    first_prompt, seen_user = None, False
    final = None  # the last assistant line's message: {"id", "synthetic", "text", "error"}
    max_turns_reached = None
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
            if line["attachment"].get("type") == "max_turns_reached":
                att = line["attachment"]
                max_turns_reached = {"max_turns": att.get("maxTurns"), "turn_count": att.get("turnCount")}
        elif kind == "user" and isinstance(line.get("message"), dict):
            content = line["message"].get("content")
            if not seen_user:
                seen_user = True
                first_prompt = _prompt_text(content)
            for pos, block in enumerate(content if isinstance(content, list) else []):
                if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("is_error") is True:
                    errors.add(block.get("tool_use_id") or (number, pos))
        elif kind == "assistant" and isinstance(line.get("message"), dict):
            msg = line["message"]
            mid = msg.get("id") if isinstance(msg.get("id"), str) else f"line-{number}"
            model = msg.get("model") if isinstance(msg.get("model"), str) else None
            if model == SYNTHETIC_MODEL:
                synthetic.add(mid)
                final = {"id": mid, "synthetic": True, "stop_reason": msg.get("stop_reason"),
                         "text": _block_text(msg.get("content")), "error": line.get("error")}
                continue
            final = {"id": mid, "synthetic": False}
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

    last_model = last_stop_reason = synthetic_text = synthetic_error = last_block_type = None
    end_inferred = False
    if final is not None and final["synthetic"]:
        last_model, last_stop_reason = SYNTHETIC_MODEL, final["stop_reason"]
        synthetic_text = (final["text"] or "")[:FINAL_TEXT_MAX] or None
        synthetic_error = final["error"] if isinstance(final["error"], str) else None
    elif final is not None:
        entry = messages[final["id"]]
        last_model = entry["model"]
        reasons = [l["stop_reason"] for l in entry["lines"] if isinstance(l["stop_reason"], str)]
        last_stop_reason = reasons[-1] if reasons else None
        own = [block for (mid, _), block in blocks.items() if mid == final["id"]]
        last_block = own[-1] if own else {}
        last_block_type = last_block.get("type") if isinstance(last_block.get("type"), str) else None
        # A text reply whose closing line Claude Code never wrote (see the
        # module doc): it ended on its own, inferred from its shape.
        end_inferred = bool(
            last_stop_reason is None and last_block_type == "text"
            and isinstance(last_block.get("text"), str) and last_block["text"].strip()
        )
    # A message no line of which carries a stop_reason never got its final
    # usage line: its output count is the largest partial one, a lower bound.
    unstopped = sum(1 for e in messages.values() if not any(l["stop_reason"] is not None for l in e["lines"]))

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
        # How the run ended (see `ending`): the final assistant message's
        # model and stop_reason, synthetic or not, and whether Claude Code
        # recorded a max_turns_reached.
        "last_model": last_model,
        "last_stop_reason": last_stop_reason,
        "last_block_type": last_block_type,
        "end_inferred": end_inferred,
        "output_tokens_lower_bound": unstopped > 0,
        "ended_synthetic": bool(final and final["synthetic"]),
        "synthetic_text": synthetic_text,
        "synthetic_error": synthetic_error,
        "max_turns_reached": max_turns_reached,
        # Who ran it: the task it was given and when it started.
        "first_prompt": first_prompt,
        "started": min(stamps).isoformat(timespec="milliseconds").replace("+00:00", "Z") if stamps else None,
        # Beyond the contract, for provenance and diagnosis.
        "efforts": sorted(efforts),
        "claude_versions": sorted(versions),
        "synthetic_messages": len(synthetic),
        "malformed_lines": malformed,
    }


# --- Was the run set up as its arm requires? ----------------------------------------------


def guard(stats, arm, model_family, arm_tools, effort=None):
    """Problems with how the run was set up, one sentence each; empty when
    the agent ran as its arm requires.

    Allowed: the arm's `mcp__tui__<tool>` tools and ToolSearch (which loads
    deferred tools). A problem is: a tool called outside that; a model not
    `claude-<family>-*`; an effort other than `effort` (when given) on any
    turn; any instruction file (agents run with omitClaudeMd); offered tools
    (the prompt_snapshots' union, plus deferred ones) that are not exactly
    the arm's, or no prompt_snapshot to tell; any deferred tool beyond the
    arm's; MCP instructions from any server but `tui`, or none from it; and a
    run with no assistant turn.

    Other servers and deferred tools are held against every name ever
    added, so one that reached the agent and later left (a server that
    disconnected) still counts. Removals matter only for `tui` itself: its
    instructions must still be there at the end.
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
    efforts = stats.get("efforts") or []
    if effort is not None and stats.get("turns") and efforts != [effort]:
        problems.append(f"ran at effort {', '.join(efforts) or '(none recorded)'}, not the plan's {effort}")
    context = stats.get("context") or {}
    files = context.get("claude_md_files") or []
    if files:
        problems.append(f"was given {len(files)} instruction file(s) despite omitClaudeMd: {', '.join(files)}")
    problems += _offered_problems(context, allowed, arm)
    deferred_ever = sorted(set(context.get("deferred_tools_added") or []) | set(context.get("deferred_tools") or []))
    extra = [t for t in deferred_ever if t not in allowed]
    if extra:
        problems.append(f"was offered {len(extra)} deferred tool(s) beyond the {arm} arm: {', '.join(extra)}")
    servers = context.get("mcp_instruction_servers") or []
    servers_ever = sorted(set(context.get("mcp_instruction_servers_added") or []) | set(servers))
    others = [s for s in servers_ever if s != SERVER]
    if others:
        problems.append(
            f"was given the instructions of {len(others)} MCP server(s) besides {SERVER}: {', '.join(others)}"
        )
    if SERVER not in servers_ever:
        problems.append(f"never got the {SERVER} server's instructions: it did not start, or was refused")
    elif SERVER not in servers:
        problems.append(f"lost the {SERVER} server's instructions mid-run: the server was removed (it disconnected)")
    if not stats.get("turns"):
        problems.append("never ran: the transcript has no assistant turn")
    return problems


def _offered_problems(context, allowed, arm):
    """The offered tools against the arm's: none known, missing, or extra."""
    snapshots = context.get("offered_tools_snapshots")
    if not snapshots:
        return ["has no prompt_snapshot, so the tools it was offered cannot be checked"]
    offered = set(context.get("offered_tools") or [])
    found = []
    missing = sorted(allowed - offered - set(context.get("deferred_tools") or []))
    if missing:
        found.append(
            f"was not offered {', '.join(missing)}: the {SERVER} server did not start, or mcp_launch refused it"
        )
    extra = sorted(offered - allowed - {TOOLSEARCH})
    if extra:
        found.append(f"was offered {len(extra)} tool(s) beyond the {arm} arm: {', '.join(extra)}")
    return found


# --- How did the run end? -------------------------------------------------------------------


def cut_off_at_max_turns(stats, max_turns):
    """Whether the run looks stopped by maxTurns: its final message asked for
    a tool, with no text after, at `max_turns` turns or more. Its stop_reason
    is tool_use, or null when Claude Code never wrote the message's closing
    line (as it sometimes does for a final text reply; see the module doc).

    An inference, isolated here: no real cut-off transcript exists yet. It
    rests on Claude Code 2.1.294's agent loop, which runs the last turn's
    tools and then stops, yielding max_turns_reached to the parent without
    writing it to the subagent's transcript.
    """
    turns = stats.get("turns")
    return bool(
        stats.get("last_stop_reason") in ("tool_use", None) and stats.get("ended_on_tool_use")
        and isinstance(turns, int) and isinstance(max_turns, int) and turns >= max_turns
    )


INFERRED_END = (
    "its last message is a text reply with no stop_reason (Claude Code never wrote the message's "
    "closing line), so the end is inferred; that message's output tokens are a lower bound"
)


def ending(stats, max_turns):
    """(kind, detail) for how the run ended.

    kind is "end_turn" (it finished on its own), "max_turns" (stopped at the
    turn limit), "harness_error" (it died on Claude Code's own error message,
    or its last message stopped on anything but end_turn outside the
    max-turns case: an API error, a usage limit, an interruption, or a run
    still going), or "never_ran" (no assistant message at all; `guard` says
    so). detail is one sentence, or None for end_turn.

    A final text reply with a null stop_reason (`end_inferred`) is end_turn,
    with INFERRED_END as its detail: Claude Code left the closing line out,
    the reply arrived. A null stop_reason on anything else (a final tool_use
    under the cap, a thinking block) is still a harness error, and so is any
    explicit stop_reason but end_turn (max_tokens, refusal, ...).
    """
    reached = stats.get("max_turns_reached")
    if isinstance(reached, dict):
        return "max_turns", (
            f"Claude Code recorded max_turns_reached (turn {reached.get('turn_count')} "
            f"of {reached.get('max_turns')})"
        )
    if stats.get("ended_synthetic"):
        error = stats.get("synthetic_error")
        return "harness_error", (
            "the run ended on Claude Code's own message, not a model reply"
            f"{f' ({error})' if error else ''}: {stats.get('synthetic_text')!r}"
        )
    if not stats.get("turns"):
        return "never_ran", "the transcript has no assistant message"
    reason = stats.get("last_stop_reason")
    if cut_off_at_max_turns(stats, max_turns):
        return "max_turns", f"stopped on a tool call at turn {stats.get('turns')} of maxTurns {max_turns}"
    if reason == END_TURN:
        return "end_turn", None
    if reason is None and stats.get("end_inferred") and not stats.get("ended_on_tool_use"):
        return "end_turn", INFERRED_END
    if reason is None:
        why = "it never finished streaming: an API error, a usage limit or an interruption cut it off"
    elif reason == "tool_use":
        why = (f"it asked for a tool at turn {stats.get('turns')}, under maxTurns {max_turns}: "
               "interrupted, or still running")
    else:
        why = "the model did not end its turn"
    return "harness_error", f"the run did not end on its own: its last message stopped on {reason!r} ({why})"


if __name__ == "__main__":
    import sys

    for arg in sys.argv[1:]:
        print(json.dumps(read_transcript(arg), indent=2))
