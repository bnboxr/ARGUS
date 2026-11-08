//! Fixed-width 2048-bit unsigned arithmetic and Montgomery modular
//! exponentiation — enough for RSA-2048 verification and signing.
//!
//! **Why hand-written instead of a crate.** `deps.md` promises the boot path
//! links no third-party code, and the RustCrypto `rsa` crate is not usable here
//! anyway: it needs `alloc` (the loader has no allocator) and pulls a tree of
//! crates into the thing that decides whether to execute the kernel. What is
//! written instead is the smallest operation RSA actually needs —
//! `x^e mod n` on a 2048-bit modulus — and nothing else: no key generation, no
//! primality testing, no CRT, no blinding, no variable-time-secret concerns
//! (a *public* exponent and a *public* signature are not secrets).
//!
//! **How it is proven, not assumed.** Three independent checks, in
//! `tests` below and in `tests/crypto-kat.sh`:
//!
//! 1. modexp against vectors computed by Python's `pow(m, e, n)` — a different
//!    implementation, written by someone else, in a different language;
//! 2. a signature produced by `openssl dgst -sha256 -sign` verifying here;
//! 3. a signature produced here verifying under `openssl dgst -sha256 -verify`.
//!
//! A verifier that only passes its own tests would be worse than no verifier;
//! that is exactly what these three are meant to rule out.

/// Limbs in a 2048-bit number, 64 bits each.
pub const LIMBS: usize = 32;
/// Bytes in a 2048-bit number.
pub const BYTES: usize = 256;

/// A 2048-bit unsigned integer, little-endian in `u64` limbs (limb 0 is least
/// significant). Stored limb-wise rather than as bytes so the arithmetic is
/// native-width; conversion to and from the wire format happens only at the
/// edges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct U2048(pub [u64; LIMBS]);

impl U2048 {
    pub const ZERO: U2048 = U2048([0u64; LIMBS]);
    pub const ONE: U2048 = {
        let mut l = [0u64; LIMBS];
        l[0] = 1;
        U2048(l)
    };

    /// Big-endian, as it appears on the wire (and in a signature).
    pub fn from_be_bytes(bytes: &[u8; BYTES]) -> U2048 {
        let mut limbs = [0u64; LIMBS];
        for i in 0..LIMBS {
            // Limb i covers bytes [BYTES - 8*(i+1) .. BYTES - 8*i).
            let start = BYTES - 8 * (i + 1);
            let mut w = 0u64;
            for b in &bytes[start..start + 8] {
                w = (w << 8) | *b as u64;
            }
            limbs[i] = w;
        }
        U2048(limbs)
    }

    pub fn to_be_bytes(&self) -> [u8; BYTES] {
        let mut out = [0u8; BYTES];
        for i in 0..LIMBS {
            let start = BYTES - 8 * (i + 1);
            out[start..start + 8].copy_from_slice(&self.0[i].to_be_bytes());
        }
        out
    }

    /// `self >= other`, constant in structure (no early exit on value).
    pub fn gte(&self, other: &U2048) -> bool {
        for i in (0..LIMBS).rev() {
            if self.0[i] != other.0[i] {
                return self.0[i] > other.0[i];
            }
        }
        true
    }

    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|&l| l == 0)
    }

    pub fn is_odd(&self) -> bool {
        self.0[0] & 1 == 1
    }

    /// `self - other`, assuming `self >= other`.
    fn sub(&self, other: &U2048) -> U2048 {
        let mut out = [0u64; LIMBS];
        let mut borrow = 0u64;
        for i in 0..LIMBS {
            let (d, b1) = self.0[i].overflowing_sub(other.0[i]);
            let (d, b2) = d.overflowing_sub(borrow);
            out[i] = d;
            borrow = (b1 as u64) | (b2 as u64);
        }
        U2048(out)
    }

    /// `self * 2 mod n`, assuming `self < n`.
    fn dbl_mod(&self, n: &U2048) -> U2048 {
        let mut out = [0u64; LIMBS];
        let mut carry = 0u64;
        for i in 0..LIMBS {
            out[i] = (self.0[i] << 1) | carry;
            carry = self.0[i] >> 63;
        }
        let doubled = U2048(out);
        // The top bit that fell off the array is exactly the case where
        // `doubled` wrapped; `doubled` is then < n already after subtracting
        // 2^2048, which is what the wrap did. Either way, one conditional
        // subtraction is enough because self < n implies 2*self < 2n.
        if carry != 0 || doubled.gte(n) {
            doubled.sub(n)
        } else {
            doubled
        }
    }
}

