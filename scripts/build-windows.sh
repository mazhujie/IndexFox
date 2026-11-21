#!/usr/bin/env bash
# Cross-build the Windows x64 GUI and CLI, then assemble the complete portable
# ZIP: app, PDFium, a statically linked Tesseract OCR (chi_sim+eng), and the
# Microsoft Fixed Version WebView2 runtime. No installer, no registry or system
# changes.
#
# Prerequisites on Linux:
#   rustup target add x86_64-pc-windows-gnu
#   x86_64-w64-mingw32-gcc (MinGW; used as linker and C compiler)
#   scripts/build-tesseract-windows.sh (once; static runtime under .deps/windows/)
#   scripts/fetch-windows-runtime.sh (once; PDFium + WebView2 under .deps/windows/)
#   app/dist built with: (cd app && npm install && npm run build)
#
# Set SKIP_WEBVIEW2=1 only for a development ZIP that relies on an installed
# Evergreen runtime. Release packages must include the Fixed Version runtime.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

TARGET="x86_64-pc-windows-gnu"
CC="${CC_x86_64_pc_windows_gnu:-x86_64-w64-mingw32-gcc}"
LINKER="${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER:-x86_64-w64-mingw32-gcc}"
export CC_x86_64_pc_windows_gnu="${CC}"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${LINKER}"

DEPS="${DEPS_DIR:-${ROOT}/.deps}"
WIN="${WIN_DEPS_DIR:-${DEPS}/windows}"
TESS_SRC="${TESSERACT_RUNTIME_DIR:-${WIN}/tesseract-minimal}"
PDFIUM_DIR="${PDFIUM_DIR:-${DEPS}/pdfium-win}"
OUT="${ROOT}/target/portable"
ZIP="${ROOT}/target/IndexFox-windows-x64-portable.zip"
STAGE="${OUT}.tmp.$$"
ZIP_TMP="${ZIP}.tmp.$$"
trap 'rm -rf "${STAGE}" "${ZIP_TMP}"' EXIT

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

sha256_of() { sha256sum "$1" | awk '{print $1}'; }

verify_sha() {
  local file=$1 want=$2 label=$3 actual
  [[ -f "${file}" ]] || die "missing ${label}: ${file}"
  actual=$(sha256_of "${file}")
  [[ "${actual}" == "${want}" ]] || die "SHA256 mismatch for ${label}: expected ${want}, got ${actual} (${file})"
}

# --- Tool and source prerequisites (fail before a long cargo build) -------
[[ "$(uname -m)" == "x86_64" ]] || die "build host must be x86_64"
command -v "${CC}" >/dev/null || die "missing ${CC}; install mingw-w64 or set CC_x86_64_pc_windows_gnu"
command -v cargo >/dev/null || die "missing cargo"
command -v python3 >/dev/null || die "missing python3 (used to write the ZIP)"
[[ -f app/dist/index.html ]] || die "missing app/dist; run: (cd app && npm install && npm run build)"
[[ -s THIRD_PARTY_NOTICES.md ]] || die "missing repository THIRD_PARTY_NOTICES.md"

# Tesseract: static build with its own manifest, licenses and build provenance.
[[ -f "${TESS_SRC}/tesseract.exe" ]] || \
  die "missing ${TESS_SRC}/tesseract.exe; run scripts/build-tesseract-windows.sh (or set TESSERACT_RUNTIME_DIR)"
[[ -f "${TESS_SRC}/SHA256SUMS.txt" ]] || \
  die "missing ${TESS_SRC}/SHA256SUMS.txt; rebuild with scripts/build-tesseract-windows.sh"
[[ -f "${TESS_SRC}/BUILD-INFO.txt" ]] || \
  die "missing ${TESS_SRC}/BUILD-INFO.txt; rebuild with scripts/build-tesseract-windows.sh"
( cd "${TESS_SRC}" && sha256sum -c --quiet SHA256SUMS.txt ) || \
  die "Tesseract runtime manifest verification failed: ${TESS_SRC}/SHA256SUMS.txt"
