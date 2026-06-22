//! Placeholder test target; real black-box scenarios land in a later leg.

#[test]
fn package_name_is_stable() {
    assert_eq!(env!("CARGO_PKG_NAME"), "arkiv-test-harness");
}
