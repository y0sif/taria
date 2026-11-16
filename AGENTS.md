# taria

ARIA for terminals: an agent accessibility layer for TUIs. A wire protocol, a
ratatui adapter that publishes an app's widget tree, and an MCP bridge that
hands that tree and its actions to an agent. Users start at
[README.md](README.md); this file is for working on the code.

## Commands

The checks are in [CONTRIBUTING.md](CONTRIBUTING.md#before-pushing). Never
push before every one passes locally. CONTRIBUTING.md also has the Wine runner
for the Windows tests and how to submit a change.

## CI gate

`.github/workflows/ci.yml`, every `run: cargo` step but `cargo fmt --check`
with `--locked`:

- Linux: every command in [Before pushing](CONTRIBUTING.md#before-pushing).
- macOS: tests, build, both scripts.
- Windows: clippy, tests, build. The scripts need a pty, `/proc` and
  `AF_UNIX`, so `crates/taria-mcp/tests/adapter_e2e.rs` is the end-to-end gate.
- MSRV 1.88: `cargo check --workspace --all-targets`, native and Windows.
- Semver: `cargo-semver-checks` on each published crate against its newest
  release. A 0.x minor counts as major.

## Layout

Each module's `//!` doc says how it works and what it must keep true. Read it
before changing the module.

| Path | Holds |
|---|---|
| `crates/taria/` | The protocol: wire types, the `wire` messages and limits, and pure modules both peers share: `key` (key grammar), `id` (`IdSpace`), `socket` (endpoint names and limits), `state` (typed node state) |
| `crates/taria-ratatui/` | The adapter: `layer.rs` (`TariaLayer`: threads, queues, acks, stamps, dedupe), `bound.rs` (trimming a snapshot to one line), `recorder.rs` and `semantic.rs` (`FrameRecorder`, `sem`), `key.rs` (keys and text to crossterm events), `transport/` (Unix socket, Windows pipe) |
| `crates/taria-mcp/` | The bridge, a library and the `taria-mcp` binary: `server.rs` (the four tools, their checks, every answer), `bridge.rs` (the connection manager), `tree_format.rs` (JSON and outline), `args.rs` (the command line), `transport/` (the connect side), `src/tests/` (against fake apps), `tests/adapter_e2e.rs` (the real binary against a real layer) |
| `examples/demo-app/` | `taria-demo`, the task manager the scripts, the bench and the README GIF drive; `tree.rs` and `update.rs` are pure and unit-tested |
| `scripts/` | `e2e.py` and `adversarial.py` (the gate scripts), `wine-test.sh` (the Windows tests under Wine), `check-docs.sh` (the docs rules and page budgets) |
| `bench/` | The agent benchmark, its harness and its results; `bench/dual-demo/` is outside the workspace |
| `demo/` | The README GIF, the tape that records it, and `drive.py`, the scripted client it shows |
| `docs/` | `protocol.md` (the normative wire spec), `integration-guide.md` (adding taria to an app), `tree-guide.md` (building a good tree), `cli.md` (the bridge's flags, environment, answers, outline), `architecture.md` (how the pieces fit, deferred work), `compatibility.md` (platforms, versions, stability, limitations), `comparison.md` (against screen-level tools), `faq.md` |
| The root | `README.md` (what it is, install, quick start), `CONTRIBUTING.md` (build, test, submit), `CHANGELOG.md`, `server.json` (the MCP registry manifest), `install.sh` (the installer) |

## Rules: protocol

- `PROTOCOL_VERSION` is 1 and frozen. Changes are additive only: new optional
  fields, new message variants, new roles and actions, which peers degrade to
  `other` and `Custom` instead of failing the snapshot. Anything else bumps
  `PROTOCOL_VERSION` ([section 16](docs/protocol.md#16-versioning)). A wire
  change updates `docs/protocol.md` in the same commit.
- Every type an addition can reach is `#[non_exhaustive]`, and so are its
  struct-like variants (the `wire` module doc lists them). Peers build them
  through constructors and destructure with `..`, so additive on the wire is
  additive in Rust too. A new type that can grow gets the attribute before
  its first release, and a new variant with fields ships with a constructor.
- Anything that adds nesting to the tree, in the core types or an adapter,
  spends the `MAX_NODE_DEPTH` budget (its doc says why there is one).
- Adoption is the project's oxygen: keep the protocol minimal. A spec that
  ratatui, bubbletea, textual and ink could all implement beats a richer
  Rust-only one.

## Rules: code

- `crates/taria` has one dependency (serde), no I/O, no framework dependency.
  What needs a runtime or a framework belongs in an adapter or the bridge.
- Never panic in a library crate; return errors.
- The layer never prints: a print under the alternate screen garbles the
  display. The app reports `bind_error()` and `stats()` after restoring the
  terminal. `bind_or_disabled` never fails, so taria cannot block startup.
- Add dependencies with `cargo add`. Justify a heavy one in a comment beside
  it in its `Cargo.toml`.
- The demo keeps `clippy::wildcard_enum_match_arm`: the wildcard arm
  `#[non_exhaustive]` forces on `AgentInput` is the one that swallows `Text`.
- Tool descriptions, the server instructions and every answer text are pinned
  byte for byte by tests and the scripts, so a rewording edits all of them.
  Descriptions are resent with every model call: keep them short.

## Rules: the agent contract

- Text through the key bindings is a bug, not a shortcut
  ([integration-guide.md](docs/integration-guide.md#typed-text-goes-where-your-app-puts-typing)).
- Never make raw keys and text the primary path in a demo.
- Every state an agent can enter needs an advertised way out, or the raw key
  becomes the only escape.
- Advertise an action only where it does something. Elsewhere report
  `Ignored`, before the next publish, so the tree and the verdict agree.

## Rules: docs

- One fact, one home: link to it, do not restate it. Version history lives
  in `CHANGELOG.md` and the version table in `docs/compatibility.md`,
  measurements in `bench/RESULTS.md`, issue numbers in `CHANGELOG.md` and
  commit messages, bug stories in commit messages. None of it goes in the
  other docs or the `//!` docs.
- `scripts/check-docs.sh` enforces these rules and the page budgets.

## Release checklist

1. In `Cargo.toml`, bump `[workspace.package] version` and the `version` of
   the workspace `taria` and `taria-ratatui` dependencies, and commit the
   `Cargo.lock` the next build updates.
2. In `server.json`, bump both `version` fields.
3. Add the `CHANGELOG.md` entry.
4. Run every check in [Before pushing](CONTRIBUTING.md#before-pushing).
5. Push a `v<version>` tag: `release.yml` makes the GitHub release with the
   `taria-mcp` binaries. Write its notes from the `CHANGELOG.md` entry.
6. Publish the crates by hand: `taria`, `taria-ratatui`, then `taria-mcp`.
7. Publish `server.json` to the MCP registry (`mcp-publisher publish`).
8. On the `gh-pages` branch, bump `softwareVersion` in the JSON-LD of
   `index.html`, the landing page.

## Context

taria is framework-side: the app describes itself. The agent-side driver
space (screen scrapers, tmux drivers) is crowded; do not drift into building
another screen-scraper. `docs/comparison.md` has the landscape.
