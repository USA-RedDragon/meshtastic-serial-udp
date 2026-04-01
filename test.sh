#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Multi-architecture cross-compile test runner
#
# Usage:
#   ./test.sh <arch>       Test one architecture
#   ./test.sh all          Test all AREDN architectures (default)
#   ./test.sh              Same as "all"
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$SCRIPT_DIR"

# shellcheck source=hack/arch-config.sh
source "$PROJECT_ROOT/hack/arch-config.sh"

usage() {
    echo "Usage: $(basename "$0") [arch|all]"
    echo ""
    echo "Architectures:"
    print_arch_list
    echo "  all                        Test all of the above (default)"
    exit 1
}

REQUESTED="${1:-all}"
if [ "$REQUESTED" = "all" ]; then
    ARCHES="$ALL_ARCHES"
else
    ARCHES="$REQUESTED"
fi

test_arch() {
    local arch="$1"
    configure_arch "$arch"

    echo ""
    echo "================================================================"
    echo "==> [$arch] Testing"
    echo "================================================================"

    echo "==> [$arch] Running tests..."
    run_cargo "$PROJECT_ROOT" "test"

    echo "==> [$arch] Tests compiled successfully"
}

for arch in $ARCHES; do
    test_arch "$arch"
done
