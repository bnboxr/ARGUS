//! `sign-manifest` — host-side producer and checker for `ARGUS.MF`.
//!
//! This is an ordinary `std` binary that runs on the build machine, never in
//! the boot path. It exists so that the format has exactly one encoder and one
//! decoder: it links `argus-security`, the same crate the UEFI loader links.
//!
//! Subcommands:
//!
//! ```text
//! sign-manifest sign   --kernel K --config C [--initrd I] --slot A|B
//!                      --security-version N --slot-generation N
//!                      [--manifest-version N] [--min-loader-version N]
//!                      [--issued-counter N] --key KEY.pem
//!                      [--key-id-from OTHER.pem] --out ARGUS.MF
//! sign-manifest verify --manifest M --kernel K --config C [--initrd I]
//!                      --slot A|B [--floor N] [--loader-version N]
//!                      --key KEY.pem|--pubkey PUB.der
//! sign-manifest print  --manifest M
//! sign-manifest pubkey --key KEY.pem [--rust] [--key-id]
//! sign-manifest floor  --floor N [--seq N] --out FLOOR
//! ```
//!
//! `verify` re-implements the loader's checks on the host so that `build.sh`
//! can prove the artifact it just produced is the artifact the loader will
//! accept — before QEMU is started. It reports the same named codes.

use std::process::ExitCode;

use argus_security::floor::{encode_floor_file, FloorRecord};
use argus_security::manifest::{Manifest, SIG_ALG_RSA2048_PKCS1_SHA256, SIG_LEN, SIZE, SLOT_A, SLOT_B};
use argus_security::rsa::{sign_pkcs1v15_sha256, verify_pkcs1v15_sha256, PublicKey};
use argus_security::sha256;

// ---------------------------------------------------------------------------
// Minimal PEM / base64 / DER, enough for an RSA key and nothing more.
// ---------------------------------------------------------------------------

fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in text.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c).ok_or_else(|| format!("invalid base64 character {:?}", c as char))?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let mut body = String::new();
    let mut inside = false;
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            inside = true;
            continue;
        }
        if line.starts_with("-----END") {
            inside = false;
            continue;
        }
        if inside {
            body.push_str(line);
        }
    }
    if body.is_empty() {
        return Err("no PEM body found (expected -----BEGIN ... KEY-----)".into());
    }
    base64_decode(&body)
}

/// One DER TLV: (tag, contents).
fn der_read(buf: &[u8], pos: &mut usize) -> Result<(u8, Vec<u8>), String> {
    if *pos + 2 > buf.len() {
        return Err("truncated DER".into());
    }
    let tag = buf[*pos];
    let mut p = *pos + 1;
    let first = buf[p];
    p += 1;
    let len = if first & 0x80 == 0 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || p + n > buf.len() {
            return Err("bad DER length".into());
        }
        let mut l = 0usize;
        for _ in 0..n {
            l = (l << 8) | buf[p] as usize;
            p += 1;
        }
        l
    };
    if p + len > buf.len() {
        return Err("DER length past end of buffer".into());
    }
    let body = buf[p..p + len].to_vec();
    *pos = p + len;
    Ok((tag, body))
}

/// An RSA private key, as the fields this tool needs.
struct PrivateKey {
    n: Vec<u8>,
    e: u32,
    d: Vec<u8>,
}

fn parse_private_key(pem: &str) -> Result<PrivateKey, String> {
    let der = pem_to_der(pem)?;
    let mut pos = 0;
    let (tag, seq) = der_read(&der, &mut pos)?;
    if tag != 0x30 {
        return Err("private key is not a DER SEQUENCE".into());
    }
    let mut p = 0;
    let mut ints: Vec<Vec<u8>> = Vec::new();
    while p < seq.len() && ints.len() < 4 {
        let (t, body) = der_read(&seq, &mut p)?;
        if t != 0x02 {
            return Err("expected a DER INTEGER in the private key".into());
        }
        ints.push(body);
    }
    if ints.len() < 3 {
        return Err("private key does not have (version, n, e, d)".into());
    }
    // INTEGERs are signed: a leading 0x00 keeps the value positive.
    let strip = |mut v: Vec<u8>| {
        while v.len() > 1 && v[0] == 0 {
            v.remove(0);
        }
        v
    };
    let n = strip(ints[1].clone());
    let e_bytes = strip(ints[2].clone());
    let d = strip(ints[3].clone());
    if e_bytes.len() > 4 {
        return Err("public exponent too large for this tool".into());
    }
    let mut e = 0u32;
    for b in &e_bytes {
        e = (e << 8) | *b as u32;
    }
    Ok(PrivateKey { n, e, d })
}

