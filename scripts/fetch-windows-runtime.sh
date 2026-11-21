#!/usr/bin/env bash
# Fetch, verify and stage the redistributable Windows x64 runtime used by the
# portable ZIP: PDFium and the Microsoft Fixed Version WebView2 runtime.
#
# Tesseract is not downloaded here. Build it once with
#   scripts/build-tesseract-windows.sh
# which produces .deps/windows/tesseract-minimal/ (statically linked CLI,
# tessdata and license evidence). This script verifies that directory's
# SHA256SUMS manifest on every run.
#
# Everything is cached under .deps/windows/ (or DEPS_DIR). Re-running with the
# cache present performs no network access; run with CURL=/bin/false to prove
# offline reacquisition. Nothing is installed and no host setting is changed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPS="${DEPS_DIR:-${ROOT}/.deps}"
WIN="${WIN_DEPS_DIR:-${DEPS}/windows}"
LIC="${WIN}/licenses"
CURL="${CURL:-curl}"
CABEXTRACT="${CABEXTRACT:-$(command -v cabextract || true)}"

TESS_VERSION="5.5.4"
TESSDATA_COMMIT="87416418657359cb625c412a48b6e1d6d41c29bd"

WV_VERSION="158.0.4370.33"
WV_CAB_NAME="Microsoft.WebView2.FixedVersionRuntime.${WV_VERSION}.x64.cab"
WV_CAB_URL="https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/ecc2761e-1380-46ee-9552-995b039e4e47/${WV_CAB_NAME}"
WV_CAB_SHA="835da387df67797f710881f553bb6934d98f24d08e370456f48d38ed4e072b62"
WV_DIR_NAME="Microsoft.WebView2.FixedVersionRuntime.${WV_VERSION}.x64"
WV_EULA_URL="https://developer.microsoft.com/microsoft-edge/api/eula/webview2?locale=en-us&fixed=true"

PDFIUM_VERSION="8165"
PDFIUM_URL="https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F${PDFIUM_VERSION}/pdfium-win-x64.tgz"
PDFIUM_TGZ_SHA="ca7338ae7779b8b1b82f4580305c4d762d214f8df1d54b47647fe3cc75989ded"
PDFIUM_DLL_SHA="31d249b8535dd5f351ae7f4b714dc4bb1d76b6069b321a309bc99271dd65f566"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

sha256_of() { sha256sum "$1" | awk '{print $1}'; }

verify_sha() {
  local file=$1 want=$2 label=$3 actual
  [[ -f "${file}" ]] || die "missing ${label}: ${file} (run this script once with network access)"
  actual=$(sha256_of "${file}")
  [[ "${actual}" == "${want}" ]] || die "SHA256 mismatch for ${label}: expected ${want}, got ${actual} (${file})"
}

download_pinned() {
  local url=$1 dest=$2 sha=$3 label=$4
  mkdir -p "$(dirname "${dest}")"
  if [[ -f "${dest}" ]]; then
    verify_sha "${dest}" "${sha}" "${label}"
    return
  fi
  command -v "${CURL%% *}" >/dev/null 2>&1 || die "curl not found; cannot download ${label}"
  say "downloading ${label}"
  rm -f "${dest}.part"
  # shellcheck disable=SC2086
  ${CURL} -fL --retry 3 --connect-timeout 20 -o "${dest}.part" "${url}"
  verify_sha "${dest}.part" "${sha}" "${label}"
  mv "${dest}.part" "${dest}"
}

need_7z() {
  if [[ -z "${SEVEN_ZIP:-}" ]]; then
    SEVEN_ZIP=$(command -v 7z || command -v 7za || command -v 7zz || true)
  fi
  [[ -n "${SEVEN_ZIP}" ]] || die "7z/7za/7zz not found; install 7zip or set SEVEN_ZIP (needed only to extract the WebView2 CAB)"
}

# --- Tesseract (static build, verified in place) --------------------------
TESS_DIR="${TESSERACT_RUNTIME_DIR:-${WIN}/tesseract-minimal}"
[[ -f "${TESS_DIR}/tesseract.exe" ]] || \
  die "missing ${TESS_DIR}/tesseract.exe; run scripts/build-tesseract-windows.sh (or set TESSERACT_RUNTIME_DIR)"
