#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Multi-architecture build + packaging for AREDN OpenWrt
#
# Usage:
#   ./build.sh <arch>       Build one architecture
#   ./build.sh all          Build all AREDN architectures
#
# Architectures correspond to AREDN SUPPORTED_DEVICES.md targets:
#   mips_24kc                  ath79        (Ubiquiti, TP-Link, Mikrotik, GL.iNet)
#   mipsel_24kc                ramips       (GL-MT1300, HaLow, Cudy TR1200)
#   arm_cortex-a7_neon-vfpv4   ipq40xx      (Mikrotik hAP ac2/ac3, GL-B1300)
#   aarch64_cortex-a53         mediatek     (OpenWrt One, Cudy TR3000)
#   x86_64                     x86/64       (VMware, Proxmox, VirtualBox, Bhyve)
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROFILE="release-cross"
PKG_NAME="meshtastic-serial-udp"
PKG_VER=0.1.0
PKG_REL="r$(($(date +%s) - $(date -d '2026-01-01 00:00:00' +%s)))"
PKG_VERSION="${PKG_VER}-${PKG_REL}"

ALL_ARCHES="mips_24kc mipsel_24kc arm_cortex-a7_neon-vfpv4 aarch64_cortex-a53 x86_64"

usage() {
    cat <<EOF
Usage: $(basename "$0") <arch|all>

Architectures:
  mips_24kc                  ath79 — Ubiquiti, TP-Link, Mikrotik SXT/LHG/LDF, GL.iNet
  mipsel_24kc                ramips — GL-MT1300, HaLowLink, Heltec, Alfa Tube-AHM, Cudy TR1200
  arm_cortex-a7_neon-vfpv4   ipq40xx — Mikrotik hAP ac2/ac3, SXTsq 5ac, GL-B1300
  aarch64_cortex-a53         mediatek/filogic — OpenWrt One, Cudy TR3000
  x86_64                     x86/64 — VMware ESXi, Proxmox, VirtualBox, Bhyve
  all                        Build all of the above
EOF
    exit 1
}

