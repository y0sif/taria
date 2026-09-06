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
python3 scripts/e2e.py           # end-to-end: demo app + bridge + MCP scenario
python3 scripts/adversarial.py   # edge-case probes (expects debug binaries built)
```

CI runs the first four; zero warnings is the bar. The two scripts are local
only, use nothing outside the Python standard library, and are where a change
that breaks the agent-facing contract shows up: several of their assertions
are the bridge's error strings verbatim, so rewording one is a deliberate edit
in both places.

## Ground rules

- `PROTOCOL_VERSION` is 1 and version 1 is frozen. Changes within it must be
  additive: a new optional field, a new message variant, a new role, a new
  action name. Peers skip lines they cannot parse, serde ignores unknown
  fields, and the two open vocabularies degrade an unknown value to
  `Role::Other` or `Action::Custom`, which is what makes additive safe.
  Removing a field, renaming one, making an optional field required, or
  changing what an existing field means bumps the version, and needs a written
  rationale first. `crates/taria/src/wire.rs` is the normative statement.
- `crates/taria` is the shared protocol crate: the wire types plus the three
  pure modules both peers need to agree on strings (`key`, `id`, `socket`).
  One dependency (serde), no I/O, no framework deps. Anything that needs a
  runtime or a framework belongs in an adapter or the bridge.
- Protocol minimalism wins arguments. A smaller spec that ratatui, bubbletea,
  textual, and ink could all implement beats a richer Rust-only one.
- Conventional commits: `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`.
- Open an issue before large protocol changes; adapters and demos can go
  straight to PR.
