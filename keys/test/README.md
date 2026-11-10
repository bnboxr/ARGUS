# Test keys — development only, NOT a trust root

**The target key hierarchy is hybrid, and none of it is built.** The owner
ratified option C (`shared/spec/boot-trust-model-v1.md` §5): the user owns K1
(`db`), K2 (kernel) and K3 (manifest); ARGUS owns only K5 (recovery), which the
owner may remove at enrolment (`recovery_key = none`), accepting that fleet-level
recovery is then lost. Private keys never touch the ARGUS machine — signing is
done on an offline host and the loader holds public keys only. Enrolment does not
exist yet, so the keys in this directory are **test keys and not a trust root**:
the build trusts them only to prove that the mechanism works.

`manifest-key.pem` (private), `manifest-pub.pem`, `manifest-pub.der` (PKCS#1
`RSAPublicKey`, DER) and `other-key.pem` (a second key used only to produce a
manifest signed by "someone else" in the negative tests) are **committed to this
repository on purpose, and they are not a trust root.**

The public half is compiled into the loader (`bootloader/src/test_key.rs`). The
private half is in this directory. Anyone who can read this repository can sign
a manifest that the loader accepts. That is the point: it lets the whole
signed-boot path be built, tested and demonstrated before the real signing root
exists (`shared/spec/boot-trust-model-v1.md` §5 — decided: option C, hybrid; the
paragraph above is what that means here).

What a boot proven with these keys proves: **that the mechanism works** — the
manifest is read before anything is loaded, the signature is checked with a
verifier that has known-answer tests against `openssl`, a tampered kernel is
refused, a downgrade is refused. It proves nothing about who may boot this
machine.

What happens when the real key exists: `test_key.rs` is regenerated
(`sign-manifest pubkey --key <real-key> --rust`), the loader is rebuilt, and
every image signed with the test key stops booting. That is the correct
behaviour, not a regression.

Regenerate the pair (do not do this casually — it invalidates every built image):

    openssl genrsa -traditional -out keys/test/manifest-key.pem 2048
    openssl rsa -in keys/test/manifest-key.pem -pubout -out keys/test/manifest-pub.pem
    openssl rsa -in keys/test/manifest-key.pem -RSAPublicKey_out -outform DER \
        -out keys/test/manifest-pub.der
    target/release/sign-manifest pubkey --key keys/test/manifest-key.pem --rust \
        > bootloader/src/test_key.rs     # keep the warning header

`tests/crypto-kat.sh` fails if `test_key.rs` and `manifest-key.pem` ever disagree.
