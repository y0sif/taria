# taria benchmark

Does an agent drive a terminal app better through taria's semantic tree than
by scraping the screen? This benchmark answers that on `taria-demo`. Both arms
get the same tasks, prompts and agent instructions; only the tools differ.
Runs are graded on what the app holds when the agent stops, never on what
either arm saw. Each run records verified success, turns, tool calls, tokens
and wall time. The models are Sonnet 5.5 and Haiku 5.5.

## The two arms

Each arm is an MCP server (Model Context Protocol, the standard way to hand an
agent a set of tools). Both are registered as `tui`, so the agent sees
`mcp__tui__<tool>` either way and no tool name gives the arm away.

- **taria**: the real bridge, `target/debug/taria-mcp --app taria-demo`, with
  `read_tree`, `act`, `key` and `type_text`.
- **tmux**: `bench/tmux_mcp.py`, with `screen` (the pane as plain text) and
  `send_keys` (tmux key names, or text with `literal: true`). Key names go
  30 ms apart, so `"Escape d"` is not alt+d, and the settle wait is capped at
  the 500 ms taria-mcp waits for an app to react.

`screen` has no colors or styles, as a screen scraper gets. The demo marks the
active tab only as bold, so on an empty tab the tmux arm cannot tell which tab
it is on. That is a real handicap of scraping, not a harness bug.

## Tasks

The seed: `task-1` 'Write the taria README', `task-3` 'Wire up the MCP
bridge' and `task-4` 'Record the killer demo' are todo; `task-2` 'Sketch the
protocol wire types' and `task-5` 'Publish a snapshot per frame' are done.
Prompts name neither arm's tools, node ids or key bindings.

| id | prompt | passes when |
|----|--------|-------------|
| T1 | Add a task titled 'Benchmark the bridge'. | exactly one new todo task with that title; seed unchanged |
| T2 | Mark the task 'Record the killer demo' as done. | `task-4` done; nothing else changed |
| T3 | Delete the task 'Wire up the MCP bridge'. | `task-3` gone; the other four seed tasks unchanged |
| T4 | Add a new task whose title is the title of the third task on the Active tab, followed by ' (follow-up)'. | exactly one new todo task 'Record the killer demo (follow-up)'; seed unchanged |
| T5 | Mark every task on the Active tab whose title starts with 'W' as done, then delete the task 'Sketch the protocol wire types'. | `task-1` and `task-3` done, `task-2` gone, `task-4` and `task-5` unchanged; working by position marks `task-4`, since completing the first match shifts the tab |
| T6 | Add a task titled 'Check the dialog'. Do not delete anything. | starts inside the delete dialog for `task-1`, opened through tmux before the agent's server starts (both arms); one new task with that title, every seed task intact |

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
is one run in flight: a demo `run.py` prepared, plus one agent file per arm
wired to it. With K slots, up to K runs go at once.

Build, write the agent files and set up isolation (below), then start the
orchestrating session in `~/Projects/taria`; a session loads a new
`.claude/agents` directory only at startup. From it, plan a batch and start
each slot. Needs `tmux` and Python 3.

```bash
cargo build --workspace     # run.py never builds, and refuses a stale build
python3 bench/run.py agents --slots 4 --dir ~/Projects/taria/.claude/agents
python3 bench/run.py plan --batch B --models sonnet,haiku --runs 5
python3 bench/run.py next --batch B --slot 1
```

`plan` writes the shuffled jobs (task x arm x model x run) and the seed to
`results/B/plan.json`, and never replaces one. `next` claims the slot's next
job, starts its demo and prints one line:

```json
{"job": "j001", "slot": 1, "agent": "tui-taria-1", "model": "haiku", "prompt": "Delete the task 'Wire up the MCP bridge'."}
```

The orchestrator spawns exactly that, with the prompt verbatim:
`Agent(subagent_type="tui-taria-1", model="haiku", prompt="...")`. When the
agent returns, hand its id back:

