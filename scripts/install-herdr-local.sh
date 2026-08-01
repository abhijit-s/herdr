#!/usr/bin/env bash
#
# install-herdr-local.sh — build herdr from this checkout and install it over an
# existing local binary using the same build + backup + fresh-inode swap +
# (macOS) adhoc-codesign steps used on the maintainer's machine.
#
# Works on any macOS or Linux machine that can build herdr. Run it from a clone
# of this fork after pulling the commit you want to install.
#
#   scripts/install-herdr-local.sh                 # build + swap over the herdr on PATH
#   HERDR_INSTALL_TARGET=~/.local/bin/herdr \
#       scripts/install-herdr-local.sh             # force a specific install path
#   ZIG=/path/to/zig scripts/install-herdr-local.sh  # override the zig used for the build
#   HERDR_SKIP_BUILD=1 scripts/install-herdr-local.sh  # swap an already-built target/release/herdr
#
# Notable choices (the "why"):
#   * ZIG: on Apple-Silicon macOS the Xcode Command Line Tools SDK ships a
#     libSystem.tbd that only declares most symbols under arm64e, so a stock
#     ziglang.org zig fails to link libghostty-vt ("undefined symbol: _abort"…).
#     Homebrew's `zig@0.15` carries the arm64-macos backport patch and is the
#     only zig that links cleanly. This script auto-selects it when present.
#   * Fresh-inode swap (rm + cp, never in-place cp): overwriting the running
#     binary in place poisons the kernel's vnode cache and can crash a live
#     process mapping it. Removing then copying gives the new file a new inode.
#   * Adhoc codesign (macOS): macOS AMFI refuses to exec an unsigned/altered
#     Mach-O; `codesign --sign -` applies a valid adhoc signature.
#
set -euo pipefail

repo_root() {
  cd "$(dirname "$0")/.." && pwd -P
}
ROOT="$(repo_root)"
cd "$ROOT"

OS="$(uname -s)"
SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

resolve_path() {
  # Resolve symlinks so we swap the real file (e.g. the Homebrew Cellar target
  # behind /opt/homebrew/bin/herdr), not the symlink.
  if command -v realpath >/dev/null 2>&1; then
    realpath "$1"
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$1"
  else
    # POSIX fallback: follow one level at a time.
    local p="$1"
    while [ -L "$p" ]; do p="$(readlink "$p")"; done
    printf '%s\n' "$p"
  fi
}

pick_zig() {
  if [ -n "${ZIG:-}" ]; then printf '%s\n' "$ZIG"; return; fi
  if [ "$OS" = "Darwin" ] && [ -x /opt/homebrew/opt/zig@0.15/bin/zig ]; then
    printf '%s\n' /opt/homebrew/opt/zig@0.15/bin/zig; return
  fi
  command -v zig || { echo "error: no zig found. Set ZIG=/path/to/zig." >&2; exit 1; }
}

# ── 1. build ───────────────────────────────────────────────────────────────
if [ "${HERDR_SKIP_BUILD:-0}" != "1" ]; then
  ZIG_BIN="$(pick_zig)"
  echo "==> building release (ZIG=$ZIG_BIN)"
  if [ "$OS" = "Darwin" ] && [ "$ZIG_BIN" != "/opt/homebrew/opt/zig@0.15/bin/zig" ]; then
    echo "    warning: not using Homebrew zig@0.15 — link may fail on Apple-Silicon macOS." >&2
  fi
  # Clear inherited herdr socket overrides so a build launched from inside a
  # running herdr session is unaffected; they don't influence cargo but are
  # harmless to drop.
  env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH \
    ZIG="$ZIG_BIN" cargo build --release --locked
fi

NEW="$ROOT/target/release/herdr"
[ -x "$NEW" ] || { echo "error: $NEW not found — build first (or unset HERDR_SKIP_BUILD)." >&2; exit 1; }
echo "==> built: $("$NEW" --version) ($(file -b "$NEW"))"

# ── 2. resolve install target ──────────────────────────────────────────────
if [ -n "${HERDR_INSTALL_TARGET:-}" ]; then
  TARGET="$HERDR_INSTALL_TARGET"
elif command -v herdr >/dev/null 2>&1; then
  TARGET="$(resolve_path "$(command -v herdr)")"
else
  TARGET="$HOME/.local/bin/herdr"
  echo "    no herdr on PATH; defaulting install target to $TARGET"
fi
mkdir -p "$(dirname "$TARGET")"
echo "==> install target: $TARGET"

# ── 3. backup existing ─────────────────────────────────────────────────────
if [ -e "$TARGET" ]; then
  BACKUP="$TARGET.pre-$SHA"
  cp -p "$TARGET" "$BACKUP"
  echo "==> backed up existing -> $BACKUP"
fi

# ── 4. fresh-inode swap ────────────────────────────────────────────────────
OLD_INODE="$( [ -e "$TARGET" ] && stat -f %i "$TARGET" 2>/dev/null || stat -c %i "$TARGET" 2>/dev/null || echo none )"
rm -f "$TARGET"
cp "$NEW" "$TARGET"
chmod 755 "$TARGET"
NEW_INODE="$(stat -f %i "$TARGET" 2>/dev/null || stat -c %i "$TARGET")"
echo "==> swapped (inode $OLD_INODE -> $NEW_INODE)"

# ── 5. codesign (macOS only) ───────────────────────────────────────────────
if [ "$OS" = "Darwin" ]; then
  codesign --force --sign - --timestamp=none "$TARGET"
  # The `-dv` read can race the just-completed signing and false-negative on
  # the first attempt, so retry briefly before treating it as a failure.
  signed=0
  for _ in 1 2 3 4 5; do
    if codesign -dv "$TARGET" 2>&1 | grep -q 'flags=0x2(adhoc)'; then signed=1; break; fi
    sleep 0.3
  done
  if [ "$signed" = 1 ]; then
    echo "==> adhoc-signed (flags=0x2)"
  else
    echo "error: adhoc signature not applied." >&2; exit 1
  fi
fi

# ── 6. verify + remind ─────────────────────────────────────────────────────
echo "==> installed: $("$TARGET" --version) ($(file -b "$TARGET"))"
echo
echo "Done. Restart the server to run the new binary:"
echo "    herdr server stop && herdr        # a running server keeps the old binary until restarted"

# A version jump / swap comes up with an EMPTY plugin registry, silently
# breaking plugin-backed keybindings. Point at the relink helper if present.
RELINK="$HOME/.config/herdr/scripts/herdr-relink-plugins.sh"
if [ -x "$RELINK" ]; then
  echo
  echo "Then relink plugins (a swap clears herdr's plugin registry):"
  echo "    $RELINK        # or: make -C ~/.dotfiles herdr-plugins"
fi
