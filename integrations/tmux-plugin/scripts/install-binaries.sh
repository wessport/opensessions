#!/usr/bin/env sh

set -eu

PLUGIN_DIR="${1:-$(cd "$(dirname "$0")/../../.." && pwd)}"
VERSION="${OPENSESSIONS_RELEASE_VERSION:-$(grep -o '"version": *"[^"]*"' "$PLUGIN_DIR/package.json" 2>/dev/null | head -1 | cut -d'"' -f4)}"
RELEASE_BASE="${OPENSESSIONS_RELEASE_BASE:-https://github.com/ataraxy-labs/opensessions/releases/download}"
BIN_DIR="$PLUGIN_DIR/bin"

if [ "${OPENSESSIONS_SKIP_BINARY_DOWNLOAD:-}" = "1" ]; then
  exit 0
fi

if [ -z "$VERSION" ]; then
  echo "opensessions: could not read package version from $PLUGIN_DIR/package.json" >&2
  exit 1
fi

github_repo_identity() {
  printf '%s\n' "$1" | awk '
    {
      original = tolower($0)
      value = original
      sub(/^https?:\/\/github\.com\//, "", value)
      sub(/^git@github\.com:/, "", value)
      sub(/^ssh:\/\/git@github\.com\//, "", value)
      sub(/\/?\.git\/?$/, "", value)
      sub(/\/$/, "", value)
      if (value == original || value !~ /^[^\/]+\/[^\/]+$/) exit 1
      print value
    }'
}

if [ -z "${OPENSESSIONS_RELEASE_BASE:-}" ]; then
  ORIGIN_URL="$(git -C "$PLUGIN_DIR" remote get-url origin 2>/dev/null || true)"
  EXPECTED_ORIGIN="${RELEASE_BASE%/releases/download}"
  ORIGIN_ID="$(github_repo_identity "$ORIGIN_URL" 2>/dev/null || true)"
  EXPECTED_ID="$(github_repo_identity "$EXPECTED_ORIGIN" 2>/dev/null || true)"
  if [ -n "$ORIGIN_URL" ] && { [ -z "$ORIGIN_ID" ] || [ "$ORIGIN_ID" != "$EXPECTED_ID" ]; }; then
    echo "opensessions: refusing upstream binaries for non-matching origin: $ORIGIN_URL" >&2
    echo "opensessions: set OPENSESSIONS_RELEASE_BASE to this fork's releases/download URL, or skip download and build locally" >&2
    exit 1
  fi
fi

target_triple() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os:$arch" in
    Darwin:arm64|Darwin:aarch64) printf '%s\n' "aarch64-apple-darwin" ;;
    Darwin:x86_64|Darwin:amd64) printf '%s\n' "x86_64-apple-darwin" ;;
    Linux:x86_64|Linux:amd64) printf '%s\n' "x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) printf '%s\n' "aarch64-unknown-linux-gnu" ;;
    *)
      echo "opensessions: unsupported platform for prebuilt binary: $os/$arch" >&2
      return 1
      ;;
  esac
}

download() {
  url="$1"
  dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    echo "opensessions: curl or wget is required to download prebuilt binaries" >&2
    return 1
  fi
}

TRIPLE="$(target_triple)"
ARTIFACT="opensessions-sidebar-${TRIPLE}.tar.gz"
URL="$RELEASE_BASE/v$VERSION/$ARTIFACT"
CHECKSUM_URL="$URL.sha256"
SIDEBAR_BIN="$BIN_DIR/opensessions-sidebar"
SERVER_BIN="$BIN_DIR/opensessions-server"
LAZYDIFF_BIN="$BIN_DIR/lazydiff"
VERSION_FILE="$BIN_DIR/.opensessions-version"
SOURCE_FILE="$BIN_DIR/.opensessions-release-source"
RELEASE_SOURCE="${RELEASE_BASE%/}:v$VERSION"

if [ -x "$SIDEBAR_BIN" ] && [ -x "$SERVER_BIN" ] && [ -x "$LAZYDIFF_BIN" ] \
  && [ "$(cat "$VERSION_FILE" 2>/dev/null || true)" = "$VERSION" ] \
  && [ "$(cat "$SOURCE_FILE" 2>/dev/null || true)" = "$RELEASE_SOURCE" ]; then
  exit 0
fi

PARENT_DIR="$(dirname "$BIN_DIR")"
STAGE_DIR="$PARENT_DIR/.opensessions-bin-stage.$$"
BACKUP_DIR="$PARENT_DIR/.opensessions-bin-backup.$$"
TMP="$PARENT_DIR/.opensessions-download.$$.$ARTIFACT"
CHECKSUM="$TMP.sha256"
cleanup() {
  if [ ! -e "$BIN_DIR" ] && [ -e "$BACKUP_DIR" ]; then
    mv "$BACKUP_DIR" "$BIN_DIR" 2>/dev/null || true
  fi
  rm -rf "$TMP" "$CHECKSUM" "$STAGE_DIR" "$BACKUP_DIR"
}
trap cleanup EXIT INT TERM

mkdir -p "$STAGE_DIR"

echo "opensessions: downloading prebuilt binaries for $TRIPLE (v$VERSION)" >&2
if ! download "$URL" "$TMP"; then
  echo "opensessions: failed to download $URL" >&2
  echo "opensessions: install from a released tag, or build locally with: cargo build --release" >&2
  exit 1
fi

if ! download "$CHECKSUM_URL" "$CHECKSUM"; then
  echo "opensessions: release checksum is missing: $CHECKSUM_URL" >&2
  exit 1
fi
EXPECTED_SUM="$(awk 'NR == 1 { print $1 }' "$CHECKSUM")"
case "$EXPECTED_SUM" in
  *[!0-9a-fA-F]*|"") echo "opensessions: invalid release checksum" >&2; exit 1 ;;
esac
[ "${#EXPECTED_SUM}" -eq 64 ] || { echo "opensessions: invalid release checksum" >&2; exit 1; }
if command -v shasum >/dev/null 2>&1; then
  ACTUAL_SUM="$(shasum -a 256 "$TMP" | awk '{ print $1 }')"
elif command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SUM="$(sha256sum "$TMP" | awk '{ print $1 }')"
else
  echo "opensessions: shasum or sha256sum is required to verify release binaries" >&2
  exit 1
fi
[ "$ACTUAL_SUM" = "$EXPECTED_SUM" ] || { echo "opensessions: release checksum verification failed" >&2; exit 1; }

tar -xzf "$TMP" -C "$STAGE_DIR"
for name in opensessions-sidebar opensessions-server lazydiff; do
  [ -f "$STAGE_DIR/$name" ] || { echo "opensessions: release bundle is missing $name" >&2; exit 1; }
  chmod +x "$STAGE_DIR/$name"
  [ -x "$STAGE_DIR/$name" ] || { echo "opensessions: release bundle contains unusable $name" >&2; exit 1; }
done
printf '%s\n' "$VERSION" >"$STAGE_DIR/.opensessions-version"
printf '%s\n' "$RELEASE_SOURCE" >"$STAGE_DIR/.opensessions-release-source"

# Publish only after the complete bundle and checksum validate. Restore the old
# directory if publishing the staged directory fails.
if [ -e "$BIN_DIR" ]; then
  mv "$BIN_DIR" "$BACKUP_DIR"
fi
if ! mv "$STAGE_DIR" "$BIN_DIR"; then
  [ ! -e "$BACKUP_DIR" ] || mv "$BACKUP_DIR" "$BIN_DIR"
  echo "opensessions: failed to publish validated binary bundle" >&2
  exit 1
fi
rm -rf "$BACKUP_DIR"
