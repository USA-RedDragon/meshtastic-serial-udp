# ---------------------------------------------------------------------------
# Shared architecture configuration for AREDN OpenWrt cross-compilation
#
# Sourced by build.sh and test.sh — not executable on its own.
#
# Sets: MUSL_TARGET, GCC_EXTRA_CONFIG, IMAGE_NAME, RUST_TARGET_ARG,
#        RUST_TARGET_DIR, CARGO_EXTRA_FLAGS, LINKER_ENV, EXTRA_RUSTFLAGS,
#        VERIFY_SOFT_FLOAT, USE_BUILD_STD, PKG_ARCH
# ---------------------------------------------------------------------------

ALL_ARCHES="mips_24kc mipsel_24kc arm_cortex-a7_neon-vfpv4 aarch64_cortex-a53 x86_64"

print_arch_list() {
    cat <<'ARCH_LIST'
  mips_24kc                  ath79 — Ubiquiti, TP-Link, Mikrotik SXT/LHG/LDF, GL.iNet
  mipsel_24kc                ramips — GL-MT1300, HaLowLink, Heltec, Alfa Tube-AHM, Cudy TR1200
  arm_cortex-a7_neon-vfpv4   ipq40xx — Mikrotik hAP ac2/ac3, SXTsq 5ac, GL-B1300
  aarch64_cortex-a53         mediatek/filogic — OpenWrt One, Cudy TR3000
  x86_64                     x86/64 — VMware ESXi, Proxmox, VirtualBox, Bhyve
ARCH_LIST
}

# renovate: datasource=docker depName=ghcr.io/USA-RedDragon/rust-cross
RUST_VERSION="1.94.1"
IMAGE_NAME="ghcr.io/usa-reddragon/rust-cross:${RUST_VERSION}"

