#!/usr/bin/env bash
# Download the pinned PDFium build used for M0 verification.
# This is a development-only helper; the application never downloads anything.
set -euo pipefail

VERSION="8165"
URL="https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F${VERSION}/pdfium-linux-x64.tgz"
SHA256="ba7865e16c0271daab75256790be06c26272190afe6e2a70eae0bb39bb99089c"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${ROOT}/.deps/pdfium"
ARCHIVE="${DEST}/pdfium-linux-x64.tgz"

mkdir -p "${DEST}"
if [[ ! -f "${ARCHIVE}" ]]; then
  echo "downloading ${URL}"
  curl -fL --retry 3 -o "${ARCHIVE}" "${URL}"
fi
echo "${SHA256}  ${ARCHIVE}" | sha256sum -c -
tar -xzf "${ARCHIVE}" -C "${DEST}"
echo "PDFium ready: export PDFIUM_LIB_PATH=${DEST}/lib/libpdfium.so"
