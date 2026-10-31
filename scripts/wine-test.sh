#!/usr/bin/env bash
# Run the Windows build of the tests under Wine, from Linux.
#
# Cross-compiles for x86_64-pc-windows-gnu (MinGW-w64 links it) and runs each
# test binary through wine. Arguments go to `cargo test` as they are, so
#
#   scripts/wine-test.sh -p taria-mcp --test adapter_e2e
#
# runs one suite. Needs the rustup target too:
#
#   rustup target add x86_64-pc-windows-gnu
#
# Wine is not Windows: named pipes, process ids and timing are Wine's, so a
# pass here is a strong hint and the windows-latest CI job is the gate.
set -euo pipefail

missing=0
if ! command -v wine >/dev/null 2>&1; then
    echo "wine-test: wine not found; install Wine (Arch: pacman -S wine, Debian/Ubuntu: apt install wine64)" >&2
    missing=1
fi
if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    echo "wine-test: x86_64-w64-mingw32-gcc not found; install MinGW-w64 (Arch: pacman -S mingw-w64-gcc, Debian/Ubuntu: apt install gcc-mingw-w64-x86-64)" >&2
    missing=1
fi
if [ "$missing" -ne 0 ]; then
    exit 1
fi

cd "$(dirname "$0")/.."
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine WINEDEBUG=-all \
    exec cargo test --target x86_64-pc-windows-gnu "$@"