```bash
python3 bench/run.py next --batch B --slot 1 --agent-id <id>
```

This finds the transcript under `~/.claude/projects`, runs the guard, grades
the demo, appends one row to `results/B/runs.jsonl`, tears the demo down and
prints the slot's next job, or `{"done": true}`. Run each slot's loop side by
side. When `next` refuses, it prints `{"error": ..., "slot": K}` and exits 2.
A harness error stops the slot: the row is recorded, nothing is claimed. Fix
the setup, then `next --batch B --slot K` resumes.

`status` shows progress and `abort` records a harness error and frees a slot.
The free check, `dry-run`, drives every task on both arms with a scripted
solver (`bench/solvers.py`) through the same server an agent gets, no model.
`--sabotage` swaps in a plausible wrong plan per task, so each check fails.

```bash
python3 bench/run.py status --batch B                  # recorded, held, pending
python3 bench/run.py abort --batch B --slot K --reason TEXT
python3 bench/run.py dry-run                           # 12/12 pass
python3 bench/run.py dry-run --sabotage                # 12/12 fail
```

## The subagent definitions

`agents --slots K --dir D` writes `tui-<arm>-<K>.md` for both arms and slots 1
to K. It refuses to overwrite a file it did not write; `--remove` deletes only
its own. The frontmatter:

- `tools: mcp__tui`: every tool of the agent's own server, nothing else.
- `model: sonnet`, a default the Agent call's `model` overrides per job.
- `effort` and `maxTurns`, default medium and 30; pass the same `--effort`
  and `--max-turns` to `agents` and `plan`.
- `omitClaudeMd: true`, and one inline stdio server, `tui`, in `mcpServers`.

The arms' files differ only in name, description and the launcher's `--arm`
and `--slot`. The body is the system prompt, the same bytes in both. Its
sha256 goes into `plan.json` as `agent_body_sha256`, and before each claim
`next` checks the slot's agent files against the plan, body included:

> You are operating a terminal application on the user's behalf. The
> application is already running, and the tools you have are your only way to
> see it and control it. Do what the user asks. When you are done, reply with
> one short sentence saying what you did.

The inline server is `bench/mcp_launch.py`. It reads the slot file `next`
wrote (`results/slots/<K>.json`) and execs the arm's real server against that
slot's demo. An exec, not a proxy: the MCP stream passes straight through. A
missing slot file, a slot not ready, or a slot holding the other arm's job
exits 1, so the agent gets no tools rather than the wrong arm's.

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
The connector switch is also the env var `ENABLE_CLAUDEAI_MCP_SERVERS=false`.
Changes to plugins and MCP servers need a session restart; only the connector
switch applied live. Then check a probe run's row:
`context.mcp_instruction_servers` must be exactly `["tui"]`. The guard does
not check this, so read it before a real batch.

What every run still receives, identical across arms: the environment block
(working directory), the date, the model identity and the session context.