/// Pad or reject to exactly 256 bytes.
fn to_fixed<const N: usize>(v: &[u8], what: &str) -> Result<[u8; N], String> {
    if v.len() > N {
        return Err(format!("{} is {} bytes, expected at most {}", what, v.len(), N));
    }
    let mut out = [0u8; N];
    out[N - v.len()..].copy_from_slice(v);
    Ok(out)
}

fn public_key_from_private(k: &PrivateKey) -> Result<PublicKey, String> {
    Ok(PublicKey {
        n: to_fixed::<256>(&k.n, "modulus")?,
        e: k.e,
    })
}

/// A PKCS#1 `RSAPublicKey` file (`openssl rsa -RSAPublicKey_out -outform DER`).
/// PEM is accepted too, because that is what the same command writes by default
/// and the difference has already cost one debugging round.
fn parse_public_key_der(bytes: &[u8]) -> Result<PublicKey, String> {
    let owned;
    let bytes = if bytes.starts_with(b"-----BEGIN") {
        let text = std::str::from_utf8(bytes).map_err(|_| "public key is not UTF-8")?;
        owned = pem_to_der(text)?;
        owned.as_slice()
    } else {
        bytes
    };
    let mut pos = 0;
    let (tag, seq) = der_read(bytes, &mut pos)?;
    if tag != 0x30 {
        return Err("public key is not a DER SEQUENCE".into());
    }
    let mut p = 0;
    let (tn, n) = der_read(&seq, &mut p)?;
    let (te, e) = der_read(&seq, &mut p)?;
    if tn != 0x02 || te != 0x02 {
        return Err("public key SEQUENCE must hold two INTEGERs".into());
    }
    let strip = |mut v: Vec<u8>| {
        while v.len() > 1 && v[0] == 0 {
            v.remove(0);
        }
        v
    };
    let n = strip(n);
    let e = strip(e);
    let mut ev = 0u32;
    for b in &e {
        ev = (ev << 8) | *b as u32;
    }
    Ok(PublicKey {
        n: to_fixed::<256>(&n, "modulus")?,
        e: ev,
    })
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn read(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {}", path, e))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn parse_slot(s: &str) -> Result<u8, String> {
    match s.to_ascii_uppercase().as_str() {
        "A" | "0" => Ok(SLOT_A),
        "B" | "1" => Ok(SLOT_B),
        other => Err(format!("slot must be A or B, got {:?}", other)),
    }
}

struct Args {
    command: String,
    flags: Vec<(String, String)>,
}

impl Args {
    fn parse() -> Result<Args, String> {
        let mut it = std::env::args().skip(1).peekable();
        let command = it.next().unwrap_or_else(|| "sign".to_string());
        let mut flags = Vec::new();
        while let Some(arg) = it.next() {
            if let Some(name) = arg.strip_prefix("--") {
                let (name, value) = match name.split_once('=') {
                    Some((n, v)) => (n.to_string(), v.to_string()),
                    None => {
                        // A bare `--flag` (no value) is a boolean switch: the
                        // next argument either starts another flag or is
                        // missing.
                        match it.peek() {
                            Some(next) if !next.starts_with("--") => {
                                let v = it.next().unwrap();
                                (name.to_string(), v)
                            }
                            _ => (name.to_string(), "true".to_string()),
                        }
                    }
                };
                flags.push((name, value));
            } else {
                return Err(format!("unexpected argument {:?}", arg));
            }
        }
        Ok(Args { command, flags })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("--{} is required", name))
    }

    fn u64_of(&self, name: &str, default: u64) -> Result<u64, String> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v
                .parse::<u64>()
                .map_err(|_| format!("--{} expects a number, got {:?}", name, v)),
        }
    }

    fn u32_of(&self, name: &str, default: u32) -> Result<u32, String> {
        Ok(self.u64_of(name, default as u64)? as u32)
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

const USAGE: &str = "\
sign-manifest — ARGUS.MF producer and checker

  sign    --kernel K --config C [--initrd I] --slot A|B
          --security-version N [--slot-generation N] [--manifest-version N]
          [--min-loader-version N] [--issued-counter N]
          --key KEY.pem [--key-id-from OTHER.pem] --out ARGUS.MF

  verify  --manifest M --kernel K --config C [--initrd I] --slot A|B
          [--floor N] [--loader-version N] --key KEY.pem|--pubkey PUB.der

  print   --manifest M
  pubkey  --key KEY.pem [--rust] [--key-id]
  floor   --floor N [--seq N] --out FLOOR
  floor-read --file FLOOR
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sign-manifest: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse()?;
    match args.command.as_str() {
        "sign" => cmd_sign(&args),
        "verify" => cmd_verify(&args),
        "print" => cmd_print(&args),
        "pubkey" => cmd_pubkey(&args),
        "floor" => cmd_floor(&args),
        "floor-read" => cmd_floor_read(&args),
        "help" | "--help" | "-h" => {
            print!("{}", USAGE);
            Ok(())
        }
        other => Err(format!("unknown command {:?}\n{}", other, USAGE)),
    }
}

