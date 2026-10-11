# Benchmark results

What an AI agent pays to drive a terminal app through taria, against the
same agent scraping the screen through tmux. Two batches, both run on
2026-10-11.

## Setup

- **App:** `taria-demo`, the task manager in `examples/demo-app` (tabs, a
  list, a text input, a confirm-delete dialog). Its tree has 8 to 14
  nodes. Each run gets its own demo and its own tmux server.
- **Tasks:** six, T1 to T6, defined in `bench/tasks.py`. Each is graded
  from the demo's own state dump, never from what the agent says it did.
- **Arms:**
  - `taria`: `taria-mcp`, the tree as the app's JSON.
  - `outline`: the same bridge with `--tree-format outline`, one indented
    line per node.
  - `tmux`: `bench/tmux_mcp.py`, a screen read plus key sends.
- **Agents:** Claude Code subagents on the subscription, Haiku 5.5 and
  Sonnet 5.5, effort medium, 30 turns at most. Claude Code 2.1.294 in both
  batches. The agent instructions are byte-identical across arms.
- **Size:** 5 runs per task, arm and model, seed 42. 180 runs in all.
- **Isolation:** each subagent saw only its arm's server: no other MCP
  server, no CLAUDE.md, no extra tools. The guard in `transcript.py`
  checks every run; 0 of 180 failed it. One extra note reached the
  outline batch anyway; see "The outline correction".

## Headline

Median per run, over every graded run.

| | taria JSON | taria outline | tmux |
|---|---|---|---|
| **Haiku** pass rate | 30/30 | 30/30 | 27/30 |
| Haiku tokens processed | 17,548 | about 16,260 | 24,226 |
| Haiku tool calls | 3 | 3 | 6.5 |
| **Sonnet** pass rate | 30/30 | 30/30 | 30/30 |
| Sonnet tokens processed | 17,388 | about 16,020 | 12,898 |
| Sonnet tool calls | 3 | 3 | 4 |

Tokens processed is input plus cache creation plus cache reads, summed
over every call of a run. Output tokens are left out; see Caveats. The
outline figures are estimates after a correction; raw, they are 17,272
and 17,040.

## Findings

1. **taria gives the same result on a small model.** Haiku on taria
   matched Sonnet: the same pass rate, the same tool calls, and median
   tokens within 1%. Haiku did worse on tmux: 27 of 30 passed, with 1.4x
   the tokens of taria over every graded run (1.14x over passed runs
   alone) and 2.2x the tool calls.

2. **The tmux failures are the failure taria is built to prevent.** All
   three were Haiku on T1, adding a task by typing its title. The typed
   characters reached the app's key bindings and marked other tasks done.
   Two of those three agents reported success; the state check caught
   them. On taria, typing goes to the app's typing surface, never through
   the bindings.

3. **On Sonnet, tmux used fewer tokens**, about 26% fewer than taria JSON.
   Three things add up to the gap, largest first: taria's tool
   definitions, resent on every call; the larger JSON results; and tmux
   finishing T1 and T4 in 3 calls instead of 4.

4. **Where the tokens go.** Every call resends the whole context, so a
   run's cost is mostly what sits in that context, times the number of
   calls.

   | | taria JSON | taria outline | tmux |
   |---|---|---|---|
   | Context at the first call, tokens | 3,160 | about 3,190 | 2,353 |
   | Tool definitions, chars | 3,682 | 3,756 | 1,441 |
   | Server instructions, chars (join at the second call) | 1,106 | 1,234 | 1,346 |
   | Growth per call, tokens | about 650 | about 370 | about 490 |
   | Tool result size, chars | 1,134 | 527 | 651 |

   The first-call gap of about 800 tokens is the tool definitions: taria
   has four, with long descriptions, and tmux two. The growth row is
   measured per call, after the step where the server instructions join;
   most of it is the tool result. The outline cut each
   result to below a tmux screen, but on runs of about 4 calls the fixed
   context weighs more.

