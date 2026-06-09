#!/usr/bin/env bash
# Install hatel: both binaries (the receiver and the hook) plus the Claude Code
# skill. By default it downloads the prebuilt archive for this platform and verifies its
# checksum; when that isn't available and the script is run from a checkout, it builds from
# source instead (and `--source` forces that). The prebuilt archive is self-contained, so the
# download path needs no separate skill fetch and no version detection.
set -euo pipefail

REPO="junyeong-ai/hatel"
BIN_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
SKILL_DIR="$HOME/.claude/skills/hatel"
VERSION="${HATEL_VERSION:-}"
WIRE=false
SERVICE=false
INSTALL_SKILL=true
SOURCE=false

# Where the binaries and the skill come from — set by the prebuilt or the source path below.
BINARY_SRC_DIR=""
SKILL_SRC=""

# A hatel checkout to build from, if this script is being run from inside one. Guard
# against `curl | bash` (BASH_SOURCE is not a real file there) and against an unrelated Cargo.toml
# in the cwd, by fingerprinting the workspace: our `crates/hook` member must be present.
PROJECT_ROOT=""
SCRIPT_SOURCE="${BASH_SOURCE[0]:-}"
if [ -n "$SCRIPT_SOURCE" ] && [ -f "$SCRIPT_SOURCE" ]; then
    SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_SOURCE")" 2>/dev/null && pwd -P || true)"
    if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/../Cargo.toml" ] && [ -f "$SCRIPT_DIR/../crates/hook/Cargo.toml" ]; then
        PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
    fi
fi

usage() {
    cat <<'EOF'
Usage: install.sh [options]

Options:
      --source         Build from source instead of downloading (needs a checkout + cargo)
      --wire           Wire Claude Code's settings.json after install (runs `init`)
      --service        Install the receiver as a background user service (runs `service`)
      --no-skill       Do not install the Claude Code skill
      --bin-dir DIR    Install binaries here (default: ~/.local/bin, or $INSTALL_DIR)
      --version VER    Install a specific release (e.g. 0.1.0); default is the latest
  -h, --help           Show this help

Run from a cloned repo and, if no prebuilt release exists for your platform, the install
falls back to building from source automatically.

Environment:
  INSTALL_DIR                 same as --bin-dir
  HATEL_VERSION   same as --version
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --source|--build-from-source) SOURCE=true ;;
        --wire) WIRE=true ;;
        --service) SERVICE=true ;;
        --no-skill) INSTALL_SKILL=false ;;
        --bin-dir) shift; BIN_DIR="${1:?--bin-dir needs a path}" ;;
        --version) shift; VERSION="${1:?--version needs a value}" ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

TMP="$(mktemp -d "${TMPDIR:-/tmp}/hatel-install.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# Map this machine to a release target triple. Linux uses the static musl build so the binary
# runs regardless of the host glibc version — the common curl|bash portability pitfall. Returns
# non-zero (rather than exiting) on an unsupported platform, so the caller can fall back to source.
detect_target() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux) os="unknown-linux-musl" ;;
        Darwin) os="apple-darwin" ;;
        *) echo "no prebuilt for OS $os" >&2; return 1 ;;
    esac
    case "$arch" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) echo "no prebuilt for architecture $arch" >&2; return 1 ;;
    esac
    printf '%s-%s\n' "$arch" "$os"
}

verify_checksum() {
    local archive="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "${archive}.sha256"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "${archive}.sha256"
    else
        echo "no sha256 tool (sha256sum/shasum) to verify the download" >&2
        return 1
    fi
}

# Download + verify + extract the prebuilt archive. On success sets BINARY_SRC_DIR and SKILL_SRC.
download_prebuilt() {
    command -v curl >/dev/null 2>&1 || { echo "curl is required to download a prebuilt binary" >&2; return 1; }
    local target archive url
    target="$(detect_target)" || return 1
    archive="hatel-${target}.tar.gz"
    if [ -n "$VERSION" ]; then
        url="https://github.com/$REPO/releases/download/v${VERSION#v}/$archive"
    else
        url="https://github.com/$REPO/releases/latest/download/$archive"
    fi
    echo "Downloading $archive..." >&2
    if ! curl -fsSL "$url" -o "$TMP/$archive" || ! curl -fsSL "$url.sha256" -o "$TMP/$archive.sha256"; then
        return 1
    fi
    echo "Verifying checksum..." >&2
    ( cd "$TMP" && verify_checksum "$archive" ) >&2 || return 1
    echo "Extracting..." >&2
    tar -C "$TMP" -xzf "$TMP/$archive" >&2 || return 1
    BINARY_SRC_DIR="$TMP"
    SKILL_SRC="$TMP/skill/SKILL.md"
}

