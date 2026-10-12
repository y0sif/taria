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
  partial count, e.g. 8 then 286; see below for a message with no such
  line). Input and cache counts are the same on
  every line of a message. Blocks are `thinking`, `text` and `tool_use`.
  Claude Code's own `<synthetic>` messages (errors it writes itself, not a
  model reply) are left out of models and turns;
- `user` lines: `message.content` is the prompt string (the first user line
  is the task the agent was given, and a real run has no other), or a list
  of `tool_result` blocks with `tool_use_id`, `is_error` and `content` (a
  string, or text blocks): what the tool returned, as the model read it,
  including anything Claude Code appended to it;
- `attachment` lines carry `attachment.type`, and `rendered[].content`, the
  text that reached the model (none for an attachment Claude Code only
  records, such as `prompt_snapshot`). Read here: `prompt_snapshot`
  (`tools[].name`: the tools the agent was offered, see below; the
  `description` of `mcp__tui__read_tree`, which names the tree format; and
  `cliPrefix`, the agent's identity line), `mcp_instructions_delta`
  (`addedNames`, `removedNames`: MCP servers whose instructions reached the
  prompt), `instructions` (`files[].path`: CLAUDE.md and its imports,
  memory), `deferred_tools_delta` (`addedNames`, `removedNames`,
  `readdedNames`, and `surfacedNames` for tools whose schema came loaded,
  with `surfacedDefinitions[].definition.description`),
  `deferred_tools_record` (`entries[].name` and `entries[].description`: a
  schema loaded by a ToolSearch call), `environment` (`snapshot.workingDirectory`),
  `skill_listing` and `max_turns_reached` (`maxTurns`, `turnCount`).

Every attachment type has exactly one rule (ATTACHMENTS_VERIFIED,
ATTACHMENTS_HARMLESS, ATTACHMENTS_FLAGGED below), because an attachment is
context added to the agent's prompt: verified ones are read and held to the
arm, harmless ones (the same for every arm) are held to the shape they are
known in, and flagged ones are refused by name. A type with no rule is
context nobody checked, and the guard refuses the run, naming the type, how
many arrived and how many characters of them were rendered. That is what
caught `remote_session_change`: with /remote-control on in the orchestrating
session, Claude Code added a ~710-character attribution note beside a tool
result in every subagent run, and a guard that only read the types it knew
let it through.

A flagged type known to be the same for every run of a batch
(ATTACHMENTS_TOLERABLE: `remote_session_change` only) can be tolerated by
the batch's plan (`run.py plan --tolerate-attachment TYPE`). Read with that
tolerance, such an attachment is held to the shape it is known in (its
fields, and the text they fill: REMOTE_SESSION_TEXT) and, when it has it,
recorded in the context's `tolerated_attachments` instead of refused; one in
any other shape is still refused. A batch that tolerates one is not a clean
batch: where every arm got the same note its arms compare with each other,
but its absolute token counts include the note.

The tools the agent was offered are in its `prompt_snapshot` attachments.
Claude Code writes the first before any tool is known, without a `tools`
key, and a new one whenever the tool list changes; each repeats the tools
already recorded, so their union is what the agent was offered. A real run
of any arm has two, the second listing exactly the arm's `mcp__tui__`
tools inline (no deferral, no ToolSearch). A run whose server never started
has no tools in any snapshot, which is what `guard` checks.

Claude Code 2.1.294 sometimes never writes the closing line of an assistant
message, anywhere in the run, not only the final reply: every line of that
message carries stop_reason null and a placeholder output_tokens (1 to 9).
Its output count here is the largest partial one, so the run's output total
is a lower bound: `unclosed_messages` counts such messages and
`output_tokens_lower_bound` is true when there is one. Input and cache
counts are the same on every line of a message, so they stay exact.

How a run ended is read off its final assistant message: `end_turn` when
the agent finished on its own. An unclosed final message whose last block is
a non-empty text, with no tool_use after it, ended on its own all the same:
`end_inferred` says the end was inferred. Claude Code's own `<synthetic>`
messages
(`isApiErrorMessage`, `error` such as `rate_limit` or `server_error`, text
such as "You've hit your session limit") are left out of models and turns,
but one written last means the run died on that error. A run stopped at
maxTurns leaves no trace of its own in the subagent's transcript: Claude
Code 2.1.294 yields `max_turns_reached` to the parent and stops before
recording it, after running the last turn's tools, so the transcript ends on
a `tool_use` message and its result. `ending` infers the cut-off from that
shape and the turn count, and takes a `max_turns_reached` attachment as
proof if a later version records one.

A harmless attachment is held to its fields and to the text it rendered:
the environment's and the session context's rendered text must be the
template Claude Code 2.1.294 renders them in (ENVIRONMENT_TEXT,
SESSION_CONTEXT_TEXT) filled with the attachment's own values (the
environment's untrusted-data line, ENVIRONMENT_UNTRUSTED, may be missing,
and is exact when there), the date's and the model's their one sentence. An
attachment whose `rendered` is there
but does not read as text is refused too, never taken for no text.

Context can also arrive inside a tool result rather than as an attachment:
Claude Code appends its reminders to a tool's output as `<system-reminder>`
blocks, inside the result or as a text block beside it in the same user
message. No real run has carried one, and none can be told harmless from its
place, so each is counted (`tool_result_reminders`) and refused. Anything
else beside a tool result (text outside a reminder, a block that is not
text) counts as a prompt.

Reading never raises: a malformed line is skipped and counted
(`malformed_lines`), and a missing field reads as absent. What a run should
not have is recorded in its context instead, and `guard` refuses the run for
each: an attachment of a type with no rule, or in a shape its rule does not
know, or whose rendered text does not read; an attachment line with no
type, or whose attachment is not an object; a `<system-reminder>` block in
or beside a tool result; a line whose type is not `user`, `assistant` or
`attachment`; and a second prompt, beside a tool result or on its own.

An orchestrating session in auto mode gives every subagent Claude Code's
SubagentHandback tool and a second prompt telling it to report through it,
and the call ends the run without closing its message (HANDBACK_TOOL). The
guard refuses such a run for each of those, and says first that the session
is in auto mode (`handback_prompts` counts that prompt).

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

