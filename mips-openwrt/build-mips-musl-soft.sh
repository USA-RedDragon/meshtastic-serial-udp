#!/usr/bin/env bash
set -euo pipefail

IMAGE_NAME="mips-sf-rust"
TARGET_JSON="mips-openwrt/mips-unknown-linux-musl-soft.json"
PROFILE="release-cross"

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
    -v "$(pwd)/../target:/target" \
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
