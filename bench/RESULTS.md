# Benchmark results

What an AI agent pays to drive a terminal app through taria, against the
same agent scraping the screen through tmux. This file leads with cycle 2:
two batches run on 2026-10-12 at one commit, all three arms' totals
measured under the same conditions, none estimated. The first round, run on 2026-10-11, is
under History.

## Setup

- **App:** `taria-demo`, the task manager in `examples/demo-app` (tabs, a
  list, a text input, a confirm-delete dialog). Its tree has 8 to 14
  nodes. Each run gets its own demo and its own tmux server.
- **Tasks:** six, T1 to T6, defined in `bench/tasks.py`. Each is graded
  from the demo's own state dump, never from what the agent says it did.
- **Arms:**
  - `taria`: `taria-mcp`, the tree as the app's JSON.
  - `outline`: the same bridge with `--tree-format outline`, one indented
    line per node, with the node's actions at the end as `act: a|b|c`.
  - `tmux`: `bench/tmux_mcp.py`, a screen read plus key sends.
- **Agents:** Claude Code subagents on the subscription, Haiku 5.5 and
  Sonnet 5.5, effort medium, 30 turns at most, Claude Code 2.1.294. The
  agent instructions are byte-identical across arms and batches.
- **Size:** 5 runs per task, arm and model, seed 42. 180 runs: batch
  cycle2a ran taria and tmux (120 runs), cycle2b the outline (60 runs).
- **The first round,** for before and after: batch full1 ran taria and tmux
  (120 runs) and batch outline1 the outline (60 runs), both on 2026-10-11 at
  earlier commits. See History. Job ids repeat across batches (the same
  seed gives the same job order), so each one below is named with its
  batch.
- **One commit, two sessions.** Both batches ran at commit 3e66fbb, with
  the same Claude Code version, models, effort, turn cap, seed and identity
  line (the first line of the agent's system prompt, which Claude Code
  chooses). They ran from two orchestrating sessions on purpose: a Claude
  Code session keeps one description per tool name, and taria and the
  outline serve the same tool names with different descriptions.
- **Clean:** each subagent saw only its arm's server. Claude Code adds
  context to a subagent's prompt as attachments, each of a named type (the
  environment, the date, the model, the server instructions and the like).
  The guard in `bench/transcript.py` reads every run's transcript and
  refuses context the arms do not share. It passed all 180 runs, and every
  run got the same eight attachment types. None needed a tolerated
  attachment, the one kind a batch's plan may let through when every arm
  gets it. Remote control (Claude Code's `/remote-control`, which adds an
  attribution note to every subagent) was off. The orchestrating session
  ran in bypass permission mode, not auto mode, which hands subagents an
  extra tool. That is what makes a batch clean.

## Headline

Median per run, over every graded run, 30 per cell.

| | taria JSON | taria outline | tmux |
|---|---|---|---|
| **Haiku** pass rate | 30/30 | 30/30 | 29/30 |
| Haiku tokens processed | 14,045 | 12,788 | 26,722 |
| Haiku tool calls | 3 | 3 | 6.5 |
| **Sonnet** pass rate | 30/30 | 30/30 | 30/30 |
| Sonnet tokens processed | 13,963 | 12,675 | 12,890 |
| Sonnet tool calls | 3 | 3 | 4 |

