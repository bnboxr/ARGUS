#!/usr/bin/env bash
#
# Run the whole Phase I milestone 1 test suite.
#
#   ./tests/run-all.sh
#
# Exits 0 only if every test passes. This is the command a follow-up session
# should run first: it re-establishes the ground truth in about two minutes.
#
# Order matters only in that the positive boot comes first — if that fails,
# every negative test is meaningless and the run stops.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RESULTS=()
overall=0

step() {
    local name="$1"; shift
    printf '\n\033[1m=== %s ===\033[0m\n' "$name"
    if "$@"; then
        RESULTS+=("PASS  $name")
    else
        RESULTS+=("FAIL  $name")
        overall=1
    fi
}

printf '\033[1m[run-all] %s\033[0m\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# 0. static checks first: they are instant and catch the class of bug that
#    already cost this project two sessions.
step "protocol GUIDs" python3 "$ROOT/tests/protocol-guid.py" "$ROOT/bootloader/src/efi.rs"

# 1. the positive boot. Everything else is meaningless if this fails.
step "boot (positive)" "$ROOT/run.sh"

# 2. negative tests.
step "corrupted ELF kernels" "$ROOT/tests/negative-tests.sh"
step "broken kernel exit status" "$ROOT/tests/break-kernel.sh"
step "BootInfo checksum refusal" "$ROOT/tests/bootinfo-checksum.sh"

# 3. firmware matrix. Slowest; last.
step "firmware matrix" "$ROOT/tests/firmware-matrix.sh"

printf '\n\033[1m[run-all] summary\033[0m\n'
for r in "${RESULTS[@]}"; do printf '  %s\n' "$r"; done

printf '\n'
if [ "$overall" -ne 0 ]; then
    printf '\033[1;31m[run-all] FAILED\033[0m — see the logs above and artifacts/\n'
    exit 1
fi
printf '\033[1;32m[run-all] ALL TESTS PASSED\033[0m\n'
exit 0
