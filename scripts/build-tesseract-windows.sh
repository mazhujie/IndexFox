#!/usr/bin/env bash
# Build a redistributable Windows x64 Tesseract OCR CLI from pinned upstream
# sources. Static libtesseract/Leptonica/libpng/zlib plus the static GCC runtime.
#
# Output: .deps/windows/tesseract-minimal/
# Cache/work: .deps/windows/tesseract-build/ (never /tmp beyond the compiler)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${ROOT}/.deps/windows/tesseract-build"
DL="${WORK}/dl"
SRC="${WORK}/src"
PREFIX="${WORK}/prefix"
OUT="${ROOT}/.deps/windows/tesseract-minimal"
TOOLCHAIN_ROOT="${MINGW_ROOT:-/tmp/mingw-full/root}"
JOBS="${JOBS:-$(nproc)}"

TESSERACT_VERSION="5.5.4"
LEPTONICA_VERSION="1.87.0"
LIBPNG_VERSION="1.6.59"
ZLIB_VERSION="1.4"
TESSDATA_COMMIT="87416418657359cb625c412a48b6e1d6d41c29bd"

TESSERACT_URL="https://codeload.github.com/tesseract-ocr/tesseract/tar.gz/refs/tags/${TESSERACT_VERSION}"
TESSERACT_SHA="a8dbf8b8c8ee427ff6ad1d99116502feaf59440af1adb893caf60a6576826cf4"
LEPTONICA_URL="https://github.com/DanBloomberg/leptonica/releases/download/${LEPTONICA_VERSION}/leptonica-${LEPTONICA_VERSION}.tar.gz"
LEPTONICA_SHA="c73363397f96eb1295602bf44d708a994ad42046c791bf03ea0505d829bdb6a7"
LIBPNG_URL="https://download.sourceforge.net/libpng/libpng-${LIBPNG_VERSION}.tar.xz"
LIBPNG_SHA="d80dd2a38a37f803cb9b6ac7b14bd6e74ddc3b654780a8380bdf93523fdb4389"
ZLIB_URL="https://github.com/madler/zlib/releases/download/v${ZLIB_VERSION}/zlib-${ZLIB_VERSION}.tar.gz"
ZLIB_SHA="02dd159783677a749c04adbdd02b55b8a2bcc7e054f51b76b549199a916ad584"
GPP_URL="http://archive.ubuntu.com/ubuntu/pool/universe/g/gcc-mingw-w64/g++-mingw-w64-x86-64-posix_13.2.0-6ubuntu1+26.1_amd64.deb"
GPP_SHA="52c3613f386e462ae71105d1a0b2a48d866ccb048ad253acff629c5a2291252d"
TESSDATA_URL="https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/${TESSDATA_COMMIT}"
ENG_SHA="7d4322bd2a7749724879683fc3912cb542f19906c83bcc1a52132556427170b2"
CHI_SIM_SHA="a5fcb6f0db1e1d6d8522f39db4e848f05984669172e584e8d76b6b3141e1f730"
TESSDATA_LICENSE_SHA="cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
GPL3_URL="https://www.gnu.org/licenses/gpl-3.0.txt"
GPL3_SHA="3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"
GCC_EXCEPTION_URL="https://raw.githubusercontent.com/gcc-mirror/gcc/releases/gcc-13.2.0/COPYING.RUNTIME"
GCC_EXCEPTION_SHA="9d6b43ce4d8de0c878bf16b54d8e7a10d9bd42b75178153e3af6a815bdc90f74"

CC="${TOOLCHAIN_ROOT}/usr/bin/x86_64-w64-mingw32-gcc-posix"
CXX="${TOOLCHAIN_ROOT}/usr/bin/x86_64-w64-mingw32-g++-posix"
RC="${TOOLCHAIN_ROOT}/usr/bin/x86_64-w64-mingw32-windres"
OBJDUMP="${TOOLCHAIN_ROOT}/usr/bin/x86_64-w64-mingw32-objdump"
GCC_LIBDIR="${TOOLCHAIN_ROOT}/usr/lib/gcc/x86_64-w64-mingw32/13-posix"

log() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

run() {
  local name="$1" logfile="$2"; shift 2
  log "${name}"
  if ! "$@" >"${logfile}" 2>&1; then
    tail -60 "${logfile}" >&2
    die "${name} failed (log: ${logfile})"
  fi
}

fetch() {
  local url="$1" dest="$2" sha="$3" tmp="${2}.part"
  if [[ -f "${dest}" ]] && printf '%s  %s\n' "${sha}" "${dest}" | sha256sum -c --status; then
    return
  fi
  rm -f "${tmp}"
  log "download ${url}"
  curl -fL --retry 3 --retry-delay 2 -o "${tmp}" "${url}"
  printf '%s  %s\n' "${sha}" "${tmp}" | sha256sum -c -
  mv "${tmp}" "${dest}"
}

extract() {
  local archive="$1" dir="$2" sha="$3"
  local marker="${SRC}/.$(basename "${dir}").sha256"
  if [[ -d "${SRC}/${dir}" && -f "${marker}" && "$(cat "${marker}")" == "${sha}" ]]; then
    return
  fi
  rm -rf "${SRC:?}/${dir}"
  log "extract ${archive}"
  tar -xf "${archive}" -C "${SRC}"
  printf '%s\n' "${sha}" >"${marker}"
}

ensure_cxx() {
  [[ -x "${CC}" ]] || die "missing C compiler: ${CC} (set MINGW_ROOT)"
  if [[ -x "${CXX}" && -x "${GCC_LIBDIR}/cc1plus" ]]; then
    return
  fi
  log "installing matching MinGW C++ frontend into local prefix ${TOOLCHAIN_ROOT}"
  fetch "${GPP_URL}" "${DL}/gpp.deb" "${GPP_SHA}"
  local gpp="${WORK}/toolchain-gpp"
  rm -rf "${gpp}"
  mkdir -p "${gpp}"
  dpkg-deb -x "${DL}/gpp.deb" "${gpp}"
  install -m 0755 "${gpp}/usr/bin/x86_64-w64-mingw32-g++-posix" "${TOOLCHAIN_ROOT}/usr/bin/"
  install -m 0755 "${gpp}/usr/bin/x86_64-w64-mingw32-c++-posix" "${TOOLCHAIN_ROOT}/usr/bin/"
  cp -a "${gpp}/usr/lib/gcc/x86_64-w64-mingw32/13-posix/." "${GCC_LIBDIR}/"
  ln -sf x86_64-w64-mingw32-g++-posix "${TOOLCHAIN_ROOT}/usr/bin/x86_64-w64-mingw32-g++"
  ln -sf x86_64-w64-mingw32-c++-posix "${TOOLCHAIN_ROOT}/usr/bin/x86_64-w64-mingw32-c++"
}

[[ "$(uname -s)" == Linux ]] || die "this cross-build script runs on Linux"
mkdir -p "${DL}" "${SRC}" "${WORK}/logs"
ensure_cxx
[[ -x "${RC}" && -x "${OBJDUMP}" ]] || die "incomplete MinGW toolchain at ${TOOLCHAIN_ROOT}"

fetch "${TESSERACT_URL}" "${DL}/tesseract-${TESSERACT_VERSION}.tar.gz" "${TESSERACT_SHA}"
fetch "${LEPTONICA_URL}" "${DL}/leptonica-${LEPTONICA_VERSION}.tar.gz" "${LEPTONICA_SHA}"
fetch "${LIBPNG_URL}" "${DL}/libpng-${LIBPNG_VERSION}.tar.xz" "${LIBPNG_SHA}"
fetch "${ZLIB_URL}" "${DL}/zlib-${ZLIB_VERSION}.tar.gz" "${ZLIB_SHA}"
fetch "${TESSDATA_URL}/eng.traineddata" "${DL}/eng.traineddata" "${ENG_SHA}"
fetch "${TESSDATA_URL}/chi_sim.traineddata" "${DL}/chi_sim.traineddata" "${CHI_SIM_SHA}"
fetch "${TESSDATA_URL}/LICENSE" "${DL}/tessdata-LICENSE" "${TESSDATA_LICENSE_SHA}"
fetch "${GPL3_URL}" "${DL}/GPL-3.0.txt" "${GPL3_SHA}"
fetch "${GCC_EXCEPTION_URL}" "${DL}/COPYING.RUNTIME" "${GCC_EXCEPTION_SHA}"