Tokens processed is everything the model read, summed over every model
call of a run: fresh input plus cache creation plus cache reads (prompt
text written to, or read back from, the model's prompt cache). A model
call is one assistant turn; it can carry several tool calls. Each model
call rereads the whole context so far, so a run's tokens grow with its
calls and with what each one carries. Output tokens are left out; see
Caveats.

## Findings

1. **On Haiku, taria costs about half of what tmux does.** tmux used 1.90x
   the tokens of taria JSON and 2.09x those of the outline, with 2.17x the
   tool calls. Over passed runs alone the token ratios are the same. Haiku
   on taria matched Sonnet on taria: the same pass rate, the same tool
   calls, median tokens within 1%. On tmux, Haiku used 2.07x Sonnet's
   tokens.

2. **tmux's one failure is the failure taria is built to prevent.** Haiku
   on T1 (cycle2a j080) sent the title 'Benchmark the bridge' as literal
   keys while the list had focus. The characters met the list's key
   bindings. The `k` in 'Benchmark' moves the selection up, and from the
   top row it wrapped to the bottom one, `task-4`. Space toggles a task: the
   first space marked `task-4` done, the selection settled on `task-3`, and
   the second space marked that done too. `i` opens the input, so only
   'dge' landed in it. The agent saw 'dge', deleted it and added the right
   title, but left the two tasks done, and its reply said it had not
   toggled them. Every Haiku T1 run on tmux started by typing the title
   into the list: 5 of 5 in cycle2a, 5 of 5 in full1. In cycle2a, 4 of the
   5 noticed and repaired the damage. The cell's medians were 14 tool calls
   and 80,014 tokens, 5.75x taria's T1 median. Sonnet pressed `i` first in
   every T1 run, in both batches. On taria, typed text goes to the app's
   typing surface, and no taria or outline run in cycle 2 sent a single
   `key` call.

3. **On Sonnet, tmux's lead is gone against the outline and small against
   JSON.** In full1, tmux used 25.8% fewer tokens than taria JSON. In cycle
   2 it used 7.7% fewer than JSON (12,890 against 13,963), and the outline
   used 1.7% fewer than tmux (12,675). Per task, tmux used fewer tokens
   than JSON on every task but T2: 0.69x JSON's on T1 and T4, 0.71x on T6,
   0.82x on T5 and 0.94x on T3, against 1.36x on T2. On T1 and T4, tmux
   sends `i`, the title and Enter in one model call and finishes in 3 model
   calls, while taria sets the value, reads the tree that now advertises
   `activate`, then activates: 4 model calls. On T6, JSON took a median of
   5 model calls to tmux's 4: three of its five runs (cycle2a j060, j076 and
   j083) sent `set_value` and `activate` in separate calls.

4. **The shorter tool texts cut taria's cost per run by a fifth.** The
   bridge's fixed text is what it sends whatever the app shows: its server
   instructions and its tool definitions, resent with every model call.
   Between full1 and cycle 2 it went from 4,788 characters to 1,992.
   tmux's fixed text did not change: its tools and instructions are
   byte-identical in both batches, so tmux is the control for everything
   else that moved. Around tmux, one text changed length: the identity line. full1's
   agents got "You are Claude Code, Anthropic's official CLI for Claude,
   running within the Claude Agent SDK."; cycle 2's got "You are a Claude
   agent, built on Anthropic's Claude Agent SDK.", 32 characters shorter.
   Everything else before tmux's first model call had the same length in
   both batches. The context at a call is everything the model reads at
   that call.

   | | full1 | cycle2a | change |
   |---|---|---|---|
   | taria, context at the first call, tokens (Haiku) | 3,160 | 2,505 | -655 |
   | tmux, context at the first call, tokens (Haiku) | 2,351 | 2,347 | -4 |
   | taria, tokens per run, Haiku | 17,548 | 14,045 | -20.0% |
   | taria, tokens per run, Sonnet | 17,388 | 13,963 | -19.7% |
   | tmux, tokens per run, Haiku | 24,226 | 26,722 | +10.3% |
   | tmux, tokens per run, Sonnet | 12,898 | 12,890 | -0.1% |

   Sonnet's first calls are 2 tokens larger than Haiku's, with the same
   changes. The 4 tokens tmux lost are attributed to the identity line, the
   only text before its first call whose length changed. That is an
   inference, not a separate measurement: the environment block's
   scratchpad path carries a different session id per batch, at the same
   length, which may tokenize differently. So about 651 of taria's 655 are
   its tool definitions, 1,899 characters shorter: about 2.92 characters
   per token. From the second call on, the server instructions join, 897
   characters shorter, and each call carried 923 fewer tokens: on Sonnet's
   T1 to T4, the median taria call after the first shows -923 at every
   position, while tmux's median calls moved by -8 to +20. tmux's +10.3% on
   Haiku is run-to-run variation at n=5, not context: its median tool calls
   stayed at 6.5, and its per-task medians moved both ways, from -4,247
   tokens on T4 to +11,959 on T1, and summed to +4.3%. Two taria cells went
   up, Haiku T4 and Sonnet T6. In full1, 4 of 5 runs in each had submitted
   through a fallback (see finding 6): `type_text` with a trailing newline
   on Haiku T4, `key enter` sent with the `set_value` on Sonnet T6. In
   cycle 2 all 10 used `act activate`, and the median run made one more
   model call: 3 to 4 on Haiku T4, 4 to 5 on Sonnet T6.