[[ -d "${TESS_SRC}/tessdata" ]] || die "missing ${TESS_SRC}/tessdata"
TESS_LICENSE_ROOT=(
  Tesseract-Apache-2.0.txt
  Tesseract-AUTHORS.txt
  tessdata-Apache-2.0.txt
)
TESS_LICENSE_DEPS=(
  Leptonica-BSD-2-Clause.txt
  libpng-LICENSE.txt
  zlib-LICENSE.txt
  GCC-Runtime-Library-Exception-3.1.txt
  GPL-3.0.txt
  MinGW-w64-COPYRIGHT.txt
)
for file in "${TESS_LICENSE_ROOT[@]}" "${TESS_LICENSE_DEPS[@]}"; do
  [[ -s "${TESS_SRC}/licenses/${file}" ]] || \
    die "missing Tesseract license evidence ${TESS_SRC}/licenses/${file}; rebuild with scripts/build-tesseract-windows.sh"
done
[[ -d "${TESS_SRC}/licenses/libpng-LICENSES" ]] || die "missing libpng component license texts"

[[ -f "${PDFIUM_DIR}/pdfium.dll" ]] || \
  die "missing ${PDFIUM_DIR}/pdfium.dll; run scripts/fetch-windows-runtime.sh"
verify_sha "${PDFIUM_DIR}/pdfium.dll" \
  "31d249b8535dd5f351ae7f4b714dc4bb1d76b6069b321a309bc99271dd65f566" "pdfium.dll"
[[ -d "${PDFIUM_DIR}/licenses" ]] || die "missing PDFium third-party licenses ${PDFIUM_DIR}/licenses"

WEBVIEW2_SRC=""
WV_LIC=""
if [[ "${SKIP_WEBVIEW2:-0}" != "1" ]]; then
  WEBVIEW2_SRC=$(find "${WIN}/webview2" -mindepth 1 -maxdepth 1 -type d \
    -name 'Microsoft.WebView2.FixedVersionRuntime.*.x64' -print -quit 2>/dev/null || true)
  [[ -n "${WEBVIEW2_SRC}" && -f "${WEBVIEW2_SRC}/msedgewebview2.exe" ]] || \
    die "missing Fixed Version WebView2 runtime under ${WIN}/webview2; run scripts/fetch-windows-runtime.sh (or set SKIP_WEBVIEW2=1 for a dev-only ZIP)"
  WV_LIC="${WIN}/licenses"
  [[ -s "${WV_LIC}/WebView2-Fixed-LICENSE.txt" ]] || \
    die "missing ${WV_LIC}/WebView2-Fixed-LICENSE.txt; run scripts/fetch-windows-runtime.sh"
  [[ -s "${WV_LIC}/WebView2-third-party-NOTICE.txt" ]] || \
    die "missing ${WV_LIC}/WebView2-third-party-NOTICE.txt; run scripts/fetch-windows-runtime.sh"
  echo "WebView2 Fixed Version: $(basename "${WEBVIEW2_SRC}")"
else
  echo "warning: SKIP_WEBVIEW2=1; ZIP will require an installed Evergreen WebView2 runtime" >&2
fi

# --- Build ----------------------------------------------------------------
cargo build --release --target "${TARGET}" -p indexfox-cli
cargo build --release --target "${TARGET}" \
  --manifest-path app/src-tauri/Cargo.toml --features custom-protocol

APP_DIR="app/src-tauri/target/${TARGET}/release"
[[ -f "${APP_DIR}/indexfox-app.exe" ]] || die "missing ${APP_DIR}/indexfox-app.exe"
[[ -f "${APP_DIR}/WebView2Loader.dll" ]] || die "missing ${APP_DIR}/WebView2Loader.dll"
[[ -f "${ROOT}/target/${TARGET}/release/indexfox-cli.exe" ]] || \
  die "missing target/${TARGET}/release/indexfox-cli.exe"

# --- Assemble -------------------------------------------------------------
rm -rf "${STAGE}"
mkdir -p "${STAGE}/ocr/tessdata" "${STAGE}/licenses/Tesseract-dependencies" "${STAGE}/runtime/webview2"

cp "${APP_DIR}/indexfox-app.exe" "${STAGE}/IndexFox.exe"
cp "${ROOT}/target/${TARGET}/release/indexfox-cli.exe" "${STAGE}/indexfox-cli.exe"
# Tauri links WebView2Loader.dll dynamically; without it the GUI starts and
# hangs before creating a window.
cp "${APP_DIR}/WebView2Loader.dll" "${STAGE}/WebView2Loader.dll"
cp "${PDFIUM_DIR}/pdfium.dll" "${STAGE}/pdfium.dll"

