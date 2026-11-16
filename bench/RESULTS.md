# Benchmark results

What an AI agent pays to drive a terminal app through taria, against the same
agent scraping the screen through tmux. Claude Code subagents on Haiku 5.5 and
Sonnet 5.5 (effort medium, at most 30 turns) ran each of the six tasks in
[README.md](README.md) 5 times per arm and model on `taria-demo`: 180 runs,
30 per cell, every one passed by the guard.

## Headline

Median per run.

| | taria JSON | taria outline | tmux |
|---|---|---|---|
| **Haiku** pass rate | 30/30 | 30/30 | 29/30 |
| Haiku tokens processed | 14,045 | 12,788 | 26,722 |
| Haiku tool calls | 3 | 3 | 6.5 |
| Haiku wall time, s | 4.7 | 4.3 | 10.6 |
| **Sonnet** pass rate | 30/30 | 30/30 | 30/30 |
| Sonnet tokens processed | 13,963 | 12,675 | 12,890 |
| Sonnet tool calls | 3 | 3 | 4 |
| Sonnet wall time, s | 6.5 | 7.8 | 6.1 |

Tokens processed is everything the model read over a run's model calls:
fresh input plus cache creation plus cache reads.

## Findings

1. **On Haiku, taria costs about half of what tmux does.** tmux used 1.90x
   the tokens of taria JSON and 2.09x those of the outline, with 2.17x the
   tool calls, and took 2.27x JSON's wall time. Haiku on taria matched
   Sonnet on taria.
2. **tmux's one failure is the one taria is built to prevent.** Haiku on T1
   typed the title while the task list had focus, and the characters hit the
   list's key bindings: two tasks were marked done. Every Haiku T1 run on
   tmux started that way, and 4 of 5 noticed and repaired it. On taria,
   typed text goes to the app's typing surface.
3. **On Sonnet, tmux and taria are close.** tmux used 7.7% fewer tokens than
   JSON and 1.7% more than the outline. On T1 and T4 tmux adds a task in 3
   model calls, taria in 4: it sets the value, reads the tree, then
   activates. Wall times show no detectable difference (0.94x,
   Mann-Whitney p=0.41).
4. **The outline cuts about 9% against JSON** (8.9% on Haiku, 9.2% on
   Sonnet), with the same pass rates and tool calls. Its median tool result
   is 548 characters, against 1,134 for JSON and 651 for a tmux screen.
5. **The raw fallbacks were almost unused.** 2 of 453 tool calls on taria
   and the outline were `key` or `type_text`.

## Against molerat

A smaller run compared taria with [molerat](https://github.com/orhun/molerat)'s
`molerat-mcp` on `bench/dual-demo`, one app publishing both trees: 2 runs per
task, arm and model, 48 runs. Both finished every task.

| | taria | molerat |
|---|---|---|
| Haiku tokens processed / tool calls | 14,570 / 3 | 36,557 / 7 |
| Sonnet tokens processed / tool calls | 14,279 / 3 | 31,962 / 5 |

The gap is in the bridges, not the trees: both serialize the screen in about
1,200 characters. molerat-mcp's `perform_action` schema adds about 7,400
characters to every call, and it answers before the app applies the action,
so the agent reads the tree again. One Sonnet taria run is left out: it
called a misspelled tool name, which the guard counts as a harness error.

## Caveats

- 5 runs per cell on one small app, whose tree has 8 to 14 nodes. No
  pass-rate gap is significant (Haiku's 30/30 against 29/30: Fisher p=1.0).
- Tokens processed count a cache read like fresh input, so they are not a
  price; cache reads were 88% to 93% of them.
- Wall time is indicative: runs went in parallel on one machine. The outline
  ran in a batch of its own, since a session keeps one description per tool
  name, so its wall times do not compare directly with the other arms'.
- The main runs predate typed state in the demo, whose first tree is now
  1,199 characters as JSON (was 1,132) and 619 as an outline (was 546).

`bench/summarize.py` prints these tables from a batch's rows;
[README.md](README.md) says how to run one.