fn load_private_key(args: &Args, flag: &str) -> Result<PrivateKey, String> {
    let path = args.need(flag)?;
    let text = String::from_utf8(read(path)?).map_err(|_| format!("{} is not UTF-8", path))?;
    parse_private_key(&text).map_err(|e| format!("{}: {}", path, e))
}

fn cmd_sign(args: &Args) -> Result<(), String> {
    let key = load_private_key(args, "key")?;
    let pubkey = public_key_from_private(&key)?;

    // `--key-id-from` lets the tests produce a manifest that claims the real
    // key's id but is signed by another key. That is exactly the "signature by
    // a different key" attack: if the id did not match, the loader would refuse
    // earlier and the signature check itself would never be exercised.
    let key_id = match args.get("key-id-from") {
        Some(path) => {
            let text = String::from_utf8(read(path)?).map_err(|_| format!("{} is not UTF-8", path))?;
            public_key_from_private(&parse_private_key(&text)?)?.key_id()
        }
        None => pubkey.key_id(),
    };

    let kernel = read(args.need("kernel")?)?;
    let config = read(args.need("config")?)?;
    let initrd = match args.get("initrd") {
        Some(p) => read(p)?,
        None => Vec::new(),
    };

    let mut m = Manifest::blank();
    m.fmt_version = argus_security::manifest::FORMAT_VERSION;
    m.flags = 0;
    m.manifest_version = args.u64_of("manifest-version", 1)?;
    m.slot = parse_slot(args.need("slot")?)?;
    m.slot_generation = args.u64_of("slot-generation", 1)?;
    m.security_version = args.u64_of("security-version", 1)?;
    m.kernel_digest = sha256::digest(&kernel);
    m.kernel_len = kernel.len() as u64;
    m.initrd_digest = if initrd.is_empty() {
        [0u8; 32]
    } else {
        sha256::digest(&initrd)
    };
    m.config_digest = sha256::digest(&config);
    m.config_len = config.len() as u64;
    m.min_loader_version = args.u32_of("min-loader-version", 1)?;
    m.key_id = key_id;
    m.issued_counter = args.u64_of("issued-counter", 1)?;
    m.sig_alg = SIG_ALG_RSA2048_PKCS1_SHA256;
    m.sig_len = SIG_LEN;

    // Two levels, per the spec: payload_digest over the fields, signature over
    // payload_digest only.
    m.payload_digest = m.compute_payload_digest();
    let d: [u8; 256] = to_fixed::<256>(&key.d, "private exponent")?;
    m.signature = sign_pkcs1v15_sha256(&d, &pubkey.n, &m.signing_digest());

    let encoded = m.encode();
    debug_assert_eq!(encoded.len(), SIZE);
    let out = args.need("out")?;
    std::fs::write(out, encoded).map_err(|e| format!("{}: {}", out, e))?;

    println!(
        "signed: {} slot={} security_version={} manifest_version={} slot_generation={} key_id={}",
        out,
        if m.slot == SLOT_A { "A" } else { "B" },
        m.security_version,
        m.manifest_version,
        m.slot_generation,
        hex(&m.key_id)
    );
    println!(
        "  kernel_digest={} kernel_len={}",
        hex(&m.kernel_digest),
        m.kernel_len
    );
    println!(
        "  config_digest={} config_len={}",
        hex(&m.config_digest),
        m.config_len
    );
    println!(
        "  initrd_digest={} min_loader_version={}",
        hex(&m.initrd_digest),
        m.min_loader_version
    );
    println!("  payload_digest={}", hex(&m.payload_digest));
    println!("  signature={}", hex(&m.signature[..16]));
    Ok(())
}