[ $# -lt 1 ] && usage

REQUESTED="$1"
if [ "$REQUESTED" = "all" ]; then
    ARCHES="$ALL_ARCHES"
else
    ARCHES="$REQUESTED"
fi

# ---------------------------------------------------------------------------
# Per-architecture configuration
#
# Sets: MUSL_TARGET, GCC_EXTRA_CONFIG, IMAGE_NAME, RUST_TARGET_ARG,
#        RUST_TARGET_DIR, CARGO_EXTRA_FLAGS, LINKER_ENV, VERIFY_SOFT_FLOAT,
#        PKG_ARCH
# ---------------------------------------------------------------------------
configure_arch() {
    local arch="$1"
    case "$arch" in
        mips_24kc)
            MUSL_TARGET="mips-linux-muslsf"
            GCC_EXTRA_CONFIG="--with-float=soft"
            IMAGE_NAME="aredn-rust-mips-24kc"
            RUST_TARGET_ARG="/src/openwrt/mips-unknown-linux-musl-soft.json"
            RUST_TARGET_DIR="mips-unknown-linux-musl-soft"
            CARGO_EXTRA_FLAGS="-Z json-target-spec"
            LINKER_ENV=""
            VERIFY_SOFT_FLOAT=1
            PKG_ARCH="mips_24kc"
            ;;
        mipsel_24kc)
            MUSL_TARGET="mipsel-linux-muslsf"
            GCC_EXTRA_CONFIG="--with-float=soft"
            IMAGE_NAME="aredn-rust-mipsel-24kc"
            RUST_TARGET_ARG="/src/openwrt/mipsel-unknown-linux-musl-soft.json"
            RUST_TARGET_DIR="mipsel-unknown-linux-musl-soft"
            CARGO_EXTRA_FLAGS="-Z json-target-spec"
            LINKER_ENV=""
            VERIFY_SOFT_FLOAT=1
            PKG_ARCH="mipsel_24kc"
            ;;
        arm_cortex-a7_neon-vfpv4)
            MUSL_TARGET="arm-linux-musleabihf"
            GCC_EXTRA_CONFIG="--with-arch=armv7-a --with-fpu=neon-vfpv4 --with-float=hard"
            IMAGE_NAME="aredn-rust-arm-cortex-a7"
            RUST_TARGET_ARG="armv7-unknown-linux-musleabihf"
            RUST_TARGET_DIR="armv7-unknown-linux-musleabihf"
            CARGO_EXTRA_FLAGS=""
            LINKER_ENV="CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-musleabihf-gcc"
            VERIFY_SOFT_FLOAT=0
            PKG_ARCH="arm_cortex-a7_neon-vfpv4"
            ;;
        aarch64_cortex-a53)
            MUSL_TARGET="aarch64-linux-musl"
            GCC_EXTRA_CONFIG=""
            IMAGE_NAME="aredn-rust-aarch64"
            RUST_TARGET_ARG="aarch64-unknown-linux-musl"
            RUST_TARGET_DIR="aarch64-unknown-linux-musl"
            CARGO_EXTRA_FLAGS=""
            LINKER_ENV="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc"
            VERIFY_SOFT_FLOAT=0
            PKG_ARCH="aarch64_cortex-a53"
            ;;
        x86_64)
            MUSL_TARGET="x86_64-linux-musl"
            GCC_EXTRA_CONFIG=""
            IMAGE_NAME="aredn-rust-x86-64"
            RUST_TARGET_ARG="x86_64-unknown-linux-musl"
            RUST_TARGET_DIR="x86_64-unknown-linux-musl"
            CARGO_EXTRA_FLAGS=""
            LINKER_ENV="CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc"
            VERIFY_SOFT_FLOAT=0
            PKG_ARCH="x86_64"
            ;;
        *)
            echo "ERROR: Unknown architecture: $arch"
            usage
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Build + verify one architecture
# ---------------------------------------------------------------------------
build_arch() {
    local arch="$1"
    configure_arch "$arch"

    echo ""
    echo "================================================================"
    echo "==> [$arch] Building $PKG_NAME"
    echo "================================================================"

    # -- Docker image -------------------------------------------------------
    if ! docker image inspect "$IMAGE_NAME" &>/dev/null; then
        echo "==> [$arch] Building toolchain Docker image (one-time)..."
        docker build -t "$IMAGE_NAME" \
            --build-arg MUSL_TARGET="$MUSL_TARGET" \
            --build-arg GCC_EXTRA_CONFIG="$GCC_EXTRA_CONFIG" \
            -f "$SCRIPT_DIR/Dockerfile.cross" "$PROJECT_ROOT"
    fi

    # -- Compile ------------------------------------------------------------
    echo "==> [$arch] Compiling..."
    docker run --rm \
        -v "$PROJECT_ROOT":/src \
        -w /src \
        ${LINKER_ENV:+-e "$LINKER_ENV"} \
        "$IMAGE_NAME" \
        bash -c '
            # Create empty libunwind.a stub — std links -lunwind even with panic=abort
            GCC_LIB_DIR=$($MUSL_TARGET-gcc -print-libgcc-file-name | xargs dirname)
            $MUSL_TARGET-ar rcs "${GCC_LIB_DIR}/libunwind.a"

            cargo +nightly build \
                -Z build-std=std,panic_abort \
                '"$CARGO_EXTRA_FLAGS"' \
                --target '"$RUST_TARGET_ARG"' \
                --profile '"$PROFILE"'
        '

    BINARY="$PROJECT_ROOT/target/$RUST_TARGET_DIR/$PROFILE/$PKG_NAME"

    echo "==> [$arch] Build complete:"
    ls -lh "$BINARY"
    file "$BINARY"

    # -- Verify soft-float (MIPS only) --------------------------------------
    if [ "$VERIFY_SOFT_FLOAT" -eq 1 ]; then
        echo "==> [$arch] Checking for hardware float instructions..."
        COUNT=$(docker run --rm \
            -v "$PROJECT_ROOT/target:/target" \
            "$IMAGE_NAME" \
            bash -c '$MUSL_TARGET-objdump -d /target/'"$RUST_TARGET_DIR/$PROFILE/$PKG_NAME"' \
            | grep -c -E '"'"'\blwc1|swc1|mtc1|mfc1|add\.s|mul\.s|div\.s|cvt\.|mov\.s\b'"'"'' \
        ) || true
        echo "    Hard-float instructions: ${COUNT:-0}"
        if [ "${COUNT:-0}" -eq 0 ]; then
            echo "    OK — pure soft-float binary"
        else
            echo "    WARNING — binary contains hardware float instructions"
        fi
    fi

    # -- Package ------------------------------------------------------------
    package_binary "$BINARY" "$PKG_ARCH"
}