5. **The outline cuts about 9% against JSON, measured.** Haiku 12,788
   against 14,045 (8.9% fewer), Sonnet 12,675 against 13,963 (9.2% fewer),
   with the same pass rates and median tool calls. The saving is in the
   results: a median outline result is 548 characters against JSON's 1,134,
   so each call from the second on adds 396 tokens (Haiku) and 378
   (Sonnet) instead of 675 and 642. The outline pays a little up front: its
   fixed text is 207 characters longer and its first call 47 tokens larger.
   Not every cell gained. On Haiku, T6 used 2.8% more tokens, and T4, 1%
   cheaper on tokens, took a median of 5 tool calls against 3. Sonnet T6,
   the largest gap at 1.46x, is mostly call batching: outline Sonnet sent
   `set_value` and `activate` in one model call in 5 of 5 runs, JSON Sonnet
   in 2 of 5, and the other 3 made a fifth model call.

6. **Fallback calls nearly stopped.** `key` and `type_text` are the raw
   fallbacks: a key press or typed text, for where no advertised action
   fits. In the first round, taria agents made 38 of their 449 tool calls
   through them (11 of 223 in JSON, 27 of 226 in the outline). In cycle 2
   they made 2 of 453 (0 of 218 in JSON, 2 of 235 in the outline). Both were
   Haiku on T6 in the outline. In cycle2b j023 the agent focused the input
   and typed the title with a trailing newline, which submitted it. In
   cycle2b j052 it typed while the delete dialog was open; the app reported
   the input ignored, and the agent dismissed the dialog and used
   `set_value` and `activate`. Every other new task on taria or the outline
   in cycle 2 was submitted with `act activate`. On the outline, Sonnet
   submitted with `key enter` in all 15 of its T1, T4 and T6 runs in
   outline1, and with `act activate` in all 15 in cycle2b. Three things
   changed in between: the outline's form (actions last, as `act: a|b|c`),
   the tool texts (`act` now says `activate` "submits a field"), and remote
   control's note, which sat in every outline1 call and in no cycle2b call.
   The data does not say which one did it.

## Where the tokens go

Every model call rereads the whole context, so a run's cost is mostly what
sits in that context, times the number of calls. Cycle 2, Haiku / Sonnet
where the two differ:

| | taria JSON | taria outline | tmux |
|---|---|---|---|
| Context at the first call, tokens | 2,505 / 2,507 | 2,552 / 2,554 | 2,347 / 2,349 |
| Tool definitions, chars | 1,783 | 1,938 | 1,441 |
| Server instructions, chars (join at the second call) | 209 | 261 | 1,346 |
| Fixed text, chars | 1,992 | 2,199 | 2,787 |
| Growth per call, tokens | 675 / 642 | 396 / 378 | 493 / 465 |
| Tool result size, chars | 1,134 | 548 | 651 |

A tool definition is counted as the server's own listing of it (its MCP, Model
Context Protocol, `tools/list` entry: name, description and parameter schema) printed by
Python's `json.dumps`. Growth per call is the median context a run adds
per model call from the second call on: the previous call's output, the
tool result and a short token-count reminder. The result size is the
median over every tool result.

taria's first call is 158 tokens above tmux's. Its four tool definitions
are 342 characters longer than tmux's two, which at the 2.92 characters
per token of finding 4 comes to about 117 tokens. The rest is
presumably the framing each of the two extra definitions carries beyond
its characters. From the second call on, when the instructions join,
taria's fixed text is 795 characters shorter than tmux's. What still puts
JSON above tmux on Sonnet is the result, 1.74x a tmux screen, and the
extra model call on T1, T4 and T6. The outline's result is below a screen.

## History: the first round

