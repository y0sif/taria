#!/usr/bin/env python3
"""Turn a real Claude Code subagent transcript into a test fixture.

    python3 bench/fixtures/scrub_transcript.py SRC.jsonl DST.jsonl --agent-id ID
        [--session-id UUID] [--servers tui] [--note TEXT] [--redact WORD ...]

Kept as Claude Code wrote it: every user and assistant line, message ids,
models, usage, stop_reasons, effort, tool_use / tool_result structure and
timestamps, and every attachment whose type bench/transcript.py has a rule
for (KNOWN_ATTACHMENTS: verified, harmless or flagged), session_context and
credential_org with their content replaced. An attachment of any other type,
or one with no type, is refused (exit 1, nothing written): give the type its
rule in bench/transcript.py first, so no fixture holds context the guard
has never judged, and none silently loses it either.

Replaced:
  - session_context: the orchestrating repo's git status (branch, user,
    status lines, commits) after Claude Code's opening sentence, in the
    context and in its rendered text alike; the e-mail address, as below;
    any other context value whole. So the attachment keeps the shape
    bench/transcript.py pins, and its rendered text stays the one its values
    fill;
  - credential_org: organizationUuid (zeroes);
  - remote_session_change and anywhere else: a claude.ai session URL's
    session id (https://claude.ai/code/session_FIXTURE);
  - mcp_instructions_delta: each addedBlocks entry by a placeholder naming its
    server (addedNames are kept), and its rendered text;
  - environment: workingDirectory (in its rendered text too, which shows
    it) and scratchpadDirectory (in every string too);
  - every line: cwd (/home/user/project), gitBranch, slug, sessionId, agentId;
  - in every string anywhere: the session ids, the agent id, the
    organization uuid, a /home/<user> prefix (/home/user), the user name,
    e-mail addresses (but Claude Code's own noreply@anthropic.com, which
    remote control's note carries and bench/transcript.py pins), and each
    --redact word (case-insensitive).

--servers NAME[,NAME] rewrites every mcp_instructions_delta's addedNames to
exactly those names (with their placeholder blocks) and marks the attachment
with a `_fixture_note` saying so. It exists for one reason: every real taria
run so far came from a session whose other MCP servers leaked their
instructions in, so a clean taria fixture has to be made from a leaked one by
editing that one field. Nothing else changes.

The meta file beside SRC (agent-<id>.meta.json) is copied beside DST
(DST minus .jsonl, plus .meta.json) keeping agentType, description and model.

Afterwards the output is searched for every value replaced; any hit is an
error (exit 1) and nothing is left written.

Python 3 stdlib only.
"""

import argparse
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from transcript import KNOWN_ATTACHMENTS  # noqa: E402  (bench/transcript.py)

FAKE_ORG = "00000000-0000-0000-0000-000000000000"
FAKE_CWD = "/home/user/project"
FAKE_SCRATCH = "/tmp/scratchpad"
FAKE_GIT_BODY = ("Current branch: fixture\n\nMain branch (you will usually use this for PRs): main\n\n"
                 "Git user: user\n\nStatus:\n(clean)\n\nRecent commits:\n0000000 (scrubbed from the fixture)")
SCRUBBED = "(scrubbed from the fixture)"
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+")
HOME = re.compile(r"/home/([^/\s\"']+)")
SAFE_EMAIL = "user@example.com"
# Claude Code's own attribution address, the same for everyone: kept, since
# bench/transcript.py pins remote control's note to it.
KEPT_EMAILS = ("noreply@anthropic.com",)
SESSION_URL = re.compile(r"(https://claude\.ai/code/session_)([0-9A-Za-z]+)")
FAKE_SESSION_URL = "https://claude.ai/code/session_FIXTURE"
META_KEYS = ("agentType", "description", "model")


def placeholder(server):
    return f"## {server}\n(instructions scrubbed from the fixture)"


def rendered(text):
    return [{"content": f"<system-reminder>\n{text}\n</system-reminder>"}]