What each row proves, from the transcript: `model_resolved` (the models its
turns ran on), `efforts_resolved` (the efforts Claude Code recorded on every
assistant line), and the guard. A run is a harness error if it called a tool
outside its arm (ToolSearch, which loads the arm's deferred tools, is
allowed), was given a CLAUDE.md or memory file, was offered a deferred tool
beyond its arm's (this catches the other arm's), ran on a model outside
`claude-<family>-*`, never took a turn, or was spawned as another slot's
agent (the transcript's `.meta.json` names the type).

## Statuses

| status | means | pass rate |
|---|---|---|
| `passed` | the check passed and the demo still runs | a pass |
| `failed` | the check said no, or the demo exited | a failure |
| `max_turns` | the transcript ends on a tool call at the batch's max turns | a failure whatever the dump says |
| `harness_error` | infrastructure, not the agent: a failed setup, a server or solver failure, no transcript, the guard, a grading failure, an abort | left out, counted on its own |

Exit codes: 0 ok (dry-run: every run passed); 1 dry-run only, a run did not
pass; 2 a harness error (a stale build, bad arguments, slot misuse, or a
graded run that was a harness error).

## Rows

One JSON object per run, the same keys in both modes, null where none applies.

| key | meaning |
|---|---|
| `ts`, `batch`, `job` | when the row was written (UTC), the batch, the plan's job id |
| `task`, `arm`, `run`, `seed` | the task, the arm, the repeat index, the shuffle seed |
| `mode`, `solver` | `dry-run` or `subagent`; `scripted`, `scripted-sabotage` or `agent` |
| `model`, `model_resolved` | the family asked for; the models the transcript shows |
| `effort`, `efforts_resolved`, `max_turns` | the plan's effort; the efforts on the assistant lines; the plan's maxTurns |
| `agent_id`, `session_id` | the subagent's id; the session id its transcript carries |
| `git_sha`, `git_dirty` | the tree when the row was written |
| `claude_version`, `claude_versions` | Claude Code versions in the transcript, joined and as a list |
| `status`, `passed` | see Statuses; `passed` is null for a harness error |
| `check_passed`, `reason` | the check's verdict and why, recorded even when the status ignores it |
| `error`, `guard_problems` | the harness error, if any; the guard's sentences |
| `demo_alive`, `demo_exit`, `state` | the demo ran at grading; `clean`, `killed` or `crashed` when quit; the graded dump |
| `turns`, `wall_ms` | assistant messages (Claude Code's synthetic ones excluded); first to last transcript timestamp (dry-run: the solver's time) |
| `tool_calls`, `tool_calls_total`, `toolsearch_calls` | calls by name; total without ToolSearch; ToolSearch calls |
| `tokens` | `input`, `cache_creation`, `cache_read`, `output`, summed over messages |
| `final_text` | the agent's last text, up to 500 characters |
| `context` | CLAUDE.md files, deferred and loaded tools, MCP instruction servers, environment cwd, skill listing, attachment types |

## Summaries

```bash
python3 bench/summarize.py                 # the newest batch, named on stderr
python3 bench/summarize.py --batch B       # one batch (repeatable)
python3 bench/summarize.py --all           # every batch, merged
python3 bench/summarize.py FILE ...        # exactly these files
```

- **Agent runs**, the headline: one row per (task, arm, model, effort, max
  turns). Pass rate with a Wilson 95% interval, harness errors (outside n),
  cut-off runs, and medians of tool calls, ToolSearch calls, turns, tokens
  processed (input + both caches), output tokens and wall seconds.
- **taria vs tmux, per model**: per task, the pass-rate difference with a
  two-sided Fisher exact p and tmux/taria ratios of median tokens and tool
  calls; each arm across all tasks; each pair of models per arm.
- **Dry runs**, **provenance**, **guard** problems and the **agent context**
  per arm and model, so the arms can be seen to match.

```bash
python3 bench/tasks.py             # tests, none calling a model: the checkers
python3 bench/test_bench.py        # the harness on live tmux, subagent path rehearsed
python3 bench/test_transcript.py   # the transcript reader and summary
```

## First runs

The probe, T3 on both arms and both models, passed 4/4. It ran before
isolation was in place (its rows list six MCP instruction servers, `taria`
among them), so it shows the pipeline works, not a fair comparison.

A leak-check run of T1 on tmux with Haiku, isolated to `["tui"]`, failed. The
agent typed the title through `send_keys` while the list held the keyboard.
The characters hit the app's bindings and marked two tasks done. This is the
failure mode taria's Text input exists to avoid: `type_text` goes to the app's
typing surface, never its bindings. That is one run, not a benchmark result.

## Known limits

- Wall time is indicative only: slots run in parallel on one machine.
- The environment block shows the orchestrator's working directory
  (`~/Projects/taria`) to both arms, not the empty dir the MCP server runs in.
- The agent files' launcher and slot paths point at this worktree until
  `feat/benchmark` merges. Rewrite them with `agents` after the merge.
- Tokens are subscription usage. Rows carry no dollar cost.