[[ -f "${TESS_DIR}/SHA256SUMS.txt" ]] || \
  die "missing ${TESS_DIR}/SHA256SUMS.txt; rebuild with scripts/build-tesseract-windows.sh"
[[ -f "${TESS_DIR}/BUILD-INFO.txt" ]] || \
  die "missing ${TESS_DIR}/BUILD-INFO.txt; rebuild with scripts/build-tesseract-windows.sh"
[[ -d "${TESS_DIR}/licenses" ]] || \
  die "missing ${TESS_DIR}/licenses; rebuild with scripts/build-tesseract-windows.sh"
( cd "${TESS_DIR}" && sha256sum -c --quiet SHA256SUMS.txt ) || \
  die "Tesseract runtime manifest verification failed: ${TESS_DIR}/SHA256SUMS.txt"
say "Tesseract ${TESS_VERSION} static runtime verified: ${TESS_DIR}"

# --- PDFium ---------------------------------------------------------------
PDFIUM_TGZ="${WIN}/pdfium-win-x64.tgz"
PDFIUM_DIR="${DEPS}/pdfium-win"
download_pinned "${PDFIUM_URL}" "${PDFIUM_TGZ}" "${PDFIUM_TGZ_SHA}" "PDFium ${PDFIUM_VERSION} win-x64"
if [[ ! -f "${PDFIUM_DIR}/pdfium.dll" || ! -d "${PDFIUM_DIR}/licenses" ]]; then
  tmp=$(mktemp -d "${DEPS}/pdfium-win.XXXXXX")
  tar -xzf "${PDFIUM_TGZ}" -C "${tmp}"
  mkdir -p "${PDFIUM_DIR}"
  cp "${tmp}/bin/pdfium.dll" "${PDFIUM_DIR}/pdfium.dll"
  cp "${tmp}/VERSION" "${PDFIUM_DIR}/VERSION" 2>/dev/null || true
  cp "${tmp}/LICENSE" "${PDFIUM_DIR}/LICENSE"
  rm -rf "${PDFIUM_DIR}/licenses"
  cp -R "${tmp}/licenses" "${PDFIUM_DIR}/licenses"
  rm -rf "${tmp}"
fi
verify_sha "${PDFIUM_DIR}/pdfium.dll" "${PDFIUM_DLL_SHA}" "PDFium pdfium.dll"

# --- Microsoft Fixed Version WebView2 ------------------------------------
mkdir -p "${LIC}"
if [[ "${SKIP_WEBVIEW2:-0}" == "1" ]]; then
  say "skipping WebView2 (SKIP_WEBVIEW2=1); build-windows.sh will refuse a complete package"
else
  WV_ROOT="${WIN}/webview2"
  WV_CAB="${WV_ROOT}/${WV_CAB_NAME}"
  WV_DIR="${WV_ROOT}/${WV_DIR_NAME}"
  download_pinned "${WV_CAB_URL}" "${WV_CAB}" "${WV_CAB_SHA}" "WebView2 Fixed ${WV_VERSION} CAB"
  if [[ ! -f "${WV_DIR}/msedgewebview2.exe" ]]; then
    tmp=$(mktemp -d "${WV_ROOT}/extract.XXXXXX")
    if [[ -n "${CABEXTRACT}" ]]; then
      say "extracting WebView2 CAB with cabextract"
      "${CABEXTRACT}" -q -d "${tmp}" "${WV_CAB}"
    else
      need_7z
      say "extracting WebView2 CAB with 7z"
      "${SEVEN_ZIP}" x -y -o"${tmp}" "${WV_CAB}" >/dev/null
    fi
    src=$(find "${tmp}" -mindepth 1 -maxdepth 1 -type d -name "${WV_DIR_NAME}" -print -quit)
    [[ -n "${src}" ]] || die "WebView2 CAB layout changed: ${WV_DIR_NAME} not found"
    rm -rf "${WV_DIR}"
    mv "${src}" "${WV_DIR}"
    rm -rf "${tmp}"
  fi
  [[ -f "${WV_DIR}/msedgewebview2.exe" ]] || die "WebView2 runtime incomplete: ${WV_DIR}/msedgewebview2.exe missing"

  if [[ ! -s "${LIC}/WebView2-Fixed-LICENSE.txt" ]]; then
    eula_json="${WV_ROOT}/eula-fixed.json"
    if [[ ! -s "${eula_json}" ]]; then
      command -v "${CURL%% *}" >/dev/null 2>&1 || die "curl not found; cannot download WebView2 license terms"
      say "license WebView2 Fixed Version EULA"
      # shellcheck disable=SC2086
      ${CURL} -fL --retry 3 -o "${eula_json}.part" "${WV_EULA_URL}"
      mv "${eula_json}.part" "${eula_json}"
    fi
    python3 - "${eula_json}" "${LIC}/WebView2-Fixed-LICENSE.txt" <<'PY'
