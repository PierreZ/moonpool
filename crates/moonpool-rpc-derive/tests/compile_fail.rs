//! Compile-fail and compile-pass cases for `#[moonpool_rpc::service]` and
//! the type-level contracts of the interface API, run by nextest.
//!
//! The expected `.stderr` files are pinned against the repository's
//! toolchain (`rust-toolchain.toml`); after a toolchain bump regenerate
//! them with `TRYBUILD=overwrite` and review the diff.

#[test]
fn compile_fail_cases() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
    cases.pass("tests/pass/*.rs");
}
