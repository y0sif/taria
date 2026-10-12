# taria benchmark

Does an agent drive a terminal app better through taria's semantic tree than
by scraping the screen? This benchmark answers that on `taria-demo`. Every arm
gets the same tasks, prompts and agent instructions; only the tools differ.
Runs are graded on what the app holds when the agent stops, never on what
any arm saw. Each run records verified success, turns, tool calls, tokens
and wall time. The models are Sonnet 5.5 and Haiku 5.5.

## The arms

Each arm is an MCP server (Model Context Protocol, the standard way to hand an
agent a set of tools). All are registered as `tui`, so the agent sees
`mcp__tui__<tool>` whichever it gets and no tool name gives the arm away.

- **taria**: the real bridge, `target/debug/taria-mcp --app taria-demo`, with
  `read_tree`, `act`, `key` and `type_text`. The tree comes as JSON, the
  bridge's default.
- **outline**: the same binary and the same four tools, started with
  `--tree-format outline`. The bridge shows the same tree as one indented
  line per node, `id role "label" = "value" *focused act: a|b|c`, the
  actions last, absent when a node has none, as in
  `input text_input "New task" = "x" *focused act: set_value|activate|dismiss`
  (the grammar is in `docs/cli.md`, The outline), and its instructions and
  its `read_tree` and `act` descriptions describe the outline instead of the
  JSON. It measures whether the outline cuts tokens without hurting pass
  rates. Batches outline1 and probe3 ran the outline before the actions
  moved to the line's end (` [a b c]` after the value); the dry-run solver
  still reads that form.
- **tmux**: `bench/tmux_mcp.py`, with `screen` (the pane as plain text) and
  `send_keys` (tmux key names, or text with `literal: true`). Key names go
  30 ms apart, so `"Escape d"` is not alt+d, and the settle wait is capped at
  the 500 ms taria-mcp waits for an app to react.

