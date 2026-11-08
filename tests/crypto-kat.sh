#!/usr/bin/env bash
#
# Cryptography known-answer tests — the part that decides whether the RSA
# verifier in the boot path is trustworthy.
#
# Three independent checks, none of which trusts this repository's own tests:
#
#   1. `cargo test -p argus-security` — FIPS SHA-256 vectors, CRC-32 check
#      values, modexp answers computed by Python's `pow`, and a signature made
#      by `openssl dgst -sha256 -sign` verifying inside the crate.
#   2. the signature *this repository's signer* produced for the real manifest
#      is opened with `openssl pkeyutl -verifyrecover`, i.e. an implementation
#      nobody here wrote recovers the same DigestInfo.
#   3. a signature made by `openssl dgst -sha256 -sign` over the manifest's
#      `payload_digest` is spliced into ARGUS.MF, and the host verifier — the
#      same code the loader links — accepts it.
#
# If any of these three disagreed, the verifier would be wrong in a way its own
# tests could not see.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tests/lib.sh"

export PATH="/opt/rust/cargo/bin:$PATH"
export RUSTUP_HOME="${RUSTUP_HOME:-/opt/rust/rustup}"
export CARGO_HOME="${CARGO_HOME:-/opt/rust/cargo}"

SIGN="${CARGO_TARGET_DIR:-$ROOT/target}/release/sign-manifest"
KEY="$ROOT/keys/test/manifest-key.pem"
WORK="$WORK_ROOT/crypto"
rm -rf "$WORK"
mkdir -p "$WORK"

# --- 1. the crate's own known-answer tests ---------------------------------
printf '\n\033[1m-- argus-security known-answer tests\033[0m\n'
if cargo test -p argus-security --release > "$WORK/cargo-test.log" 2>&1; then
    pass "cargo test -p argus-security ($(grep -c 'test result: ok' "$WORK/cargo-test.log") suite(s))"
    grep -E "^test [a-z]" "$WORK/cargo-test.log" | sed 's/^/        /' | head -40
else
    fail "cargo test -p argus-security — see $WORK/cargo-test.log"
    grep -E "^(error|failures:|---- )" -A5 "$WORK/cargo-test.log" | sed 's/^/        /' | head -30
fi

# --- 2. openssl recovers what our signer signed ----------------------------
printf '\n\033[1m-- openssl reads the signature our signer produced\033[0m\n'
[ -x "$SIGN" ] || { fail "sign-manifest not built"; summary "crypto-kat"; exit 1; }
openssl rsa -in "$KEY" -pubout -out "$WORK/pub.pem" 2>/dev/null
MF="$ROOT/dist/esp/ARGUS/ARGUS.MF"
if [ -f "$MF" ]; then
    python3 - "$MF" "$WORK" <<'PY'
import hashlib, sys
mf, work = sys.argv[1], sys.argv[2]
b = open(mf, 'rb').read()
payload_digest = b[181:213]
signature = b[217:473]
open(work + '/payload_digest.bin', 'wb').write(payload_digest)
open(work + '/signature.bin', 'wb').write(signature)
# What PKCS#1 v1.5 should recover: DigestInfo(SHA-256(payload_digest)).
open(work + '/digest.bin', 'wb').write(hashlib.sha256(payload_digest).digest())
PY
    if openssl pkeyutl -verifyrecover -pubin -inkey "$WORK/pub.pem" \
            -in "$WORK/signature.bin" -out "$WORK/recovered.bin" \
            -pkeyopt rsa_padding_mode:pkcs1 >/dev/null 2>&1; then
        python3 - "$WORK" <<'PY'
import sys
work = sys.argv[1]
rec = open(work + '/recovered.bin', 'rb').read()
digest = open(work + '/digest.bin', 'rb').read()
prefix = bytes.fromhex('3031300d060960864801650304020105000420')
ok = rec == prefix + digest
print(('PASS' if ok else 'FAIL') + ' openssl pkeyutl recovered DigestInfo(SHA-256(payload_digest)) from our signature')
sys.exit(0 if ok else 1)
PY
        [ $? -eq 0 ] || TESTS_FAILED=1
    else
        fail "openssl pkeyutl -verifyrecover refused the signature our signer produced"
    fi
else
    fail "$MF missing; run ./build.sh first"
fi

# --- 3. an openssl signature verifies here --------------------------------
printf '\n\033[1m-- a signature made by openssl dgst -sha256 -sign verifies here\033[0m\n'
if [ -f "$MF" ]; then
    # Sign the 32-byte payload_digest with openssl, splice it into the manifest,
    # and let the host verifier (same crate the loader links) check it.
    openssl dgst -sha256 -sign "$KEY" -out "$WORK/openssl.sig" "$WORK/payload_digest.bin"
    python3 - "$MF" "$WORK" <<'PY'
import sys
mf, work = sys.argv[1], sys.argv[2]
b = bytearray(open(mf, 'rb').read())
sig = open(work + '/openssl.sig', 'rb').read()
assert len(sig) == 256, len(sig)
b[217:473] = sig
open(work + '/openssl-signed.mf', 'wb').write(b)
PY
    if "$SIGN" verify --manifest "$WORK/openssl-signed.mf" \
            --kernel "$ROOT/dist/esp/ARGUSKRN.ELF" \
            --config "$ROOT/dist/esp/ARGUS/BOOT.CFG" \
            --initrd "$ROOT/dist/esp/INITRD.BIN" \
            --slot A --floor 1 --loader-version 3 \
            --pubkey "$ROOT/keys/test/manifest-pub.der" > "$WORK/verify.log" 2>&1; then
        pass "openssl-signed manifest accepted: $(cat "$WORK/verify.log")"
    else
        fail "openssl-signed manifest refused: $(cat "$WORK/verify.log")"
    fi

    # ... and the same manifest with one signature byte flipped must be refused.
    python3 - "$WORK" <<'PY'
import sys
work = sys.argv[1]
b = bytearray(open(work + '/openssl-signed.mf', 'rb').read())
b[300] ^= 0x01
open(work + '/tampered.mf', 'wb').write(b)
PY
    if "$SIGN" verify --manifest "$WORK/tampered.mf" \
            --kernel "$ROOT/dist/esp/ARGUSKRN.ELF" \
            --config "$ROOT/dist/esp/ARGUS/BOOT.CFG" \
            --slot A --pubkey "$ROOT/keys/test/manifest-pub.der" > "$WORK/verify2.log" 2>&1; then
        fail "a manifest with one flipped signature byte was accepted"
    else
        pass "one flipped signature byte refused: $(cat "$WORK/verify2.log")"
    fi
fi

# --- 4. the key compiled into the loader is the key on disk ---------------
printf '\n\033[1m-- loader constant matches keys/test\033[0m\n'
ON_DISK="$("$SIGN" pubkey --key "$KEY" --key-id 2>/dev/null)"
IN_LOADER="$(python3 - <<'PY'
import re
s = open('bootloader/src/test_key.rs').read()
m = re.search(r'TEST_MANIFEST_KEY_ID: \[u8; 16\] = \[(.*?)\];', s, re.S)
print(bytes(int(x, 16) for x in re.findall(r'0x([0-9a-f]{2})', m.group(1))).hex())
PY
)"
if [ "$ON_DISK" = "$IN_LOADER" ]; then
    pass "key_id in bootloader/src/test_key.rs == keys/test/manifest-key.pem ($ON_DISK)"
else
    fail "key_id mismatch: loader has $IN_LOADER, key file has $ON_DISK"
fi

summary "crypto-kat"