# --- What a transcript may carry -----------------------------------------------------------
#
# Every attachment type has exactly one rule. Verified: the guard reads what
# these carry and holds the run to it.
ATTACHMENTS_VERIFIED = frozenset({
    "prompt_snapshot",         # the tools on offer, read_tree's tree format, the identity line
    "mcp_instructions_delta",  # the MCP servers whose instructions reached the agent
    "deferred_tools_delta",    # the deferred tools on offer
    "deferred_tools_record",   # a deferred tool's schema, loaded
    "max_turns_reached",       # the turn cap, as proof of a cut-off
    "instructions",            # CLAUDE.md and memory: any file is a problem
})
# Harmless, and the same for every arm: each is held to the shape it is
# known in (see _harmless_shape), so new content in it is refused like an
# unknown type.
ATTACHMENTS_HARMLESS = frozenset({
    "environment",            # the working directory, platform and shell
    "session_context",        # the user's e-mail and the orchestrator's git status
    "date",                   # today's date
    "model",                  # the model's name and id
    "credential_org",         # the organization id, recorded and never rendered
    "thinking_drop",          # thinking blocks left out of a request: a diagnostic, never rendered
    "total_tokens_reminder",  # the tokens left in the context window
})
# Known, and refused by name: (what it is, what to do).
ATTACHMENTS_FLAGGED = {
    "remote_session_change": ("Claude Code's remote-control attribution note",
                              "turn /remote-control off in the orchestrating session"),
    "skill_listing": ("a skill listing", "no arm's agent should get one, as its tools are mcp__tui only"),
}
# Flagged, and a plan may tolerate it (`run.py plan --tolerate-attachment`):
# a type known to be the same for every run of a batch, so every arm gets it
# alike. Tolerated, it is held to the shape it is known in (_tolerated_shape)
# and recorded in the context's tolerated_attachments rather than refused.
# skill_listing is not one: nothing shows it would be the same for every run.
ATTACHMENTS_TOLERABLE = frozenset({"remote_session_change"})
KNOWN_ATTACHMENTS = ATTACHMENTS_VERIFIED | ATTACHMENTS_HARMLESS | frozenset(ATTACHMENTS_FLAGGED)
LINE_TYPES = frozenset({"user", "assistant", "attachment"})
# The top-level fields each harmless type is known with, `type` included: any
# other is refused. thinking_drop's are those of every record Claude Code
# 2.1.280 and 2.1.294 wrote outside the bench (no bench run has had one).
HARMLESS_FIELDS = {
    "environment": ("type", "snapshot"),
    "session_context": ("type", "context"),
    "date": ("type", "date"),
    "model": ("type", "identity", "text"),
    "credential_org": ("type", "organizationUuid"),
    "thinking_drop": ("type", "requestId", "model", "querySource", "thinkingBlocksSent", "thinkingTurnsSent",
                      "newlyDropped", "blockHashes", "firstReportForThreadInProcess", "clientChange"),
    "total_tokens_reminder": ("type", "text"),
}
SESSION_CONTEXT_KEYS = ("gitStatus", "userEmail")
# environment.snapshot: the keys every real run's has (a tmux run from another
# orchestrating session had all but scratchpadDirectory), and each one's type.
ENVIRONMENT_KEYS = {
    "additionalWorkingDirectories": list, "isGitRepo": bool, "isWorktree": bool, "osVersion": str,
    "platform": str, "scratchpadDirectory": str, "shell": str, "workingDirectory": str,
}
# The text the environment and session_context attachments render, as Claude
# Code 2.1.294 wrote it in every archived run (full1, outline1 and probe3,
# every arm) and in every fixture: the rendered text must be the template
# filled with the attachment's own values, character for character, so a
# sentence Claude Code adds is refused. Each {field} is the snapshot's value
# (a bool as true or false); the scratchpad line is there only when the
# snapshot has a scratchpadDirectory, as the real tmux fixture's has not. A
# snapshot of a worktree renders more lines, which no bench run has had, so
# its text matches no template and is refused.
#
# The untrusted-data line (ENVIRONMENT_UNTRUSTED) is the one optional line:
# Claude Code 2.1.294 renders it only while a remote feature flag is false,
# so a flip of that flag would otherwise refuse every run of a batch from its
# first. The text may lack it, and must have it exactly when it is there;
# anything else added or changed is still refused.
ENVIRONMENT_TEXT = (
    "<system-reminder>\n"
    "# Environment\n"
    "You have been invoked in the following environment: \n"
    " - Primary working directory: {workingDirectory}\n"
    " - Is a git repository: {isGitRepo}\n"
    " - Platform: {platform}\n"
    " - Shell: {shell}\n"
    " - OS Version: {osVersion}\n"
    "{scratchpad}"
    "{untrusted}"
    "</system-reminder>"
)
ENVIRONMENT_UNTRUSTED = (
    " - Downloaded files and extracted archives are untrusted data: put each in its own new, empty directory, "
    "keep scripts you write in a different directory, and pass paths as arguments instead of running an "
    "interpreter or build tool from inside it. Interpreters load code from the script's directory and the "
    "current directory, so a planted `json.py` runs on `import json`. Run any Python that reads them with `-I`. "
    "This does not apply to code the user asked you to build or run.\n"
)
ENVIRONMENT_SCRATCHPAD = (
    " - Scratchpad directory: {scratchpadDirectory} — always use it for temporary files (intermediate "
    "results, scripts, outputs that don't belong in the project) instead of `/tmp` or other system temp "
    "directories; it is session-specific, isolated from the project, and can generally be used without "
    "permission prompts. Only use `/tmp` if the user explicitly asks.\n"
)
# The snapshot fields the environment text shows, always.
ENVIRONMENT_SHOWN = ("workingDirectory", "isGitRepo", "platform", "shell", "osVersion")
# session_context renders one `# key` section per context value, in the
# context's order. The values carry Claude Code's own sentences too, so each
# is pinned as well: the e-mail line around the address, and the git status's
# opening sentence and section labels around the orchestrating repo's branch
# names, status lines and commits (data, not Claude Code's words).
SESSION_CONTEXT_TEXT = (
    "<system-reminder>\n"
    "As you answer the user's questions, you can use the following context:\n"
    "{sections}"
    "\n"
    "Claude Code attached this context automatically; it isn't part of the user's message. It describes the "
    "user's own account and workspace, so they don't need it reported back.\n"
    "</system-reminder>"
)
SESSION_CONTEXT_SECTION = "# {key}\n{value}\n"
USER_EMAIL = re.compile(re.escape("The user's email address is ") + r"[^\s@]+@[^\s@]+" + re.escape(
    ". Use it only to identify the user, such as for authorship, attribution, or filtering their own work. "
    "Never send it to an unrelated service, such as in a request header, URL, or payload, unless the user "
    "explicitly asks."))
