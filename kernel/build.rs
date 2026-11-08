//! Hands the linker script to rustc.
//!
//! `build.rs` is used rather than `.cargo/config.toml` because cargo reads
//! config from the *invocation* directory, not the crate directory — so a
//! `kernel/.cargo/config.toml` would be silently ignored when building from
//! the workspace root. Emitting an absolute `-T` argument here works from
//! anywhere.

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    println!("cargo:rustc-link-arg=-T{manifest_dir}/linker.ld");
    println!("cargo:rustc-link-arg=-nostdlib");
    // The loader requires ET_EXEC: this kernel is linked at a fixed physical
    // address and relocated by nobody, so a position-independent executable
    // would be a lie about how it runs. `-no-pie` makes lld emit ET_EXEC and
    // drops the .dynamic/.dynsym/.rela.dyn machinery that goes with PIE.
    println!("cargo:rustc-link-arg=-no-pie");
    println!("cargo:rustc-link-arg=--no-dynamic-linker");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=build.rs");
}
