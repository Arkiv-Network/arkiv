Vendored from crates.io proc-macro-error2 2.0.1, retaining its MIT/Apache-2.0 licenses.

Local change: make `extern crate proc_macro` public in src/lib.rs so the existing
public re-export is valid. This fixes Rust future-incompatibility E0365:
https://github.com/rust-lang/rust/issues/127909

Both the root workspace and devnet/malachite patch crates.io to this copy.
Remove the patches when upstream dependencies no longer need this workaround.