The first round ran on 2026-10-11 as two batches: full1 (taria and tmux,
120 runs, commit eb0788a) and outline1 (the outline, 60 runs, commit
aed04c3). It found:

- On Haiku, tmux passed 27 of 30 against taria's 30 of 30, with 2.17x the
  tool calls and 1.38x the tokens over every graded run, 1.14x over passed
  runs alone. Cycle 2's 1.90x holds both ways. All three failures were T1,
  the typing failure of finding 2.
- On Sonnet, tmux used 25.8% fewer tokens than taria JSON (12,898 against
  17,388). The write-up named taria's tool definitions, resent with every
  call, as the largest part of that gap: 3,682 characters against tmux's
  1,441. Cycle 2 cut them, and the instructions with them.
- The outline's raw medians were 17,272 (Haiku) and 17,040 (Sonnet), 1.6%
  and 2.0% below JSON.

outline1 is not a clean batch. Remote control was on in its orchestrating
session, and Claude Code put a `remote_session_change` attribution note,
709 to 710 rendered characters, into every outline1 run, before the first
model call, so it sat in every call. full1's runs had none. The first
write-up estimated the note's cost from outline1's first-call excess over
full1's JSON runs, less 27 tokens for the outline's longer tool
definitions, and subtracted it from every call: about 16,260 and 16,020
tokens per run, 7.4% and 7.9% below JSON. cycle2b's measurement, 8.9% and
9.2%, supersedes that estimate. It does not test it: between the two, both
arms' tool texts were cut, the outline's actions moved to the end of each
line as `act: a|b|c`, and the JSON arm it is measured against moved from
full1 to cycle2a. The estimate also left out the identity line, which
differed between full1 and outline1 and to which cycle 2 attributes about
4 tokens per call, from tmux's first-call change.

**A correction to the first write-up's finding 6.** It said that in JSON,
all 10 Sonnet runs of T1 and T4 submitted the new task with `act activate`,
while the outline drew `key enter`. That holds for T1 and T4. It left out
T6: there, JSON Sonnet in full1 submitted with `key enter` in 4 of 5 runs
(full1 j060, j076, j079 and j083), against 5 of 5 on the outline in
outline1. On T6 both sent
the `key enter` in the same model call as the `set_value`, before any tree
had advertised `activate`, which the demo shows only once the draft would
submit. So the outline's extra Sonnet fallbacks were the 10 on T1 and T4,
each sent alone after a tree showing `[set_value activate dismiss]`. On T6
the two formats drew nearly the same fallback. One Haiku T6 run in JSON,
full1 j024, also submitted with `key enter`.

## What to try next

- **Done: shorter tool descriptions.** The fixed text went from 4,788 to
  1,992 characters, and taria's median tokens per run fell 20.0% on Haiku
  and 19.7% on Sonnet.
- **Done: outline actions that read as a call.** Sonnet's outline submits
  moved from `key enter` (15 of 15) to `act activate` (15 of 15). The
  shorter texts landed in the same step, and outline1 carried remote
  control's note, so the data cannot credit the move to any one of the
  three.
- **Open: re-measure on a bigger app.** The demo's tree has 8 to 14 nodes.
  The outline's per-result saving should weigh more with more nodes, and
  the fixed text less.
- **Option: save the submit's extra model call.** On Sonnet T1 and T4,
  taria and the outline took 4 model calls to tmux's 3, because the agent
  set the value, read the new tree, then activated; on T6, JSON's median
  run took 5 to tmux's 4. In cycle 2, 7 Sonnet T6 runs (cycle2a j079 and
  j097, and all 5 outline runs of cycle2b) sent `set_value` and `activate`
  in one model call. All 7 passed, in 4 model calls, where the JSON T6 runs
  that split the two took 5. A line in
  `act`'s description saying the two can go together would test whether
  agents take it.
- **Option: make the outline the default.** In cycle 2 it cost 8.9% and
  9.2% less per run than JSON, with the same pass rates and 2 fallback calls
  in 235. Against it: 5 runs per cell on one small app, and Haiku took more
  tool calls on T4.

## Caveats