GIT_STATUS = re.compile(
    re.escape("This is the git status at the start of the conversation. Note that this status is a snapshot "
              "in time, and will not update during the conversation.")
    + r"\n\nCurrent branch: [^\n]*"
    + r"\n\nMain branch \(you will usually use this for PRs\): [^\n]*"
    + r"(?:\n\nGit user: [^\n]*)?"
    + r"\n\nStatus:\n[^\n]+(?:\n[^\n]+)*"
    + r"\n\nRecent commits:\n[0-9a-f]{7,40} [^\n]*(?:\n[0-9a-f]{7,40} [^\n]*)*"
)
SESSION_CONTEXT_VALUES = {"userEmail": USER_EMAIL, "gitStatus": GIT_STATUS}
MODEL_IDENTITY_KEYS = ("knowledgeCutoff", "marketingName", "modelId")
# The model attachment's text, spelled from its identity, and the date's.
MODEL_TEXT = ("You are powered by the model named {marketingName}. The exact model ID is {modelId}. "
              "Assistant knowledge cutoff is {knowledgeCutoff}.")
DATE = re.compile(r"\d{4}-\d{2}-\d{2}")
DATE_TEXT = "Today's date is {date}."
REMINDER = re.compile(r"<system-reminder>\s*(.*?)\s*</system-reminder>", re.S)
REMINDER_OPEN = "<system-reminder>"
TOTAL_TOKENS = re.compile(
    r"(?:<system-reminder>\s*)?<total_tokens>\d+ tokens left</total_tokens>(?:\s*</system-reminder>)?"
)
# remote_session_change, when a plan tolerates it: the fields every real one
# has (the 62 archived runs of outline1 and probe3, and live subagent runs of
# Claude Code 2.1.294), and the text it renders, the attribution lines its
# commit and pr fill. The url is null or a claude.ai session URL. The commit
# is Claude's Co-Authored-By line (`Claude <model> <noreply@anthropic.com>`),
# plus a Claude-Session line naming its url when it has one (a url of null
# comes with neither session line, as some live subagent runs of 2.1.294
# had), and the pr is Claude Code's own line, plus the url. managedCommit
# and managedPr are false in every real one. The SendUserFile line is there
# only when sendUserFileHint is true, which an orchestrating session's own
# transcript has had and no subagent's yet. Anything else (another field,
# another sentence, another co-author, a managed attribution) is refused.
REMOTE_SESSION_FIELDS = ("type", "url", "commit", "pr", "sendUserFileHint", "managedCommit", "managedPr")
REMOTE_SESSION_FLAGS = ("sendUserFileHint", "managedCommit", "managedPr")
REMOTE_SESSION_UNMANAGED = ("managedCommit", "managedPr")
REMOTE_SESSION_URL = re.compile(r"https://claude\.ai/code/session_\w+", re.ASCII)
REMOTE_SESSION_COMMIT = r"Co-Authored-By: Claude [A-Za-z0-9 .]+ <noreply@anthropic\.com>"
REMOTE_SESSION_PR = "\U0001f916 Generated with [Claude Code](https://claude.com/claude-code)"
REMOTE_SESSION_TEXT = (
    "<system-reminder>\n"
    "Attribution for git commits and pull requests you create from here on (this replaces Claude Code's own "
    "earlier attribution guidance, such as a previous copy of this reminder; the user's own instructions about "
    "these lines, such as a CLAUDE.md or memory rule, take precedence over this reminder, but do not add "
    "attribution lines this reminder leaves out):\n"
    "- End git commit messages with:\n"
    "{commit}\n"
    "- End pull request descriptions with:\n"
    "{pr}\n"
    "{hint}"
    "</system-reminder>"
)
REMOTE_SESSION_HINT = (
    "\nThe user can follow this conversation from another device; to put a file in front of them there (a "
    "report, a screenshot, a built artifact), send it with SendUserFile.\n"
)
# Claude Code's handback tool. When the orchestrating session's permission
# mode is auto, Claude Code 2.1.294 gives every subagent this tool and, right
# after the task, a <system-reminder> prompt telling it to report through it
# (gated by the mode and a remote flag, tengu_lively_waffle, or the env var
# CLAUDE_CODE_SENDMESSAGE_HANDBACK). Its call ends the run without closing
# the message. The guard refuses such a run for the tool, the prompt and the
# ending alike, and names the cause first (AUTO_MODE).
HANDBACK_TOOL = "SubagentHandback"
AUTO_MODE = (f"the orchestrating session is in auto mode, which gives subagents {HANDBACK_TOOL}: switch it to "
             "bypass or default mode")
# The likely cause of a read_tree in the other bridge arm's format: taria and
# outline serve the same tool names, and a Claude Code session keeps the
# first definitions it sees for a name (probe4's taria runs, spawned after an
# outline run from the same session, were offered the outline's).
TREE_FORMAT_CAUSE = ("as when the orchestrating session saw the other bridge arm's tools first: a session keeps "
                     "the first definitions it sees for a tool name, so run taria and outline from separate sessions")


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


def _text_block(block):
    """Whether `block` is a text block: type text, a string text."""
    return isinstance(block, dict) and block.get("type") == "text" and isinstance(block.get("text"), str)


def _result_texts(content):
    """The text a tool_result block's `content` put in front of the model:
    the string, or each text block's text."""
    if isinstance(content, str):
        return [content]
    if not isinstance(content, list):
        return []
    return [b["text"] for b in content if isinstance(b, dict) and isinstance(b.get("text"), str)]


def _is_tool_result(content):
    return isinstance(content, list) and any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content)


def _prompt_text(content):
    """The prompt a user line carries, or None for a tool result."""
    if _is_tool_result(content):
        return None
    return _block_text(content)