extract "${DL}/tesseract-${TESSERACT_VERSION}.tar.gz" "tesseract-${TESSERACT_VERSION}" "${TESSERACT_SHA}"
extract "${DL}/leptonica-${LEPTONICA_VERSION}.tar.gz" "leptonica-${LEPTONICA_VERSION}" "${LEPTONICA_SHA}"
extract "${DL}/libpng-${LIBPNG_VERSION}.tar.xz" "libpng-${LIBPNG_VERSION}" "${LIBPNG_SHA}"
extract "${DL}/zlib-${ZLIB_VERSION}.tar.gz" "zlib-${ZLIB_VERSION}" "${ZLIB_SHA}"

rm -rf "${WORK}/build" "${PREFIX}" "${OUT}"
mkdir -p "${WORK}/build" "${PREFIX}/lib" "${OUT}/tessdata/configs" "${OUT}/tessdata/tessconfigs" "${OUT}/licenses"
# Tesseract emits -lWs2_32; the Linux-hosted mingw sysroot only has libws2_32.a.
ln -sf "${TOOLCHAIN_ROOT}/usr/x86_64-w64-mingw32/lib/libws2_32.a" "${PREFIX}/lib/libWs2_32.a"

cat >"${WORK}/toolchain.cmake" <<EOF
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER "${CC}")
set(CMAKE_CXX_COMPILER "${CXX}")
set(CMAKE_RC_COMPILER "${RC}")
set(CMAKE_FIND_ROOT_PATH "${TOOLCHAIN_ROOT}/usr/x86_64-w64-mingw32" "${TOOLCHAIN_ROOT}/usr" "${PREFIX}")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
set(CMAKE_EXE_LINKER_FLAGS_INIT "-static -static-libgcc -static-libstdc++ -Wl,--no-insert-timestamp -Wl,-s -L${PREFIX}/lib")
set(CMAKE_INSTALL_LIBDIR "lib")
set(CMAKE_INSTALL_BINDIR "bin")
set(CMAKE_INSTALL_INCLUDEDIR "include")
EOF

common=(-G Ninja
  -DCMAKE_TOOLCHAIN_FILE="${WORK}/toolchain.cmake"
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_INSTALL_PREFIX="${PREFIX}"
  -DCMAKE_PREFIX_PATH="${PREFIX}"
  -DPKG_CONFIG_EXECUTABLE=/bin/false)

PKG_CONFIG=/bin/false PKG_CONFIG_LIBDIR=/nonexistent \
run "configure zlib ${ZLIB_VERSION}" "${WORK}/logs/zlib-config.log" \
  cmake -S "${SRC}/zlib-${ZLIB_VERSION}" -B "${WORK}/build/zlib" "${common[@]}" \
    -DZLIB_BUILD_SHARED=OFF -DZLIB_BUILD_STATIC=ON -DZLIB_BUILD_EXAMPLES=OFF
run "build zlib" "${WORK}/logs/zlib-build.log" \
  cmake --build "${WORK}/build/zlib" --target install -j "${JOBS}"
ln -sf libzs.a "${PREFIX}/lib/libz.a"

