#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Package pre-built binaries as OpenWrt IPK + APK
#
# Usage:
#   ./openwrt/build.sh <arch>       Package one architecture
#   ./openwrt/build.sh all          Package all AREDN architectures
#
# Expects binaries already built under target/<rust-target>/release/.
# Typically called by the root build.sh after compiling.
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROFILE="release"
PKG_NAME="meshtastic-serial-udp"
PKG_VERSION="${PKG_VERSION:-0.0.0}"

# shellcheck source=../hack/arch-config.sh
source "$PROJECT_ROOT/hack/arch-config.sh"

usage() {
    echo "Usage: $(basename "$0") <arch|all>"
    echo ""
    echo "Architectures:"
    print_arch_list
    echo "  all                        Package all of the above"
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
# Package one architecture
# ---------------------------------------------------------------------------
package_arch() {
    local arch="$1"
    configure_arch "$arch"

    BINARY="$PROJECT_ROOT/target/$RUST_TARGET_DIR/$PROFILE/$PKG_NAME"

    if [ ! -f "$BINARY" ]; then
        echo "ERROR: Binary not found: $BINARY"
        echo "       Run ./build.sh $arch first to compile."
        exit 1
    fi

    package_binary "$BINARY" "$PKG_ARCH"
}

# ---------------------------------------------------------------------------
# Turn PKG_VERSION into a version apk accepts. CI versions look like
# 1.2.3-<sha> or dev-<sha>, and apk only allows -r<number> after a dash.
# ---------------------------------------------------------------------------
apk_version() {
    local v="$1"
    case "$v" in
        [0-9]*-r[0-9]*) echo "$v" ;;
        [0-9]*-*)       echo "${v%%-*}" ;;
        [0-9]*)         echo "$v" ;;
        *)              echo "0.0.0" ;;
    esac
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
    # AREDN 4.26 (OpenWrt 25.12) uses apk-tools 3, which rejects a plain
    # tar.gz with a .PKGINFO ("v2 package format error"). Build a native
    # apk v3 package with apk mkpkg, the same tool OpenWrt uses.
    local apk="${PKG_NAME}-${pkg_arch}-${PKG_VERSION}.apk"
    rm -f "$SCRIPT_DIR/${PKG_NAME}-${pkg_arch}-"*.apk

    # File list in the same form OpenWrt's include/package-pack.mk writes:
    # every file and symlink, with a leading slash, sorted, not listing
    # itself. AREDN's Packages page takes the package name from this file
    # and only then keeps the package for reinstall after a firmware upgrade.
    mkdir -p "$root/data/lib/apk/packages"
    (cd "$root/data" && find . -type f,l -printf "/%P\n" | LC_ALL=C sort > "$root/files.list")
    mv "$root/files.list" "$root/data/lib/apk/packages/${PKG_NAME}.list"

    mkdir -p "$root/scripts"
    cp "$SCRIPT_DIR/postinst"    "$root/scripts/post-install"
    cp "$SCRIPT_DIR/prerm"       "$root/scripts/pre-deinstall"
    cp "$SCRIPT_DIR/postupgrade" "$root/scripts/post-upgrade"

    docker run --rm \
        -v "$root":/pkg:ro \
        -v "$SCRIPT_DIR":/out \
        -e "APK=$apk" \
        -e "PKG_NAME=$PKG_NAME" \
        -e "PKG_VERSION=$(apk_version "$PKG_VERSION")" \
        -e "PKG_ARCH=$pkg_arch" \
        -e "OWNER=$(id -u):$(id -g)" \
        "$APK_IMAGE" \
        sh -euc '
            cp -a /pkg/data /build
            chown -R 0:0 /build
            apk mkpkg \
                --info "name:$PKG_NAME" \
                --info "version:$PKG_VERSION" \
                --info "description:Bridge a USB-serial Meshtastic radio to UDP multicast" \
                --info "arch:$PKG_ARCH" \
                --info "origin:$PKG_NAME" \
                --info "url:https://github.com/USA-RedDragon/meshtastic-serial-udp" \
                --info "maintainer:jacob@mcswain.dev" \
                --info "depends:kmod-usb-acm" \
                --script "post-install:/pkg/scripts/post-install" \
                --script "pre-deinstall:/pkg/scripts/pre-deinstall" \
                --script "post-upgrade:/pkg/scripts/post-upgrade" \
                --files /build \
                --output "/out/$APK"
            chown "$OWNER" "/out/$APK"
        '
    echo "    APK: $SCRIPT_DIR/$apk"

    # Convienience symlink for latest version
    ln -sf "$SCRIPT_DIR/$ipk" "$SCRIPT_DIR/${PKG_NAME}_${pkg_arch}.ipk"
    ln -sf "$SCRIPT_DIR/$apk" "$SCRIPT_DIR/${PKG_NAME}_${pkg_arch}.apk"

    rm -rf "$root"
}

# ---------------------------------------------------------------------------
# Main — iterate requested architectures
# ---------------------------------------------------------------------------
for arch in $ARCHES; do
    package_arch "$arch"
done

echo ""
echo "==> All requested packages complete."