class Scrubber:
    def __init__(self, lines, src, agent_id, session_id, redact):
        self.agent_id, self.session_id = agent_id, session_id
        sessions = {l.get("sessionId") for l in lines if isinstance(l.get("sessionId"), str)}
        session_dir = os.path.basename(os.path.dirname(os.path.dirname(os.path.abspath(src))))
        if re.fullmatch(r"[0-9a-f-]{36}", session_dir):
            sessions.add(session_dir)
        agents = {l.get("agentId") for l in lines if isinstance(l.get("agentId"), str)}
        base = os.path.basename(src)
        if base.startswith("agent-") and base.endswith(".jsonl"):
            agents.add(base[len("agent-"):-len(".jsonl")])
        orgs = {
            l["attachment"].get("organizationUuid") for l in lines
            if isinstance(l.get("attachment"), dict) and l["attachment"].get("type") == "credential_org"
        }
        users = set()
        for line in lines:
            for value in (line.get("cwd"), json.dumps(line.get("attachment") or {})):
                if isinstance(value, str):
                    users.update(m.group(1) for m in HOME.finditer(value))
        users.discard("user")
        scratch = {
            l["attachment"]["snapshot"].get("scratchpadDirectory") for l in lines
            if isinstance(l.get("attachment"), dict) and l["attachment"].get("type") == "environment"
            and isinstance(l["attachment"].get("snapshot"), dict)
        }
        # Longest first: the scratchpad path holds the session id and the user name.
        self.exact = [(s, FAKE_SCRATCH) for s in scratch if isinstance(s, str) and s]
        self.exact += [(s, session_id) for s in sessions if s] + [(a, agent_id) for a in agents if a]
        self.exact += [(o, FAKE_ORG) for o in orgs if isinstance(o, str) and o]
        self.users = sorted(users)
        self.words = sorted({w for w in redact if w})
        self.secrets = [s for s, _ in self.exact] + self.users + self.words

    def text(self, value):
        for old, new in self.exact:
            value = value.replace(old, new)
        value = HOME.sub("/home/user", value)
        value = EMAIL.sub(lambda m: m.group(0) if m.group(0) in KEPT_EMAILS else SAFE_EMAIL, value)
        value = SESSION_URL.sub(FAKE_SESSION_URL, value)
        for word in self.users + self.words:
            value = re.sub(re.escape(word), "user", value, flags=re.IGNORECASE)
        return value

    def walk(self, value):
        if isinstance(value, str):
            return self.text(value)
        if isinstance(value, list):
            return [self.walk(v) for v in value]
        if isinstance(value, dict):
            return {self.text(k): self.walk(v) for k, v in value.items()}
        return value

    def leaks(self, text):
        found = [s for s in self.secrets if s.lower() in text.lower()]
        found += [m for m in EMAIL.findall(text) if m != SAFE_EMAIL and m not in KEPT_EMAILS]
        found += [m.group(0) for m in SESSION_URL.finditer(text) if m.group(0) != FAKE_SESSION_URL]
        return found


def scrub_context_value(key, value):
    """A session context value with the orchestrating session's data out:
    the git status after its opening sentence, which Claude Code wrote and
    the guard pins; the e-mail line as it is (the address is replaced with
    every other one); anything else whole."""
    if key == "userEmail" and isinstance(value, str):
        return value
    if key == "gitStatus" and isinstance(value, str):
        head, sep, _ = value.partition("\n\nCurrent branch: ")
        if sep:
            return f"{head}\n\n{FAKE_GIT_BODY}"
    return SCRUBBED


def replace_in_rendered(line, old, new, what):
    """Replace `old` with `new` in the line's rendered text, refusing when a
    rendered text does not hold `old`: what it rendered would then be kept
    unscrubbed, or lost."""
    for item in line.get("rendered") or []:
        content = item.get("content") if isinstance(item, dict) else None
        if not isinstance(content, str) or old not in content:
            raise ValueError(f"{what}: its rendered text does not hold the value verbatim; scrub it by hand")
        item["content"] = content.replace(old, new)


