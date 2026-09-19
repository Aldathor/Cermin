#!/usr/bin/env bash
set -euo pipefail

TARGET="x86_64-pc-windows-gnu"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OPENH264_DLL="openh264-2.6.0-win64.dll"
OPENH264_URL="http://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2"
VENDOR_DIR="${ROOT}/vendor"
DLL_CACHE="${VENDOR_DIR}/${OPENH264_DLL}"

if ! command -v x86_64-w64-mingw32-gcc &>/dev/null; then
    echo "MinGW-w64 not found. Install it:"
    echo "  Ubuntu/Debian/WSL: sudo apt install mingw-w64"
    echo "  Fedora:            sudo dnf install mingw64-gcc"
    echo "  Arch:              sudo pacman -S mingw-w64-gcc"
    exit 1
fi

if ! rustup target list --installed | grep -q "^${TARGET}$"; then
    echo "Adding Rust target ${TARGET}..."
    rustup target add "${TARGET}"
fi

cd "${ROOT}"
cargo build --release -p rotten-app --target "${TARGET}" --no-default-features --features encode-dll
cargo build --release -p rotten-probe --target "${TARGET}"

OUT_DIR="${ROOT}/target/${TARGET}/release"
OUT="${OUT_DIR}/cermin.exe"

echo "Checking PE dependencies..."
if x86_64-w64-mingw32-objdump -p "${OUT}" 2>/dev/null | grep -q "libstdc++-6.dll"; then
    echo "Warning: exe still needs libstdc++-6.dll — copying MinGW runtime DLL"
    MINGW_BIN="/usr/x86_64-w64-mingw32/sys-root/mingw/bin"
    for dll in libstdc++-6.dll libgcc_s_seh-1.dll libwinpthread-1.dll; do
        if [[ -f "${MINGW_BIN}/${dll}" ]]; then
            cp "${MINGW_BIN}/${dll}" "${OUT_DIR}/"
            echo "  copied ${dll}"
        fi
    done
else
    echo "OK: no libstdc++ runtime DLL required"
fi

if [[ ! -f "${DLL_CACHE}" ]]; then
    echo "Downloading ${OPENH264_DLL} from Cisco..."
    mkdir -p "${VENDOR_DIR}"
    if command -v curl &>/dev/null && command -v bunzip2 &>/dev/null; then
        curl -fsSL "${OPENH264_URL}" | bunzip2 > "${DLL_CACHE}"
    elif command -v wget &>/dev/null && command -v bunzip2 &>/dev/null; then
        wget -qO- "${OPENH264_URL}" | bunzip2 > "${DLL_CACHE}"
    else
        echo "Error: need curl or wget plus bunzip2 to fetch ${OPENH264_DLL}"
        echo "  Manual: download ${OPENH264_URL} and place at ${DLL_CACHE}"
        exit 1
    fi
fi

cp "${DLL_CACHE}" "${OUT_DIR}/${OPENH264_DLL}"

if command -v go &>/dev/null; then
    echo "Building fpsap-helper.exe..."
    (cd "${ROOT}/tools/fpsap-helper" && GOOS=windows GOARCH=amd64 CGO_ENABLED=0 go build -o "${OUT_DIR}/fpsap-helper.exe" .)
else
    echo "Warning: go not found; fpsap-helper.exe not built (fp-setup step2 will fail)."
    echo "  Install Go and re-run this script."
fi

echo ""
echo "Built: ${OUT}"
echo "Built: ${OUT_DIR}/cermin-probe.exe (minimal startup test)"
echo "Built: ${OUT_DIR}/${OPENH264_DLL}"
if [[ -f "${OUT_DIR}/fpsap-helper.exe" ]]; then
    echo "Built: ${OUT_DIR}/fpsap-helper.exe"
fi
echo ""
echo "Copy to Windows (same folder):"
echo "  cermin.exe"
echo "  ${OPENH264_DLL}"
echo "  fpsap-helper.exe"
if ls "${OUT_DIR}"/libstdc++-6.dll &>/dev/null; then
    echo "  libstdc++-6.dll (and any other lib*.dll copied above)"
fi
echo ""
echo "Smoke tests on Windows (run in order):"
echo "  1. .\\cermin-probe.exe"
echo "  2. .\\cermin.exe probe"
echo "  3. .\\cermin.exe --version"
echo "  4. .\\cermin.exe mirror -t 192.168.2.111 --test"
