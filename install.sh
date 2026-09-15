#!/usr/bin/env sh
# taria-mcp installer. Downloads the latest prebuilt release binary from GitHub,
# verifies its published sha256, and installs it into /usr/local/bin (if
# writable) or ~/.local/bin.
#
# taria-mcp is the bridge an agent harness talks to. It is only half of taria:
# the app you want driven has to have adopted the adapter. See
# https://github.com/y0sif/taria for the other half.
#
# Usage:
#   curl -sSf https://y0sif.github.io/taria/install.sh | sh
#   curl -sSf https://raw.githubusercontent.com/y0sif/taria/main/install.sh | sh
#   ./install.sh
#
# Environment:
#   TARIA_VERSION=v0.2.0   Pin to a specific tag (default: latest)

set -eu

REPO="y0sif/taria"
BIN="taria-mcp"

# ---- Detect platform ---------------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64|amd64) TARGET="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
            *)
                echo "Unsupported Linux architecture: $ARCH" >&2
                echo "Please build from source: https://github.com/$REPO" >&2
                exit 1
                ;;
        esac
        ;;
    Darwin)
        case "$ARCH" in
            x86_64) TARGET="x86_64-apple-darwin" ;;
            arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
            *)
                echo "Unsupported macOS architecture: $ARCH" >&2
                exit 1
                ;;
        esac
        ;;
    MINGW*|MSYS*|CYGWIN*|Windows_NT)
        # Not a packaging gap: the transport is a Unix domain socket bound
        # through unix-only APIs, so there is no Windows build to download.
        echo "Windows is not supported: taria's transport is a Unix domain socket." >&2
        echo "See https://github.com/$REPO for what is supported." >&2
        exit 1
        ;;
    *)
        echo "Unsupported operating system: $OS" >&2
        exit 1
        ;;
esac

# ---- Required tools ----------------------------------------------------------
for cmd in curl tar mktemp uname sed; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Required command not found: $cmd" >&2
        exit 1
    fi
done

# ---- Resolve release tag -----------------------------------------------------
if [ -n "${TARIA_VERSION:-}" ]; then
    TAG="$TARIA_VERSION"
    echo "Pinned release: $TAG"
else
    echo "Resolving latest release of $REPO..."
    # Follow the /releases/latest redirect to the canonical tag URL, then
    # extract the trailing tag name. No GitHub API token or jq required.
    TAG=$(curl -sSL -o /dev/null -w '%{url_effective}' \
        "https://github.com/$REPO/releases/latest" \
        | sed 's|.*/tag/||' | tr -d '[:space:]')

    if [ -z "${TAG:-}" ] || [ "$TAG" = "latest" ]; then
        echo "Could not resolve the latest release tag." >&2
        echo "Set TARIA_VERSION=v0.2.0 (or similar) and re-run." >&2
        exit 1
    fi
    echo "Latest release: $TAG"
fi

# ---- Download ----------------------------------------------------------------
# Asset names match release.yml: taria-mcp-<target>.tar.gz plus its .sha256.
ASSET="$BIN-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT INT HUP TERM

echo "Downloading $URL"
if ! curl -fL "$URL" -o "$TMPDIR/$ASSET"; then
    echo "Download failed." >&2
    echo "Check that release $TAG has artifact $ASSET:" >&2
    echo "  https://github.com/$REPO/releases/tag/$TAG" >&2
    exit 1
fi

# ---- Verify the published checksum -------------------------------------------
# Every release attaches one next to the tarball, so a download that is short,
# corrupted, or not what was built is caught here rather than at first run.
if curl -fLsS "$URL.sha256" -o "$TMPDIR/$ASSET.sha256"; then
    if command -v shasum >/dev/null 2>&1; then
        CHECK="shasum -a 256 -c"
    elif command -v sha256sum >/dev/null 2>&1; then
        CHECK="sha256sum -c"
    else
        CHECK=""
    fi

    if [ -n "$CHECK" ]; then
        echo "Verifying checksum..."
        # The checksum file names the asset with no path, so verify from the
        # directory holding it.
        if ! (cd "$TMPDIR" && $CHECK "$ASSET.sha256" >/dev/null 2>&1); then
            echo "Checksum verification FAILED for $ASSET." >&2
            echo "Refusing to install. Re-run, or report this at" >&2
            echo "  https://github.com/$REPO/issues" >&2
            exit 1
        fi
    else
        echo "Note: neither shasum nor sha256sum found, skipping verification."
    fi
else
    echo "Note: no published checksum for $ASSET, skipping verification."
fi

# ---- Extract -----------------------------------------------------------------
echo "Extracting..."
tar -xzf "$TMPDIR/$ASSET" -C "$TMPDIR"

# Locate the binary within the extracted tree. The archive also carries the
# README and both licence texts.
BIN_PATH=$(find "$TMPDIR" -type f -name "$BIN" -print | head -n1)
if [ -z "${BIN_PATH:-}" ] || [ ! -f "$BIN_PATH" ]; then
    echo "Could not find '$BIN' binary in the downloaded archive." >&2
    exit 1
fi

chmod +x "$BIN_PATH"

# ---- Choose install destination ----------------------------------------------
SYSTEM_DIR="/usr/local/bin"
USER_DIR="$HOME/.local/bin"

if [ "$(id -u)" = "0" ]; then
    INSTALL_DIR="$SYSTEM_DIR"
elif [ -w "$SYSTEM_DIR" ]; then
    INSTALL_DIR="$SYSTEM_DIR"
else
    INSTALL_DIR="$USER_DIR"
    mkdir -p "$INSTALL_DIR"
fi

DEST="$INSTALL_DIR/$BIN"
mv "$BIN_PATH" "$DEST"
chmod +x "$DEST"

# ---- Done --------------------------------------------------------------------
echo ""
echo "Installed $BIN $TAG to $DEST"

case ":${PATH:-}:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo ""
        echo "Note: $INSTALL_DIR is not in your PATH."
        echo "Add this to your shell profile (~/.profile, ~/.bashrc, ~/.zshrc, or ~/.config/fish/config.fish):"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

echo ""
echo "Next: point your harness at it, with the label of a taria-enabled app."
echo "  claude mcp add taria -- $BIN --app <label>"
echo "Run '$BIN --help' for the socket options."