def scrub_attachment(line, att, servers, note):
    kind = att.get("type")
    if kind == "session_context":
        context = att.get("context") if isinstance(att.get("context"), dict) else {}
        att["context"] = {key: scrub_context_value(key, value) for key, value in context.items()}
        for key, value in context.items():
            if att["context"][key] != value:
                replace_in_rendered(line, value, att["context"][key], f"session_context {key}")
    elif kind == "credential_org":
        att["organizationUuid"] = FAKE_ORG
    elif kind == "environment":
        snapshot = att.get("snapshot")
        if isinstance(snapshot, dict):
            if isinstance(snapshot.get("workingDirectory"), str):
                replace_in_rendered(line, snapshot["workingDirectory"], FAKE_CWD, "environment workingDirectory")
                snapshot["workingDirectory"] = FAKE_CWD
            if "scratchpadDirectory" in snapshot:
                snapshot["scratchpadDirectory"] = FAKE_SCRATCH
    elif kind == "mcp_instructions_delta":
        if servers is not None:
            att["addedNames"] = list(servers)
            att["_fixture_note"] = note
        names = [n for n in att.get("addedNames") or [] if isinstance(n, str)]
        att["addedBlocks"] = [placeholder(n) for n in names]
        if "rendered" in line:
            line["rendered"] = rendered(
                "# MCP Server Instructions\n\nThe following MCP servers have provided instructions "
                "for how to use their tools and resources:\n\n" + "\n\n".join(att["addedBlocks"])
            )


def scrub(src, dst, agent_id, session_id, servers=None, note=None, redact=()):
    with open(src, encoding="utf-8") as f:
        lines = [json.loads(raw) for raw in f if raw.strip()]
    scrubber = Scrubber(lines, src, agent_id, session_id, redact)
    out = []
    unknown = sorted({
        repr(att.get("type")) if isinstance(att, dict) else "(not an object)"
        for line in lines if line.get("type") == "attachment"
        for att in [line.get("attachment")]
        if not isinstance(att, dict) or att.get("type") not in KNOWN_ATTACHMENTS
    })
    if unknown:
        raise ValueError(f"attachment type(s) bench/transcript.py has no rule for: {', '.join(unknown)}; "
                         "give each its rule there first")
    for line in lines:
        att = line.get("attachment")
        if line.get("type") == "attachment":
            scrub_attachment(line, att, servers, note)
        for key, value in (("cwd", FAKE_CWD), ("gitBranch", "fixture"), ("slug", "fixture-slug"),
                           ("sessionId", session_id), ("agentId", agent_id)):
            if key in line:
                line[key] = value
        out.append(scrubber.walk(line))
    text = "".join(json.dumps(line, ensure_ascii=False) + "\n" for line in out)

    meta_src = src[: -len(".jsonl")] + ".meta.json"
    meta_text = None
    if os.path.exists(meta_src):
        with open(meta_src, encoding="utf-8") as f:
            meta = json.load(f)
        meta_text = json.dumps(scrubber.walk({k: meta[k] for k in META_KEYS if k in meta}), indent=2) + "\n"

    leaks = scrubber.leaks(text) + (scrubber.leaks(meta_text) if meta_text else [])
    if leaks:
        raise ValueError(f"the scrubbed output still holds {sorted(set(leaks))}")
    with open(dst, "w", encoding="utf-8") as f:
        f.write(text)
    if meta_text is not None:
        with open(dst[: -len(".jsonl")] + ".meta.json", "w", encoding="utf-8") as f:
            f.write(meta_text)
    return len(lines), len(out)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("src")
    parser.add_argument("dst")
    parser.add_argument("--agent-id", required=True, help="the fixture's agent id")
    parser.add_argument("--session-id", default="00000000-0000-4000-8000-000000000000")
    parser.add_argument("--servers", type=lambda v: [s for s in v.split(",") if s],
                        help="rewrite mcp_instructions_delta addedNames to exactly these")
    parser.add_argument("--note", default=None, help="the _fixture_note for --servers")
    parser.add_argument("--redact", action="append", default=[], help="a word to replace anywhere")
    args = parser.parse_args(argv)
    if not args.dst.endswith(".jsonl"):
        parser.error("DST must end in .jsonl")
    note = args.note or (
        f"addedNames edited to {args.servers} from a real run whose session leaked other "
        "servers' instructions; every other field is as Claude Code wrote it"
    )
    try:
        read, wrote = scrub(args.src, args.dst, args.agent_id, args.session_id, args.servers, note, args.redact)
    except (OSError, ValueError) as err:
        print(f"scrub_transcript: {err}", file=sys.stderr)
        return 1
    print(f"scrub_transcript: {args.dst}: {wrote} of {read} lines kept")
    return 0


if __name__ == "__main__":
    sys.exit(main())
