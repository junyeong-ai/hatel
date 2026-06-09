#!/usr/bin/env bash
# Uninstall hatel: unwire Claude Code (while the binary is still present, so no
# hook is left pointing at a deleted file), then remove the binaries and the skill. Collected
# telemetry under the state dir is left untouched — data is removed only by you, by hand.
set -euo pipefail

BIN_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
SKILL_DIR="$HOME/.claude/skills/hatel"
KEEP_SKILL=false
KEEP_WIRING=false

usage() {
    cat <<'EOF'
Usage: uninstall.sh [options]

Options:
      --keep-skill     Leave the Claude Code skill installed
      --keep-wiring    Leave settings.json wiring in place (not recommended)
      --bin-dir DIR    Where the binaries were installed (default: ~/.local/bin)
  -h, --help           Show this help
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --keep-skill) KEEP_SKILL=true ;;
        --keep-wiring) KEEP_WIRING=true ;;
        --bin-dir) shift; BIN_DIR="${1:?--bin-dir needs a path}" ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

# Unwire first, using the binary's own typed remover, so it strips exactly our hooks and leaves
# the user's. This must happen before the binary is deleted — and fail closed: if unwiring fails
# (e.g. a malformed settings.json), abort rather than delete the binary and strand hook entries
# pointing at a file that no longer exists.
if [ "$KEEP_WIRING" = false ]; then
    if [ -x "$BIN_DIR/hatel" ]; then
        if ! "$BIN_DIR/hatel" init --scope user --remove; then
            echo "error: could not unwire settings.json (malformed or unreadable?)." >&2
            echo "       Fix it, or re-run with --keep-wiring to remove the binaries anyway" >&2
            echo "       (that would leave hook entries pointing at the deleted binary)." >&2
            exit 1
        fi
    else
        # Without the binary we can't unwire — and deleting the hook would strand any settings
        # entries pointing at it. Fail closed; --keep-wiring is the explicit opt-out.
        echo "error: $BIN_DIR/hatel not found, so the wiring can't be removed." >&2
        echo "       Reinstall it and run \`hatel init --remove\`, or re-run with" >&2
        echo "       --keep-wiring to remove the remaining binaries and leave settings.json." >&2
        exit 1
    fi
fi

# Remove the background service too, while the binary can still do it. A dangling launchd/systemd
# unit pointing at a deleted binary only fail-retries under throttling — lower stakes than a
# dangling hook — so this is best-effort and never aborts the uninstall.
if [ -x "$BIN_DIR/hatel" ]; then
    "$BIN_DIR/hatel" service --remove || true
fi

for bin in hatel hatel-hook; do
    if [ -f "$BIN_DIR/$bin" ]; then
        rm -f "$BIN_DIR/$bin"
        echo "Removed $BIN_DIR/$bin"
    fi
done

if [ "$KEEP_SKILL" = false ] && [ -d "$SKILL_DIR" ]; then
    rm -rf "$SKILL_DIR"
    echo "Removed $SKILL_DIR"
    # Tidy now-empty parents, but never beyond ~/.claude/skills.
    rmdir "$HOME/.claude/skills" 2>/dev/null || true
fi

echo
echo "Done. Collected telemetry was left untouched; remove it yourself if you want — its"
echo "location is shown by \`hatel doctor\` (default ~/.local/state/hatel)."