# OCR: one statically linked tesseract.exe, models and configs. No DLL closure.
cp "${TESS_SRC}/tesseract.exe" "${STAGE}/ocr/tesseract.exe"
cp -R "${TESS_SRC}/tessdata/." "${STAGE}/ocr/tessdata/"

# WebView2 must sit directly at runtime/webview2/msedgewebview2.exe: the app
# bootstrap checks exactly that path before creating the WebView2 environment.
if [[ -n "${WEBVIEW2_SRC}" ]]; then
  cp -R "${WEBVIEW2_SRC}/." "${STAGE}/runtime/webview2/"
  [[ -f "${STAGE}/runtime/webview2/msedgewebview2.exe" ]] || \
    die "WebView2 copy incomplete: runtime/webview2/msedgewebview2.exe missing"
fi

# License evidence: Tesseract build manifest + exact PDFium third-party set +
# WebView2 EULA/notice. Never copy the retired UB Mannheim license cache.
for file in "${TESS_LICENSE_ROOT[@]}"; do
  cp "${TESS_SRC}/licenses/${file}" "${STAGE}/licenses/${file}"
done
for file in "${TESS_LICENSE_DEPS[@]}"; do
  cp "${TESS_SRC}/licenses/${file}" "${STAGE}/licenses/Tesseract-dependencies/${file}"
done
cp -R "${TESS_SRC}/licenses/libpng-LICENSES" "${STAGE}/licenses/Tesseract-dependencies/"
cp "${TESS_SRC}/BUILD-INFO.txt" "${STAGE}/licenses/Tesseract-BUILD-INFO.txt"
cp "${TESS_SRC}/SHA256SUMS.txt" "${STAGE}/licenses/Tesseract-SHA256SUMS.txt"
mkdir -p "${STAGE}/licenses/PDFium-third-party"
cp "${PDFIUM_DIR}/LICENSE" "${STAGE}/licenses/PDFium-binaries-MIT.txt"
cp "${PDFIUM_DIR}/licenses/pdfium.txt" "${STAGE}/licenses/PDFium-BSD-3-Clause.txt"
cp -R "${PDFIUM_DIR}/licenses/." "${STAGE}/licenses/PDFium-third-party/"
if [[ -n "${WV_LIC}" ]]; then
  cp "${WV_LIC}/WebView2-Fixed-LICENSE.txt" "${STAGE}/licenses/WebView2-Fixed-LICENSE.txt"
  cp "${WV_LIC}/WebView2-third-party-NOTICE.txt" "${STAGE}/licenses/WebView2-third-party-NOTICE.txt"
fi

cat > "${STAGE}/licenses/SOURCES.txt" <<'EOF'
IndexFox M4.7 Windows package provenance
=======================================

Tesseract
  static CLI build; versions, toolchain, link flags, PE imports and
  tesseract.exe SHA256 are in Tesseract-BUILD-INFO.txt.
  Every file in the build is listed in Tesseract-SHA256SUMS.txt.
  Apache-2.0: Tesseract-Apache-2.0.txt, Tesseract-AUTHORS.txt.
  tessdata_fast models: tessdata-Apache-2.0.txt.
  Static dependencies: Tesseract-dependencies/.

PDFium
  Chromium 160.0.8165.0 (bblanchon/pdfium-binaries, build 8165).
  pdfium.dll SHA256 31d249b8535dd5f351ae7f4b714dc4bb1d76b6069b321a309bc99271dd65f566.
  BSD-3-Clause + third-party licenses: PDFium-BSD-3-Clause.txt,
  PDFium-third-party/. Packaging MIT: PDFium-binaries-MIT.txt.

Microsoft Edge WebView2 Runtime (Fixed Version)
  version 158.0.4370.33 x64, from the official Microsoft download page.
  Complete CAB contents, unmodified, under runtime/webview2/.
  EULA: WebView2-Fixed-LICENSE.txt. Notice: WebView2-third-party-NOTICE.txt.
  Distribution guidance:
  https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution
EOF

cat > "${STAGE}/README.txt" <<'EOF'
IndexFox · 索引狐 — Windows x64 portable
========================================
No installer, no admin rights required. Extract anywhere on a local disk and
run IndexFox.exe. Default data directory: %LOCALAPPDATA%\IndexFox\

Measured target: Windows 11 x64. Windows 10 is not part of this measurement;
see the WebView2 notes below before running there.