def _beside_results(content):
    """(reminders, prompt) for the blocks of a tool-result user message that
    are not tool results: how many <system-reminder> blocks they hold, and 1
    when anything else is there (text outside a reminder, a block that is not
    text), which counts as one more prompt, else 0."""
    reminders, other = 0, False
    for block in content:
        if isinstance(block, dict) and block.get("type") == "tool_result":
            continue
        text = block if isinstance(block, str) else block["text"] if _text_block(block) else None
        if text is None:
            other = True
            continue
        reminders += text.count(REMINDER_OPEN)
        other = other or bool(REMINDER.sub("", text).strip())
    return reminders, int(other)


def _is_prompt(content):
    """Whether a user line's content is a prompt: a string, or blocks that
    are not tool results."""
    return isinstance(content, str) or (isinstance(content, list) and bool(content)
                                        and not _is_tool_result(content))


def _is_handback_prompt(content):
    """Whether a prompt is Claude Code's SubagentHandback reminder: one
    <system-reminder> naming HANDBACK_TOOL, which auto mode sends a subagent
    right after its task."""
    text = _block_text(content)
    return isinstance(text, str) and REMINDER.fullmatch(text.strip()) is not None and HANDBACK_TOOL in text


def _last_call(blocks):
    """The name of the tool the run's last block (thinking aside) called,
    or None when it is not a tool_use."""
    for block in reversed(blocks):
        kind = block.get("type")
        if kind == "thinking" or kind == "redacted_thinking":
            continue
        return block.get("name") if kind == "tool_use" else None
    return None


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


# How taria-mcp's read_tree description names the tree's format: "Read the
# TUI app's current semantic tree as JSON. Call this first." by default, "...
# semantic tree as an outline, one node per line, ..." under --tree-format
# outline (crates/taria-mcp/src/server.rs, READ_TREE_PASSAGE). The batches
# archived so far (full1, outline1, probe3) were served the longer texts of
# the day, "... the connected TUI app's current semantic tree as JSON: every
# node ...", which carry the same markers. The taria and outline arms list
# the same tools, so this is what tells their runs apart.
TREE_FORMAT_MARKERS = (("outline", "tree as an outline"), ("json", "tree as JSON"))
READ_TREE = SERVER_PREFIX + "read_tree"


def tree_format_of(description):
    """"json", "outline" or None: the tree format a read_tree description names."""
    if not isinstance(description, str):
        return None
    return next((fmt for fmt, marker in TREE_FORMAT_MARKERS if marker in description), None)


def _snapshot_tree_formats(att):
    """The tree format each read_tree a prompt_snapshot lists describes
    ("unknown" when it names none)."""
    tools = att.get("tools")
    return {
        tree_format_of(t.get("description")) or "unknown"
        for t in tools if isinstance(t, dict) and t.get("name") == READ_TREE
    } if isinstance(tools, list) else set()


def _deferred_tree_formats(definitions):
    """The tree format each read_tree among deferred-tool `definitions`
    (dicts with `name` and `description`) describes ("unknown" when it names
    none). A definition without a description says nothing, so it adds none:
    a read_tree offered only deferred, with no description recorded
    anywhere, leaves its format undetermined, which `guard` refuses."""
    return {
        tree_format_of(d["description"]) or "unknown"
        for d in definitions
        if isinstance(d, dict) and d.get("name") == READ_TREE and isinstance(d.get("description"), str)
    }


def _rendered_texts(line):
    """(texts, unreadable) for an attachment line: the text it put in front
    of the model, each `rendered[].content` (a string, or a list of text
    blocks), and how many characters of `rendered`, as JSON, are in no shape
    that reads so (0 when all of it reads). Real runs hold a list of objects
    whose one key is `content`, a string. No `rendered`, null or [] is no
    text; anything else that does not read (a plain string, an item with
    another key or without `content`, a block that is not text) is
    unreadable, and the guard refuses the attachment rather than take it for
    no text."""
    rendered = line.get("rendered")
    if rendered is None:
        return [], 0
    if not isinstance(rendered, list):
        return [], len(json.dumps(rendered))
    texts, unreadable = [], 0
    for item in rendered:
        content = item["content"] if isinstance(item, dict) and list(item) == ["content"] else None
        if isinstance(content, str):
            texts.append(content)
        elif isinstance(content, list) and all(_text_block(b) for b in content):
            texts += [b["text"] for b in content]
        else:
            unreadable += len(json.dumps(item))
    return texts, unreadable


def _label(kind):
    """A line's or an attachment's type as a guard sentence names it."""
    if isinstance(kind, str):
        return kind
    return "<no type>" if kind is None else f"<type {json.dumps(kind)}>"


def _beyond(obj, known):
    """The keys of `obj` outside `known`, sorted, as one string, or None."""
    extra = sorted(str(k) for k in obj if k not in known)
    return ", ".join(extra) if extra else None


def _unwrapped(text):
    """`text` stripped, and out of the <system-reminder> wrapper Claude Code
    renders an attachment in, if it is in one."""
    text = text.strip() if isinstance(text, str) else ""
    match = REMINDER.fullmatch(text)
    return match.group(1) if match else text


def _listed(names):
    """`names` as prose: "a", "a and b", "a, b and c"."""
    names = list(names)
    return " and ".join(names) if len(names) < 3 else f"{', '.join(names[:-1])} and {names[-1]}"


def _parting_line(text, expected):
    """The number of the first line where `text` parts from `expected`."""
    got, want = text.split("\n"), expected.split("\n")
    return next((n for n, (a, b) in enumerate(zip(got, want), 1) if a != b), min(len(got), len(want)) + 1)


def _first_difference(text, expected):
    """Where `text` first parts from `expected`: the line's number and the
    text's line there, cut to 80 characters."""
    got, number = text.split("\n"), _parting_line(text, expected)
    line = got[number - 1] if number <= len(got) else "<end of text>"
    return f"line {number}, {line if len(line) <= 80 else line[:77] + '...'!r}"


def _rendered_problem(texts, expected, what):
    """None when every rendered text is `expected`, or one of them when it
    is a tuple of the texts it may be, else where one is not: measured
    against the text it follows furthest, the first on a tie."""
    accepted = expected if isinstance(expected, tuple) else (expected,)
    for text in texts:
        if text not in accepted:
            nearest = max(accepted, key=lambda want: _parting_line(text, want))
            return f"its rendered text is not {what}: it differs at {_first_difference(text, nearest)}"
    return None