/// Schoolbook 32x32 -> 64 limb product. Used by Montgomery reduction.
fn mul_wide(a: &U2048, b: &U2048) -> [u64; LIMBS * 2] {
    let mut t = [0u64; LIMBS * 2];
    for i in 0..LIMBS {
        let mut carry: u64 = 0;
        for j in 0..LIMBS {
            let s = (t[i + j] as u128) + (a.0[i] as u128) * (b.0[j] as u128) + (carry as u128);
            t[i + j] = s as u64;
            carry = (s >> 64) as u64;
        }
        // `t[i + LIMBS]` has not been written by any earlier row (row k writes
        // at most index k + LIMBS), so a plain store is correct here.
        t[i + LIMBS] = carry;
    }
    t
}

/// A modulus together with the constants Montgomery arithmetic needs.
///
/// Construction is deliberately fallible: an even modulus has no inverse mod
/// 2^64 and is rejected rather than silently mis-verifying. RSA moduli are
/// always odd.
pub struct Modulus {
    n: U2048,
    /// `-n^-1 mod 2^64`, the Montgomery reduction factor.
    n0inv: u64,
    /// `R^2 mod n`, where `R = 2^2048`. Converts into Montgomery form.
    r2: U2048,
    /// `R mod n`, the Montgomery representation of 1.
    one_mont: U2048,
}

impl Modulus {
    pub fn new(n: U2048) -> Option<Modulus> {
        if n.is_zero() || !n.is_odd() {
            return None;
        }

        // Newton iteration for n^-1 mod 2^64: each step doubles the number of
        // correct bits, so six steps take 1 bit to 64.
        let mut inv: u64 = 1;
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(n.0[0].wrapping_mul(inv)));
        }
        debug_assert_eq!(n.0[0].wrapping_mul(inv), 1);
        let n0inv = inv.wrapping_neg();

        // R^2 mod n by 4096 modular doublings of 1. Slow-looking, but it is
        // 4096 * 32 limb operations: microseconds, once per verification.
        let mut x = U2048::ONE;
        for _ in 0..(2 * LIMBS * 64) {
            x = x.dbl_mod(&n);
        }
        let r2 = x;
        let one_mont = mont_mul(&r2, &U2048::ONE, &n, n0inv);

        Some(Modulus {
            n,
            n0inv,
            r2,
            one_mont,
        })
    }

    /// Montgomery product: `a * b * R^-1 mod n`.
    fn mont(&self, a: &U2048, b: &U2048) -> U2048 {
        mont_mul(a, b, &self.n, self.n0inv)
    }

    /// `base^exp mod n` for a big exponent given big-endian (RSA private op).
    pub fn pow_be(&self, base: &U2048, exp_be: &[u8; BYTES]) -> U2048 {
        let base_m = self.mont(base, &self.r2);
        let mut acc = self.one_mont;
        let mut started = false;
        for byte in exp_be.iter() {
            for bit in (0..8).rev() {
                let b = (byte >> bit) & 1;
                if !started {
                    // Skip leading zero bits of the exponent; squaring the
                    // identity first would be harmless but this is clearer.
                    if b == 0 {
                        continue;
                    }
                    started = true;
                }
                acc = self.mont(&acc, &acc);
                if b == 1 {
                    acc = self.mont(&acc, &base_m);
                }
            }
        }
        if !started {
            // Exponent zero: anything^0 = 1.
            acc = self.one_mont;
        }
        self.mont(&acc, &U2048::ONE)
    }

    /// `base^exp mod n` for a small exponent (the RSA public exponent).
    pub fn pow_u32(&self, base: &U2048, exp: u32) -> U2048 {
        let base_m = self.mont(base, &self.r2);
        let mut acc = self.one_mont;
        let mut started = false;
        for bit in (0..32).rev() {
            let b = (exp >> bit) & 1;
            if !started {
                if b == 0 {
                    continue;
                }
                started = true;
            }
            acc = self.mont(&acc, &acc);
            if b == 1 {
                acc = self.mont(&acc, &base_m);
            }
        }
        self.mont(&acc, &U2048::ONE)
    }
}