5. **The outline used about 7 to 8% fewer tokens than JSON** (7.4% on
   Haiku, 7.9% on Sonnet), with the same pass rates and tool calls. That
   figure rests on the correction; raw, the saving is 1.6% and 2.0%. Not
   every task gained: on Haiku, T4 and T6 used more. A clean batch will
   settle the size. On a larger app, with more nodes per tree, the
   per-result saving would weigh more.

6. **The outline drew more fallback calls.** In JSON, taria agents used
   `key` or `type_text` in 11 of 223 calls; in outline, 27 of 226. Of
   the 16 extra, 10 are Sonnet on T1 and T4: in every one of those 10
   runs it submitted the new task with `key enter`, right after a tree
   showing `input ... [set_value activate dismiss]`. In JSON, all 10 used
   `act activate`. Both paths worked, but the project's rule is that the
   raw fallbacks never become the main path.

## What to try next

- **Shorter tool descriptions.** The tool definitions are the largest
  cost difference measured: `act`, `key` and `type_text` run 1,000 to
  1,300 characters each. Untested so far.
- **Make the outline's actions read as callable**, so agents use `act`
  instead of `key`, then measure again.
- **Re-measure on a bigger app.** The tree's share of the cost should
  grow with the app.

## Caveats

- **Small samples.** n=5 per cell. The Haiku pass-rate gap, 30/30 against
  27/30, gives Fisher p=0.237, so it is directional, not significant. On
  T1 alone, 5/5 against 2/5, p=0.167.
- **One small app.** Results may not carry to larger apps.
- **Two batches.** The outline ran hours after the other two arms, at a
  different commit (see Data and provenance). Between the two commits the
  demo, the task prompts and the JSON-mode tool texts did not change, and
  the Claude Code version, models, effort, turn limit, seed and agent
  instructions were the same.
- **Output tokens are incomplete.** Claude Code sometimes never writes an
  assistant message's closing line, leaving its output count at a
  placeholder of 1 to 26 tokens. 21 runs have at least one. The summary
  leaves them out of the output-token medians. Input and cache counts are
  unaffected.

## The outline correction

Remote control was on in the session that ran the outline batch, and
Claude Code added a `remote_session_change` note (commit and PR
attribution text) to every outline subagent. The first batch had none.
The guard did not catch it: it checks tools, servers, instructions and
CLAUDE.md, not other attachments.

The note sits in the context of every call. At the first call, every
outline run carried 281 (Haiku) or 282 (Sonnet) more tokens than the
taria JSON runs of the same task. Two things differ at that point: the
note, and the outline's tool definitions, which are 74 characters longer
and so a real cost of the outline. At the 2.77 characters per token
measured between the taria and tmux definitions, those 74 characters are
about 27 tokens. The estimates above subtract the rest, 254 tokens
(Haiku) or 255 (Sonnet), from every call of every outline run.

Run future batches with remote control off. A clean outline batch would
replace these estimates with measurements.

## Data and provenance

| batch | arms | runs | git SHA | tag |
|---|---|---|---|---|
| full1 | taria, tmux | 120 | eb0788a | `bench-full1` |
| outline1 | outline | 60 | aed04c3 | `bench-outline1` |

The tags are local. Both batches were graded, or regraded, at aed04c3.

- Raw rows, plans and archived transcripts: `bench/results/full1/` and
  `bench/results/outline1/`, on the machine that ran them only, in
  `~/Projects/taria` (the archive) and in the `~/Projects/taria-bench`
  worktree. They are not in git.
- The full tables, from a checkout that has `bench/` (until the branch
  merges, the worktree):
  `python3 bench/summarize.py --batch full1 --batch outline1`.
- The cost breakdown and the correction: `bench/results/analysis/` in the
  same place (`prefix.py`, `adjust.py`, `toolsize.py`, and the summary as
  it ran). `adjust.py`, run from `bench/results`, prints the estimates
  above.
