# taria benchmark

Measures taria against tmux screen-scraping on the same agent tasks, run
against `taria-demo`: verified task success, tokens, tool calls, turns, wall
time and cost.

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
  500 ms, as an unchanged tree does in taria-mcp. A test reads `UPDATE_WAIT`
  from `crates/taria-mcp/src/server.rs` and fails if the two caps drift.

Each run starts a fresh demo in its own tmux server and temp dir. The
server's only config turns the status line off, so the demo gets the whole
120x40 terminal in both arms (a test checks the pane size). Success is never
read off the screen or the tree: the demo writes its state to the file
`TARIA_DEMO_STATE` names, and each task in `bench/tasks.py` checks that file
against the seed, every field of every task included, so a run passes only if
the asked-for change happened and nothing else did. The demo process must
also still be running when the run is graded: a crashed demo leaves a dump
that says `quit: false`, so the harness asks the process, and a dead one
fails the run with the reason `demo exited`.

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
python3 bench/summarize.py                            # the newest batch
python3 bench/summarize.py --batch <id>               # one batch by id
python3 bench/summarize.py --all                      # every batch, merged
python3 bench/summarize.py results.jsonl              # exactly these files
python3 bench/tasks.py                                # checker self-test
python3 bench/test_bench.py                           # harness tests, live tmux and fake claude
```

`--dry-run` drives each task with a scripted solver (`bench/solvers.py`), no
model involved, through the same MCP server and config an agent would get.
It proves the harness end to end and costs nothing. `--sabotage` swaps in a
plausible wrong plan per task (the wrong task, an off-by-one read, toggling
by position) so each check is seen to fail. Jobs are shuffled with `--seed`
(recorded), so neither arm always runs first. Each run prints one JSON line
and appends it to `bench/results/<batch>.jsonl` (gitignored), or to
`--results`.

## Outcomes

Every run ends in exactly one `status`:

| status | means | pass rate |
|---|---|---|
| `passed` | the check passed and the demo is still running | a pass |
| `failed` | the check said no, or the demo exited | a failure |
| `timeout` | the agent ran out of wall clock (`--timeout`) | a failure, counted in its own column |
| `budget` | the result subtype is `error_max_budget_usd` | graded on the dump like any run, counted in its own column |
| `harness_error` | infrastructure, not the agent | left out, counted in its own column |

A harness error is a failed setup, a server crash, the init guard, a stale
build, an exception in the solver or the harness, claude exiting without a
result event, a result with `is_error` set other than the budget stop (an
API or auth error, an unknown `--model`), or any permission denial (the agent
was refused a tool the arm should have approved). The reason is recorded in
`error`; the dump's verdict is still recorded in `check_passed` and `reason`
for diagnosis. A timed-out run keeps what it streamed before it was killed:
its usage is summed from the assistant messages (`usage_source: "stream"`),
or is null when none arrived. Its cost is null, since only a result event
carries cost.

Exit codes: `0` every run passed; `1` a run did not pass (failed, timeout, or
a budget run that failed its check); `2` a harness error (including a stale
or missing build, the guard, SIGTERM or SIGHUP). A harness error wins.
SIGTERM, SIGHUP and Ctrl-C tear down the run's demo, tmux server and temp dir
before exiting.

## Provenance

Every record carries `batch` (one id per `run.py` invocation, `--batch` to
name it), `git_sha` and `git_dirty` of the tree, `claude_version` (`claude
--version`, free), the requested `model` and `effort` (null for a dry run),
and `seed`; a spend record adds the `agent_model` system/init reported. `summarize.py` reads only what it is told
to (files, `--batch`, `--all`, or else the newest batch, named on stderr),
prints a provenance table of every batch it read, and warns when agent rows
pool more than one git SHA or claude version, when records come from a dirty
tree, or when the agent reported a model other than the one asked for.

## Spend runs

`--spend` runs one real agent per (task, arm, run) and costs money. It
requires `ANTHROPIC_API_KEY`, because `--bare` reads no other credential;
without it `run.py` exits 2 (a subscription run is not wired yet). Each agent
is, from `<tmp>/work`:

```
claude -p "<prompt>" --bare --setting-sources= \
  --strict-mcp-config --mcp-config <tmp>/mcp.json \
  --tools "" --allowedTools mcp__tui --permission-mode dontAsk \
  --disable-slash-commands --output-format stream-json --verbose \
  --no-session-persistence --max-budget-usd <--budget-usd, default 1> \
  --model <--model, default claude-sonnet-5-5> --effort <--effort, default medium>
