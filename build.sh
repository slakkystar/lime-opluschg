#!/bin/bash
# Usage: ./build.sh [debug|release]

set -eu

PROFILE="${1:-release}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

case "$PROFILE" in
    debug|release) ;;
    *)
        echo "error: profile must be 'debug' or 'release'" >&2
        exit 2
        ;;
esac

ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [ -z "$ANDROID_NDK_HOME" ]; then
    echo "error: set ANDROID_NDK_HOME or ANDROID_NDK_ROOT" >&2
    exit 1
fi
NDK_BIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"

if [ ! -x "$NDK_BIN/aarch64-linux-android26-clang" ]; then
    echo "error: Android NDK toolchain not found under $NDK_BIN" >&2
    exit 1
fi

export CC_aarch64_linux_android="$NDK_BIN/aarch64-linux-android26-clang"
export CXX_aarch64_linux_android="$NDK_BIN/aarch64-linux-android26-clang++"
export AR_aarch64_linux_android="$NDK_BIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"

CARGO_BIN="${CARGO:-cargo}"
if [ -x "$HOME/.cargo/bin/cargo" ]; then
    CARGO_BIN="$HOME/.cargo/bin/cargo"
fi

echo "==> Building oplus-charger-hal ($PROFILE) for aarch64-linux-android..."

cd "$SCRIPT_DIR"

if [ "$PROFILE" = "release" ]; then
    "$CARGO_BIN" build --target aarch64-linux-android --release
    BIN="target/aarch64-linux-android/release/vendor-oplus-hardware-charger-service"
else
    "$CARGO_BIN" build --target aarch64-linux-android
    BIN="target/aarch64-linux-android/debug/vendor-oplus-hardware-charger-service"
fi
OUT_BIN="$(dirname "$BIN")/vendor.oplus.hardware.charger-V3-service"
DIST_DIR="$SCRIPT_DIR/dist"

if [ "$PROFILE" = "release" ]; then
    echo "==> Stripping..."
    "$NDK_BIN/llvm-strip" "$BIN"
fi
cp "$BIN" "$OUT_BIN"
mkdir -p "$DIST_DIR"
cp "$OUT_BIN" "$DIST_DIR/vendor.oplus.hardware.charger-V3-service"
cp "$SCRIPT_DIR/vendor.oplus.hardware.charger-V3-service.rc" "$DIST_DIR/vendor.oplus.hardware.charger-V3-service.rc"
cp "$SCRIPT_DIR/manifest_oplus_charger_aidl.xml" "$DIST_DIR/manifest_oplus_charger_aidl.xml"
chmod 0755 "$DIST_DIR/vendor.oplus.hardware.charger-V3-service"
(
    cd "$DIST_DIR"
    sha256sum \
        vendor.oplus.hardware.charger-V3-service \
        vendor.oplus.hardware.charger-V3-service.rc \
        manifest_oplus_charger_aidl.xml > SHA256SUMS
)

echo "==> Done: $BIN"
ls -lh "$BIN"
file "$BIN"
echo "==> ROM replacement: $OUT_BIN"
ls -lh "$OUT_BIN"
echo "==> Soong prebuilt: $DIST_DIR/vendor.oplus.hardware.charger-V3-service"

ODM_DIR="$SCRIPT_DIR/odm"
mkdir -p "$ODM_DIR/bin/hw" "$ODM_DIR/etc/init" "$ODM_DIR/etc/vintf/manifest"
cp "$DIST_DIR/vendor.oplus.hardware.charger-V3-service" "$ODM_DIR/bin/hw/vendor.oplus.hardware.charger-V3-service"
cp "$DIST_DIR/vendor.oplus.hardware.charger-V3-service.rc" "$ODM_DIR/etc/init/vendor.oplus.hardware.charger-V3-service.rc"
cp "$DIST_DIR/manifest_oplus_charger_aidl.xml" "$ODM_DIR/etc/vintf/manifest/manifest_oplus_charger_aidl.xml"
chmod 0755 "$ODM_DIR/bin/hw/vendor.oplus.hardware.charger-V3-service"

echo "==> odm/ layout: $ODM_DIR"
find "$ODM_DIR" -type f -exec ls -lh {} \;