def _environment_text(snapshot, untrusted=True):
    """The text Claude Code renders from an environment `snapshot`
    (ENVIRONMENT_TEXT), with the untrusted-data line or, for `untrusted`
    false, without it; None when the snapshot lacks a field the text shows."""
    if any(key not in snapshot for key in ENVIRONMENT_SHOWN):
        return None
    values = {key: json.dumps(snapshot[key]) if isinstance(snapshot[key], bool) else snapshot[key]
              for key in ENVIRONMENT_SHOWN}
    scratchpad = snapshot.get("scratchpadDirectory")
    values["scratchpad"] = "" if scratchpad is None else ENVIRONMENT_SCRATCHPAD.format(scratchpadDirectory=scratchpad)
    values["untrusted"] = ENVIRONMENT_UNTRUSTED if untrusted else ""
    return ENVIRONMENT_TEXT.format(**values)


def _session_context_text(context):
    """The text Claude Code renders from a session `context`
    (SESSION_CONTEXT_TEXT): a section per value, in the context's order."""
    return SESSION_CONTEXT_TEXT.format(sections="".join(
        SESSION_CONTEXT_SECTION.format(key=key, value=value) for key, value in context.items()))


def _harmless_shape(kind, att, texts):
    """None when a harmless attachment has the shape it is known in, else
    what differs. Each is held to its top-level fields (HARMLESS_FIELDS),
    then to its content: the environment's snapshot keys, their types, one
    line each; the session context's two keys, each value in the words a
    real run's has; the date's format; the model identity's keys and the
    line it spells. Its rendered text is then held to the one text its own
    values fill: ENVIRONMENT_TEXT (with or without its one optional line,
    ENVIRONMENT_UNTRUSTED), SESSION_CONTEXT_TEXT, the date sentence, the
    model line, a bare <total_tokens> count, or no text at all for
    credential_org and thinking_drop. So a field or a sentence Claude Code
    adds is refused. An attachment with no rendered text put nothing in front
    of the model, and only its fields are checked."""
    fields = HARMLESS_FIELDS[kind]
    extra = _beyond(att, fields)
    if extra:
        return f"fields beyond {_listed(fields[1:])}: {extra}"
    if kind == "environment":
        snapshot = att.get("snapshot")
        if not isinstance(snapshot, dict):
            return "it has no snapshot object"
        extra = _beyond(snapshot, ENVIRONMENT_KEYS)
        if extra:
            return f"snapshot keys beyond the {len(ENVIRONMENT_KEYS)} a real run's has: {extra}"
        wrong = sorted(k for k, v in snapshot.items() if not isinstance(v, ENVIRONMENT_KEYS[k]))
        if wrong:
            return f"snapshot values of another type than a real run's: {', '.join(wrong)}"
        spanning = sorted(k for k, v in snapshot.items() if isinstance(v, str) and "\n" in v)
        if spanning:
            return f"snapshot values that span lines: {', '.join(spanning)}"
        expected = _environment_text(snapshot)
        if expected is None:
            missing = [key for key in ENVIRONMENT_SHOWN if key not in snapshot]
            return f"its rendered text cannot be checked: its snapshot has no {', '.join(missing)}" if texts else None
        return _rendered_problem(texts, (expected, _environment_text(snapshot, untrusted=False)),
                                 "the environment text its snapshot fills")
    if kind == "session_context":
        context = att.get("context")
        if not isinstance(context, dict):
            return "it has no context object"
        extra = sorted(str(k) for k in context if k not in SESSION_CONTEXT_KEYS)
        if extra:
            return f"context beyond {' and '.join(SESSION_CONTEXT_KEYS)}: {', '.join(extra)}"
        wrong = sorted(k for k, v in context.items()
                       if not (isinstance(v, str) and SESSION_CONTEXT_VALUES[k].fullmatch(v)))
        if wrong:
            return f"context values not in the words a real run's has: {', '.join(wrong)}"
        return _rendered_problem(texts, _session_context_text(context), "the session context text its values fill")
    if kind == "date":
        date = att.get("date")
        if not isinstance(date, str):
            return "it has no date string"
        if not DATE.fullmatch(date):
            return "its date is not YYYY-MM-DD"
        if any(_unwrapped(t) != DATE_TEXT.format(date=date) for t in texts):
            return f"its rendered text is not {DATE_TEXT.format(date='<date>')!r}"
        return None
    if kind == "model":
        text, identity = att.get("text"), att.get("identity")
        if not isinstance(text, str) or not isinstance(identity, dict):
            return "it has no model text and identity"
        if sorted(identity) != list(MODEL_IDENTITY_KEYS) or not all(isinstance(v, str) for v in identity.values()):
            return f"its identity is not {', '.join(MODEL_IDENTITY_KEYS)}, as strings"
        if text != MODEL_TEXT.format(**identity):
            return "its text is not the model line its identity spells"
        if any(_unwrapped(t) != text for t in texts):
            return "its rendered text is not its text"
        return None
    if kind == "credential_org" or kind == "thinking_drop":
        return "it carries rendered text, which it never has" if any(t.strip() for t in texts) else None
    if kind == "total_tokens_reminder":
        if all(isinstance(t, str) and TOTAL_TOKENS.fullmatch(t.strip()) for t in [att.get("text"), *texts]):
            return None
        return "its text is not a <total_tokens> count"
    return None


