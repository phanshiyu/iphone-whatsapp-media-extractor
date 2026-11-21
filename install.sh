#!/bin/sh
# One-command installer for iPhone WhatsApp Media Extractor (macOS).
#   Install / update: curl -fsSL https://raw.githubusercontent.com/phanshiyu/iphone-whatsapp-media-extractor/main/install.sh | sh
#   Uninstall:        curl -fsSL .../install.sh | sh -s -- --uninstall
# Flags: --uninstall, --no-deps, --cli-only
set -eu

REPO="phanshiyu/iphone-whatsapp-media-extractor"
BASE="https://github.com/$REPO/releases/latest/download"
APP_NAME="iPhone WhatsApp Media Extractor.app"
CLI_NAME="iphone-whatsapp-media-extractor"
ZIP="iPhone-WhatsApp-Media-Extractor-macos.zip"
TARBALL="iphone-whatsapp-media-extractor-macos-universal.tar.gz"

UNINSTALL=0
NO_DEPS=0
CLI_ONLY=0

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

for arg in "$@"; do
  case "$arg" in
    --uninstall) UNINSTALL=1 ;;
    --no-deps) NO_DEPS=1 ;;
    --cli-only) CLI_ONLY=1 ;;
    -h|--help)
      say "usage: install.sh [--uninstall] [--no-deps] [--cli-only]"
      exit 0
      ;;
    *) die "unknown option: $arg" ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || die "this installer only supports macOS. On other systems: cargo install --git https://github.com/$REPO $CLI_NAME"

HOME_DIR=${HOME:?HOME is not set}
SYS_APPS="/Applications"
USER_APPS="$HOME_DIR/Applications"
SYS_BIN="/usr/local/bin"
USER_BIN="$HOME_DIR/.local/bin"

if [ "$UNINSTALL" -eq 1 ]; then
  for d in "$SYS_APPS" "$USER_APPS"; do
    if [ -d "$d/$APP_NAME" ]; then
      if rm -rf "${d:?}/$APP_NAME" 2>/dev/null; then
        say "Removed $d/$APP_NAME"
      else
        warn "could not remove $d/$APP_NAME (try: sudo rm -rf \"$d/$APP_NAME\")"
      fi
    fi
  done
  for d in "$SYS_BIN" "$USER_BIN"; do
    if [ -e "$d/$CLI_NAME" ]; then
      if rm -f "$d/$CLI_NAME" 2>/dev/null; then
        say "Removed $d/$CLI_NAME"
      else
        warn "could not remove $d/$CLI_NAME (try: sudo rm \"$d/$CLI_NAME\")"
      fi
    fi
  done
  say "Uninstalled. libimobiledevice was left in place (remove with: brew uninstall libimobiledevice)."
  exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl is required"

TMP=$(mktemp -d)
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM

download() {
  # download <file>: fetch it and its .sha256, then verify
  say "Downloading $1 ..."
  curl -fsSL "$BASE/$1" -o "$TMP/$1" || die "download failed: $BASE/$1"
  curl -fsSL "$BASE/$1.sha256" -o "$TMP/$1.sha256" || die "download failed: $BASE/$1.sha256"
  (cd "$TMP" && shasum -a 256 -c "$1.sha256" >/dev/null) || die "checksum mismatch for $1"
}

writable_dir() {
  # writable_dir <dir>: true if it exists and is writable, or can be created
  if [ -d "$1" ]; then [ -w "$1" ]; else mkdir -p "$1" 2>/dev/null && [ -w "$1" ]; fi
}

if [ "$CLI_ONLY" -eq 0 ]; then
  download "$ZIP"
  mkdir -p "$TMP/app"
  ditto -x -k "$TMP/$ZIP" "$TMP/app"
  [ -d "$TMP/app/$APP_NAME" ] || die "the downloaded archive does not contain $APP_NAME"

  if writable_dir "$SYS_APPS" && { [ ! -e "$SYS_APPS/$APP_NAME" ] || [ -w "$SYS_APPS/$APP_NAME" ]; }; then
    APPS="$SYS_APPS"
  else
    mkdir -p "$USER_APPS"
    APPS="$USER_APPS"
  fi
  PREVIOUS=""
  if [ -d "$APPS/$APP_NAME" ]; then
    PREVIOUS=$(defaults read "$APPS/$APP_NAME/Contents/Info" CFBundleShortVersionString 2>/dev/null || echo "?")
    rm -rf "${APPS:?}/$APP_NAME"
  fi
  ditto "$TMP/app/$APP_NAME" "$APPS/$APP_NAME"
  xattr -dr com.apple.quarantine "$APPS/$APP_NAME" 2>/dev/null || true
  NEW=$(defaults read "$APPS/$APP_NAME/Contents/Info" CFBundleShortVersionString 2>/dev/null || echo "?")
  if [ -n "$PREVIOUS" ]; then
    say "Updated to v$NEW (was v$PREVIOUS): $APPS/$APP_NAME"
  else
    say "Installed v$NEW: $APPS/$APP_NAME"
  fi
fi

download "$TARBALL"
mkdir -p "$TMP/cli"
tar -xzf "$TMP/$TARBALL" -C "$TMP/cli"
CLI_SRC=$(find "$TMP/cli" -type f -name "$CLI_NAME" | head -n 1)
[ -n "$CLI_SRC" ] || die "the downloaded archive does not contain $CLI_NAME"
if writable_dir "$SYS_BIN"; then
  BIN_DIR="$SYS_BIN"
else
  BIN_DIR="$USER_BIN"
  mkdir -p "$BIN_DIR"
fi
cp "$CLI_SRC" "$BIN_DIR/$CLI_NAME.new"
chmod +x "$BIN_DIR/$CLI_NAME.new"
xattr -d com.apple.quarantine "$BIN_DIR/$CLI_NAME.new" 2>/dev/null || true
mv -f "$BIN_DIR/$CLI_NAME.new" "$BIN_DIR/$CLI_NAME"
say "Installed command line tool: $BIN_DIR/$CLI_NAME"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH. Add it with: echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac

if [ "$NO_DEPS" -eq 0 ]; then
  have_idevice=0
  for d in /opt/homebrew/bin /usr/local/bin /opt/local/bin; do
    [ -x "$d/idevicebackup2" ] && have_idevice=1
  done
  command -v idevicebackup2 >/dev/null 2>&1 && have_idevice=1
  if [ "$have_idevice" -eq 0 ]; then
    BREW=$(command -v brew 2>/dev/null || true)
    [ -z "$BREW" ] && [ -x /opt/homebrew/bin/brew ] && BREW=/opt/homebrew/bin/brew
    [ -z "$BREW" ] && [ -x /usr/local/bin/brew ] && BREW=/usr/local/bin/brew
    if [ -n "$BREW" ]; then
      say "Installing libimobiledevice (needed to back up a connected iPhone) ..."
      "$BREW" install libimobiledevice || warn "could not install libimobiledevice; run: brew install libimobiledevice"
    else
      warn "libimobiledevice is needed to back up a connected iPhone, and Homebrew was not found. Install Homebrew from https://brew.sh and run: brew install libimobiledevice. Using an existing backup works without it."
    fi
  fi
fi

say ""
if [ "$CLI_ONLY" -eq 0 ]; then
  say "Done! Open \"iPhone WhatsApp Media Extractor\" from Applications or Launchpad."
else
  say "Done! Run: $CLI_NAME --help"
fi
