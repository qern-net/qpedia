#!/usr/bin/env bash
# Download the pdfium dynamic library for the current platform into ./vendor/pdfium/.
# Source: https://github.com/bblanchon/pdfium-binaries
set -euo pipefail

VERSION="${PDFIUM_VERSION:-chromium/7802}"

uname_s=$(uname -s)
case "$uname_s" in
  Linux*)               PLATFORM=linux-x64; LIB=libpdfium.so;    SUBDIR=lib ;;
  Darwin*)              PLATFORM=mac-x64;   LIB=libpdfium.dylib; SUBDIR=lib ;;
  MINGW*|MSYS*|CYGWIN*) PLATFORM=win-x64;   LIB=pdfium.dll;      SUBDIR=bin ;;
  *) echo "unsupported OS: $uname_s" >&2; exit 1 ;;
esac

URL="https://github.com/bblanchon/pdfium-binaries/releases/download/${VERSION}/pdfium-${PLATFORM}.tgz"
DEST="vendor/pdfium"

mkdir -p "$DEST"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "downloading $URL"
curl -sSLf "$URL" -o "$TMP/pdfium.tgz"
tar -xzf "$TMP/pdfium.tgz" -C "$TMP"

cp "$TMP/$SUBDIR/$LIB" "$DEST/"
echo "installed: $DEST/$LIB"