configure_arch() {
    local arch="$1"
    case "$arch" in
        mips_24kc)
            MUSL_TARGET="mips-linux-muslsf"
            GCC_EXTRA_CONFIG="--with-float=soft"
            RUST_TARGET_ARG="mips-unknown-linux-musl"
            RUST_TARGET_DIR="mips-unknown-linux-musl"
            CARGO_EXTRA_FLAGS=""
            LINKER_ENV="CARGO_TARGET_MIPS_UNKNOWN_LINUX_MUSL_LINKER=mips-linux-muslsf-gcc"
            EXTRA_RUSTFLAGS="-C target-feature=+crt-static -C link-self-contained=no"
            VERIFY_SOFT_FLOAT=1
            USE_BUILD_STD=1
            PKG_ARCH="mips_24kc"
            RUNNER_ENV="CARGO_TARGET_MIPS_UNKNOWN_LINUX_MUSL_RUNNER=qemu-mips-static"
            ;;
        mipsel_24kc)
            MUSL_TARGET="mipsel-linux-muslsf"
            GCC_EXTRA_CONFIG="--with-float=soft"
            RUST_TARGET_ARG="mipsel-unknown-linux-musl"
            RUST_TARGET_DIR="mipsel-unknown-linux-musl"
            CARGO_EXTRA_FLAGS=""
            LINKER_ENV="CARGO_TARGET_MIPSEL_UNKNOWN_LINUX_MUSL_LINKER=mipsel-linux-muslsf-gcc"
            EXTRA_RUSTFLAGS="-C target-feature=+crt-static -C link-self-contained=no"
            VERIFY_SOFT_FLOAT=1
            USE_BUILD_STD=1
            PKG_ARCH="mipsel_24kc"
            RUNNER_ENV="CARGO_TARGET_MIPSEL_UNKNOWN_LINUX_MUSL_RUNNER=qemu-mipsel-static"
            ;;
        arm_cortex-a7_neon-vfpv4)
            MUSL_TARGET="arm-linux-musleabihf"
            GCC_EXTRA_CONFIG="--with-arch=armv7-a --with-fpu=neon-vfpv4 --with-float=hard"
            RUST_TARGET_ARG="armv7-unknown-linux-musleabihf"
            RUST_TARGET_DIR="armv7-unknown-linux-musleabihf"
            CARGO_EXTRA_FLAGS=""
            LINKER_ENV="CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-musleabihf-gcc"
            EXTRA_RUSTFLAGS="-C link-self-contained=no -C link-arg=-lgcc"
            VERIFY_SOFT_FLOAT=0
            USE_BUILD_STD=0
            PKG_ARCH="arm_cortex-a7_neon-vfpv4"
            RUNNER_ENV="CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_RUNNER=qemu-arm-static"
            ;;
        aarch64_cortex-a53)
            MUSL_TARGET="aarch64-linux-musl"
            GCC_EXTRA_CONFIG=""
            RUST_TARGET_ARG="aarch64-unknown-linux-musl"
            RUST_TARGET_DIR="aarch64-unknown-linux-musl"
            CARGO_EXTRA_FLAGS=""
            LINKER_ENV="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc"
            EXTRA_RUSTFLAGS="-C link-self-contained=no"
            VERIFY_SOFT_FLOAT=0
            USE_BUILD_STD=0
            PKG_ARCH="aarch64_cortex-a53"
            RUNNER_ENV="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64-static"
            ;;
        x86_64)
            MUSL_TARGET="x86_64-linux-musl"
            GCC_EXTRA_CONFIG=""
            RUST_TARGET_ARG="x86_64-unknown-linux-musl"
            RUST_TARGET_DIR="x86_64-unknown-linux-musl"
            CARGO_EXTRA_FLAGS=""
            LINKER_ENV="CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc"
            EXTRA_RUSTFLAGS="-C link-self-contained=no"
            VERIFY_SOFT_FLOAT=0
            USE_BUILD_STD=0
            PKG_ARCH="x86_64"
            RUNNER_ENV=""
            ;;
        *)
            echo "ERROR: Unknown architecture: $arch"
            return 1
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Run a cargo command inside the toolchain container
#
# Usage: run_cargo <project_root> <cargo_args...>
# ---------------------------------------------------------------------------
run_cargo() {
    local project_root="$1"
    shift
    local cargo_args="$*"

    if [ "$USE_BUILD_STD" -eq 1 ]; then
        docker run --rm \
            -v "$project_root":/src \
            -w /src \
            -e "MUSL_TARGET=$MUSL_TARGET" \
            ${LINKER_ENV:+-e "$LINKER_ENV"} \
            ${EXTRA_RUSTFLAGS:+-e "RUSTFLAGS=$EXTRA_RUSTFLAGS"} \
            ${RUNNER_ENV:+-e "$RUNNER_ENV"} \
            ${PKG_VERSION:+-e "PKG_VERSION=$PKG_VERSION"} \
            "$IMAGE_NAME" \
            bash -c '
                GCC_LIB_DIR=$($MUSL_TARGET-gcc -print-libgcc-file-name | xargs dirname)
                cp "${GCC_LIB_DIR}/libgcc_eh.a" "${GCC_LIB_DIR}/libunwind.a"

                cargo +nightly '"$cargo_args"' \
                    -Z build-std=std,panic_abort \
                    --target '"$RUST_TARGET_ARG"'
            '
    else
        docker run --rm \
            -v "$project_root":/src \
            -w /src \
            -e "MUSL_TARGET=$MUSL_TARGET" \
            ${LINKER_ENV:+-e "$LINKER_ENV"} \
            ${EXTRA_RUSTFLAGS:+-e "RUSTFLAGS=$EXTRA_RUSTFLAGS"} \
            ${RUNNER_ENV:+-e "$RUNNER_ENV"} \
            ${PKG_VERSION:+-e "PKG_VERSION=$PKG_VERSION"} \
            "$IMAGE_NAME" \
            bash -c '
                GCC_LIB_DIR=$($MUSL_TARGET-gcc -print-libgcc-file-name | xargs dirname)
                cp "${GCC_LIB_DIR}/libgcc_eh.a" "${GCC_LIB_DIR}/libunwind.a"

                cargo '"$cargo_args"' \
                    --target '"$RUST_TARGET_ARG"'
            '
    fi
}