import html, json, re, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
raw = data.get("fixedHtml") or data.get("fixedEulaHtml")
if not raw:
    sys.exit("WebView2 EULA response has no fixedHtml")
text = re.sub(r"<br\s*/?>", "\n", raw)
text = re.sub(r"</(p|h1|h2|h3|div|li)>", "\n", text)
text = re.sub(r"<[^>]+>", "", text)
text = html.unescape(text)
text = re.sub(r"[ \t]+\n", "\n", text)
text = re.sub(r"\n{3,}", "\n\n", text).strip() + "\n"
open(sys.argv[2], "w", encoding="utf-8").write(text)
PY
  fi

  cat > "${LIC}/WebView2-third-party-NOTICE.txt" <<'EOF'
Microsoft Edge WebView2 Runtime (Fixed Version)
================================================

The complete Fixed Version runtime is bundled unmodified under
runtime/webview2/. Microsoft's Fixed Version license is in
WebView2-Fixed-LICENSE.txt.

The runtime carries Microsoft's and third-party notices. The unmodified
distribution provides the official credits command:

  runtime/webview2/show_third_party_software_licenses.bat

which launches msedgewebview2.exe with --embedded-browser-webview=show-credits.
The same Chromium third-party credits are also available inside the runtime's
resources.pak. Do not remove or modify these notices.

Required SmartScreen notice (Microsoft Fixed Version EULA, section 8):
IndexFox includes Microsoft Defender SmartScreen. SmartScreen is enabled by
default and collects and sends end-user information to Microsoft as disclosed
in Microsoft's Privacy Statement (https://aka.ms/privacy) and the Microsoft
Edge Privacy Whitepaper
(https://learn.microsoft.com/microsoft-edge/privacy-whitepaper#smartscreen).

Microsoft Edge WebView2 Runtime (Fixed Version) license source:
https://developer.microsoft.com/microsoft-edge/api/eula/webview2?locale=en-us&fixed=true
Microsoft distribution guidance:
https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution
EOF
fi

# --- Provenance manifest --------------------------------------------------
cat > "${LIC}/SOURCES.txt" <<EOF
IndexFox M4.7 Windows runtime provenance
=======================================

Tesseract (static build, not downloaded)
  version   ${TESS_VERSION}
  build     scripts/build-tesseract-windows.sh
  output    ${TESS_DIR}
  evidence  BUILD-INFO.txt (toolchain, flags, PE imports, tesseract.exe SHA256)
            SHA256SUMS.txt (every file in the runtime directory)
  models    tessdata_fast commit ${TESSDATA_COMMIT}
  license   Apache-2.0; see licenses/ in the Tesseract runtime directory

PDFium
  version   Chromium ${PDFIUM_VERSION} (bblanchon/pdfium-binaries)
  archive   ${PDFIUM_URL}
  sha256    ${PDFIUM_TGZ_SHA}
  pdfium.dll sha256 ${PDFIUM_DLL_SHA}
  license   BSD-3-Clause + third-party licenses (PDFium-*.txt, PDFium-third-party/)

Microsoft Edge WebView2 Runtime (Fixed Version)
  version   ${WV_VERSION}
  source    official Microsoft download page
            ${WV_CAB_URL}
  sha256    ${WV_CAB_SHA}
  license   Microsoft Fixed Version EULA (WebView2-Fixed-LICENSE.txt)
EOF

say "Windows runtime ready under ${WIN}"
