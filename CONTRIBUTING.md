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
```

CI runs the same three; zero warnings is the bar.

## Ground rules

- `crates/taria` is wire types only: serde, no I/O, no framework deps.
  Breaking wire changes bump `PROTOCOL_VERSION` and need a written rationale.
- Protocol minimalism wins arguments. A smaller spec that ratatui, bubbletea,
  textual, and ink could all implement beats a richer Rust-only one.
- Conventional commits: `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`.
- Open an issue before large protocol changes; adapters and demos can go
  straight to PR.
