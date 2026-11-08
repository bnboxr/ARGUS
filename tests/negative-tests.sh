#!/usr/bin/env bash
#
# Negative tests: a corrupted kernel must be rejected with its own named error
# code, the loader must NOT jump to it, and `run.sh` must exit non-zero.
#
#   ./tests/negative-tests.sh
#
# Cases, each corrupting exactly one thing:
#   1. bad e_ident magic            -> E_MAGIC
#   2. truncated program header table -> E_PHDR_TRUNCATED
#   3. p_vaddr outside usable RAM   -> E_VADDR_UNUSABLE
#   4. kernel file absent           -> the loader's own "not found" path
#
# Each case builds its own copy of the disk image, so nothing here can leave a
# corrupted kernel behind in dist/.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib.sh
. "$ROOT/tests/lib.sh"

KERNEL="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-unknown-none/release/argus-kernel"
if [ ! -f "$KERNEL" ]; then
    # build.sh may have used a different target dir; fall back to dist/.
    KERNEL="$ROOT/dist/ARGUSKRN.ELF"
fi
[ -f "$KERNEL" ] || { echo "build first: ./build.sh"; exit 2; }
[ -f "$ROOT/dist/argus-disk.img" ] || { echo "build first: ./build.sh"; exit 2; }

WORK="$WORK_ROOT/elf"
rm -rf "$WORK"; mkdir -p "$WORK"
python3 "$ROOT/tests/corrupt.py" "$KERNEL" "$WORK" >/dev/null

printf '\033[1m[negative] corrupted-ELF tests\033[0m\n'

run_case() { # <name> <file|-> <expected-code> <expected-detail-substring>
    local name="$1" file="$2" code="$3" detail="$4"
    local dir="$WORK/$name"
    local img
    img="$(new_image "$dir")"

    if [ "$file" = "-" ]; then
        remove_kernel "$img"
    else
        put_kernel "$img" "$file"
    fi

    local rc
    rc="$(boot "$dir" "$name")"
    local log="$dir/$name.log"

    printf '\n-- %s (expecting %s) --\n' "$name" "$code"
    expect_nonzero "$rc" "$name"
    if [ -n "$code" ]; then
        expect_contains "$log" "code=$code" "$name"
    else
        expect_contains "$log" "stage=kernel-read event=fail" "$name loader refused the read"
    fi
    if [ -n "$detail" ]; then
        expect_contains "$log" "$detail" "$name detail"
    fi
    expect_absent "$log" "[ARGUS-KERNEL] stage=entry event=banner" "$name no kernel banner"
    expect_absent "$log" "stage=handoff event=jump" "$name no handoff"
    show_evidence "$log" "elf-validate|stage=handoff result=REFUSED|stage=kernel-read event=fail|reason=" "$name"
    drop_image "$dir"
}

run_case bad-magic        "$WORK/bad-magic.elf"        E_MAGIC            "e_ident"
run_case truncated-phdr   "$WORK/truncated-phdr.elf"   E_PHDR_TRUNCATED   "program header"
run_case vaddr-unusable   "$WORK/vaddr-unusable.elf"   E_VADDR_UNUSABLE   "usable"
run_case kernel-missing   "-"                          ""                 ""

summary "negative-tests"
