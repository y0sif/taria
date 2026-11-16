# taria benchmark

The benchmark measures how well an AI agent drives a terminal app through
taria's semantic tree, against the same agent scraping the screen. Every arm
gets the same app, tasks, prompts and agent instructions; only the tools
differ. Results are in [RESULTS.md](RESULTS.md).

## Arms

Each arm is an MCP server registered as `tui`, so the agent sees
`mcp__tui__<tool>` whichever it gets and no tool name gives the arm away.

- **taria**: `taria-mcp --app taria-demo`, with `read_tree`, `act`, `key` and
  `type_text`. The tree comes as JSON.
- **outline**: the same bridge with `--tree-format outline`, one indented
  line per node ([docs/cli.md](../docs/cli.md), The outline).
- **tmux**: `bench/tmux_mcp.py`, with `screen` (the pane as plain text, no
  colors or styles) and `send_keys`.
- **molerat**: [molerat](https://github.com/orhun/molerat)'s `molerat-mcp`
  (`get_ui`, `perform_action`) on `bench/dual-demo`, taria-demo publishing
  both trees through one code path ([MAPPING.md](dual-demo/MAPPING.md)).

## Tasks and runs

The demo starts with `task-1` to `task-5`; `task-2` and `task-5` are done.
Prompts name no tool, node id or key binding.

| id | prompt | passes when |
|----|--------|-------------|
| T1 | Add a task titled 'Benchmark the bridge'. | one new todo task with that title; seed unchanged |
| T2 | Mark the task 'Record the killer demo' as done. | `task-4` done; nothing else changed |
| T3 | Delete the task 'Wire up the MCP bridge'. | `task-3` gone; the other four unchanged |
| T4 | Add a new task whose title is the title of the third task on the Active tab, followed by ' (follow-up)'. | one new todo task 'Record the killer demo (follow-up)'; seed unchanged |
| T5 | Mark every task on the Active tab whose title starts with 'W' as done, then delete the task 'Sketch the protocol wire types'. | `task-1` and `task-3` done, `task-2` gone, the rest unchanged |
| T6 | Add a task titled 'Check the dialog'. Do not delete anything. | starts inside the delete dialog for `task-1`; one new task, every seed task intact |

A run is one task on one arm with one model: a Claude Code subagent with a
fresh demo in its own tmux server. It is graded on the demo's state dump
(`bench/tasks.py`), never on what the agent says, and ends `passed`,
`failed`, `max_turns` (a failure) or `harness_error` (outside the pass rate).
The guard (`bench/transcript.py`) makes it a harness error when the agent saw
anything beyond its own arm, such as another server's instructions.

## Running it

You need Python 3 (stdlib only), tmux, a Rust toolchain and Claude Code. An
interactive Claude Code session spawns the agents with its Agent tool, on
your subscription; Python never calls a model. A dry run drives every task
through every arm's server with a scripted solver instead:

```bash
cargo build --workspace                       # run.py never builds, and refuses a stale build
python3 bench/run.py dry-run                  # every run passes
python3 bench/run.py dry-run --sabotage       # every check fails, exit 1 (expected)
```

A batch with agents runs in slots, up to K at once:

```bash
python3 bench/run.py agents --slots 4         # writes .claude/agents/tui-<arm>-<K>.md
python3 bench/run.py plan --batch B --models sonnet,haiku --runs 5
python3 bench/run.py next --batch B --slot 1  # prints a job: agent, model, prompt
python3 bench/run.py next --batch B --slot 1 --agent-id <id>   # grades it, prints the next
```

The session spawns each job as `Agent(subagent_type=<agent>, model=<model>,
prompt=<prompt>)` and passes the agent id back, until `next` prints
`{"done": true}`. `status`, `abort`, `regrade` and `archive` also take
`--batch B`; `--help` lists every option.

Keep the orchestrating session clean, or the guard refuses every run:

- Start it from the repo root after `agents` (a session loads agent files
  only at startup), and do not `cd` or `/add-dir` during a batch.
- Give it no MCP server but the agents' own: in `.claude/settings.local.json`,
  allow `mcp__tui`, set `"disableClaudeAiConnectors": true`, and disable
  every other server and every plugin that brings one. Try `plan --tasks T3`
  first.
- Use bypass or default permission mode, not auto, with `/remote-control`
  off (or `plan --tolerate-attachment remote_session_change`, which marks
  the batch not clean).
- taria and outline share tool names, and a session keeps the first
  descriptions it sees: run them in separate batches from separate sessions,
  restarting between them. `plan` and `next` refuse to mix them.

The molerat arm needs `molerat-mcp` on PATH, Claude Code's PATH included,
and `--demo-bin` on every command that runs it:

```bash
cargo install --git https://github.com/orhun/molerat --rev 0c62c33 --features mcp molerat
cargo build --manifest-path bench/dual-demo/Cargo.toml
DUAL=$PWD/bench/dual-demo/target/debug/dual-demo
python3 bench/run.py dry-run --arms taria,molerat --demo-bin $DUAL
python3 bench/run.py plan --batch B --arms taria,molerat --models sonnet,haiku --runs 2 --demo-bin $DUAL
```

## Output and summaries

Each batch writes to `bench/results/<batch>/`, which git ignores: the plan,
`runs.jsonl` with one JSON row per run, and every graded transcript.
`summarize.py` turns rows into Markdown tables: pass rates with Wilson 95%
intervals, medians of tool calls, turns, tokens and wall time, and arm pairs
with a Fisher exact p.

```bash
python3 bench/summarize.py                        # the newest batch
python3 bench/summarize.py --batch A --batch B    # several, merged
python3 bench/summarize.py --all                  # every batch
python3 bench/summarize.py FILE ...               # exactly these runs.jsonl files
```

## Tests

None calls a model. `test_bench.py` needs tmux and a fresh `cargo build`.

```bash
python3 bench/tasks.py             # the checkers' self-test
python3 bench/test_bench.py        # the harness, on live tmux
python3 bench/test_transcript.py   # the transcript reader, guard and summary
```