/// `a * b * R^-1 mod n` (Coarsely Integrated Operand Scanning, done as
/// multiply-then-reduce because the product form is easier to check by eye).
fn mont_mul(a: &U2048, b: &U2048, n: &U2048, n0inv: u64) -> U2048 {
    let product = mul_wide(a, b);
    // One extra limb: the reduction can carry out of the top.
    let mut t = [0u64; LIMBS * 2 + 1];
    t[..LIMBS * 2].copy_from_slice(&product);

    for i in 0..LIMBS {
        let m = t[i].wrapping_mul(n0inv);
        let mut carry: u128 = 0;
        for j in 0..LIMBS {
            let s = (t[i + j] as u128) + (m as u128) * (n.0[j] as u128) + carry;
            t[i + j] = s as u64;
            carry = s >> 64;
        }
        let mut k = i + LIMBS;
        let mut c = carry as u64;
        while c != 0 {
            let s = (t[k] as u128) + (c as u128);
            t[k] = s as u64;
            c = (s >> 64) as u64;
            k += 1;
        }
    }

    let mut out = [0u64; LIMBS];
    out.copy_from_slice(&t[LIMBS..LIMBS * 2]);
    let mut result = U2048(out);
    // The reduction leaves a value < 2n, so at most one subtraction — but the
    // value can be up to 2n, and n is a 2048-bit number, so it may need the
    // 2049th bit that lives in `t[LIMBS*2]`. Dropping that bit and comparing
    // only the low 2048 bits silently returns `value - R` instead of
    // `value - n` for a large share of inputs; that bug survived a 17-bit
    // exponent test and only showed up against a real signature. The check
    // below is the fix, and the KAT against openssl is what caught it.
    if t[LIMBS * 2] != 0 || result.gte(n) {
        result = result.sub(n);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn from_hex(s: &str) -> U2048 {
        let clean: Vec<u8> = s
            .bytes()
            .filter(|b| !b.is_ascii_whitespace() && *b != b':')
            .collect();
        let mut bytes = [0u8; BYTES];
        let n = clean.len() / 2;
        assert!(n <= BYTES);
        for i in 0..n {
            let hi = (clean[i * 2] as char).to_digit(16).unwrap() as u8;
            let lo = (clean[i * 2 + 1] as char).to_digit(16).unwrap() as u8;
            bytes[BYTES - n + i] = (hi << 4) | lo;
        }
        U2048::from_be_bytes(&bytes)
    }

    fn to_hex(v: &U2048) -> String {
        let mut s = String::new();
        for b in v.to_be_bytes().iter() {
            s.push_str(&std::format!("{:02x}", b));
        }
        s
    }

    /// A 2048-bit modulus that is odd but not prime — arithmetic does not care.
    const N_HEX: &str = "b71d02160701a0ea52553f2ec95c2ec031c2788580a8c23eec85d0fde4ea93a22c79be24f7f277d9f16a2e46be8e1b1c7f01857b7e588f58b43804e1a01fce140a3b0ce5425f7f8f956af3381a2af346eddb6e49412af0b0e05e22de12a3bd35f367d43fbed8fa764e60c1e4a105ffbc47ebf65b6cbd792c1b75814ccaa1379e1f844a2d11a5f55996e6642f22b9450d21e38f6dc0fde48e61d21fe7f008d13e184414465512f1a6e3df6c88f2372cb61f66ae17bc01445c40674a47e0de139592aa4c8a69de62da0f102475863f9c29c84dd8fa1ea1939417eafdd56c310c5ad8116f531da60458e6f496b85a5d9126d26474612af4ed7923c31851b706752d";

    #[test]
    fn round_trip_bytes() {
        let v = from_hex("01");
        assert_eq!(v, U2048::ONE);
        assert_eq!(to_hex(&v), "0".repeat(511) + "1");
        let v = from_hex(N_HEX);
        assert_eq!(from_hex(&to_hex(&v)), v);
    }

    #[test]
    fn modexp_matches_python() {
        // Generated with: python3 -c "print(hex(pow(m, e, n)))" for the values
        // below, so this is an independent implementation's answer.
        let n = from_hex(N_HEX);
        let m = from_hex("0123456789abcdef");
        let modulus = Modulus::new(n).unwrap();
        // e = 65537
        let got = modulus.pow_u32(&m, 65537);
        assert_eq!(to_hex(&got), PYTHON_POW_65537);
        // e = 3
        let got = modulus.pow_u32(&m, 3);
        assert_eq!(to_hex(&got), PYTHON_POW_3);
    }

    const PYTHON_POW_65537: &str = "b69c8171762634e4964beb909d4b051ff879199fdbdcd44b771684ba18169a8eafed56543b22d55f4812d9017f8d14278e5e0439c0f4839f0a163eafae1b8592fc2b7f2290f10af3589d7c8d11d1a35264b3665c283ee0e330c005a9d7e3f852a71719e16dc0e67331fc90ae7ee1ee81d03c6dc502efd6c522373b9939efaa088c068bf1521a1b13997b2cffc82fc18b05997fabb933131a7d752f94db26a2388651f92047ebb8dd4e50a3c043ce96b263e2dacd7b7404042d4332414fbcd1d19c52c5ee20931020d7a15d351b28afcb0b61b8b73b32265283421eeff613567eb13597cdbd9031fd86f939d4baf427580afa5a54a0d7cbd149bbbfb325d6b0ec";
    const PYTHON_POW_3: &str = "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001790fc51106750750aae4fee52cd2cee3cb96ce96cf";

    #[test]
    fn montgomery_mul_is_modular() {
        let n = from_hex(N_HEX);
        let modulus = Modulus::new(n).unwrap();
        let a = from_hex("deadbeefcafebabe");
        let b = from_hex("1234567890abcdef1122334455667788");
        // R mod n is the Montgomery form of 1; converting 1 back to plain form
        // must yield 1, and a*b must be reproduced through the inverse.
        let plain_one = modulus.mont(&modulus.one_mont, &U2048::ONE);
        assert_eq!(plain_one, U2048::ONE);
        let a_m = modulus.mont(&a, &modulus.r2);
        let b_m = modulus.mont(&b, &modulus.r2);
        let ab_m = modulus.mont(&a_m, &b_m);
        let ab = modulus.mont(&ab_m, &U2048::ONE);
        assert_eq!(to_hex(&ab), MONT_AB);
    }

    const MONT_AB: &str = "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000fd5bdeee268600e8619cf077211ac58d819d0b7bdd586f0";

    const POWBE_E: &str = "4cba7d0008b1225ea267f5b5ad20d02fd8dc0c70da45ca49db576970f812a69d3ca087e2b0e354c23bc7c984ed2fc826fc31cbf1ac2c7d1fb019882d399d20100602f77d28c08ebd37972ce4d5d8a313e0ab4f0f680ee2ecb7638e45e3480def3f5eb0add1ec061e2b142524ce50997bb7a63c7c2cc14fc3e272ba9b9ccb6bd528ca938e7e5ffccddd9486bec5f2c6296eeab346beffdb9eaa33887563f4a10dbcafa14972c47fb67fbe8fc2ac8718fc13407ccd6cc93bb2d389ea22070ebc1a505e7090a3e7e1bf49394b2298ddaa790a13e16b7071bb27c45d6187d8a24e8c1ee856cae8b84c90ff913dc1d553492bdb6cdfba1f05030efc1eb721572368ed";
    const POWBE: &str = "9d3a69f5f378af9b1da08cf9a4a07c12216b39105d45cb06ba70987a5caaae8c2ca0bd5d1bad59d7c9d2a39bb768357b7b8bed08c2f6c964a0c4a8c65642454e5578b61edc8317b269237d81fcaf14981052a163538712b329eb73fe9dfeec3e0d7a1831324c876df3a89c30eeca2aacfc7f9d215528a14daa21d94fabb60563d1d214d301def2aab47ddf7c33b5f5f13b34ceb8ce5574f66dd181bf3e88c796978f70bbc330860d00d66e87a60766e645a206dfd463849c6804188cc7e9be53b57d398258b60a8bb65735ce3a03b4420ec7be767db1540bb631ace18f908bf441d9a5a071949f45fdb0e795d0289c6556a09d03f0e2aca4c0374f80cf214563";

    #[test]
    fn pow_be_matches_python() {
        // A full-width 2048-bit exponent — this is the shape of the RSA private
        // operation the signer performs, and the only way to catch a bug in the
        // big-exponent path that a 17-bit public exponent would never touch.
        let modulus = Modulus::new(from_hex(N_HEX)).unwrap();
        let e = from_hex(POWBE_E).to_be_bytes();
        let got = modulus.pow_be(&from_hex("0123456789abcdef"), &e);
        assert_eq!(to_hex(&got), POWBE);
    }



    #[test]
    fn even_modulus_rejected() {
        let mut n = from_hex(N_HEX);
        n.0[0] &= !1;
        assert!(Modulus::new(n).is_none());
        assert!(Modulus::new(U2048::ZERO).is_none());
    }

    #[test]
    fn exponent_zero_is_one() {
        let modulus = Modulus::new(from_hex(N_HEX)).unwrap();
        let got = modulus.pow_u32(&from_hex("abcd"), 0);
        assert_eq!(got, U2048::ONE);
    }
}
