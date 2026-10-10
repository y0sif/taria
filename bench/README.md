# taria benchmark

Measures taria against tmux screen-scraping on the same agent tasks, run
against `taria-demo`: verified task success, tokens, tool calls, turns and
wall time.

Both arms hand the agent an MCP server, registered under the same name
(`tui`), so the only difference is what the tools see:

- **taria**: the real bridge, `target/debug/taria-mcp`, with `read_tree`,
  `act`, `key` and `type_text`.
- **tmux**: `bench/tmux_mcp.py`, with `screen` (the pane as text) and
  `send_keys` (tmux key names or literal text, then up to 500 ms for the
  screen to settle, the same budget taria-mcp waits for an app to react).

Each run starts a fresh demo in its own tmux session on a private tmux
server. Success is never read off the screen or the tree: the demo writes
its state to the file `TARIA_DEMO_STATE` names, and each task in
`bench/tasks.py` checks that file against the seed, so a run passes only if
the asked-for change happened and nothing else did.

## Run it

Build first (`cargo build --workspace`); `run.py` never builds and refuses a
`target/debug` older than the sources. Needs `tmux` and Python 3.

```bash
python3 bench/run.py --dry-run --task T3 --arm taria
python3 bench/run.py --dry-run --task T3 --arm tmux
python3 bench/run.py --dry-run --task T3 --arm tmux --sabotage   # must fail
python3 bench/tasks.py                                            # checker self-test
```

`--dry-run` drives the task with a scripted solver, no model involved,
through the same MCP server and config an agent would get. It proves the
harness end to end and costs nothing. Each run prints one JSON line and
appends it to `bench/results/<timestamp>.jsonl` (gitignored); tokens, cost
and turns are `null` in a dry run, and `wall_ms` is the solver's time once
the server is up.

`--spend` runs a real agent through `claude -p` and costs money. It is not
wired up yet and refuses to run. CI never runs the benchmark in either mode.

## Tasks

| id | prompt | passes when |
|----|--------|-------------|
| T3 | Delete the task 'Wire up the MCP bridge'. | `task-3` is gone, the other four seed tasks are unchanged, no dialog is open, no draft is left, the demo is still running |
