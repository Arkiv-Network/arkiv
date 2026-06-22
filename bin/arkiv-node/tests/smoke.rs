//! Placeholder test target. Real black-box driving of the node lives in the
//! `arkiv-test-harness` binary; this just keeps the `tests/` dir wired up.

#[test]
fn package_name_is_stable() {
    assert_eq!(env!("CARGO_PKG_NAME"), "arkiv-node");
}
