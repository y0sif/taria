# Changelog

Notable changes to the taria crates. The shape is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/): one section per
release, newest first, dated. Inside a release the body is grouped by audience
rather than only by kind, because a second implementer of the protocol, an
agent driving an app, and an author retrofitting taria into one each need a
different part of the same list, and splitting it into Added, Changed and
Fixed scatters every one of them across all three.

The crates follow [semantic versioning](https://semver.org/spec/v2.0.0.html).
The wire format carries its own number, `PROTOCOL_VERSION`, which moves
independently of the crate version: a crate release is not a format bump, and
a format bump would not wait for one.

## [0.2.0] - 2026-09-12

**The crates are on crates.io.** 0.1.0 froze the wire format and never left
this repository, so 0.2.0 is the first version cargo can resolve, and the
0.1.0 section below, which says nothing is published yet, is now a statement
about that release rather than about the project.

```bash
cargo add taria-ratatui   # in a TUI app that agents should be able to use
cargo install taria-mcp   # the bridge, for driving one
```

A tag also builds `taria-mcp` for x86_64 and aarch64 Linux and for both macOS
architectures and attaches the tarballs, each with its sha256, to a GitHub
release, so driving an app does not require a Rust toolchain.

Nothing on the wire moved. `PROTOCOL_VERSION` stays 1, no message or type
gained or lost a field, and no crate's Rust API changed. Everything below is a
specification, a description an agent reads, a test that was missing,
packaging, or one action the demo app now advertises, which `Action::Focus`
already existed to carry.

### The wire format is specified, message by message

`docs/protocol.md` is the normative per-message specification of version 1,
written for someone implementing a peer from that document alone: an adapter
for a framework that is not ratatui, in a language that is not Rust. Until now
`docs/architecture.md` said the per-message spec was deferred and that the
rustdoc on `crates/taria` was normative until it existed, which is a fine
answer for a Rust adopter and no answer at all for anyone else.

It covers framing, the handshake, every message with a literal line, both
vocabularies with their degrade rules, the ack model, connection ownership,
the key grammar, the limits with their units, socket discovery and lifecycle,
the focus contract, and what a version bump costs. Rationale stays in
`architecture.md`. Behaviour the reference peers add beyond what the protocol
requires is marked as convention and is not binding on a conforming peer, and
Rust-specific obligations are confined to one section.

Several normative facts existed only in code and are written down for the
first time: that `AgentInput` is tagged on `kind` while the message envelope
is tagged on `type`, one level apart in the same line, so a peer that
conflates them fails every input; that the 1 MiB line cap counts bytes,
excludes the newline, and closes the connection rather than skipping a line;
that depth counts the root as level one; and that an input id is a `u64`.
Every literal line in the document is real output, either lifted from the
frozen test vectors or captured from a live session against the demo. What is
genuinely undecided is listed as undecided rather than invented.

### What an agent is told about typing

The agent-facing text still carried the v0 model, in which the adapter lowered
`Text` into one key event per character. The MCP instructions said to use
`type_text` "for anything you would otherwise spell out with `key`", which is
exactly the conflation that once deleted a task, offered to every agent on
connect.

- **`type_text` now says where the characters go.** To whatever surface the
  app puts typing into, never through its key bindings. A newline arrives as
  Enter and submits, a tab arrives as Tab, carriage returns are dropped,
  characters after a submit usually have nowhere left to go, and an app
  accepting no typing answers ignored, which reaches the agent as a result
  carrying the tree rather than as an error. The lowering is named as an
  adapter convention, not a protocol rule, since the core crate does not
  dictate how an app consumes characters.
- **The server instructions describe `key` and `type_text` as the two
  different lowerings they are**, rather than as a slow path and a fast one.
- **`act` says that an action needing a value and arriving without one is
  answered ignored.**
- **The ignored result names every kind of input that can earn it.** It was
  written act-first, so it covered neither text sent into a surface taking
  none nor a key the grammar refuses.
- **`InputStatus::Ignored` documents the same list**, and the line the demo
  was alone in knowing: a key that parses but is bound to nothing is
  `Delivered`, the same verdict a person gets for pressing an unbound key.
  `Ignored` is for input the app could not act on at all, which is what makes
  it worth reporting to an agent waiting on an effect.
- **The seven built-in actions had no documentation**, `Custom` aside. For
  `Action::Focus` that left "focus the target first" as advice with no way to
  follow it. Each action now says what it does, and `Focus` says what
  advertising it promises: an act with it puts the keyboard on that node,
  and the next snapshot shows that node as the focused one, so an agent can
  check the move landed rather than assume it. That is what makes it the
  advertised way to aim typing. `set_value` is not a focus call, even in an
  app that moves the keyboard as a side effect of one.
- A docs.rs link that pointed through a private module is fixed.
- **The integration guide teaches aiming.** A new section works through
  routing typed text to a surface, with the handler and the rule that
  `set_value` is not a focus call, and the running sample advertises `focus`
  on its text input while the keyboard is elsewhere. The hand-check list at
  the end grows from five items to six.

### The demo advertises the way in

`Action::Focus` became the documented way to aim typing, and the reference app
advertised it on nothing, so the advice had no worked example and no test
behind it. A live agent found the hole immediately: it wanted the keyboard and
nothing else, saw only `set_value` advertised on the input, tried to aim with
an empty value, and ended up letting `set_value` carry the whole title because
there was no other semantic way in.

The demo's input now advertises `focus` while the keyboard is elsewhere, the
mirror of the `dismiss` it already advertised while it held the keyboard, and
the semantic form of the `i` key a person presses. Acting on it moves the
keyboard and nothing else: the draft is untouched, which is the point. It is
reported `ignored` when the input already has the keyboard and while the
dialog is up, so the advertisement and the verdict agree, and the tree stops
offering it in exactly those states.

`scripts/e2e.py` gains the step that was missing: aim with `focus`, type the
whole title with one `type_text`, submit with `activate`. The title carries
the demo's own list bindings (`d`, `j`, `k`, `q`, `y`, `i`) so a character
routed to them cannot pass unnoticed, and a second `focus` asserts the
ignored verdict against the same frame that stopped advertising it.

### The ignore path is gated

The claim that new wording makes to an agent, that text sent while nothing is
accepting typing comes back ignored with a tree rather than as an error, was
the one claim neither suite tested. `scripts/adversarial.py` gains a probe
that types `dy` at the list: the pair that opens the confirm-delete dialog and
confirms it. Lowered through the key bindings it would destroy a task and
still look like success, because the tree would change. The probe asserts that
the ack is `Ignored`, that a tree came with it, that the tree does not show
the input focused, and that every node is untouched either side of the call.

The gate is 21 end-to-end steps and 20 adversarial probes.

### Packaging and CI

- **All three crates carry a `readme`.** `taria-ratatui` and `taria-mcp` had
  none, so their crates.io pages would have rendered empty next to `taria`'s.
- **The workspace dependency on `taria` carries its own version**, and cargo
  has no way to make it follow `workspace.package`, so it is a second literal
  to bump by hand every release. Bumping `workspace.package` alone would have
  published 0.2.0 crates depending on a 0.1.0 that does not exist.
- **A tag releases binaries.** `taria-mcp` is what a person installs, so a tag
  builds it for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-apple-darwin` and `aarch64-apple-darwin`, and attaches a tarball and
  its sha256 for each to a GitHub release. Windows is absent on purpose: the
  transport is a Unix domain socket. Publishing the crates themselves stays
  manual and in dependency order.
- **CI checks the MSRV.** The crates promise `rust-version = "1.88"` on their
  crates.io pages and nothing verified it. A `cargo check` on a pinned 1.88
  does, cheaply: what an old compiler rejects is syntax and library items, not
  test outcomes.

### Docs for someone deciding whether to adopt this

- **`docs/comparison.md`** places taria beside ht, tmux with `send-keys`, the
  tmux and PTY MCP servers, agent-tui, tui-use and agent-terminal: what each
  does, why working inside the app rather than outside it changes what an
  agent can know, and the cases where the right answer is one of the others.
  The first of those is the largest: taria reaches only apps whose authors
  adopted it, so anything you did not write and cannot patch is screen-level
  territory.
- **The README has an FAQ**, in the words people search with: ARIA for
  terminals, an MCP server for a TUI, ht and tmux `send-keys`, whether the app
  has to be ratatui, what adoption costs an author, Windows, two agents at
  once, whether the protocol is stable, and what happens to the widgets you
  have not annotated.
- **The README has install instructions**, split by which side of the socket
  the reader is on: a dependency for an app author, a binary for whoever
  drives the app.
- Three claims in the docs were wrong and are corrected: the temp-dir fallback
  path is namespaced by the effective uid, else `$USER`, else `$LOGNAME`, else
  the literal `default`, rather than by the uid alone; "exactly one focused
  node" is the app's obligation rather than something either peer enforces,
  since the adapter guarantees at least one and the bridge never reads the
  field; and eleven types are `#[non_exhaustive]`, not ten.

## [0.1.0] - 2026-09-10

The first release, and the one the wire format freezes in. Nothing is
published to crates.io yet, so both crates still come from the repository.
"v0" below means the vertical slice that preceded this release: three MCP
tools, 18 roles, no input ids, and no acknowledgement.

### The format is frozen at `PROTOCOL_VERSION` 1

Version 1 is fixed. Changes within it must be additive: a new optional field,
a new message variant, a new role, a new action name. Two properties make that
safe, and both peers already rely on them. A reader skips a line it cannot
parse instead of dropping the connection, and serde ignores unknown fields, so
an addition reaches an older peer as something it quietly ignores while the
rest of the stream keeps working.

Neither property covers a value nested inside a message the peer does want, so
the vocabularies carry their own fallbacks: an unknown `Role` reads as
`Role::Other`, an unknown `Action` name as `Action::Custom` keeping that name,
and an `AgentInput` kind this build cannot read as `AgentInput::Unknown`.
Without them, one leaf node using a role added later makes every snapshot
unparseable, the reader skips each line and goes on serving its last good
tree, and the app looks frozen with no error anywhere.

One rule is narrower than "additive". An ack answers an `InputId`, and the id
sits on the message rather than inside the input, so a new `BridgeToApp`
variant carrying agent input would reach an old adapter as a skipped line with
nothing sent back, and the agent that sent it would wait out a timeout. An
addition that carries agent input must therefore be a new `AgentInput` kind,
never a new `BridgeToApp` variant. New `BridgeToApp` variants stay available
for anything that wants no answer.

Removing a field, renaming one, making an optional field required, or changing
what an existing field means bumps `PROTOCOL_VERSION`. The last is the
dangerous one, because an old peer parses it and acts on the old meaning.

The normative statement, with the reasoning behind each clause, is the module
documentation in `crates/taria/src/wire.rs`. That is the text a second
implementer should read in full; `docs/architecture.md` explains it and
`CONTRIBUTING.md` states it as a ground rule.

### Breaking changes since v0

Anything built against the v0 slice needs these.

**The wildcard arm the compiler makes you add to every `match` over
`AgentInput` is the arm that swallows `AgentInput::Text`, so check every
wildcard you add for `Text` before you trust a green build.** The two changes
below that cause this sit in different lists: `Text` is a wire change and
`#[non_exhaustive]` a Rust one, and it is their product that bites. A v0
`match` named `Act` and `Key`; rebuilt, it fails with
``non-exhaustive patterns: `_` not covered``, an error that never names
`Text`, and the arm that silences it takes every `type_text`. On the first
real app migrated, the four edits the compiler asked for left a clean build,
a clean `clippy -D warnings`, 181 passing tests, and a `type_text` that did
nothing at all. `docs/integration-guide.md` names a clippy lint that catches
it.

On the wire:

- `BridgeToApp::Input` carries an `InputId`, and the app answers it with
  `AppToBridge::Ack` naming the same id and one of `Delivered`, `Dropped` or
  `Ignored`. This replaces inferring the outcome from snapshot diffs, which
  counted any unrelated change as success.
- `AgentInput::Text` carries literal text, so typing costs one message rather
  than one round trip per character.
- `AgentInput::Unknown` is the fallback described above. An app never sees it:
  the adapter acks it `Ignored` at the parse and never queues it, because the
  variant carries nothing an app could act on.
- The role vocabulary went from 18 names to 29, and a custom action name now
  travels folded onto the built-in envelope, so a name that later graduates to
  a built-in reaches the newer app's own arm instead of being accepted and
  silently ignored.

In Rust:

- `#[non_exhaustive]` on the eleven types a version-1 addition can reach:
  `AppToBridge`, `BridgeToApp`, `InputStatus`, `AgentInput`, `Action`, `Role`,
  `Node`, `Snapshot`, `key::Key`, `key::Modifiers` and `key::KeyPress`. And on
  the six struct-like variants where a new optional field would land:
  `AppToBridge::Hello` and `Ack`, `BridgeToApp::Input`, and `AgentInput`'s
  `Act`, `Key` and `Text`. Marking a type is itself a breaking change, which is
  why all of it happens before the first release rather than at the first
  addition. It costs an outside peer one wildcard arm per match, or a trailing
  `..` in a pattern, and buys back that a new variant, field, role, action or
  key is a recompile rather than a repair. On `AgentInput` the wildcard costs
  more than that for anyone coming from v0: it is where `Text` goes missing,
  as the warning at the top of this section says.
- A marked variant has no struct literal outside the crate, so each ships with
  a constructor beside it: `AppToBridge::hello` and `ack`,
  `BridgeToApp::input`, and `AgentInput::act`, `key` and `text`. `Modifiers`
  gains `NONE` and a const `new` in exchange for the struct literal it closes.
- `taria_ratatui::key::to_crossterm` returns `Option<KeyEvent>`. `None` means
  a key a newer taria's grammar learned that this adapter cannot express. It is
  reported rather than approximated, because lowering it to a near-miss event
  would put a keystroke into the app that the agent never asked for and ack it
  as delivered.
- Socket path resolution left both peers for `taria::socket::resolve_path`,
  which returns `Result<PathBuf, InvalidAppLabel>`. The label becomes a file
  name and the adapter binds and unlinks whatever comes out, so an absolute
  label, or one carrying a separator or `..`, is refused rather than resolved.
- Three modules moved into the core crate so the two peers cannot disagree
  about a string: `taria::key` (the key grammar), `taria::id` (`IdSpace`) and
  `taria::socket` (path resolution and the AF_UNIX limit).
- `serde_json` moved to dev-dependencies. The core crate defines the wire
  types and leaves encoding to the transport, so an adopter inherits one
  dependency, serde, instead of two.

In the tool surface: three tools became four. `read_tree`, `act` and `key`
are joined by `type_text`.

### What an agent gains

- **Typing costs one call.** `type_text` sends a literal string of up to 4096
  characters where a 100-character string used to be 100 `key` calls.
- **A malformed key is rejected with the grammar instead of vanishing.** `key`
  parses the string with `taria::key` before sending anything, so a key the app
  would have silently swallowed comes back as an error naming what the grammar
  accepts. An agent sent `inx` once and never learned it was dropped. `key`
  also gains `repeat`, bounded at 1 to 64, so moving five rows is one call.
- **Every result is anchored on the app's acknowledgement** rather than
  inferred from whether the tree happened to change. `Dropped` is an error.
  `Ignored` returns the tree of the frame that caused the ignore, with a note
  to re-plan from it. `Delivered` distinguishes a change from no change. A
  partial burst, acks lost to a slow read, no ack at all, and the app
  disconnecting after taking the input each report as themselves. An adapter
  that sends no acks still behaves exactly as it did before.
- **`read_tree` relays the app's own snapshot line** instead of the bridge's
  re-serialization of its parse of it, so a role, action or field this bridge
  has never heard of arrives under its real name. An app publishing
  `"role":"sparkline"` had the agent read `"role":"other"`, with the real name
  on the wire the whole time. The parse stays authoritative for what the bridge
  decides, since `act` validates node ids and advertised actions against it.
  The ndjson envelope key is stripped, because that is transport framing and
  not part of the tree.
- **A protocol mismatch splits the tool surface by direction** instead of
  swallowing input. `read_tree` keeps working for as long as the peer's
  snapshots still parse, and `act`, `key` and `type_text` refuse up front with
  an error naming both versions, having sent nothing.

### What an app author gains

- `TariaLayer::bind_or_disabled` cannot fail. A socket that will not bind
  yields an inert layer, so taria can never block an app's startup, and the
  layer stays silent about it, because printing once the app owns the alternate
  screen garbles the display. The app reports `bind_error()` where it chooses.
- `publish(nodes)`, `drain` and `drain_with_ids` absorb boilerplate every
  integration was writing by hand, and `ack(id, InputStatus::Ignored)` is how
  an app says it looked at an input and deliberately did nothing.
- `drain_acking` takes the `InputStatus` the handler returns and sends the
  `Ignored` ones itself, so saying "I looked at that and did nothing" needs no
  input id and no enum of the app's own. The demo and the typing tutor each
  wrote the same two-variant enum, and the same loop around `drain_with_ids`
  and `ack`, before it existed. `drain_with_ids` stays for an app that needs
  the id for something else.
- `taria::IdSpace` gives nodes ids that are identities rather than positions,
  which is the difference between an agent acting on the row it read and
  acting on whatever moved into that slot. It sits at the crate root beside
  `Node` and `Role`, as well as in `taria::id`. `new` stays const and
  infallible for the usual case of a prefix declared in a const; `try_new` is
  for one built at runtime.
- Socket failures have names and say what to do about themselves.
  `SocketPathTooLong` carries the path, its length and the platform's limit,
  in place of the bare "path must be shorter than SUN_LEN" that std reports,
  and `InvalidAppLabel` names a label that is not a plain file name.
- Four counters record agent traffic that went nowhere: `dropped_inputs()`,
  `stale_inputs()`, `unknown_inputs()` and `dropped_acks()`. All are monotonic
  across reconnects, all are 0 on a disabled layer, and all are read after the
  terminal is restored, because the layer never prints. `dropped_acks` is the
  one that points at the bridge rather than the app: the input landed and the
  caller was answered nowhere.
- `MAX_NODE_DEPTH` states the depth past which a snapshot exceeds what a JSON
  parser will recurse into and arrives as nothing at all. The adapter cuts
  deeper branches at publish, since a cut tree is the only outcome where the
  agent gets a current one, and reports the cut through `truncated_snapshots()`
  and `last_truncation()` to the only party that can fix the tree.

### The role vocabulary went from 18 to 29

Added: `tree`, `tree_item`, `image`, `chart`, `scrollbar`, `log`, `status`,
`terminal`, `link`, `select` and `option`.

A census of 15 real ratatui apps found 21% of on-screen widgets had no
defensible role, and 8 of ratatui's 15 publishable built-ins had no role at
all. `scrollbar` appeared in 6 of the 15 apps, more often than `tabs`, which
did have one, while `button` appeared zero times: the old vocabulary came from
an HTML forms model rather than from what terminal UIs put on screen. The
census, the ecosystem figures, and the alternative that was rejected (letting
apps name their own roles, which every comparable accessibility vocabulary
tried and abandoned) are in `docs/landscape.md`. Adding roles is additive, so
`PROTOCOL_VERSION` stays 1.

### Fixed

Defects a v0 user would have hit.

- **Node ids shifted on delete.** The demo derived a task's node id from its
  position in a vector, which is the obvious thing to write and what an adopter
  copying it would have written too. Deleting a task renumbered every later
  one, and an agent acting on an id from an earlier `read_tree` hit the wrong
  task. Ids are identities assigned at creation now, reproducible across
  restarts, and `IdSpace` is there so that stays the easy way to write them.
- **An input from a dead connection still reached the app.** The queue outlived
  the connection that filled it, so an agent's `key q` could quit the app after
  the bridge session that sent it was gone, with nothing left to report it to.
  Inputs now carry their connection's generation and are discarded on dequeue
  and counted in `stale_inputs()`, deliberately without an ack, since the peer
  that would read one is the peer that went away.
- **Running two copies hung the first forever at exit.** `Drop` woke its
  listener by connecting to the socket path, but a second instance had already
  unlinked and rebound that path, so the connect reached the second instance's
  listener, looked like success, and the join blocked on a thread parked on a
  socket nothing could reach. The listener polls a shutdown flag now, and
  binding removes only a socket, and only once a connect to it has been
  refused, so a live one is left alone.
- **`type_text` into a focused list triggered keybindings.**
  `type_text("deploy")` on a fresh demo deleted a task and the bridge reported
  success, because with the list focused `d` opened the confirm-delete dialog
  and the later `y` confirmed it. Typed text now reaches an app only where the
  app puts typing, and is acked `Ignored` otherwise. Raw `key` still lowers
  into the bindings on purpose, which is the whole difference between the two
  inputs.

### Deliberately not in this release

Node attributes: `selected` as a fact distinct from `focused`, position in a
list, scroll offset, `expanded` and `level` for trees, and `mode` for modal
editors. The role census found a second group, larger than the one the new
roles fix: 26% of widgets already have a role worth keeping and need only to
say more about themselves. That is not a naming problem, and no role added to
the vocabulary reaches it. Attributes are what it wants. They are additive
under the freeze, so they can land in a later 0.x without moving
`PROTOCOL_VERSION`, and they are held back to be designed rather than because
they were missed.

`mode` is the one that is a safety gap rather than a completeness one. Without
it, an agent typing into a vim-modal editor runs commands instead of typing.

[0.2.0]: https://github.com/y0sif/taria/releases/tag/v0.2.0
[0.1.0]: https://github.com/y0sif/taria/releases/tag/v0.1.0