def _tolerated_shape(kind, att, texts):
    """None when an attachment a plan tolerates has the shape it is known
    in, else what differs. remote_session_change: exactly its seven fields
    (REMOTE_SESSION_FIELDS), a url of null or a claude.ai session URL, a
    commit and a pr in the words real ones have (Claude's Co-Authored-By
    line, and the session lines naming that url when there is one), three
    booleans of which managedCommit and managedPr are false, and rendered
    text that is the note its commit and pr fill (REMOTE_SESSION_TEXT, with
    the SendUserFile line exactly when sendUserFileHint is true). One with
    no rendered text put nothing in front of the model, and only its fields
    are checked."""
    if kind != "remote_session_change":
        return f"{kind} has no tolerated shape"
    fields = REMOTE_SESSION_FIELDS
    extra = _beyond(att, fields)
    if extra:
        return f"fields beyond {_listed(fields[1:])}: {extra}"
    missing = [field for field in fields if field not in att]
    if missing:
        return f"it has no {', '.join(missing)}"
    url, commit, pr = att["url"], att["commit"], att["pr"]
    if url is not None and not (isinstance(url, str) and REMOTE_SESSION_URL.fullmatch(url)):
        return "its url is neither null nor a claude.ai session URL (https://claude.ai/code/session_<id>)"
    if not (isinstance(commit, str) and isinstance(pr, str)):
        return "its commit and pr are not strings"
    if not all(isinstance(att[flag], bool) for flag in REMOTE_SESSION_FLAGS):
        return f"its {_listed(REMOTE_SESSION_FLAGS)} are not all booleans"
    if any(att[flag] for flag in REMOTE_SESSION_UNMANAGED):
        return (f"its {_listed(REMOTE_SESSION_UNMANAGED)} are not both false: a managed attribution, which no real "
                "note has had")
    session = "" if url is None else f"\nClaude-Session: {url}"
    if not re.fullmatch(REMOTE_SESSION_COMMIT + re.escape(session), commit):
        return ("its commit is not a Co-Authored-By line for Claude at noreply@anthropic.com"
                + (" and the Claude-Session line of its url" if url else ""))
    if pr != REMOTE_SESSION_PR + ("" if url is None else f"\n\n{url}"):
        return "its pr is not Claude Code's line" + (" and its url" if url else "")
    expected = REMOTE_SESSION_TEXT.format(commit=commit, pr=pr,
                                          hint=REMOTE_SESSION_HINT if att["sendUserFileHint"] else "")
    return _rendered_problem(texts, expected, "the attribution note its commit and pr fill")


UNREADABLE = ("its rendered field is in no shape the guard reads (a list of objects whose one key is content, "
              "a string or text blocks), so the text it put in front of the model cannot be checked")


def tolerable_problem(kind):
    """None when a plan may tolerate attachment type `kind`
    (ATTACHMENTS_TOLERABLE), else why not, in one sentence."""
    only = f"only {_listed(sorted(ATTACHMENTS_TOLERABLE))} can be tolerated"
    if not isinstance(kind, str):
        return f"an attachment type is a name, not {kind!r}; {only}"
    if kind in ATTACHMENTS_TOLERABLE:
        return None
    if kind in ATTACHMENTS_FLAGGED:
        return (f"{kind} ({ATTACHMENTS_FLAGGED[kind][0]}) is flagged, but not known to be the same for every run "
                f"of a batch, so a plan cannot tolerate it; {only}")
    if kind in ATTACHMENTS_VERIFIED or kind in ATTACHMENTS_HARMLESS:
        rule = "verified" if kind in ATTACHMENTS_VERIFIED else "harmless"
        return (f"{kind} is not flagged: the guard holds it to its own rule ({rule}), so there is nothing to "
                f"tolerate; {only}")
    return f"unknown attachment type {kind!r}: the guard has no rule for it and refuses it in every batch; {only}"


def check_tolerated(types):
    """`types`, the attachment types a plan tolerates, sorted and without
    repeats. Raises ValueError when it is not a list of names, or names a
    type a plan may not tolerate (tolerable_problem)."""
    if not isinstance(types, (list, tuple, set, frozenset)):
        raise ValueError(f"the tolerated attachments are a list of type names, not {types!r}")
    for kind in types:
        problem = tolerable_problem(kind)
        if problem:
            raise ValueError(problem)
    return sorted(set(types))


def _refusal(kind, att, texts, unreadable=0, tolerated=frozenset()):
    """(rule, detail) when the guard refuses this attachment, else None:
    "unknown" for a type with no rule (or no type, or no attachment object),
    "flagged" for one refused by name, "shape" for a known type whose
    rendered text does not read (`unreadable` characters of it, as JSON, the
    count the guard's sentence includes), a harmless type in a shape it is
    not known in, or a `tolerated` one in a shape it is not known in. A
    flagged type in `tolerated` in its known shape is not refused."""
    if not isinstance(att, dict) or not isinstance(kind, str) or kind not in KNOWN_ATTACHMENTS:
        return "unknown", None
    if kind in ATTACHMENTS_FLAGGED and kind not in tolerated:
        return "flagged", None
    if unreadable:
        return "shape", UNREADABLE
    if kind in ATTACHMENTS_FLAGGED:
        detail = _tolerated_shape(kind, att, texts)
    else:
        detail = _harmless_shape(kind, att, texts) if kind in ATTACHMENTS_HARMLESS else None
    return ("shape", detail) if detail else None


