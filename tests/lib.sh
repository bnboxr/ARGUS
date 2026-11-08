#!/usr/bin/env bash
# Shared helpers for the ARGUS Phase I tests.
#
# Every test works the same way, which is the point: build a disk image that
# differs from the good one in exactly one way, boot it with the real `run.sh`,
# and assert on `run.sh`'s exit status plus named lines in the serial log. The
# verdict logic therefore lives in one place and is exercised by every test.
#
# Sourced, not executed. Expects ROOT to be set.

set -uo pipefail

# Tests must never touch the real dist/: a failing test that leaves a corrupted
# kernel in dist/ would make the next `./run.sh` lie about the build.
WORK_ROOT="${ARGUS_TEST_WORK:-$ROOT/artifacts/negative}"

# Timeout for one test boot. The kernel reaches its halt marker at ~3.5 s; a
# refused boot stops at ~3.4 s. 10 s is margin, not patience.
TEST_TIMEOUT="${TEST_TIMEOUT:-10}"

# --- a scratch copy of the boot disk ---------------------------------------
# new_image <dir>  ->  echoes the image path
new_image() {
    local dir="$1"
    mkdir -p "$dir/dist"
    cp "$ROOT/dist/argus-disk.img" "$dir/dist/argus-disk.img"
    # run.sh reports build time from this file; copy it so the log is complete.
    [ -f "$ROOT/dist/build-timing.txt" ] && cp "$ROOT/dist/build-timing.txt" "$dir/dist/"
    printf '%s\n' "$dir/dist/argus-disk.img"
}

# put_kernel <image> <file> — replace \ARGUSKRN.ELF inside the image.
put_kernel() {
    local img="$1" file="$2"
    MTOOLS_SKIP_CHECK=1 mcopy -o -i "$img@@1M" "$file" ::/ARGUSKRN.ELF
}

# remove_kernel <image> — delete \ARGUSKRN.ELF, for the "kernel missing" case.
remove_kernel() {
    local img="$1"
    MTOOLS_SKIP_CHECK=1 mdel -i "$img@@1M" ::/ARGUSKRN.ELF
}

# boot <dir> <logname> — boot that image with run.sh; echoes the exit status.
#
# The log goes to <dir>/<logname>.log and the exit status is returned, so the
# caller asserts on both without re-implementing the verdict.
boot() {
    local dir="$1" logname="$2"
    ARGUS_DIST="$dir/dist" \
    ARGUS_LOG="$dir/$logname.log" \
    BOOT_TIMEOUT="$TEST_TIMEOUT" \
    "$ROOT/run.sh" > "$dir/$logname.stdout" 2>&1
    printf '%s\n' "$?"
}

# --- assertions ------------------------------------------------------------
TESTS_FAILED=0

pass() { printf '\033[1;32mPASS\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31mFAIL\033[0m %s\n' "$*"; TESTS_FAILED=1; }

# expect_contains <log> <literal> <label>
expect_contains() {
    if grep -qF "$2" "$1"; then
        pass "$3: found \"$2\""
    else
        fail "$3: \"$2\" missing from $1"
    fi
}

# expect_absent <log> <literal> <label>
expect_absent() {
    if grep -qF "$2" "$1"; then
        fail "$3: \"$2\" should not appear in $1"
    else
        pass "$3: \"$2\" correctly absent"
    fi
}

# expect_status <actual> <expected> <label>
expect_status() {
    if [ "$1" -eq "$2" ]; then
        pass "$3: exit status $1"
    else
        fail "$3: exit status $1, expected $2"
    fi
}

# expect_nonzero <actual> <label>
expect_nonzero() {
    if [ "$1" -ne 0 ]; then
        pass "$2: exit status $1 (non-zero, as required)"
    else
        fail "$2: exit status 0 — a broken boot was reported as success"
    fi
}

# show_evidence <log> <grep-pattern> <label> — print the lines that prove it.
show_evidence() {
    printf '      evidence (%s):\n' "$3"
    grep -E "$2" "$1" | sed 's/^/        /' | head -8
}

summary() {
    printf '\n'
    if [ "$TESTS_FAILED" -ne 0 ]; then
        printf '\033[1;31m%s: FAILED\033[0m\n' "$1"
        return 1
    fi
    printf '\033[1;32m%s: PASSED\033[0m\n' "$1"
    return 0
}