fn cmd_verify(args: &Args) -> Result<(), String> {
    let manifest_bytes = read(args.need("manifest")?)?;

    let pubkey = if let Some(pk) = args.get("pubkey") {
        parse_public_key_der(&read(pk)?).map_err(|e| format!("{}: {}", pk, e))?
    } else {
        public_key_from_private(&load_private_key(args, "key")?)?
    };

    let kernel = read(args.need("kernel")?)?;
    let config = read(args.need("config")?)?;
    let initrd = match args.get("initrd") {
        Some(p) => read(p)?,
        None => Vec::new(),
    };
    let slot = parse_slot(args.need("slot")?)?;
    let floor = args.u64_of("floor", 0)?;
    let loader_version = args.u32_of("loader-version", 1)?;

    let fail = |code: &str, detail: String| -> Result<(), String> {
        Err(format!("REFUSED code={} detail=\"{}\"", code, detail))
    };

    let m = match Manifest::parse(&manifest_bytes) {
        Ok(m) => m,
        Err(e) => return fail(e.code(), format!("{}", e.detail())),
    };
    if m.key_id != pubkey.key_id() {
        return fail("E_KEY_UNKNOWN", "key_id does not match the verifying key".into());
    }
    if let Err(e) = verify_pkcs1v15_sha256(&pubkey, &m.signing_digest(), &m.signature) {
        return fail("E_SIG_INVALID", e.reason().into());
    }
    if m.min_loader_version > loader_version {
        return fail(
            "E_LOADER_TOO_OLD",
            format!("needs loader {} > {}", m.min_loader_version, loader_version),
        );
    }
    if m.slot != slot {
        return fail(
            "E_SLOT_MISMATCH",
            format!("manifest slot {} but booting slot {}", m.slot, slot),
        );
    }
    if m.security_version < floor {
        return fail(
            "E_MANIFEST_ROLLBACK",
            format!("security_version {} < floor {}", m.security_version, floor),
        );
    }
    if m.kernel_len != kernel.len() as u64 {
        return fail(
            "E_KERNEL_LEN",
            format!("manifest {} bytes, file {} bytes", m.kernel_len, kernel.len()),
        );
    }
    if sha256::digest(&kernel) != m.kernel_digest {
        return fail("E_KERNEL_DIGEST", "kernel bytes do not match the signed digest".into());
    }
    if m.config_len != config.len() as u64 {
        return fail(
            "E_CONFIG_LEN",
            format!("manifest {} bytes, file {} bytes", m.config_len, config.len()),
        );
    }
    if sha256::digest(&config) != m.config_digest {
        return fail("E_CONFIG_DIGEST", "config bytes do not match the signed digest".into());
    }
    if m.initrd_digest != [0u8; 32] {
        if sha256::digest(&initrd) != m.initrd_digest {
            return fail("E_INITRD_DIGEST", "initrd bytes do not match the signed digest".into());
        }
    }

    println!(
        "VERIFIED slot={} security_version={} manifest_version={} kernel_len={} config_len={}",
        if m.slot == SLOT_A { "A" } else { "B" },
        m.security_version,
        m.manifest_version,
        m.kernel_len,
        m.config_len
    );
    Ok(())
}