Layout
------
IndexFox.exe              GUI (Tauri + WebView2)
indexfox-cli.exe          command-line index/search
pdfium.dll                PDF renderer (loaded from the EXE directory)
WebView2Loader.dll        Tauri WebView2 loader
ocr\tesseract.exe         statically linked OCR engine (no DLL closure)
ocr\tessdata\             eng + chi_sim models and configs
runtime\webview2\         complete Microsoft Edge WebView2 Fixed Version runtime
                          (msedgewebview2.exe is directly in this folder)
licenses\                 exact third-party licenses and build provenance
SHA256SUMS.txt            hashes of every packaged file

Automatic package-relative detection
------------------------------------
No environment variables are required.

WebView2: IndexFox.exe looks for runtime\webview2\msedgewebview2.exe next to
itself and sets WEBVIEW2_BROWSER_EXECUTABLE_FOLDER for its own process before
WebView2 starts. It does not install or update the runtime, use the Evergreen
runtime, or write registry keys.

OCR: IndexFox.exe and indexfox-cli.exe look for ocr\tesseract.exe next to the
EXE and use ocr\tessdata for models. Example (no path flags needed):
  indexfox-cli.exe --db index.db index C:\Docs --ocr --lang chi_sim+eng
Optional overrides, only if you replace the bundled runtime:
  set TESSERACT_PATH=C:\path\to\tesseract.exe
  set TESSDATA_PREFIX=C:\path\to\tessdata

WebView2 Fixed Version notes
----------------------------
The bundled runtime never auto-updates. Ship a newer package to pick up
security fixes. It cannot run from a network location or UNC path: keep the
extracted package on a local folder.

On Windows 10 (not Windows 11), unpackaged Win32 apps using Fixed Version 120+
must grant AppContainer read/execute on the runtime folder; otherwise the
WebView2 window may fail to start. From the package folder in Command Prompt:
  icacls runtime\webview2 /grant *S-1-15-2-2:(OI)(CI)(RX)
  icacls runtime\webview2 /grant *S-1-15-2-1:(OI)(CI)(RX)
Then confirm that ALL APPLICATION PACKAGES and ALL RESTRICTED APPLICATION
PACKAGES have Read & execute in the folder's Security tab. Official source:
https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution

Verification
------------
  certutil -hashfile <file> SHA256
  # or in PowerShell:
  Get-FileHash -Algorithm SHA256 -Path .\IndexFox.exe
Compare with SHA256SUMS.txt. License texts, sources and Tesseract build
provenance are in licenses\SOURCES.txt, licenses\Tesseract-BUILD-INFO.txt and
licenses\Tesseract-SHA256SUMS.txt.

Required notice (Microsoft WebView2 Fixed Version license, section 8)
---------------------------------------------------------------------
IndexFox includes Microsoft Defender SmartScreen. SmartScreen is enabled by
default and collects and sends end-user information to Microsoft as disclosed in
Microsoft's Privacy Statement (https://aka.ms/privacy) and the Microsoft Edge
Privacy Whitepaper
(https://learn.microsoft.com/microsoft-edge/privacy-whitepaper#smartscreen).
EOF

# Single source for the notices: the repository copy.
cp "${ROOT}/THIRD_PARTY_NOTICES.md" "${STAGE}/THIRD_PARTY_NOTICES.md"

# --- Hashes + ZIP ---------------------------------------------------------
(
  cd "${STAGE}"
  find . -type f ! -name 'SHA256SUMS.txt' -print0 |
    LC_ALL=C sort -z |
    xargs -0 sha256sum
) > "${STAGE}/SHA256SUMS.txt"

rm -f "${ZIP_TMP}"
python3 - "${STAGE}" "${ZIP_TMP}" <<'PY'
import os
import sys
import zipfile

root, out = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=1) as archive:
    for base, dirs, files in os.walk(root):
        dirs.sort()
        for name in sorted(files):
            path = os.path.join(base, name)
            archive.write(path, os.path.relpath(path, root))
print(out)
PY

# Publish only after the ZIP is complete; a failed run keeps the previous ZIP.
mv -f "${ZIP_TMP}" "${ZIP}"
rm -rf "${OUT}"
mv "${STAGE}" "${OUT}"
trap - EXIT

echo "package size: $(du -h "${ZIP}" | cut -f1)"
echo "package SHA256: $(sha256_of "${ZIP}")"