def _context(attachments, tolerated=frozenset()):
    """The context row field from `attachments`: (attachment, rendered texts,
    unreadable rendered characters) in transcript order, the attachment as
    the line carries it (not always an object). An attachment of a type in
    `tolerated`, in the shape it is known in, is tallied in
    `tolerated_attachments` instead of refused."""
    claude_md, deferred, loaded, servers, types = [], set(), set(), set(), set()
    deferred_added, servers_added = set(), set()  # every name ever added, removals ignored
    snapshots, offered, tree_formats, prefixes = [], set(), set(), set()
    refused = {}  # (label, rule, detail) -> [count, rendered chars]
    kept = {kind: [0, 0] for kind in sorted(tolerated)}  # tolerated type -> [count, rendered chars]
    cwd, skills = None, False
    for att, texts, unreadable in attachments:
        kind = att.get("type") if isinstance(att, dict) else None
        refusal = _refusal(kind, att, texts, unreadable, tolerated)
        if refusal is not None:
            label = _label(kind) if isinstance(att, dict) else "<not an object>"
            tally = refused.setdefault((label, *refusal), [0, 0])
            tally[0] += 1
            tally[1] += sum(len(t) for t in texts) + unreadable
        elif kind in tolerated:
            kept[kind][0] += 1
            kept[kind][1] += sum(len(t) for t in texts)
        if not isinstance(kind, str):
            continue
        types.add(kind)
        if kind == "prompt_snapshot":
            names = _snapshot_tools(att)
            snapshots.append(names)
            offered.update(names or [])
            tree_formats.update(_snapshot_tree_formats(att))
            if isinstance(att.get("cliPrefix"), str):
                prefixes.add(att["cliPrefix"])
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
            # A tool whose schema came loaded carries its definition here.
            surfaced = att.get("surfacedDefinitions")
            tree_formats.update(_deferred_tree_formats(
                s.get("definition") for s in (surfaced if isinstance(surfaced, list) else [])
                if isinstance(s, dict)
            ))
        elif kind == "deferred_tools_record":
            # A schema loaded by ToolSearch: the tool was on offer.
            entries = att.get("entries") if isinstance(att.get("entries"), list) else []
            for entry in entries:
                name = entry.get("name") if isinstance(entry, dict) else None
                if isinstance(name, str):
                    loaded.add(name)
            tree_formats.update(_deferred_tree_formats(entries))
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
        # The tree formats the offered read_tree tools described, over every
        # snapshot and every deferred-tool record that carries a definition:
        # ["json"] for a taria run, ["outline"] for an outline run, [] when
        # no read_tree was offered or none of its descriptions was recorded.
        "tree_formats": sorted(tree_formats),
        # The agent's identity line (prompt_snapshot.cliPrefix), which Claude
        # Code chooses: recorded so batches can be compared on it, not
        # checked.
        "cli_prefixes": sorted(prefixes),
        # The attachments the guard refuses, one entry per (type, rule,
        # detail): rule "unknown" (no rule for the type, no type, or no
        # attachment object), "flagged" (refused by name) or "shape" (a
        # rendered field that does not read, or a harmless type in a shape
        # it is not known in, detail saying how).
        "refused_attachments": [
            {"type": label, "rule": rule, "detail": detail, "count": count, "rendered_chars": chars}
            for (label, rule, detail), (count, chars) in sorted(refused.items(), key=lambda kv: (
                kv[0][0], kv[0][1], kv[0][2] or ""))
        ],
        # The attachments the plan tolerates (ATTACHMENTS_TOLERABLE), each
        # type with how many arrived in the shape it is known in and their
        # rendered characters: count 0 for one that never came. Empty for a
        # plan that tolerates nothing, a clean batch's.
        "tolerated_attachments": {kind: {"count": count, "rendered_chars": chars}
                                  for kind, (count, chars) in kept.items()},
    }


def read_transcript(path, tolerated=()):
    """The bench's numbers for the transcript at `path` (see the module doc),
    read with the attachment types the batch's plan tolerates (`tolerated`,
    see check_tolerated, which raises ValueError for a type a plan may not
    tolerate)."""
    tolerated = frozenset(check_tolerated(tolerated))
    lines, malformed = _lines(path)
    agent_id = session_id = None
    stamps, attachments, versions, efforts = [], [], set(), set()
    messages = {}  # message id -> {"model", "lines"}, in first-seen order
    blocks = {}  # (message id, block index) -> block, in first-seen order
    errors = set()
    synthetic = set()
    first_prompt, seen_user, prompts = None, False, 0
    handback_prompts = 0  # prompts that are auto mode's SubagentHandback reminder
    reminders = 0  # <system-reminder> blocks in or beside tool results
    other_lines = Counter()  # line types other than user, assistant and attachment
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
        if kind not in LINE_TYPES:
            other_lines[_label(kind)] += 1
        if kind == "attachment":
            att = line.get("attachment")
            attachments.append((att, *_rendered_texts(line)))
            if isinstance(att, dict) and att.get("type") == "max_turns_reached":
                max_turns_reached = {"max_turns": att.get("maxTurns"), "turn_count": att.get("turnCount")}
        elif kind == "user" and isinstance(line.get("message"), dict):
            content = line["message"].get("content")
            if not seen_user:
                seen_user = True
                first_prompt = _prompt_text(content)
            if _is_prompt(content):
                prompts += 1
                handback_prompts += _is_handback_prompt(content)
            elif _is_tool_result(content):
                # Blocks beside the results, in the same user message: a
                # reminder there is context like one inside a result, and
                # anything else came from outside the run, as a prompt does.
                beside, other = _beside_results(content)
                reminders += beside
                prompts += other
            for pos, block in enumerate(content if isinstance(content, list) else []):
                if not isinstance(block, dict) or block.get("type") != "tool_result":
                    continue
                if block.get("is_error") is True:
                    errors.add(block.get("tool_use_id") or (number, pos))
                reminders += sum(text.count(REMINDER_OPEN) for text in _result_texts(block.get("content")))
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
    # A message no line of which carries a stop_reason never got its closing
    # line, at the end of the run or in the middle of it: its output count is
    # the largest partial one, so the run's output total is a lower bound.
    unclosed = sum(1 for e in messages.values() if not any(l["stop_reason"] is not None for l in e["lines"]))
    context = _context(attachments, tolerated)
    # The lines no rule covers, and how many prompts the agent was given
    # (user lines that are not tool results): the task, and in a real run
    # nothing else.
    context["unknown_line_types"] = dict(sorted(other_lines.items()))
    context["prompts"] = prompts
    # Of those, the ones that are auto mode's SubagentHandback reminder (the
    # orchestrating session was in auto mode; see HANDBACK_TOOL).
    context["handback_prompts"] = handback_prompts
    # Context Claude Code appended to a tool's output, which no attachment
    # rule sees: <system-reminder> blocks inside tool results, or beside them
    # in the same user message.
    context["tool_result_reminders"] = reminders

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
        # The run's last block is a SubagentHandback call (auto mode).
        "ended_on_handback": _last_call(ordered) == HANDBACK_TOOL,
        "tool_errors": len(errors),
        "context": context,
        # How the run ended (see `ending`): the final assistant message's
        # model and stop_reason, synthetic or not, and whether Claude Code
        # recorded a max_turns_reached.
        "last_model": last_model,
        "last_stop_reason": last_stop_reason,
        "last_block_type": last_block_type,
        "end_inferred": end_inferred,
        # Output tokens: a lower bound when any message was never closed.
        "unclosed_messages": unclosed,
        "output_tokens_lower_bound": unclosed > 0,
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