fn cmd_print(args: &Args) -> Result<(), String> {
    let bytes = read(args.need("manifest")?)?;
    let m = Manifest::parse(&bytes).map_err(|e| format!("{}: {}", e.code(), e.detail()))?;
    println!("magic            ARGUSMF\\0");
    println!("fmt_version      {}", m.fmt_version);
    println!("flags            0x{:08x}", m.flags);
    println!("manifest_version {}", m.manifest_version);
    println!("slot             {}", if m.slot == SLOT_A { "A" } else { "B" });
    println!("slot_generation  {}", m.slot_generation);
    println!("security_version {}", m.security_version);
    println!("kernel_digest    {}", hex(&m.kernel_digest));
    println!("kernel_len       {}", m.kernel_len);
    println!("initrd_digest    {}", hex(&m.initrd_digest));
    println!("config_digest    {}", hex(&m.config_digest));
    println!("config_len       {}", m.config_len);
    println!("min_loader_ver   {}", m.min_loader_version);
    println!("key_id           {}", hex(&m.key_id));
    println!("issued_counter   {}", m.issued_counter);
    println!("payload_digest   {}", hex(&m.payload_digest));
    println!("sig_alg          {}", m.sig_alg);
    println!("sig_len          {}", m.sig_len);
    println!("signature        {}", hex(&m.signature));
    Ok(())
}

fn cmd_pubkey(args: &Args) -> Result<(), String> {
    let key = load_private_key(args, "key")?;
    let pk = public_key_from_private(&key)?;
    if args.has("rust") {
        // Emitted once into bootloader/src/test_key.rs and committed; the
        // loader embeds a literal so the binary does not depend on files at
        // build time. `tests/crypto-kat.sh` checks the two still agree.
        println!("//! TEST KEY — NOT A TRUST ROOT. See keys/test/README.md.");
        println!("pub const TEST_MANIFEST_PUBKEY_N: [u8; 256] = [");
        for chunk in pk.n.chunks(16) {
            let line: Vec<String> = chunk.iter().map(|b| format!("0x{:02x}", b)).collect();
            println!("    {},", line.join(", "));
        }
        println!("];");
        println!("pub const TEST_MANIFEST_PUBKEY_E: u32 = {};", pk.e);
        println!("pub const TEST_MANIFEST_KEY_ID: [u8; 16] = [");
        let id = pk.key_id();
        for chunk in id.chunks(16) {
            let line: Vec<String> = chunk.iter().map(|b| format!("0x{:02x}", b)).collect();
            println!("    {},", line.join(", "));
        }
        println!("];");
    } else if args.has("key-id") {
        println!("{}", hex(&pk.key_id()));
    } else {
        println!("modulus={}", hex(&pk.n));
        println!("exponent={}", pk.e);
        println!("key_id={}", hex(&pk.key_id()));
    }
    Ok(())
}

/// Read a `\ARGUS\FLOOR` file and print its value. Used by `build.sh --check`
/// so the check compares the manifest against the floor that is actually on the
/// image, not against a number it assumed.
fn cmd_floor_read(args: &Args) -> Result<(), String> {
    use argus_security::floor::{parse_floor_file, CopyState};
    let path = args.need("file")?;
    let bytes = read(path)?;
    match parse_floor_file(&bytes) {
        CopyState::Valid(r) => {
            println!("seq={}", r.seq);
            println!("floor={}", r.floor);
            Ok(())
        }
        CopyState::Corrupt => Err(format!("{}: neither copy passes its CRC", path)),
        CopyState::Absent => Err(format!("{}: no floor record", path)),
    }
}

fn cmd_floor(args: &Args) -> Result<(), String> {
    let rec = FloorRecord {
        seq: args.u32_of("seq", 1)?,
        floor: args.u64_of("floor", 0)?,
    };
    let out = args.need("out")?;
    std::fs::write(out, encode_floor_file(&rec)).map_err(|e| format!("{}: {}", out, e))?;
    println!("floor file: {} seq={} floor={}", out, rec.seq, rec.floor);
    Ok(())
}
