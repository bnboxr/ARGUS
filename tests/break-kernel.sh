#!/usr/bin/env bash
#
# Negative test: a broken kernel must make `run.sh` exit non-zero.
#
#   ./tests/break-kernel.sh
#
# The point of this test is the *exit status*, not the log: an automated build
# that reports success on a failed boot is worse than no automation. Two
# independent breakages are checked, because they fail in different places:
#
#   1. \ARGUSKRN.ELF replaced with a file that is not an ELF at all
#      -> the loader refuses at the ELF header, never jumps
#   2. \ARGUSKRN.ELF deleted
#      -> the loader cannot find the file at all
#
# Each uses its own copy of the disk image; dist/ is never modified.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib.sh
. "$ROOT/tests/lib.sh"

[ -f "$ROOT/dist/argus-disk.img" ] || { echo "build first: ./build.sh"; exit 2; }

WORK="$WORK_ROOT/break-kernel"
rm -rf "$WORK"; mkdir -p "$WORK"

printf '\033[1m[break-kernel] a broken boot must never exit 0\033[0m\n'

# --- 1. not an ELF ----------------------------------------------------------
dir="$WORK/not-an-elf"
img="$(new_image "$dir")"
printf 'this is not an ELF file\n' > "$dir/ARGUSKRN.ELF"
put_kernel "$img" "$dir/ARGUSKRN.ELF"

rc="$(boot "$dir" not-an-elf)"
log="$dir/not-an-elf.log"
printf '\n-- not-an-elf --\n'
expect_nonzero "$rc" "not-an-elf"
expect_contains "$log" "stage=elf-validate result=REJECT" "not-an-elf"
expect_absent   "$log" "stage=handoff event=jump" "not-an-elf no handoff"
show_evidence "$log" "elf-validate|REFUSED" "not-an-elf"
printf '      run.sh exit status: %s\n' "$rc"

# --- 2. kernel file absent --------------------------------------------------
dir="$WORK/kernel-absent"
img="$(new_image "$dir")"
remove_kernel "$img"

rc="$(boot "$dir" kernel-absent)"
log="$dir/kernel-absent.log"
printf '\n-- kernel-absent --\n'
expect_nonzero "$rc" "kernel-absent"
expect_contains "$log" "stage=kernel-read event=fail" "kernel-absent"
expect_absent   "$log" "[ARGUS-KERNEL] stage=entry event=banner" "kernel-absent no banner"
show_evidence "$log" "kernel-read event=fail|no_volume" "kernel-absent"
printf '      run.sh exit status: %s\n' "$rc"

summary "break-kernel"
