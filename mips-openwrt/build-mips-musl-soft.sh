#!/usr/bin/env bash
set -euo pipefail

IMAGE_NAME="mips-sf-rust"
TARGET_JSON="mips-openwrt/mips-unknown-linux-musl-soft.json"
PROFILE="release-cross"
PKG_NAME="meshtastic-serial-udp"
PKG_VER=0.1.0
PKG_REL="r$(($(date +%s) - $(date -d '2026-01-01 00:00:00' +%s)))"
PKG_VERSION="${PKG_VER}-${PKG_REL}"
PKG_ARCH="mips_24kc"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Build the Docker image if it doesn't exist
if ! docker image inspect "$IMAGE_NAME" &>/dev/null; then
    echo "==> Building soft-float MIPS toolchain Docker image (one-time)..."
    docker build -t "$IMAGE_NAME" -f mips-openwrt/Dockerfile.mips-softfloat .
fi

echo "==> Building meshtastic-serial-udp for MIPS soft-float..."
docker run --rm \
    -v "$(pwd)":/src \
    -w /src \
    "$IMAGE_NAME" \
    bash -c '
        # Create empty libunwind.a stub — std links -lunwind even with panic=abort
        mips-linux-muslsf-ar rcs /opt/mips-sf/lib/gcc/mips-linux-muslsf/9.4.0/libunwind.a
        cargo +nightly build \
            -Z build-std=std,panic_abort \
            -Z json-target-spec \
            --target /src/'"$TARGET_JSON"' \
            --profile '"$PROFILE"'
    '

BINARY="target/mips-unknown-linux-musl-soft/$PROFILE/meshtastic-serial-udp"

echo "==> Build complete:"
ls -lh "$BINARY"
file "$BINARY"

# Verify zero hard-float instructions
echo "==> Checking for hardware float instructions..."
COUNT=$(docker run --rm \
    -v "$(pwd)/target:/target" \
    "$IMAGE_NAME" \
    bash -c "mips-linux-muslsf-objdump -d /target/mips-unknown-linux-musl-soft/$PROFILE/meshtastic-serial-udp \
    | grep -c -E '\blwc1|swc1|mtc1|mfc1|add\.s|mul\.s|div\.s|cvt\.|mov\.s\b'" \
) || true
echo "    Hard-float instructions: $COUNT"
if [ "${COUNT:-0}" -eq 0 ]; then
    echo "    OK — pure soft-float binary"
else
    echo "    WARNING — binary contains hardware float instructions"
fi

# ---------------------------------------------------------------------------
# Package as IPK (opkg) and APK (apk-tools)
# ---------------------------------------------------------------------------
echo "==> Packaging IPK + APK (version ${PKG_VERSION})..."

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT

# -- Stage data tree --------------------------------------------------------
mkdir -p "$ROOT/data/usr/bin" \
         "$ROOT/data/etc/init.d" \
         "$ROOT/data/etc" \
         "$ROOT/data/etc/arednsysupgrade.d"

cp "$BINARY"                                "$ROOT/data/usr/bin/$PKG_NAME"
chmod 755                                   "$ROOT/data/usr/bin/$PKG_NAME"
cp "$SCRIPT_DIR/meshtastic-serial-udp.init" "$ROOT/data/etc/init.d/$PKG_NAME"
chmod 755                                   "$ROOT/data/etc/init.d/$PKG_NAME"
cp "$SCRIPT_DIR/meshtastic-serial-udp.conf" "$ROOT/data/etc/$PKG_NAME.conf"

# Preserve config across AREDN firmware upgrades
echo "/etc/$PKG_NAME.conf" > "$ROOT/data/etc/arednsysupgrade.d/KI5VMF.$PKG_NAME.conf"

# -- Build IPK ---------------------------------------------------------------
mkdir -p "$ROOT/control"

cat > "$ROOT/debian-binary" <<'EOF'
2.0
EOF

cat > "$ROOT/control/control" <<EOF
Package: ${PKG_NAME}
Version: ${PKG_VERSION}
Depends: kmod-usb-acm
Provides:
Source: package/${PKG_NAME}
Section: net
Priority: optional
Maintainer: Jacob McSwain (KI5VMF)
Architecture: ${PKG_ARCH}
Description: Bridge a USB-serial Meshtastic radio to UDP multicast
EOF

# Mark config as conffile so opkg preserves user edits on package upgrade
echo "/etc/$PKG_NAME.conf" > "$ROOT/control/conffiles"

cp "$SCRIPT_DIR/postinst" "$ROOT/control/postinst"
cp "$SCRIPT_DIR/prerm"    "$ROOT/control/prerm"
chmod 755 "$ROOT/control/postinst" "$ROOT/control/prerm"

(cd "$ROOT/control" && tar czf ../control.tar.gz .)
(cd "$ROOT/data"    && tar czf ../data.tar.gz .)
(cd "$ROOT"         && tar czf "${PKG_NAME}_${PKG_VERSION}_${PKG_ARCH}.ipk" \
                            control.tar.gz data.tar.gz debian-binary)

IPK="${PKG_NAME}_${PKG_VERSION}_${PKG_ARCH}.ipk"
rm -f "$SCRIPT_DIR"/${PKG_NAME}_*_${PKG_ARCH}.ipk
mv "$ROOT/$IPK" "$SCRIPT_DIR/"

echo "    IPK: $SCRIPT_DIR/$IPK"

# -- Build APK ---------------------------------------------------------------
cp "$SCRIPT_DIR/postinstall" "$ROOT/data/.post-install"
cp "$SCRIPT_DIR/prerm"       "$ROOT/data/.pre-deinstall"
cp "$SCRIPT_DIR/postupgrade" "$ROOT/data/.post-upgrade"
chmod 755 "$ROOT/data/.post-install" "$ROOT/data/.pre-deinstall" "$ROOT/data/.post-upgrade"

APK="${PKG_NAME}-${PKG_VERSION}.apk"
rm -f "$SCRIPT_DIR"/${PKG_NAME}-*.apk

mkdir -p "$ROOT/apk"
cat > "$ROOT/apk/.PKGINFO" <<EOF
pkgname = ${PKG_NAME}
pkgver = ${PKG_VERSION}
pkgdesc = Bridge a USB-serial Meshtastic radio to UDP multicast
url = https://github.com/USA-RedDragon/meshtastic-serial-udp
arch = ${PKG_ARCH}
origin = ${PKG_NAME}
maintainer = jacob@mcswain.dev
depend = kmod-usb-acm
EOF
# Combine .PKGINFO + data into a single tar.gz
cp "$ROOT/apk/.PKGINFO" "$ROOT/data/.PKGINFO"
(cd "$ROOT/data" && tar czf "$SCRIPT_DIR/$APK" .PKGINFO .post-install .pre-deinstall .post-upgrade *)

echo "    APK: $SCRIPT_DIR/$APK"

echo "==> Packaging complete."