# ---------------------------------------------------------------------------
# Package a compiled binary as IPK + APK
# ---------------------------------------------------------------------------
package_binary() {
    local binary="$1"
    local pkg_arch="$2"

    echo "==> [$pkg_arch] Packaging IPK + APK (version $PKG_VERSION)..."

    local root
    root=$(mktemp -d)

    # -- Stage data tree ----------------------------------------------------
    mkdir -p "$root/data/usr/bin" \
             "$root/data/etc/init.d" \
             "$root/data/etc" \
             "$root/data/etc/arednsysupgrade.d"

    cp "$binary"                                "$root/data/usr/bin/$PKG_NAME"
    chmod 755                                   "$root/data/usr/bin/$PKG_NAME"
    cp "$SCRIPT_DIR/meshtastic-serial-udp.init" "$root/data/etc/init.d/$PKG_NAME"
    chmod 755                                   "$root/data/etc/init.d/$PKG_NAME"
    cp "$SCRIPT_DIR/meshtastic-serial-udp.conf" "$root/data/etc/$PKG_NAME.conf"

    # Preserve config across AREDN firmware upgrades
    echo "/etc/$PKG_NAME.conf" > "$root/data/etc/arednsysupgrade.d/KI5VMF.$PKG_NAME.conf"

    # -- Build IPK ----------------------------------------------------------
    mkdir -p "$root/control"
    echo "2.0" > "$root/debian-binary"

    cat > "$root/control/control" <<EOF
Package: ${PKG_NAME}
Version: ${PKG_VERSION}
Depends: kmod-usb-acm
Provides:
Source: package/${PKG_NAME}
Section: net
Priority: optional
Maintainer: Jacob McSwain (KI5VMF)
Architecture: ${pkg_arch}
Description: Bridge a USB-serial Meshtastic radio to UDP multicast
EOF

    echo "/etc/$PKG_NAME.conf" > "$root/control/conffiles"

    cp "$SCRIPT_DIR/postinst" "$root/control/postinst"
    cp "$SCRIPT_DIR/prerm"    "$root/control/prerm"
    chmod 755 "$root/control/postinst" "$root/control/prerm"

    (cd "$root/control" && tar czf ../control.tar.gz .)
    (cd "$root/data"    && tar czf ../data.tar.gz .)

    local ipk="${PKG_NAME}_${PKG_VERSION}_${pkg_arch}.ipk"
    (cd "$root" && tar czf "$ipk" control.tar.gz data.tar.gz debian-binary)

    rm -f "$SCRIPT_DIR/${PKG_NAME}_"*"_${pkg_arch}.ipk"
    mv "$root/$ipk" "$SCRIPT_DIR/"
    echo "    IPK: $SCRIPT_DIR/$ipk"

    # -- Build APK ----------------------------------------------------------
    cp "$SCRIPT_DIR/postinstall" "$root/data/.post-install"
    cp "$SCRIPT_DIR/prerm"       "$root/data/.pre-deinstall"
    cp "$SCRIPT_DIR/postupgrade" "$root/data/.post-upgrade"
    chmod 755 "$root/data/.post-install" "$root/data/.pre-deinstall" "$root/data/.post-upgrade"

    local apk="${PKG_NAME}-${pkg_arch}-${PKG_VERSION}.apk"
    rm -f "$SCRIPT_DIR/${PKG_NAME}-${pkg_arch}-"*.apk

    cat > "$root/data/.PKGINFO" <<EOF
pkgname = ${PKG_NAME}
pkgver = ${PKG_VERSION}
pkgdesc = Bridge a USB-serial Meshtastic radio to UDP multicast
url = https://github.com/USA-RedDragon/meshtastic-serial-udp
arch = ${pkg_arch}
origin = ${PKG_NAME}
maintainer = jacob@mcswain.dev
depend = kmod-usb-acm
EOF

    (cd "$root/data" && tar czf "$SCRIPT_DIR/$apk" .PKGINFO .post-install .pre-deinstall .post-upgrade *)
    echo "    APK: $SCRIPT_DIR/$apk"

    rm -rf "$root"
}

# ---------------------------------------------------------------------------
# Main — iterate requested architectures
# ---------------------------------------------------------------------------
for arch in $ARCHES; do
    build_arch "$arch"
done

echo ""
echo "==> All requested builds complete."
