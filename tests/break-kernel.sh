#!/usr/bin/env bash
# Proves run.sh fails loudly: replace the kernel with a file that is not an
# ELF at all, rebuild the image, and assert run.sh exits non-zero.
# STATUS: written, NOT yet verified — see README "STATUS".
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
cp dist/ARGUSKRN.ELF /tmp/kernel.good
printf 'this is not an ELF file\n' > dist/ARGUSKRN.ELF
cp dist/argus-disk.img /tmp/disk.good
MTOOLS_SKIP_CHECK=1 mcopy -o -i "dist/argus-disk.img@@1M" dist/ARGUSKRN.ELF ::/ARGUSKRN.ELF
./run.sh; rc=$?
cp /tmp/kernel.good dist/ARGUSKRN.ELF
cp /tmp/disk.good dist/argus-disk.img
if [ "$rc" -eq 0 ]; then echo "FAIL: run.sh exited 0 on a broken kernel"; exit 1; fi
echo "PASS: run.sh exited $rc on a broken kernel"