- **Small samples.** n=5 per cell. A Fisher p, from Fisher's exact test, is
  the chance of a pass-rate split at least this uneven if both arms truly
  passed equally often; only a small p says the gap is unlikely to be luck.
  Haiku's gap, 30/30 on JSON and on the outline against 29/30 on tmux,
  gives p=1.000, and T1 alone, 5/5 against 4/5, also p=1.000. Pooling
  full1 and cycle2a, Haiku passed T1 on tmux 6 of 10 times against taria's
  10 of 10, p=0.087, across two commits. Every other cycle 2 comparison is
  a full pass rate against a full pass rate.
- **One small app.** Results may not carry to larger apps.
- **Two sessions in cycle 2,** by design, so that taria and the outline
  never shared a session's tool descriptions. The summary's run conditions
  list the two batches as agreeing on commit, Claude Code version, identity
  line, models, effort, turn cap, seed and tolerated attachments.
- **Before and after crosses commits.** full1 (eb0788a) to cycle2a
  (3e66fbb) changed the bridge's texts and, outside the repo, the identity
  line, to which tmux's first-call change attributes about 4 tokens per
  call. outline1 to cycle2b also drops remote control's note and changes
  the outline's form.
- **Output tokens are incomplete.** Claude Code sometimes never writes an
  assistant message's closing line, leaving its output count short at 1 to
  80 tokens. 18 of the 180 cycle 2 runs have at least one such message (15
  in cycle2a, 3 in cycle2b). The summary leaves them out of the
  output-token medians. Input and cache counts are unaffected.
- **Tokens processed are not a price.** They count a cache read the same as
  fresh input, and cache reads were 88.0% to 92.7% of cycle 2's, depending
  on the arm.

## Data and provenance

| batch | arms | runs | passed | git SHA | tag | run on | context beyond the shared set |
|---|---|---|---|---|---|---|---|
| full1 | taria, tmux | 120 | 117 | eb0788a | `bench-full1` | 2026-10-11 | none; the older identity line |
| outline1 | outline | 60 | 60 | aed04c3 | `bench-outline1` | 2026-10-11 | remote control's note, every run |
| cycle2a | taria, tmux | 120 | 119 | 3e66fbb | `bench-cycle2a` | 2026-10-12 | none |
| cycle2b | outline | 60 | 60 | 3e66fbb | `bench-cycle2b` | 2026-10-12 | none |

The tags are local. full1 was regraded, and outline1 graded, at aed04c3;
cycle2a and cycle2b were graded at 3e66fbb. No batch had a harness error
(a run lost to the harness, such as a failed setup, the guard or an
abort, not to the agent).

- Raw rows, plans and archived transcripts: `bench/results/<batch>/`, on
  the machine that ran them only. All four batches are in
  `~/Projects/taria` (the archive); cycle2a and cycle2b are also in the
  `~/Projects/taria-cycle2` worktree. They are not in git.
- Analysis scripts: `~/Projects/taria/bench/results/analysis/`.
  `cycle2.py` reads the archive and prints every breakdown above (fixed
  text, context per call, before and after, fallback calls and submits);
  `cycle2.md` is its output as it ran. `toolsize.py` measures the fixed
  text from the live servers. `prefix.py`, `adjust.py` and `three-way.md`
  are the first round's; `adjust.py`'s estimates are superseded.

The tables, from a checkout that has `bench/` (until `feat/cycle2` merges,
the `~/Projects/taria-cycle2` worktree):

```bash
python3 bench/summarize.py --batch cycle2a --batch cycle2b
python3 bench/summarize.py ~/Projects/taria/bench/results/full1/runs.jsonl \
    ~/Projects/taria/bench/results/outline1/runs.jsonl
python3 ~/Projects/taria/bench/results/analysis/cycle2.py
cargo build --workspace
python3 ~/Projects/taria/bench/results/analysis/toolsize.py
```

`toolsize.py` runs from the checkout's root and measures the commit it is
at: 1,992 (JSON), 2,199 (outline) and 2,787 (tmux) characters at 3e66fbb.
`cycle2.py` rebuilds the same counts from the transcripts, and from them
the first round's 4,788 (JSON) and 4,990 (outline).