PKG_CONFIG=/bin/false PKG_CONFIG_LIBDIR=/nonexistent \
run "configure libpng ${LIBPNG_VERSION}" "${WORK}/logs/libpng-config.log" \
  cmake -S "${SRC}/libpng-${LIBPNG_VERSION}" -B "${WORK}/build/libpng" "${common[@]}" \
    -DPNG_SHARED=OFF -DPNG_STATIC=ON -DPNG_TESTS=OFF -DPNG_TOOLS=OFF -DPNG_EXECUTABLES=OFF \
    -DZLIB_LIBRARY="${PREFIX}/lib/libz.a" -DZLIB_INCLUDE_DIR="${PREFIX}/include"
run "build libpng" "${WORK}/logs/libpng-build.log" \
  cmake --build "${WORK}/build/libpng" --target install -j "${JOBS}"

PNG_LIB="$(find "${PREFIX}/lib" -maxdepth 1 -name 'libpng*.a' | head -1)"
[[ -n "${PNG_LIB}" ]] || die "libpng static library not installed"
PKG_CONFIG=/bin/false PKG_CONFIG_LIBDIR=/nonexistent \
run "configure Leptonica ${LEPTONICA_VERSION}" "${WORK}/logs/leptonica-config.log" \
  cmake -S "${SRC}/leptonica-${LEPTONICA_VERSION}" -B "${WORK}/build/leptonica" "${common[@]}" \
    -DBUILD_SHARED_LIBS=OFF -DBUILD_PROG=OFF -DSW_BUILD=OFF \
    -DENABLE_ZLIB=ON -DENABLE_PNG=ON -DENABLE_TIFF=OFF -DENABLE_JPEG=OFF \
    -DENABLE_GIF=OFF -DENABLE_WEBP=OFF -DENABLE_OPENJPEG=OFF \
    -DZLIB_LIBRARY="${PREFIX}/lib/libz.a" -DZLIB_INCLUDE_DIR="${PREFIX}/include" \
    -DPNG_LIBRARY="${PNG_LIB}" -DPNG_PNG_INCLUDE_DIR="${PREFIX}/include"
run "build Leptonica" "${WORK}/logs/leptonica-build.log" \
  cmake --build "${WORK}/build/leptonica" --target install -j "${JOBS}"

PKG_CONFIG=/bin/false PKG_CONFIG_LIBDIR=/nonexistent \
run "configure Tesseract ${TESSERACT_VERSION}" "${WORK}/logs/tesseract-config.log" \
  cmake -S "${SRC}/tesseract-${TESSERACT_VERSION}" -B "${WORK}/build/tesseract" "${common[@]}" \
    -DBUILD_SHARED_LIBS=OFF -DBUILD_TRAINING_TOOLS=OFF -DBUILD_TESTS=OFF \
    -DOPENMP_BUILD=OFF -DDISABLE_ARCHIVE=ON -DDISABLE_CURL=ON -DDISABLE_TIFF=ON \
    -DGRAPHICS_DISABLED=ON -DENABLE_NATIVE=OFF -DENABLE_LTO=OFF -DINSTALL_CONFIGS=OFF \
    -DCMAKE_CXX_FLAGS=-DTESSERACT_DISABLE_DEBUG_FONTS \
    -DLEPT_TIFF_RESULT=FAILED_TO_RUN \
    -DLeptonica_DIR="${PREFIX}/lib/cmake/leptonica"
run "build Tesseract" "${WORK}/logs/tesseract-build.log" \
  cmake --build "${WORK}/build/tesseract" -j "${JOBS}"

TESS_EXE="$(find "${WORK}/build/tesseract" -type f -name 'tesseract.exe' | head -1)"
[[ -n "${TESS_EXE}" ]] || die "tesseract.exe not produced"
install -m 0755 "${TESS_EXE}" "${OUT}/tesseract.exe"
install -m 0644 "${DL}/eng.traineddata" "${OUT}/tessdata/eng.traineddata"
install -m 0644 "${DL}/chi_sim.traineddata" "${OUT}/tessdata/chi_sim.traineddata"
install -m 0644 "${SRC}/tesseract-${TESSERACT_VERSION}/tessdata/pdf.ttf" "${OUT}/tessdata/pdf.ttf"
cp -a "${SRC}/tesseract-${TESSERACT_VERSION}/tessdata/configs/." "${OUT}/tessdata/configs/"
cp -a "${SRC}/tesseract-${TESSERACT_VERSION}/tessdata/tessconfigs/." "${OUT}/tessdata/tessconfigs/"

