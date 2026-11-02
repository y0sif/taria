# taria and molerat on the same app

A pilot run on 2026-11-01: AI agents doing the same tasks in one ratatui
app, once through taria and once through
[molerat](https://github.com/orhun/molerat), each stack's own MCP bridge.
Both finished every task. taria used 2.2 to 2.5x fewer tokens per run and
about half the tool calls, and the gap comes from the bridges' tool
surfaces, not from the trees.

## Setup

- **App:** `dual-demo`, which is taria-demo (`examples/demo-app`: tabs, a
  task list, a text input, a confirm-delete dialog) publishing a molerat
  tree beside the taria one every frame. Both stacks change one `App`
  through one code path: every molerat action is turned into the taria
  input it stands for and goes through the same `apply_agent_input`, so one
  state dump grades both arms. A molerat action is registered exactly where
  the taria node advertises the action it maps to, so both trees offer the
  same affordances in every state. The node and action mapping is in
  `bench/dual-demo/MAPPING.md`.
- **Tasks:** the six in `bench/tasks.py`, graded from the app's state dump,
  never from what the agent says it did.

  | task | prompt |
  |---|---|
  | T1 | Add a task titled 'Benchmark the bridge'. |
  | T2 | Mark the task 'Record the killer demo' as done. |
  | T3 | Delete the task 'Wire up the MCP bridge'. |
  | T4 | Add a task whose title is the third Active task's title plus a suffix. |
  | T5 | Mark every Active task with a given prefix as done, then delete one task. |
  | T6 | Add a task titled 'Check the dialog'. Do not delete anything. (Starts with the delete dialog open.) |

- **Arms:**
  - `taria`: taria-mcp 0.4.0, the tree as JSON. Tools `read_tree`, `act`,
    `key`, `type_text`.
  - `molerat`: molerat-mcp from orhun/molerat at `0c62c33`, unmodified,
    built with `--features mcp`. Tools `get_ui`, `perform_action`.
- **Agents:** Claude Code 2.1.301 subagents, Haiku 5.5 and Sonnet 5.5,
  effort medium, 30 turns at most. The agent instructions are the same for
  both arms, and each agent saw only its arm's server.
- **Size:** 2 runs per task, arm and model, seed 42: 48 runs.

## Results

Median per run, across all six tasks.

| | taria | molerat | molerat/taria |
|---|---|---|---|
| **Haiku** tasks completed | 12/12 | 12/12 | |
| Haiku tokens processed | 14,570 | 36,557 | 2.51x |
| Haiku tool calls | 3 | 7 | 2.33x |
| Haiku output tokens | 419 | 611 | 1.46x |
| **Sonnet** tasks completed | 11/11 | 12/12 | |
| Sonnet tokens processed | 14,279 | 31,962 | 2.24x |
| Sonnet tool calls | 3 | 5 | 1.67x |
| Sonnet output tokens | 249 | 370 | 1.49x |

Tokens processed is everything the model read, summed over every model
call of a run: fresh input plus cache creation plus cache reads. Each model
call rereads the whole context so far, so a run's tokens grow with its calls
and with what each one carries.

One Sonnet taria run (T5) is left out: it called a misspelled tool name,
`mcp__tui::act`, which Claude Code refused, and the bench counts any tool
outside the arm as a harness error. The app's state shows it finished the
task.

Per task, median tokens processed and tool calls:

| task | model | taria tokens | molerat tokens | taria calls | molerat calls |
|---|---|---|---|---|---|
| T1 | Haiku | 14,286 | 36,956 | 3 | 7 |
| T1 | Sonnet | 14,279 | 35,235 | 3 | 5 |
| T2 | Haiku | 9,656 | 16,501 | 2 | 3 |
| T2 | Sonnet | 9,641 | 21,908 | 2 | 3 |
| T3 | Haiku | 14,200 | 36,373 | 3 | 7 |
| T3 | Sonnet | 14,147 | 28,678 | 3 | 5 |
| T4 | Haiku | 14,996 | 36,557 | 3.5 | 6 |
| T4 | Sonnet | 14,394 | 29,118 | 3 | 5 |
| T5 | Haiku | 38,302 | 74,628 | 10 | 12.5 |
| T5 | Sonnet | 27,293 (1 run) | 62,302 | 6 | 9.5 |
| T6 | Haiku | 21,238 | 49,933 | 5 | 9 |
| T6 | Sonnet | 17,256 | 34,810 | 4 | 6 |

## Where the tokens go

Measured from the 47 graded runs' transcripts.

| | taria | molerat |
|---|---|---|
| first model call's context (tokens, median) | 2,611 | 4,839 |
| tool definitions sent (JSON chars) | 2,143 (4 tools) | 7,871 (2 tools) |
| largest single tool schema (chars) | 297 (`key`) | 7,391 (`perform_action`) |
| server instructions (chars) | 376 | 324 |
| tree read (chars, median) | 1,199 (`read_tree`) | 1,181 (`get_ui`) |
| action result (chars, median) | 1,232 (`act`) | 36 (`perform_action`) |
| calls over all runs | 32 `read_tree` + 57 `act` = 89 | 83 `get_ui` + 73 `perform_action` = 156 |

1. **A fixed cost on every call.** `perform_action`'s input schema
   (`$defs` and `oneOf` over AccessKit's action requests) is 7,391
   characters, about 2,200 tokens more context than taria's on every model
   call.
2. **More calls.** `perform_action` answers "Action dispatched to the
   application" once the action is queued, before the app applies it, so
   the agent reads the effect with another `get_ui`. taria's `act` waits for
   the app's answer and returns the tree when it changed.
3. **Not the tree.** Both serialize the same screen in about 1.2K
   characters.

Both causes sit in molerat-mcp, not in AccessKit: a flat action input
checked on the server, and the new tree in `perform_action`'s answer, would
close most of the gap.

## What differs between the stacks

- taria has raw `key` and `type_text` tools as a fallback for anything the
  tree does not cover. molerat-mcp has none; text goes in through
  `setValue`. The dual demo maps every affordance, so no task needed them.
- taria acks every input: `act` reports whether the app applied it,
  ignored it, or changed nothing. On the molerat side an input the app
  ignores (a stale node id, say) is dropped silently.
- molerat's `get_ui` sends part of the AccessKit node state: label, value,
  description, `selected`, `focused`, `disabled`, and a numeric value with
  its range. Toggled, expanded, set position and table cells stay in the
  AccessKit tree, so the dual demo
  puts a task's done state in its `description` ("Done" / "Not done"),
  where taria sends `checked`. This pilot measures the two bridges, not
  AccessKit against taria's tree.
- molerat speaks AT-SPI through AccessKit (its `atspi` feature), so screen
  readers work. taria has no screen reader support today.

## Caveats

- **A pilot.** 2 runs per cell. Every run completed its task, so this
  separates cost, not success.
- **One harness.** Claude Code resends every tool definition with every
  model call and counts cache reads; another client may weigh a large
  schema differently.
- Both arms' requests also carried Claude Code 2.1.301's deferred `advisor`
  tool. No run called it.

## Run it yourself

Two ways: drive the dual demo by hand from one Claude Code session, or run
the benchmark. Both need molerat-mcp on PATH and the dual demo built:

```bash
cargo install --git https://github.com/orhun/molerat --rev 0c62c33 --features mcp molerat
cargo build --manifest-path bench/dual-demo/Cargo.toml
```

By hand:

```bash
cargo install taria-mcp
bench/dual-demo/target/debug/dual-demo        # in its own terminal
claude mcp add taria -- taria-mcp --app taria-demo
claude mcp add molerat -- molerat-mcp
```

Then give an agent the same task once per server, with the other one
disabled (`/mcp`), and compare.

The benchmark (Python 3, tmux, Claude Code):

```bash
cargo build --workspace
DUAL=$PWD/bench/dual-demo/target/debug/dual-demo
python3 bench/run.py dry-run --arms taria,molerat --demo-bin $DUAL     # scripted, no model: 12/12 pass
python3 bench/run.py agents --slots 4
python3 bench/run.py plan --batch B --arms taria,molerat --models sonnet,haiku --runs 2 \
    --demo-bin $DUAL --seed 42 --effort medium --max-turns 30
```

A Claude Code session then runs the batch as subagents, and
`bench/summarize.py --batch B` prints the tables above. How runs execute,
the isolation the guard checks and the slot loop are in `bench/README.md`.
