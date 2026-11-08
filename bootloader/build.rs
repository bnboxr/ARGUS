//! Tells rustc that `argus_test_corrupt_bootinfo` is a real cfg, not a typo.
//!
//! The cfg is set by `build.sh` only when `ARGUS_TEST_CORRUPT_BOOTINFO=1`, and
//! only `tests/bootinfo-checksum.sh` sets that. Declaring it here keeps
//! `unexpected_cfgs` quiet without weakening the lint for anything else.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(argus_test_corrupt_bootinfo)");
}