def guard(stats, arm, model_family, arm_tools, effort=None, tree_format=None, tolerated=()):
    """Problems with how the run was set up, one sentence each; empty when
    the agent ran as its arm requires.

    `tolerated` is the attachment types the batch's plan tolerates (see
    check_tolerated, which raises ValueError for one a plan may not). A
    tolerated attachment the context recorded (read_transcript with the same
    list) is not a problem; one recorded as tolerated that `tolerated` does
    not name is refused by name, as if read without it.

    Allowed: the arm's `mcp__tui__<tool>` tools and ToolSearch (which loads
    deferred tools). A problem is: a tool called outside that; a model not
    `claude-<family>-*`; an effort other than `effort` (when given) on any
    turn; any instruction file (agents run with omitClaudeMd); offered tools
    (the prompt_snapshots' union, plus deferred ones) that are not exactly
    the arm's, or no prompt_snapshot to tell; any deferred tool beyond the
    arm's; MCP instructions from any server but `tui`, or none from it; a
    read_tree whose description shows the tree in another format than
    `tree_format` (when given: the taria and outline arms list the same
    tools, so this is what holds each to its own), or one offered only
    deferred whose description no record carries, so its format cannot be
    checked; an attachment the context refuses (see ATTACHMENTS_VERIFIED and
    the rest: a type with no rule, one flagged by name, a rendered field that
    does not read, a harmless one in a new shape or with new rendered text);
    a <system-reminder> block in or beside a tool result; a transcript
    line of a type other than user, assistant and attachment; more than one
    prompt; and a run with no assistant turn. The
    identity line (`cli_prefixes`) is recorded, not checked.

    A run that was offered SubagentHandback, called it, or was told to
    report through it (`handback_prompts`) ran under an orchestrating
    session in auto mode: AUTO_MODE comes first, saying so and what to do,
    and the problems it caused (a tool beyond the arm, a second prompt)
    follow as usual. A read_tree in the other bridge arm's format names its
    likely cause (TREE_FORMAT_CAUSE).

    Other servers and deferred tools are held against every name ever
    added, so one that reached the agent and later left (a server that
    disconnected) still counts. Removals matter only for `tui` itself: its
    instructions must still be there at the end.
    """
    tolerated = frozenset(check_tolerated(tolerated))
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
    formats = context.get("tree_formats") or []
    read_tree_offered = READ_TREE in (context.get("offered_tools") or []) or READ_TREE in deferred_ever
    if tree_format is not None and read_tree_offered and formats != [tree_format]:
        if formats:
            # Only the other bridge arm's format: "unknown" has another cause.
            cause = f", {TREE_FORMAT_CAUSE}" if "unknown" not in formats else ""
            problems.append(f"was offered a read_tree that shows the tree as {', '.join(formats)}, not as the "
                            f"{arm} arm's {tree_format}{cause}")
        else:
            problems.append(f"was offered read_tree only as a deferred tool, and no record carries its "
                            f"description, so its tree format cannot be checked against the {arm} arm's "
                            f"{tree_format}")
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
    problems += [attachment_problem(entry) for entry in context.get("refused_attachments") or []
                 if isinstance(entry, dict)]
    # Read as tolerated, but not tolerated here: refused as if read without.
    kept = context.get("tolerated_attachments")
    for kind, tally in sorted(kept.items() if isinstance(kept, dict) else []):
        if kind not in tolerated and isinstance(tally, dict) and tally.get("count"):
            problems.append(attachment_problem({"type": kind, "rule": "flagged", "count": tally.get("count"),
                                                "rendered_chars": tally.get("rendered_chars")}))
    reminders = context.get("tool_result_reminders")
    if isinstance(reminders, int) and reminders > 0:
        problems.append(f"has {reminders} <system-reminder> block(s) in or beside tool results: context Claude "
                        "Code added to a tool's output, which the guard cannot check")
    for kind, count in sorted((context.get("unknown_line_types") or {}).items()):
        problems.append(f"has {count} transcript line(s) of type {kind}, which the guard does not know")
    prompts = context.get("prompts")
    if isinstance(prompts, int) and prompts > 1:
        problems.append(f"was given {prompts} prompts: the task, then {prompts - 1} more from outside the run "
                        "(a message sent to the agent, a hook)")
    if not stats.get("turns"):
        problems.append("never ran: the transcript has no assistant turn")
    handback = _handback_signs(stats, context, deferred_ever)
    if handback:
        problems.insert(0, f"{AUTO_MODE} (this run {handback})")
    return problems


def _handback_signs(stats, context, deferred_ever):
    """What shows the run got auto mode's SubagentHandback, as prose ("was
    offered it, called it 1x and ..."), or None."""
    signs = []
    if HANDBACK_TOOL in (context.get("offered_tools") or []) or HANDBACK_TOOL in deferred_ever:
        signs.append("was offered it")
    calls = (stats.get("tool_calls") or {}).get(HANDBACK_TOOL)
    if isinstance(calls, int) and calls > 0:
        signs.append(f"called it {calls}x")
    prompts = context.get("handback_prompts")
    if isinstance(prompts, int) and prompts > 0:
        signs.append(f"got {prompts} prompt{'s' if prompts > 1 else ''} telling it to report through it")
    return _listed(signs) if signs else None


def attachment_problem(entry):
    """The guard's sentence for one `refused_attachments` entry: the type,
    how many arrived and how many characters of them were rendered."""
    kind, rule = entry.get("type"), entry.get("rule")
    size = f"{entry.get('count')}x, {entry.get('rendered_chars')} chars rendered"
    if rule == "flagged" and kind in ATTACHMENTS_FLAGGED:
        what, remedy = ATTACHMENTS_FLAGGED[kind]
        return f"was given {what} ({kind}, {size}): {remedy}"
    if rule == "shape":
        return f"was given a {kind} attachment in a shape the guard does not know ({size}): {entry.get('detail')}"
    return f"was given an attachment the guard does not know: {kind} ({size})"


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
    line (as it sometimes does for any message; see the module doc).

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
    explicit stop_reason but end_turn (max_tokens, refusal, ...). A run that
    ends on a SubagentHandback call (`ended_on_handback`) is one too, and
    its detail says why: the orchestrating session was in auto mode.
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
    if stats.get("ended_on_handback"):
        why = (f"its last block is a {HANDBACK_TOOL} call, which ends a run without closing its message: the "
               "orchestrating session is in auto mode")
    elif reason is None:
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