taria and outline offer the same tool names, so what tells their runs apart
is the `read_tree` description each server serves ("the current semantic
tree as JSON" or "as an outline"). Transcripts record it, and the guard holds
each arm to its own (see Isolation); the dry run checks the live server's.
For the same reason the two never share an orchestrating session: a Claude
Code session keeps the first definitions it sees for a tool name (see How
runs execute).

`screen` has no colors or styles, as a screen scraper gets. The demo marks the
active tab only as bold, so on an empty tab the tmux arm cannot tell which tab
it is on. That is a real handicap of scraping, not a harness bug.

## Tasks

The demo starts with this seed: `task-1` 'Write the taria README', `task-3` 'Wire up the MCP
bridge' and `task-4` 'Record the killer demo' are todo; `task-2` 'Sketch the
protocol wire types' and `task-5` 'Publish a snapshot per frame' are done.
Prompts name no arm's tools, node ids or key bindings.

| id | prompt | passes when |
|----|--------|-------------|
| T1 | Add a task titled 'Benchmark the bridge'. | exactly one new todo task with that title; seed unchanged |
| T2 | Mark the task 'Record the killer demo' as done. | `task-4` done; nothing else changed |
| T3 | Delete the task 'Wire up the MCP bridge'. | `task-3` gone; the other four seed tasks unchanged |
| T4 | Add a new task whose title is the title of the third task on the Active tab, followed by ' (follow-up)'. | exactly one new todo task 'Record the killer demo (follow-up)'; seed unchanged |
| T5 | Mark every task on the Active tab whose title starts with 'W' as done, then delete the task 'Sketch the protocol wire types'. | `task-1` and `task-3` done, `task-2` gone, `task-4` and `task-5` unchanged; working by position marks `task-4`, since completing the first match shifts the tab |
| T6 | Add a task titled 'Check the dialog'. Do not delete anything. | starts inside the delete dialog for `task-1`, opened through tmux before the agent's server starts (every arm); one new task with that title, every seed task intact |

## Grading

The demo writes its state as JSON to the file `TARIA_DEMO_STATE` names. Each
check in `bench/tasks.py` holds that dump against the seed, every field of
every task, so a run passes only if the asked-for change happened and nothing
else did: no open dialog, no leftover draft, app not quit. Which tab shows
does not count. The demo process must also still run, or the run fails with
the reason `demo exited`. Each run gets a fresh demo in its own tmux server
and `tui-` temp dir, 120x40. A `TMPDIR` naming taria, tmux or bench is
refused, since the demo shows its socket path on screen.

## How runs execute

An interactive Claude Code session, on the user's subscription, is the
orchestrator. It runs `bench/run.py` and spawns one subagent per run with its
Agent tool. A subagent is a separate agent with its own system prompt, tools
and context, which reports back when done. Python never calls a model. A slot
is one run in flight: a demo that `run.py` started, plus one agent file per arm
wired to it. With K slots, up to K runs go at once.

Build, write the agent files and set up isolation (below), then start the
orchestrating session in `~/Projects/taria`; a session loads a new
`.claude/agents` directory only at startup. From it, plan a batch and start
each slot. Needs `tmux` and Python 3.

```bash
cargo build --workspace     # run.py never builds, and refuses a stale build
python3 bench/run.py agents --slots 4
python3 bench/run.py plan --batch B --models sonnet,haiku --runs 5
python3 bench/run.py next --batch B --slot 1
```

`agents --dir` and `next --agents-dir` both default to `.claude/agents` at
the root of the repo the bench is in. When the orchestrating session starts
somewhere else, pass that session's `.claude/agents` to both.

Until `feat/benchmark` merges, `run.py` lives in the `~/Projects/taria-bench`
worktree: call it by full path, and pass `--dir ~/Projects/taria/.claude/agents`
to `agents` and `--agents-dir ~/Projects/taria/.claude/agents` to `next`.

Run the orchestrating session from the main checkout, not a worktree, and do
not `cd` or `/add-dir` during a batch: both change the environment text, which
the guard refuses. Call `run.py` by absolute path. Run it in bypass or
default permission mode, not auto (see Isolation).

Run the taria and outline arms in separate batches, each from its own
orchestrating session, and restart the session between them. The two serve
the same tool names (`mcp__tui__read_tree`, `act`, `key`, `type_text`) with
different descriptions, and a Claude Code session keeps the first
definitions it sees for a tool name for the rest of the session. In probe4,
outline job j001 started first, and taria jobs j002 and j005 (j005 spawned
alone, after every other run had finished) were offered the outline's
descriptions while their own bridge served JSON trees and JSON
instructions. The guard caught them by their tree format, but the runs were
lost. tmux has tool names of its own, so taria with tmux (full1) and outline
alone (outline1) never collided. So:

- `plan` refuses a batch whose arms share tool names, and its `--arms`
  defaults to `taria,tmux`;
- `next` refuses to hand out a job of a batch with an arm that shares tool
  names with another arm the same orchestrating session was already
  handed, or graded a run of, in any batch: it claims nothing and says to
  restart the session. It knows the session by `CLAUDE_PID`, the pid Claude
  Code sets for the commands its Bash tool runs, plus that process's start
  time, and records in `results/slots/handouts.jsonl` each job it hands out
  and each run it grades. A graded run's arm goes on record for the session
  that grades it, whoever claimed the job: a restarted session that spawned
  the agent for the old session's slot has seen that arm's tools. A
  `next --agent-id` that grades a slot and is then refused the next job
  keeps the row and frees the slot, and its error says so. When it cannot
  tell the session, it says so in one line on stderr every time, and only
  this procedure protects the batch: `CLAUDE_PID` unset (run outside Claude
  Code), or set but naming no process it can see in `/proc` (the process is
  gone, or run.py runs in a PID namespace of its own, as a sandbox may run
  it), or no `/proc` at all (off Linux). It also refuses every job of a plan
  written with both arms before `plan` refused that (probe4's).

`plan` writes the shuffled jobs (task x arm x model x run) and the seed to
`results/B/plan.json`, and never replaces one. It also refuses a batch whose
`runs.jsonl` already has rows (a dry run's, say): their job ids would match
the plan's, and `next` takes a job with a row as done.

`plan --tolerate-attachment remote_session_change` (repeatable) plans a batch
that runs with `/remote-control` on in the orchestrating session, for when
turning it off is not an option (it would cut off the person following the
session from another device). The plan records `tolerated_attachments`
(sorted; `[]` by default), and `next` and `regrade` grade every run of the
batch with it: remote control's attribution note, in the shape real ones
have, is recorded in the row's `context.tolerated_attachments` instead of
making the run a harness error (see Isolation). It is acceptable only when
every arm of the batch carries the same note, which holds when one session
with remote control on orchestrates the whole batch: then the arms of the
batch compare with each other as fairly as in a clean one. What it costs:
about 710 characters beside the task, before the agent's first model call,
carried in every call, so the batch's absolute token counts include the
note, and it is not a clean batch. Do not hold its absolute numbers against a clean batch's;
`summarize.py` marks every table built from it. Only
`remote_session_change` can be tolerated: `plan` refuses `skill_listing`
(nothing shows it would be the same for every run), a type the guard holds
to its own rule, and a type it does not know, saying which and why.

```bash
python3 bench/run.py plan --batch B --models sonnet,haiku --runs 5 --tolerate-attachment remote_session_change
```

`next` claims the
slot's next job, starts its demo and prints one line:

```json
{"job": "j001", "slot": 1, "agent": "tui-taria-1", "model": "haiku", "prompt": "Delete the task 'Wire up the MCP bridge'."}
```

The orchestrator spawns exactly that, with the prompt verbatim:
`Agent(subagent_type="tui-taria-1", model="haiku", description="bench j001", prompt="...")`.
When the agent returns, pass the `agentId` from the Agent tool's result:

```bash
python3 bench/run.py next --batch B --slot 1 --agent-id <id>
```

This finds the transcript under `~/.claude/projects`, copies it and its
`.meta.json` into `results/B/transcripts/`, runs the guard (the transcript
checks under Isolation), grades the demo, appends one row to
`results/B/runs.jsonl`, tears the demo down and prints the slot's next job,
or `{"done": true}`. Run each slot's loop side by side. When `next` refuses,
it prints `{"error": ..., "slot": K}` and exits 2. A harness error stops the
slot: the row is recorded, nothing is claimed. Fix the setup, then
`next --batch B --slot K` resumes. A `harness_error` job is not retried: it
stays a row outside n, and so does an aborted job.

Before it grades, `next --agent-id` checks that the agent can have run the
slot's job. It refuses when:

- the batch already recorded that agent id;
- no transcript exists for the id;
- the transcript has no `.meta.json` beside it;
- the `.meta.json` names another slot's agent;
- the transcript's first prompt, compared stripped, is not the job's task;
- the transcript started before the slot was prepared.

These are the orchestrator's mistakes, not runs. A refusal exits 2, records
nothing and keeps the slot, so pass the right id or `abort` the slot.

`status` shows progress and `abort` records a harness error and frees a slot.
`dry-run` costs nothing. It drives every task on every arm with a scripted
solver (`bench/solvers.py`) through the same server an agent gets, no model.
The taria solver reads node ids and actions from the JSON tree; the outline
solver parses the outline text back into the same tree (ids are the line's
first token, JSON-quoted when not bare) and runs the same steps; the tmux
solver reads the screen. `--sabotage` swaps in a plausible wrong plan per
task, so each check fails. `--arms` defaults to all three arms for dry-run,
which spawns no agent, and to `taria,tmux` for plan, which refuses taria and
outline together.

```bash
python3 bench/run.py status --batch B                  # recorded, held, pending
python3 bench/run.py abort --batch B --slot K --reason TEXT
python3 bench/run.py dry-run                           # 18/18 pass
python3 bench/run.py dry-run --sabotage                # 18/18 fail, exit 1 (expected)
```

## The subagent definitions

`agents --slots K --dir D` writes `tui-<arm>-<K>.md` for every arm (taria,
tmux, outline) and slots 1 to K. It refuses to overwrite a file it did not write; `--remove` deletes only
its own. The frontmatter:

- `tools: mcp__tui`: every tool of the agent's own server, nothing else.
- `model: sonnet`, a default the Agent call's `model` overrides per job.
- `effort` and `maxTurns`, default medium and 30; pass the same `--effort`
  and `--max-turns` to `agents` and `plan`.
- `omitClaudeMd: true`, and one inline stdio server, `tui`, in `mcpServers`.

The arms' files differ only in name, description and the launcher's `--arm`
and `--slot`. The body is the system prompt, the same bytes in all three. Its
sha256 goes into `plan.json` as `agent_body_sha256`, and before each claim
`next` checks the slot's agent files against the plan, body included. Names
are matched whole (`tui-outline-1` is never read as `tui-taria-1`), and the
launcher refuses an agent of one arm in a slot that holds another's job:

> You are operating a terminal application on the user's behalf. The
> application is already running, and the tools you have are your only way to
> see it and control it. Do what the user asks. When you are done, reply with
> one short sentence saying what you did.

The inline server is `bench/mcp_launch.py`. It reads the slot file `next`
wrote (`results/slots/<K>.json`) and execs the arm's real server against that
slot's demo. An exec, not a proxy: the MCP stream passes straight through. A
missing slot file, a slot not ready, or a slot holding another arm's job
exits 1, so the agent gets no tools rather than the wrong arm's.

One launch per job. Before the exec, the launcher creates the marker
`results/slots/<K>.<job>.launched`, and it refuses when that marker exists,
so a second agent cannot drive a demo another agent already used. `run.py`
removes a slot's markers when it frees the slot. A refused agent is offered
no tools, and the guard makes that run a harness error.

## Isolation

This is what makes the numbers fair. Measured on 2026-10-11 from subagent
transcripts: a subagent receives the `instructions` text of every MCP server
connected to the orchestrating session, not just its own, even though its
tools are restricted. It arrives as the transcript's `mcp_instructions_delta`
attachment. In the first probe that was about 7k characters from the claude.ai
connectors, furcule, context7, posthog and the repo's own `taria` server, so
the tmux arm was told to "call read_tree first".

The orchestrating session must therefore run with no MCP servers but the
subagents' own. `--strict-mcp-config` does not work: it also drops the agents'
inline servers, so the subagent launches with zero tools. What works is this,
in the orchestrating project's `.claude/settings.local.json`:

```json
{
  "permissions": { "allow": ["mcp__tui"] },
  "disableClaudeAiConnectors": true,
  "enabledPlugins": {
    "context7@claude-plugins-official": false,
    "posthog@claude-plugins-official": false
  },
  "disabledMcpjsonServers": ["taria"],
  "deniedMcpServers": [{"serverName": "furcule"}, {"serverName": "linear"}]
}
```

Adjust the plugin and server names to whatever your session has connected.
List what is connected with `/mcp` and `/plugin`. The connector switch is
also the env var `ENABLE_CLAUDEAI_MCP_SERVERS=false`. Changes to plugins and
MCP servers need a session restart; only the connector switch applied live. The guard checks the result: every MCP server whose
instructions ever reached the agent must be `tui`, even one that later
disconnected (`context.mcp_instruction_servers_added`). A leaking session
turns every run into a harness error, so run a probe before a real batch: a
small batch such as `plan --batch probe3 --tasks T3 --models sonnet,haiku`,
driven like any other. Then check that every row has an empty
`guard_problems` and `context.mcp_instruction_servers_added` equal to
`["tui"]`.

What every run still receives, identical across arms: the environment block
(working directory), the date, the model identity, the session context (the
user's e-mail and the orchestrating repo's git status), the tokens left in
the context window, and the organization id (recorded, never shown to the
model).

Each of those arrives as an attachment, and every attachment type has
exactly one rule in `bench/transcript.py`, since an attachment is context
added to the agent's prompt:

| rule | types | the guard |
|---|---|---|
| verified | `prompt_snapshot`, `mcp_instructions_delta`, `deferred_tools_delta`, `deferred_tools_record`, `max_turns_reached`, `instructions` | reads what they carry and holds the run to it (below) |
| harmless | `environment`, `session_context`, `date`, `model`, `credential_org`, `thinking_drop`, `total_tokens_reminder` | holds each to the top-level fields it is known with (any other field is refused) and to the shape it is known in: a snapshot of the 8 keys a real run's has, their types, one line each; session context of `userEmail` and `gitStatus` only, each in Claude Code's words around the data (the e-mail sentence around the address, the git status's opening sentence and section labels around the repo's branches, status and commits); a `YYYY-MM-DD` date; a model identity of `modelId`, `marketingName` and `knowledgeCutoff`; a `<total_tokens>` count. Its rendered text is then held to the text its own values fill. Only the environment's and the session context's are compared character for character, against the template every archived run rendered them in (`ENVIRONMENT_TEXT` and `SESSION_CONTEXT_TEXT` in `bench/transcript.py`), so a sentence Claude Code adds is refused; the one exception is the environment's untrusted-data line (`ENVIRONMENT_UNTRUSTED`), which Claude Code 2.1.294 renders only while a remote feature flag is false, so it may be missing, and is exact when there. The date's and the model's are compared after stripping surrounding whitespace and the `<system-reminder>` wrapper: "Today's date is ..." with the attachment's date, and the model line its identity spells (the attachment's `text` must be that line too). The token reminder's `text` and rendered text must each be a bare `<total_tokens>N tokens left</total_tokens>`, wrapper optional, with any N: the rendered count is not tied to the one in its `text`. `credential_org` and `thinking_drop` must render nothing but whitespace (diagnostics Claude Code records and never shows the model) |
| flagged | `remote_session_change`, `skill_listing` | refuses it by name, unless the batch's plan tolerates it (`remote_session_change` only, below): then holds it to its seven fields (`type`, `url`, `commit`, `pr`, `sendUserFileHint`, `managedCommit`, `managedPr`, no other), a `url` of null or a claude.ai session URL (`https://claude.ai/code/session_<id>`), a `commit` that is Claude's own `Co-Authored-By` line (`Co-Authored-By: Claude <model> <noreply@anthropic.com>`, the model in letters, digits, spaces and dots; and the `Claude-Session` line of its url when it has one), a `pr` that is Claude Code's "Generated with" line (and its url), three booleans of which `managedCommit` and `managedPr` are false (a managed attribution is refused), and rendered text that is the note those fill (`REMOTE_SESSION_TEXT` in `bench/transcript.py`, with the SendUserFile line exactly when `sendUserFileHint` is true), and records it in `context.tolerated_attachments` |

Any other type, an attachment line with no type, or one whose attachment is
not an object makes the run a harness error, and the guard's sentence names
the type, how many arrived and how many characters of them were rendered.
So does an attachment of any type whose `rendered` field is there but does
not read as text: real runs hold a list of objects whose one key is
`content`, a string (or text blocks), and anything else (a plain string, an
object keyed otherwise, a block that is not text) is refused rather than
read as no text, which would skip every check of the text and count 0
characters. Its characters are counted as JSON.
That is how `remote_session_change` was caught after the fact: with
`/remote-control` on in the orchestrating session, Claude Code put a
~710-character attribution note, as a user-role reminder beside the task,
before the first model call, into every subagent run of outline1 and
probe3, and the guard of the day read only the types it knew. Turn `/remote-control` off before a
batch.

Do not orchestrate in auto mode either. When the orchestrating session's
permission mode is `auto`, Claude Code 2.1.294 gives every subagent a
`SubagentHandback` tool and, right after its task, a `<system-reminder>`
prompt telling it to report through that tool (gated by the mode and a
remote flag, `tengu_lively_waffle`, or the env var
`CLAUDE_CODE_SENDMESSAGE_HANDBACK`). The agent obeys, and the call ends its
run without closing the message. The guard refuses such a run three ways (a
tool beyond its arm, a second prompt, a last message that did not end on its
own), and its first sentence says why: "the orchestrating session is in auto
mode, which gives subagents SubagentHandback: switch it to bypass or default
mode". Every graded run of probe4 was refused so. Switch the mode before
a batch, as with remote control.

Likewise, run the orchestrating session from the main checkout, not a
worktree, and do not `cd` or `/add-dir` during a batch: both change the
environment text, which the guard refuses. Call `run.py` by absolute path.
A new Claude Code version that adds a type stops a batch at its
first run, which is the point: give the type its rule, then resume.

When `/remote-control` cannot be off, plan the batch with
`--tolerate-attachment remote_session_change` (see How runs execute). The
guard then reads every run of the batch with that tolerance: a note in the
shape the table above gives is no guard problem, and the row's
`context.tolerated_attachments` says how many came and how many characters
they rendered, as
`{"remote_session_change": {"count": 1, "rendered_chars": 710}}`; a run that
got none records count 0. A note in any other shape (a field added or
missing, a sentence more, rendered text that does not read) is still
refused, and so is a note in a batch whose plan does not tolerate it, which
is every batch planned before the option, outline1 and probe3 among them:
their regrade still refuses all their runs. Every other check is unchanged.
It is acceptable only when every arm of the batch carries the same note,
which one orchestrating session gives them all. It costs the batch its
clean status: its absolute token counts include the note in every call
after it arrives, so they are not comparable with a clean batch's, and the
summary marks them.

The agent's identity line, the first line of its system prompt
(`prompt_snapshot.cliPrefix`), is Claude Code's choice and changed between
full1 ("You are Claude Code, ...") and outline1 ("You are a Claude agent,
..."). It is recorded in `context.cli_prefixes`, not checked, and the
summary's run conditions compare it between batches. The change is 32
characters. The tmux arm's first-call context fell 4 tokens between full1
and cycle2a, and that change is attributed to the identity line, about 4
tokens per call: it is the only text before that call whose length
changed (RESULTS.md).

What each row proves, from the transcript: `model_resolved` (the models its
turns ran on), `efforts_resolved` (the efforts Claude Code recorded on every
assistant line), and the guard. The guard makes a run a harness error if it:

- called a tool outside its arm (ToolSearch, which loads the arm's deferred
  tools, is allowed);
- ran on a model outside `claude-<family>-*`;
- ran at an effort other than the plan's on any turn, or recorded none;
- was given a CLAUDE.md or memory file;
- was offered tools that are not exactly its arm's, or has no
  `prompt_snapshot` to tell (a server that never started, or that
  `mcp_launch` refused, offers none);
- was ever offered a deferred tool beyond its arm's (this catches the other
  arm's), even one later removed;
- was offered a `read_tree` whose description shows the tree in another
  format than its arm's (json for taria, outline for outline), which is how
  the two bridge arms, with the same tool names, are told apart (the other
  bridge arm's format is what an orchestrating session that ran that arm
  first gives, and the sentence says so: see How runs execute). A
  `read_tree` offered only as a deferred tool is read from the deferred-tool
  records that carry its description (a ToolSearch load, or a schema that
  came loaded); when none does, its format cannot be checked, and that is a
  problem too;
- ever got the instructions of an MCP server other than `tui`, got none
  from `tui`, or lost `tui`'s mid-run;
- was given an attachment without a rule, one flagged by name that its
  plan does not tolerate, one whose rendered text does not read, or a
  harmless or tolerated one in a shape or with rendered text it is not
  known in (the table above);
- has a `<system-reminder>` block inside a tool result, or beside one in
  the same user message, which is where Claude Code appends its reminders
  to a tool's output, out of reach of the attachment rules
  (`context.tool_result_reminders` counts them; no real run has had one);
- has a transcript line whose type is not `user`, `assistant` or
  `attachment`;
- was given more than one prompt (a message sent to the agent mid-run, a
  hook, or text or any other block beside a tool result that is not a
  `<system-reminder>`);
- never took a turn.

A run that was offered `SubagentHandback`, called it, or got auto mode's
prompt to report through it (`context.handback_prompts` counts those
prompts) is refused for each of those as above, and the guard's first
sentence names the cause: the orchestrating session in auto mode (above).

A run is also a harness error when it did not end on its own: Claude Code's
own `<synthetic>` message comes last (a usage limit, an API error), its last
message stopped on anything but `end_turn` (`max_tokens`, `refusal` and the
like), or it stopped on a tool call under the turn cap (a `SubagentHandback`
call included, which the error says comes from auto mode).

Claude Code 2.1.294 sometimes never writes the closing line of an assistant
message, in the middle of a run as well as at its end, so that message's
lines carry stop_reason null and an output count cut short: 1 to 80 tokens
in full1, outline1, cycle2a and cycle2b. One
such shape counts as ended: a final text reply whose stop_reason is null.
The reply arrived, so the run ended. The row says `end_inferred: true`, and
its `notes` say that message's output tokens are a lower bound.

Any unclosed message, final or not, makes the row's output tokens a lower
bound: the row says `output_tokens_lower_bound: true` and counts such
messages in `unclosed_messages`. Summaries leave those rows out of the
output-token median and say how many per cell. Input and cache counts are
the same on every line of a message, so tokens processed stay exact.

Which agent ran is settled before grading, by refusing (see How runs
execute), not by the guard.

## Statuses

| status | means | pass rate |
|---|---|---|
| `passed` | the check passed and the demo still runs | a pass |
| `failed` | the check said no, or the demo exited | a failure |
| `max_turns` | the transcript ends on a tool call (stop_reason `tool_use`, or null) at the batch's max turns | a failure whatever the dump says |
| `harness_error` | infrastructure, not the agent: a failed setup, a server or solver failure, an unreadable transcript, the guard (offered tools, deferred tools, MCP instruction servers, model, effort, instruction files, attachments, line types, prompts), a run that did not end on its own (a trailing synthetic message, a stop other than `end_turn`, a tool call under the cap), a grading failure, an abort | left out, counted on its own |

A final text reply with a null stop_reason is not a harness error: it ended
on its own (`end_inferred`, above).

Exit codes: 0 ok (dry-run: every run passed); 1 dry-run only, a run did not
pass; 2 a harness error (a stale build, bad arguments, slot misuse, an agent
`next` refused, a graded run that was a harness error, or an `archive` that
found a row's transcript neither archived nor live).

## Regrading

```bash
python3 bench/run.py regrade --batch B
python3 bench/run.py archive --batch B    # copy live transcripts into results/B/transcripts
```

This grades every row of batch B again with the current code. Use it after a
change to the grading logic (the guard, how an ending is read, a task's
check), so that one batch keeps one grading logic for all its rows. It uses
only what persists: each row's recorded `state` (the task's check runs on it
again), its `demo_alive`, and its transcript. The transcript is read from
the batch's archive (`results/B/transcripts/`, see Known limits), or, when
it is not there, found again by agent id under `~/.claude/projects`. It is
read with the plan's effort, max turns and tolerated attachments (none for
a plan from before the option), so a batch planned clean stays held to a
clean batch's guard. A plan that names a type no plan may tolerate (edited
by hand) is refused by `regrade` and `next` alike, before anything is
graded. Nothing runs again and no model is called.

It refuses while any slot of the batch is held. It copies `runs.jsonl` to
`runs.orig-<UTC time>.jsonl` first, never over an existing backup, then
replaces `runs.jsonl` in one atomic step. A regraded row keeps every field.
Its status, verdict, error, guard problems and transcript stats are
recomputed. It gains `status_at_run` (the status recorded at run time, kept
across regrades), `regraded_at`, `regrade_git_sha` and `regrade_git_dirty`.

Dry-run rows stay as they are. A row with no agent id (an abort, a failed
prepare), no state, or a transcript that can no longer be found keeps its
status and stats, and gets a `regrade_note` saying why. The command prints
how many rows changed status, each change as `job: old -> new (reason)`, and
how many transcripts came from the archive and how many live. A second
regrade changes nothing.

## Rows

One JSON object per run, the same keys in both modes, null where none applies.

| key | meaning |
|---|---|
| `ts`, `batch`, `job` | when the row was written (UTC), the batch, the plan's job id |
| `task`, `arm`, `run`, `slot`, `seed` | the task, the arm, the repeat index, the slot it ran in (null in a dry run), the shuffle seed |
| `mode`, `solver` | `dry-run` or `subagent`; `scripted`, `scripted-sabotage` or `agent` |
| `model`, `model_resolved` | the family asked for; the models the transcript shows |
| `effort`, `efforts_resolved`, `max_turns` | the plan's effort; the efforts on the assistant lines; the plan's maxTurns |
| `agent_id`, `session_id` | the subagent's id; the session id its transcript carries |
| `git_sha`, `git_dirty` | the tree when the row was written |
| `claude_version`, `claude_versions` | Claude Code versions in the transcript, joined and as a list |
| `status`, `passed` | see Statuses; `passed` is null for a harness error |
| `check_passed`, `reason` | the check's verdict and why, recorded even when the status ignores it |
| `error`, `guard_problems` | the harness error, if any; the guard's sentences |
| `end_inferred`, `notes` | the run's end was inferred from a final text reply with a null stop_reason; sentences such as that its last message's output tokens are a lower bound |
| `demo_alive`, `demo_exit`, `state` | the demo ran at grading; how it ended: `clean` (it quit when asked, or the agent had quit it, its dump saying `quit: true`), `killed` (it would not quit) or `crashed` (gone before grading without quitting); the graded dump |
| `turns`, `wall_ms` | assistant messages (Claude Code's synthetic ones excluded); first to last transcript timestamp (dry-run: the solver's time) |
| `tool_calls`, `tool_calls_total`, `toolsearch_calls` | calls by name; total without ToolSearch; ToolSearch calls |
| `tokens` | `input`, `cache_creation`, `cache_read`, `output`, summed over messages |
| `output_tokens_lower_bound`, `unclosed_messages` | `tokens.output` is a lower bound because a message never got its closing line; how many messages did not |
| `final_text` | the agent's last text, up to 500 characters |
| `context` | `offered_tools` and `offered_tools_snapshots`, the tools the prompt snapshots listed (their union, and each snapshot's list); `claude_md_files`; `deferred_tools`, `loaded_tools` and `deferred_tools_added` (at the end, loaded by ToolSearch, every one ever added); `mcp_instruction_servers` and `mcp_instruction_servers_added` (at the end, every one ever added); `environment_cwd`; `skill_listing`; `attachment_types`; `tree_formats`: the tree format each offered `read_tree` described, inline or in a deferred-tool record (`["json"]`, `["outline"]`, or `[]` without one or without a recorded description); `cli_prefixes`, the agent's identity line; `refused_attachments`, one entry per type, rule (`unknown`, `flagged` or `shape`) and detail the guard refused, with its count and rendered characters; `tolerated_attachments`, per type the plan tolerates, how many arrived in their known shape and their rendered characters (`{"remote_session_change": {"count": 1, "rendered_chars": 710}}`, count 0 when none came, `{}` in a clean batch); `unknown_line_types`; `prompts`, the user lines that are not tool results, plus each tool-result line with anything beside its results but reminders; `handback_prompts`, those of them that are auto mode's `SubagentHandback` reminder; and `tool_result_reminders`, the `<system-reminder>` blocks in or beside tool results |
| `status_at_run`, `regraded_at`, `regrade_git_sha`, `regrade_git_dirty`, `regrade_note` | added by `regrade` only: see Regrading |

## Summaries

The results of the batches run so far, with their caveats, are in
[RESULTS.md](RESULTS.md).

```bash
python3 bench/summarize.py                          # the newest batch, named on stderr
python3 bench/summarize.py --batch B                # one batch
python3 bench/summarize.py --batch A --batch B      # several, merged (or --batch A,B)
python3 bench/summarize.py --all                    # every batch, merged
python3 bench/summarize.py FILE ...                 # exactly these files
```

- **Not a clean batch**, first of all, when a batch's plan tolerated an
  attachment (`plan --tolerate-attachment`, read from its rows'
  `context.tolerated_attachments`): per such batch "B carries remote
  control's note in every call: not a clean batch" when every run got it,
  else "B tolerated remote control's note; N of M runs got it: not a clean
  batch", then how many of its runs got the note and its size, and, merged
  with clean batches, that an arm from one held against an arm from the
  other counts the note's tokens on one side only. The line repeats under every agent section's heading, and every
  table row built from such runs has `[not clean]` beside the arm, task or
  pair it is about, or `[not clean: X only]` for a comparison where only X's
  side carries the note. The agent context table counts the runs of each arm
  and model that got it, which should be all of them.
- **Run conditions**, when the rows come from more than one batch:
  per batch the git SHA (and dirty flag), claude versions, the agents'
  identity line (`context.cli_prefixes`, `not recorded` for a row graded
  before it existed), models requested and resolved, effort, max turns,
  seed, arms, graded n, harness errors and tolerated attachments (`none` for
  a clean batch), then which of SHA, claude, identity line, models resolved,
  effort, max turns, seed and tolerated attachments differ between the
  batches with agent runs (read from their agent rows).
  A batch of dry runs only has no claude, model, effort or max turns, so it
  is listed but left out of that comparison, and the header names it.
  Merged batches are compared as if they ran together, so read this
  before any cross-batch comparison. An arm with runs in several batches is
  pooled, and the header says so.
- **Agent runs**, the headline: one row per (task, arm, model, effort, max
  turns). Pass rate with a Wilson 95% interval, harness errors (outside n),
  cut-off runs, and medians of tool calls, ToolSearch calls, turns, tokens
  processed (input + both caches), output tokens and wall seconds. The
  output-token median leaves out rows whose count is a lower bound
  (`output_tokens_lower_bound`), and the cell says how many it left out.
  A subagent row graded before that field existed, and not `end_inferred`,
  cannot be checked for a message left unclosed mid-run: it stays in the
  median, the cell counts it as `unchecked`, and a footer (and stderr) warns
  that its output tokens may be low. `run.py regrade` records the field.
- **Arms per model, across all tasks**: per model and arm, the pass rate
  (Wilson 95%) and median [IQR] tokens processed, tool calls and output
  tokens; then each pair of arms per model with the pass-rate difference, a
  two-sided Fisher exact p and baseline/subject ratios of those medians.
  The pairs are outline vs taria and every arm vs tmux, where both ran.
- **Pairs per model**, headed by the pairs present (`taria vs tmux` for a
  two-arm batch, as before): per task and pair, the pass-rate difference
  with a Fisher exact p and baseline/subject ratios of median tokens and
  tool calls; each arm across all tasks; then each pair of models per arm.
- **Dry runs**, **provenance**, **guard** problems and the **agent context**
  per arm and model, so the arms can be seen to match. The context table
  lists every deferred tool and MCP instruction server that ever reached a
  run, as the guard checks, not only those left at the end, and the tree
  formats the runs' `read_tree` descriptions named (`json`, `outline`, none
  for tmux, or `not recorded` for a row graded before `tree_formats`
  existed).

## The outline arm

Run it as its own batch beside a taria and tmux batch at the same commit,
as cycle2b ran beside cycle2a, and summarize the two together. Run it from
its own orchestrating session, one that has run no taria job, and restart
before a taria batch after it (see How runs execute). Use the other batch's
seed, effort and max turns (`results/cycle2a/plan.json`: seed 42, effort
medium, max turns 30); the seed fixes this batch's job order, not the
other's.

```bash
cargo build --workspace     # target/debug/taria-mcp must take --tree-format
python3 bench/run.py dry-run --arms outline                   # 6/6 pass
python3 bench/run.py agents --slots 4                         # adds tui-outline-1..4
python3 bench/run.py plan --batch cycle2b --arms outline --models sonnet,haiku --runs 5 \
    --seed 42 --effort medium --max-turns 30
python3 bench/run.py next --batch cycle2b --slot 1            # and so on, per slot
python3 bench/summarize.py --batch cycle2a --batch cycle2b
```

A session loads new agent files only at startup, so restart the
orchestrating session after `agents`. From the worktree, pass `--dir` and
`--agents-dir` as in How runs execute. The 60 jobs then run like any batch.
At one commit, with remote control off in both sessions, the run conditions
header lists the two batches as agreeing on everything it compares, as it
does for cycle2a and cycle2b. The first outline batch, outline1, ran
against full1 at another commit, with another identity line and with
remote control's note, so its ratios against full1 read as a cross-batch
comparison (RESULTS.md, History).

## Tests

None of them calls a model.

```bash
python3 bench/tasks.py             # the checkers' self-test
python3 bench/test_bench.py        # the harness on live tmux, subagent path rehearsed
python3 bench/test_transcript.py   # the transcript reader and summary
```

## Probes

Two probes ran T3 on both arms and both models. Each recorded 4/4 passed.
The first (`results/probe`) ran before isolation was in place: its rows list
six MCP instruction servers, `taria` among them, and today's guard refuses
all four. The second (`results/probe2`) ran isolated, and every row lists
only `tui`.

Four more ran one job per arm and model before a batch:

- `probe3`: the outline on T3, at aed04c3. 2/2 passed, both with remote
  control's note, as in outline1.
- `probe4`: all three arms on T3, at c81cc4d, from a session in auto mode.
  The guard refused the four runs that were graded, for auto mode's
  `SubagentHandback`, and probe4's taria job j002 also for the outline's
  descriptions (see How runs execute). A fifth was aborted and the sixth never ran.
- `c2probe-a`: taria and tmux on T1, at 3e66fbb. 3 of 4 passed. Haiku on
  tmux typed the title into the list's key bindings and left two tasks
  done, the failure RESULTS.md describes.
- `c2probe-b`: the outline on T1, at 3e66fbb. 2/2 passed.

c2probe-a and c2probe-b passed the guard with no tolerated attachment.
Each finished under a minute before its batch started, from the
orchestrating session that then ran it: c2probe-a before cycle2a,
c2probe-b before cycle2b. Probes show the
pipeline works end to end, not how the arms compare. Batch results are in
RESULTS.md.

## Known limits

- Wall time is indicative only: slots run in parallel on one machine.
- The environment block shows the orchestrator's working directory
  (`~/Projects/taria`) to every arm, not the empty dir the MCP server runs in.
- The agent files hold absolute paths to the launcher and the slots dir of
  the checkout that wrote them. When the bench moves (as when
  `feat/benchmark` merges), rewrite them with `agents`.
- Claude Code deletes transcripts after `cleanupPeriodDays` (default 30),
  and `regrade` needs them. So `next --agent-id` copies each graded run's
  transcript and `.meta.json` into `results/B/transcripts/` (as
  `agent-<id>.jsonl` and `agent-<id>.meta.json`) and grades that copy, and
  `regrade` reads the copy first. For a batch graded before the archive
  existed, `python3 bench/run.py archive --batch B` copies the live
  transcripts while they last. It never replaces an archived copy, names
  each row whose transcript is gone, and exits 2 if there is one.
- Tokens are subscription usage. Rows carry no dollar cost.
