#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Multi-architecture build for AREDN OpenWrt
#
# Usage:
#   ./build.sh <arch>       Build one architecture
#   ./build.sh all          Build all AREDN architectures
#
# Builds cross-compiled binaries, verifies them, then packages as IPK + APK.
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$SCRIPT_DIR"
PROFILE="release"
PKG_NAME="meshtastic-serial-udp"
PKG_VERSION="${PKG_VERSION:-0.0.0}"

# shellcheck source=hack/arch-config.sh
source "$PROJECT_ROOT/hack/arch-config.sh"

usage() {
    echo "Usage: $(basename "$0") <arch|all>"
    echo ""
    echo "Architectures:"
    print_arch_list
    echo "  all                        Build all of the above (default)"
    exit 1
}

REQUESTED="${1:-all}"
if [ "$REQUESTED" = "all" ]; then
    ARCHES="$ALL_ARCHES"
else
    ARCHES="$REQUESTED"
fi

build_arch() {
    local arch="$1"
    configure_arch "$arch"

    echo ""
    echo "================================================================"
    echo "==> [$arch] Building $PKG_NAME"
    echo "================================================================"

    build_docker_image "$arch" "$PROJECT_ROOT"

    echo "==> [$arch] Compiling..."
    run_cargo "$PROJECT_ROOT" "build" "$CARGO_EXTRA_FLAGS" "--profile" "$PROFILE"

    BINARY="$PROJECT_ROOT/target/$RUST_TARGET_DIR/$PROFILE/$PKG_NAME"

    echo "==> [$arch] Build complete:"
    ls -lh "$BINARY"
    file "$BINARY"
}

for arch in $ARCHES; do
    build_arch "$arch"
done

echo ""
echo "==> All binaries built. Packaging..."
export PKG_VERSION
"$PROJECT_ROOT/openwrt/build.sh" "$REQUESTED"