install -m 0644 "${SRC}/tesseract-${TESSERACT_VERSION}/LICENSE" "${OUT}/licenses/Tesseract-Apache-2.0.txt"
install -m 0644 "${SRC}/tesseract-${TESSERACT_VERSION}/AUTHORS" "${OUT}/licenses/Tesseract-AUTHORS.txt"
install -m 0644 "${SRC}/leptonica-${LEPTONICA_VERSION}/leptonica-license.txt" "${OUT}/licenses/Leptonica-BSD-2-Clause.txt"
install -m 0644 "${SRC}/libpng-${LIBPNG_VERSION}/LICENSE" "${OUT}/licenses/libpng-LICENSE.txt"
mkdir -p "${OUT}/licenses/libpng-LICENSES"
cp -a "${SRC}/libpng-${LIBPNG_VERSION}/LICENSES/." "${OUT}/licenses/libpng-LICENSES/"
install -m 0644 "${SRC}/zlib-${ZLIB_VERSION}/LICENSE" "${OUT}/licenses/zlib-LICENSE.txt"
install -m 0644 "${DL}/tessdata-LICENSE" "${OUT}/licenses/tessdata-Apache-2.0.txt"
install -m 0644 "${DL}/COPYING.RUNTIME" "${OUT}/licenses/GCC-Runtime-Library-Exception-3.1.txt"
install -m 0644 "${DL}/GPL-3.0.txt" "${OUT}/licenses/GPL-3.0.txt"
MINGW_COPYRIGHT="${TOOLCHAIN_ROOT}/usr/share/doc/mingw-w64-common/copyright"
[[ -f "${MINGW_COPYRIGHT}" ]] || die "missing MinGW-w64 copyright: ${MINGW_COPYRIGHT}"
install -m 0644 "${MINGW_COPYRIGHT}" "${OUT}/licenses/MinGW-w64-COPYRIGHT.txt"

IMPORTS="$("${OBJDUMP}" -p "${OUT}/tesseract.exe" | awk '/DLL Name/ {print $3}' | sort -u)"
[[ -n "${IMPORTS}" ]] || die "no PE import table found"
EXPECTED_IMPORTS=$'KERNEL32.dll\nmsvcrt.dll'
if [[ "${IMPORTS}" != "${EXPECTED_IMPORTS}" ]]; then
  printf '%s\n' "${IMPORTS}" >&2
  die "unexpected PE imports; expected only KERNEL32.dll and msvcrt.dll"
fi

{
  printf 'tesseract %s (Windows x64, static)\n' "${TESSERACT_VERSION}"
  printf 'Leptonica %s, libpng %s, zlib %s\n' "${LEPTONICA_VERSION}" "${LIBPNG_VERSION}" "${ZLIB_VERSION}"
  printf 'tessdata_fast commit %s (eng, chi_sim)\n' "${TESSDATA_COMMIT}"
  printf 'compiler: %s\n' "$("${CXX}" --version | head -1)"
  printf 'linker flags: -static -static-libgcc -static-libstdc++ -Wl,--no-insert-timestamp -Wl,-s\n'
  printf 'PE imports:\n'
  printf '%s\n' "${IMPORTS}" | sed 's/^/  /'
  printf 'tesseract.exe SHA256: %s\n' "$(sha256sum "${OUT}/tesseract.exe" | cut -d' ' -f1)"
} >"${OUT}/BUILD-INFO.txt"

(
  cd "${OUT}"
  find . -type f ! -name SHA256SUMS.txt -printf '%P\n' | sort | xargs -d '\n' sha256sum >SHA256SUMS.txt
)

log "output: ${OUT}"
du -sh "${OUT}"
cat "${OUT}/BUILD-INFO.txt"
