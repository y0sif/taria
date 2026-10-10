# taria benchmark

Measures taria against tmux screen-scraping on the same agent tasks, run
against `taria-demo`: verified task success, tokens, tool calls, turns and
wall time.

Both arms hand the agent an MCP server, registered under the same name
(`tui`), so the only difference is what the tools see:

- **taria**: the real bridge, `target/debug/taria-mcp`, with `read_tree`,
  `act`, `key` and `type_text`.
- **tmux**: `bench/tmux_mcp.py`, with `screen` (the pane as text) and
  `send_keys` (tmux key names or literal text). Each key name is its own
  `tmux send-keys`, 30 ms apart, so `"Escape d"` is Escape then d; sent in
  one command the two bytes arrive together and the app reads alt+d. After
  sending, it waits for the screen to differ from the one before the keys,
  then for two captures 50 ms apart to agree, within the 500 ms taria-mcp
  waits for an app to react. A key that changes nothing costs the full
  500 ms, as an unchanged tree does in taria-mcp.

Each run starts a fresh demo in its own tmux server and temp dir. Success is
never read off the screen or the tree: the demo writes its state to the file
`TARIA_DEMO_STATE` names, and each task in `bench/tasks.py` checks that file
against the seed, every field of every task included, so a run passes only if
the asked-for change happened and nothing else did.

## What the agent sees

The agent runs in an empty working directory, `<tmp>/work`. The state dump,
the MCP config and both sockets live outside it, and `<tmp>` has a neutral
`tui-` prefix: the demo prints its socket path (`<tmp>/s/app.sock`) on its
header line, which the tmux arm reads, so nothing on screen names the
benchmark. Prompts are identical across arms and never mention taria, tmux,
node ids or key bindings.

## The baseline's honest limits

`screen` returns text without colours or styles, which is what a
screen-scraping agent gets, and some of the demo's state is drawn only as
style:

- The active tab is bold and the other dim (`ui.rs`, `render_tabs`); the
  text reads ` Active │ Done ` either way. On a non-empty tab the
  checkboxes give it away (a tab holds only `[ ]` or only `[x]` rows), but
  on an empty tab, `(no tasks here)`, the tmux arm cannot tell which tab it
  is on.
- Which region holds the keyboard is drawn as a bold border on the input box
  or a bold cursor row (`render_input`, `render_tasks`). The footer's hint
  line changes with focus, so the text does say it, less directly.

These are the baseline's real handicaps, not harness bugs: they are why the
tasks are graded on the state dump rather than on what either arm saw.

## Run it

Build first (`cargo build --workspace`); `run.py` never builds and refuses a
`target/debug` older than the sources. Needs `tmux` and Python 3.

```bash
python3 bench/run.py --dry-run                        # every task, both arms
python3 bench/run.py --dry-run --tasks T3,T5 --arms tmux --runs 3
python3 bench/run.py --dry-run --sabotage             # every run must fail
python3 bench/summarize.py                            # tables over bench/results/
python3 bench/tasks.py                                # checker self-test
python3 bench/test_bench.py                           # harness tests, live tmux ones included
```

`--dry-run` drives each task with a scripted solver (`bench/solvers.py`), no
model involved, through the same MCP server and config an agent would get.
It proves the harness end to end and costs nothing. `--sabotage` swaps in a
plausible wrong plan per task (the wrong task, an off-by-one read, toggling
by position) so each check is seen to fail. Jobs are shuffled with `--seed`
(recorded), so neither arm always runs first. Each run prints one JSON line
and appends it to `bench/results/<timestamp>.jsonl` (gitignored).

Exit codes: `0` every run passed, `1` a task failed, `2` a harness error (a
stale or missing build, a server crash, a timeout, a failed setup, SIGTERM or
SIGHUP). A harness error wins over a failure. SIGTERM, SIGHUP and Ctrl-C tear
down the run's demo, tmux server and temp dir before exiting.

## Spend runs

`--spend` runs one real agent per (task, arm, run) and costs money. It
requires `ANTHROPIC_API_KEY`, because `--bare` reads no other credential;
without it `run.py` exits 2 (a subscription run is not wired yet). Each agent
is, from `<tmp>/work`:

```
claude -p "<prompt>" --bare --strict-mcp-config --mcp-config <tmp>/mcp.json \
  --tools "" --allowedTools mcp__tui --disable-slash-commands \
  --output-format stream-json --verbose --no-session-persistence \
  --max-budget-usd <--budget-usd, default 1> --model <--model, default claude-sonnet-5-5>
```

with a wall-clock limit (`--timeout`, default 600 s). Before the agent's
first turn, the `system/init` event's tool list must be exactly the arm's
`mcp__tui__*` tools, or the whole batch stops with exit 2. From the stream it
records the `result` subtype, `num_turns`, `duration_ms`, `duration_api_ms`,
`total_cost_usd`, `usage` (input, output, cache read, cache creation; their
sum is `tokens`) and tool calls by name, then grades the state dump like a
dry run. A run is graded on the dump alone, whatever the result subtype
(which is recorded); a timed-out run is a harness error and stays out of the
pass rate.

`summarize.py` prints one markdown row per task and arm: runs, harness
errors, pass rate with a Wilson 95% interval, median and IQR of tokens, tool
calls, turns and wall time, and summed cost. Agent runs make the headline
table; dry runs get their own, labelled, and never feed a headline number.
CI never runs the benchmark in either mode.

## Tasks

| id | prompt | passes when |
|----|--------|-------------|
| T1 | Add a task titled 'Benchmark the bridge'. | exactly one new todo task with that title; seed unchanged |
| T2 | Mark the task 'Record the killer demo' as done. | `task-4` done; nothing else changed |
| T3 | Delete the task 'Wire up the MCP bridge'. | `task-3` gone; the other four seed tasks unchanged |
| T4 | Add a new task whose title is the title of the third task on the Active tab, followed by ' (follow-up)'. | exactly one new todo task 'Record the killer demo (follow-up)'; seed unchanged |
| T5 | Mark every task on the Active tab whose title starts with 'W' as done, then delete the task 'Sketch the protocol wire types'. | `task-1` and `task-3` done, `task-2` gone, `task-4` and `task-5` unchanged (derived from the seed; completing the first match shifts the tab, so working by position marks `task-4`) |
| T6 | Add a task titled 'Check the dialog'. Do not delete anything. | starts inside the delete dialog for `task-1` (opened through tmux before the agent's server starts, for both arms); one new task with that title, every seed task intact |

Every task also requires no open dialog, no leftover draft and a running
demo. Which tab is showing does not count.
