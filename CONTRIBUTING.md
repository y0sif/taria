# Contributing to taria

Thanks for helping make terminals legible to agents.

## Setup

```bash
git clone https://github.com/y0sif/taria
cd taria
cargo check --workspace
```

The directory map is in [AGENTS.md](AGENTS.md#layout), and so are the rules
a change has to keep.

## Before pushing

```bash
scripts/check-docs.sh            # docs rules and line budgets, no build needed
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --target x86_64-pc-windows-gnu --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --locked
cargo test --workspace --locked
cargo build --workspace --locked
python3 scripts/e2e.py           # end-to-end: 27 steps, demo app + bridge + MCP
python3 scripts/adversarial.py   # 24 edge-case probes
```

The Windows-target clippy needs only `rustup target add x86_64-pc-windows-gnu`,
no linker. The two scripts need only the Python standard library. Both build
the debug binaries they test; `--no-build` skips the build and still refuses
a `target/debug` older than the sources. What each CI job runs is in
[AGENTS.md](AGENTS.md#ci-gate).

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
pipe owned by another account, or something Windows does and Wine may not,
such as a DACL (the pipe's access list) that shuts out the pipe's own user,
or a bridge's open handle keeping the name of an app that exited in use,
says in its output when it fell back.

## Submitting a change

- Follow the rules in [AGENTS.md](AGENTS.md#rules-protocol): the protocol,
  the code and the agent contract.
- Use conventional commits: `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`.
- Add a `CHANGELOG.md` entry, with an upgrade note for any change to the Rust
  API.
- Open the PR with the template and tick its checklist.
- Open an issue, with the rationale, before a large protocol change. Adapters
  and demos can go straight to a PR.
