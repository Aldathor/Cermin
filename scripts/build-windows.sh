#!/usr/bin/env bash
set -euo pipefail

TARGET="x86_64-pc-windows-gnu"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OPENH264_DLL="openh264-2.6.0-win64.dll"
OPENH264_URL="https://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2"
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
cargo build --locked --release -p rotten-app --target "${TARGET}" --no-default-features --features encode-dll,gui --bins
cargo build --locked --release -p rotten-probe --target "${TARGET}"

TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}"
OUT_DIR="$(cd "${TARGET_DIR}/${TARGET}/release" && pwd)"
OUT="${OUT_DIR}/cermin.exe"

echo "Checking PE dependencies..."
# Resolve runtime imports for every executable and copied runtime DLL. Compiler
# lookup supports Debian/Ubuntu as well as Fedora's different MinGW layout.
pe_queue=("${OUT}" "${OUT_DIR}/cermin-cli.exe" "${OUT_DIR}/cermin-probe.exe")
declare -A copied_runtime=()
for ((i = 0; i < ${#pe_queue[@]}; i++)); do
    imports="$(x86_64-w64-mingw32-objdump -p "${pe_queue[i]}")"
    for dll in libstdc++-6.dll libgcc_s_seh-1.dll libwinpthread-1.dll; do
        if [[ "${imports}" == *"${dll}"* && -z "${copied_runtime[$dll]:-}" ]]; then
            dll_path="$(x86_64-w64-mingw32-gcc -print-file-name="${dll}")"
            if [[ ! -f "${dll_path}" ]]; then
                echo "Error: required MinGW runtime ${dll} could not be located"
                exit 1
            fi
            cp "${dll_path}" "${OUT_DIR}/${dll}"
            copied_runtime[$dll]=1
            pe_queue+=("${OUT_DIR}/${dll}")
            echo "  copied ${dll}"
        fi
    done
done

if [[ ! -s "${DLL_CACHE}" ]]; then
    echo "Downloading ${OPENH264_DLL} from Cisco..."
    mkdir -p "${VENDOR_DIR}"
    dll_temp="$(mktemp "${VENDOR_DIR}/openh264.XXXXXX")"
    trap 'rm -f -- "${dll_temp}"' EXIT
    if command -v curl &>/dev/null && command -v bunzip2 &>/dev/null; then
        curl -fsSL "${OPENH264_URL}" | bunzip2 > "${dll_temp}"
    elif command -v wget &>/dev/null && command -v bunzip2 &>/dev/null; then
        wget -qO- "${OPENH264_URL}" | bunzip2 > "${dll_temp}"
    else
        echo "Error: need curl or wget plus bunzip2 to fetch ${OPENH264_DLL}"
        echo "  Manual: download ${OPENH264_URL} and place at ${DLL_CACHE}"
        exit 1
    fi
    if [[ ! -s "${dll_temp}" ]]; then
        echo "Error: downloaded OpenH264 DLL is empty"
        exit 1
    fi
    mv "${dll_temp}" "${DLL_CACHE}"
    trap - EXIT
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
echo "Built: ${OUT_DIR}/cermin-probe.exe (receiver session diagnostic)"
echo "Built: ${OUT_DIR}/${OPENH264_DLL}"
if [[ -f "${OUT_DIR}/fpsap-helper.exe" ]]; then
    echo "Built: ${OUT_DIR}/fpsap-helper.exe"
fi
echo ""
echo "Copy to Windows (same folder):"
echo "  cermin.exe (GUI) + cermin-cli.exe"
echo "  ${OPENH264_DLL}"
echo "  fpsap-helper.exe"
if ls "${OUT_DIR}"/libstdc++-6.dll &>/dev/null; then
    echo "  libstdc++-6.dll (and any other lib*.dll copied above)"
fi
echo ""
echo "Smoke tests on Windows (run in order):"
echo "  1. .\\cermin-cli.exe probe"
echo "  2. .\\cermin-cli.exe --version"
echo "  3. .\\cermin-cli.exe mirror -t <receiver-ip> --test"
echo ""
echo "Or double-click cermin.exe for the GUI."
