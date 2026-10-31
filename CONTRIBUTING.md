# Contributing to taria

Thanks for helping make terminals legible to agents.

## Setup

```bash
git clone https://github.com/y0sif/taria
cd taria
cargo check --workspace
```

## Before pushing

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
python3 scripts/e2e.py           # end-to-end: 27 steps, demo app + bridge + MCP
python3 scripts/adversarial.py   # 24 edge-case probes
```

CI runs all six on Linux; zero warnings is the bar. The two scripts use
nothing outside the Python standard library, and are where a change that
breaks the agent-facing contract shows up: several of their assertions are the
bridge's error strings verbatim, so rewording one is a deliberate edit in both
places. Both build the debug binaries they test, because two reviews were once
scored against stale ones without noticing. `--no-build` skips the build for a
caller that has just built and still refuses a `target/debug` older than the
sources.

CI also runs the tests and both scripts on macOS, and clippy, the tests and
the build on Windows, all three with `--locked`. The Linux job cross-checks
clippy for the Windows target and builds the docs with warnings denied, and
the MSRV job checks the Windows target too, so `cfg(windows)` code is
compiled on every push. The scripts cannot run on Windows: they drive the
demo through a pseudo-terminal, read `/proc`, and talk to the app over
`AF_UNIX`, none of which Windows has. There,
`crates/taria-mcp/tests/adapter_e2e.rs` is the end-to-end gate: a real
`TariaLayer` in a child process, driven by the real `taria-mcp` binary over
stdio, on every platform.

## Testing the Windows build from Linux

`scripts/wine-test.sh` cross-compiles the tests for Windows and runs each test
binary under Wine (a layer that runs Windows programs on Linux). It needs
MinGW-w64, the cross toolchain that links them, Wine itself, and the rustup
target:

```bash
sudo pacman -S mingw-w64-gcc wine              # Arch
sudo apt install gcc-mingw-w64-x86-64 wine64   # Debian, Ubuntu
rustup target add x86_64-pc-windows-gnu
```

Arguments go to `cargo test` as they are:

```bash
scripts/wine-test.sh --workspace
scripts/wine-test.sh -p taria-mcp --test adapter_e2e
```

Wine is a stand-in, not Windows: its named pipes, process ids and timing are
its own, so a pass there is a strong hint and the Windows CI job is the final
word. A test that needs something Wine allows and Windows may not, such as a
pipe owned by another account, or something Windows enforces and Wine may
not, such as a DACL that shuts out the pipe's own user, says in its output
when it fell back.

## Ground rules

- `PROTOCOL_VERSION` is 1 and version 1 is frozen. Changes within it must be
  additive: a new optional field, a new message variant, a new role, a new
  action name. Peers skip lines they cannot parse, serde ignores unknown
  fields, and the two open vocabularies degrade an unknown value to
  `Role::Other` or `Action::Custom`, which is what makes additive safe.
  Removing a field, renaming one, making an optional field required, or
  changing what an existing field means bumps the version, and needs a written
  rationale first. `crates/taria/src/wire.rs` is the normative statement.
- Additive on the wire is not automatically additive in Rust, so the twelve
  types a version-1 addition can reach are `#[non_exhaustive]`: the two wire
  message enums, `InputStatus`, `AgentInput`, `Action`, `Role`, `Node`,
  `Checked`, `Snapshot`, `key::Key`, `key::Modifiers` and `key::KeyPress`. So
  are the six struct-like variants where a new optional field would land:
  `AppToBridge::Hello` and `Ack`, `BridgeToApp::Input`, and `AgentInput`'s
  `Act`, `Key` and `Text`. Adding a variant, field, role or action goes to one
  of those, and it stays a recompile for every peer that integrated taria. A
  marked variant has no struct literal outside the crate, so anything new that
  carries fields ships with a constructor beside it. A new type that can grow
  the same way gets the attribute when it lands, not at its first addition,
  because marking one later is itself breaking.
- A tree is capped at `MAX_NODE_DEPTH`, because a snapshot deeper than a JSON
  parser will recurse into arrives as nothing at all rather than as an error.
  Anything that adds nesting to the tree, in the core types or in an adapter,
  is spending that budget.
- `crates/taria` is the shared protocol crate: the wire types plus the three
  pure modules both peers need to agree on strings (`key`, `id`, `socket`).
  One dependency (serde), no I/O, no framework deps. Anything that needs a
  runtime or a framework belongs in an adapter or the bridge.
- Protocol minimalism wins arguments. A smaller spec that ratatui, bubbletea,
  textual, and ink could all implement beats a richer Rust-only one.
- Conventional commits: `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`.
- Open an issue before large protocol changes; adapters and demos can go
  straight to PR.