```

with a wall-clock limit (`--timeout`, default 600 s), in its own process
group: a timeout sends the group SIGTERM, then SIGKILL 5 s later, so a child
that ignores SIGTERM goes too.

**Isolation.** The child's environment drops `CLAUDECODE` and every
`CLAUDE_*` variable but `CLAUDE_CONFIG_DIR`. A Claude Code session exports
these (`CLAUDECODE=1`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_ENTRYPOINT`,
`CLAUDE_EFFORT`, ...), they mark a child as nested in that session, and some
steer it: `CLAUDE_CODE_EFFORT_LEVEL` overrides `--effort`. The names dropped
(never the values) are recorded in `env_stripped`. `--setting-sources=` loads
no user, project or local settings file, so an `effortLevel: xhigh`, a
`model`, an `outputStyle` or `enabledPlugins` in `~/.claude/settings.json`
does not apply; managed (policy) settings still would. `--effort` is always
passed, so the effort is the flag's, not a default's.

**The init guard.** Before the agent's first turn, the `system/init` event
must show exactly the arm's `mcp__tui__*` tools, no plugins, no skills, one
MCP server (`tui`, `connected`), permission mode `dontAsk`, and the default
output style (or none). Anything else records a harness error with the
reasons, kills the agent at once, and stops the batch with exit 2. The init's
`model`, `tools`, `mcp_servers`, `plugins`, `skills`, `output_style`,
`permissionMode`, `claude_code_version`, `apiKeySource`, `plugin_errors`,
`mcp_server_errors` and `memory_paths` are recorded in `agent_env` on every
spend record.

From the stream it records the `result` subtype, `num_turns`, `duration_ms`,
`duration_api_ms`, `total_cost_usd`, tokens (`input_tokens`,
`output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, plus
`tokens_in` for input and both caches, and `tokens_out`), tool calls by name
and permission denials, then grades the dump.

### What is verified without a paid call, and what is not

Verified free:

- Every flag above exists in `claude --help` (2.1.294); `--effort` takes
  low, medium, high, xhigh or max; `dontAsk` is a `--permission-mode` choice.
- In the 2.1.294 binary, `--setting-sources` with an empty value parses to
  an empty source list (the same path `--restricted` takes), leaving only
  flag and policy settings; `CLAUDE_CODE_EFFORT_LEVEL` overrides effort for
  the session; `error_max_budget_usd` results carry `is_error: true`; init
  reports `permissionMode: "dontAsk"` for that mode and lists `plugins`,
  `skills` and `output_style`.
- The process handling, the guard, the outcome mapping, the env stripping
  and the provenance, against a fake `claude` on PATH
  (`bench/fixtures/fake_claude.py`, driven by `bench/test_bench.py`):
  success, a solved run, timeout with a SIGTERM-ignoring child, no result,
  an extra tool, a plugin, a permission denial, an error result, the budget
  stop, a guard stopping the batch, and `--spend` without a key.

Not verifiable without a paid call:

- That the real init event reports the `tui` server `connected` and its
  tools before the first turn, rather than still pending. If it does not,
  the guard aborts the batch before any model call.
- That `--effort medium` is the effort the model ran at: init does not
  report effort, and Claude Code may lower a level a model does not offer.
- That `--bare` with `--setting-sources=` loads no plugin or skill in
  practice (the guard checks the init's lists, so a failure aborts).
- The real stream's exact usage fields per assistant message, which the
  partial-usage recovery for timeouts relies on.

`summarize.py` prints one markdown row per (task, arm, model, effort): runs,
passed of graded, failed, timeout, budget, harness errors, pass rate with a
Wilson 95% interval over the graded runs, medians and IQRs of input+cache
tokens, output tokens, tool calls, turns and wall time, summed cost, cost per
passed run, and the result subtypes seen. Below each table, per task, a
two-sided Fisher exact test on pass counts between the arms, and a note when
a row has fewer than 5 graded runs, where the IQR and the test are weak. A
cost sum that leaves out runs without a known cost is marked `*` and
explained, and runs whose tokens could not be recovered are counted in a note
rather than silently dropped from the medians. Agent runs make the headline table; dry runs get their own,
labelled, and never feed a headline number. CI never runs the benchmark in
either mode.

## Tasks

| id | prompt | passes when |
|----|--------|-------------|
| T1 | Add a task titled 'Benchmark the bridge'. | exactly one new todo task with that title; seed unchanged |
| T2 | Mark the task 'Record the killer demo' as done. | `task-4` done; nothing else changed |
| T3 | Delete the task 'Wire up the MCP bridge'. | `task-3` gone; the other four seed tasks unchanged |
| T4 | Add a new task whose title is the title of the third task on the Active tab, followed by ' (follow-up)'. | exactly one new todo task 'Record the killer demo (follow-up)'; seed unchanged |
| T5 | Mark every task on the Active tab whose title starts with 'W' as done, then delete the task 'Sketch the protocol wire types'. | `task-1` and `task-3` done, `task-2` gone, `task-4` and `task-5` unchanged (derived from the seed; completing the first match shifts the tab, so working by position marks `task-4`) |
| T6 | Add a task titled 'Check the dialog'. Do not delete anything. | starts inside the delete dialog for `task-1` (opened through tmux before the agent's server starts, for both arms); one new task with that title, every seed task intact |

Every task also requires no open dialog, no leftover draft and a demo
process still running at grading time. Which tab is showing does not count.
