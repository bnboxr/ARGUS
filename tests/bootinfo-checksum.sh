#!/usr/bin/env bash
#
# Negative test: a BootInfo whose checksum does not match must be refused by
# the kernel, and the boot must be reported as failed.
#
#   ./tests/bootinfo-checksum.sh
#
# How the corruption is produced without weakening the product: the loader is
# rebuilt with `--cfg argus_test_corrupt_bootinfo` (build.sh sets it only when
# ARGUS_TEST_CORRUPT_BOOTINFO=1). That build writes one flag bit *after*
# sealing the structure, which is exactly the silent-corruption case the
# checksum exists for. The cfg is compile-time, so a normal build cannot be
# talked into doing this at runtime.
#
# The corrupted loader is built into its own dist/, so dist/ is untouched.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib.sh
. "$ROOT/tests/lib.sh"

# A separate target dir: toggling RUSTFLAGS would otherwise rebuild the whole
# workspace every time this test and ./build.sh alternate.
CORRUPT_TARGET="${ARGUS_CORRUPT_TARGET:-/opt/argus-target-corrupt}"
WORK="$WORK_ROOT/bootinfo"
rm -rf "$WORK"; mkdir -p "$WORK/dist"

printf '\033[1m[bootinfo-checksum] building a loader that corrupts BootInfo after sealing\033[0m\n'
if ! ARGUS_DIST="$WORK/dist" \
     ARGUS_TEST_CORRUPT_BOOTINFO=1 \
     CARGO_TARGET_DIR="$CORRUPT_TARGET" \
     "$ROOT/build.sh" > "$WORK/build.log" 2>&1; then
    printf '\033[1;31mFAIL\033[0m corrupted-loader build failed, see %s\n' "$WORK/build.log"
    exit 1
fi
grep -q "TEST HOOK" "$WORK/build.log" \
    && pass "build.sh reported the test hook" \
    || fail "build.sh did not report the test hook"

# Boot the corrupted build with the *good* kernel: the kernel must reject the
# structure the loader handed it, not the other way round.
rc="$(boot "$WORK" bootinfo-checksum)"
log="$WORK/bootinfo-checksum.log"

printf '\n-- bootinfo-checksum (expecting E_BOOTINFO_CHECKSUM) --\n'
expect_nonzero "$rc" "bootinfo-checksum"
expect_contains "$log" "stage=bootinfo event=test_corruption injected=1" "loader injected the corruption"
expect_contains "$log" "reason=E_BOOTINFO_CHECKSUM" "kernel refused the structure"
expect_contains "$log" "stage=halt event=refused reason=E_BOOTINFO_CHECKSUM" "kernel halted instead of continuing"
expect_absent   "$log" "stage=halt event=boot_proven" "no boot_proven marker"
expect_absent   "$log" "field=memory_map_meta" "kernel stopped before dumping fields"
show_evidence "$log" "checksum|REFUSED|refused|test_corruption" "bootinfo-checksum"
drop_image "$WORK"

summary "bootinfo-checksum"