# Build both binaries from the checkout. On success sets BINARY_SRC_DIR and SKILL_SRC.
build_from_source() {
    if [ -z "$PROJECT_ROOT" ]; then
        echo "source build needs a hatel checkout (run this script from a clone)" >&2
        return 1
    fi
    command -v cargo >/dev/null 2>&1 || { echo "source build needs cargo — install Rust from https://rustup.rs" >&2; return 1; }
    echo "Building from source ($PROJECT_ROOT)..." >&2
    # Pin the target dir so we know exactly where the binaries land, even if the environment sets
    # CARGO_TARGET_DIR elsewhere.
    ( cd "$PROJECT_ROOT" && CARGO_TARGET_DIR="$PROJECT_ROOT/target" cargo build --release --locked --bin hatel --bin hatel-hook ) >&2 || return 1
    BINARY_SRC_DIR="$PROJECT_ROOT/target/release"
    SKILL_SRC="$PROJECT_ROOT/.claude/skills/hatel/SKILL.md"
}

# Acquire the binaries: forced source, else prebuilt with a source fallback when a checkout is
# here. A pinned --version is never silently satisfied by a source build of a possibly-different
# checkout: if that release has no asset, we fail honestly rather than build something else.
if [ "$SOURCE" = true ]; then
    [ -n "$VERSION" ] && echo "note: --version is ignored for a source build (building the checkout)" >&2
    build_from_source || exit 1
elif ! download_prebuilt; then
    if [ -z "$VERSION" ] && [ -n "$PROJECT_ROOT" ]; then
        echo "No prebuilt release for this platform — building from the checkout instead." >&2
        build_from_source || exit 1
    else
        {
            if [ -n "$VERSION" ]; then
                echo "Release v${VERSION#v} has no prebuilt binary for your platform."
                echo "Drop --version to build the checkout (if you have one), pass --source, or:"
            else
                echo "Could not download a prebuilt binary, and there's no checkout here to build from."
                echo "Run this from a cloned repo, or:"
            fi
            echo "  cargo install --git https://github.com/$REPO hatel-cli"
            echo "  cargo install --git https://github.com/$REPO hatel-hook"
        } >&2
        exit 1
    fi
fi

mkdir -p "$BIN_DIR"
for bin in hatel hatel-hook; do
    [ -x "$BINARY_SRC_DIR/$bin" ] || { echo "did not produce $bin" >&2; exit 1; }
    cp "$BINARY_SRC_DIR/$bin" "$BIN_DIR/$bin"
    chmod +x "$BIN_DIR/$bin"
    # Ad-hoc sign on macOS so Gatekeeper doesn't kill a freshly-copied binary.
    if [ "$(uname -s)" = "Darwin" ]; then
        codesign --force --sign - "$BIN_DIR/$bin" 2>/dev/null || true
    fi
done
echo "Installed hatel, hatel-hook -> $BIN_DIR"

if [ "$INSTALL_SKILL" = true ]; then
    if [ -f "$SKILL_SRC" ]; then
        mkdir -p "$SKILL_DIR"
        cp "$SKILL_SRC" "$SKILL_DIR/SKILL.md"
        echo "Installed Claude Code skill -> $SKILL_DIR"
    else
        echo "warning: skill source not found ($SKILL_SRC); skipping" >&2
    fi
fi

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        echo
        echo "note: $BIN_DIR is not on your PATH. Add it to your shell profile:"
        echo "  export PATH=\"$BIN_DIR:\$PATH\""
        ;;
esac

echo
"$BIN_DIR/hatel" --version

# Wiring and the service are opt-in post-steps (each idempotent and re-runnable). They print
# their own diagnostics; we capture the exit code so an explicitly-requested step that fails
# surfaces in this script's exit code — the binaries are installed either way. `|| rc=$?` keeps
# `set -e` from aborting the rest of the script.
rc=0
echo
if [ "$WIRE" = true ]; then
    "$BIN_DIR/hatel" init --scope user || rc=$?
else
    echo "To wire Claude Code (adds telemetry env + lifecycle hooks, idempotent):"
    echo "  hatel init        # or re-run this installer with --wire"
fi

echo
if [ "$SERVICE" = true ]; then
    "$BIN_DIR/hatel" service || rc=$?
else
    echo "For gap-free collection, install the background service (launchd/systemd):"
    echo "  hatel service     # or re-run this installer with --service"
    echo "Or run it in the foreground when you want it:  hatel serve --all"
fi

echo
echo "Check the wiring anytime: hatel doctor"
exit "$rc"
