//! The marker `src/main.rs` gives Suru's binary, given to a test binary by its
//! crate root including this file once.
//!
//! A Server identifies the executable it runs from, and a test binary is
//! hundreds of megabytes: without a marker, each Server a test starts hashes
//! all of it, which was most of what the integration tests spent. Nothing
//! refers to the static, but `#[used]` keeps it in the binary on every
//! platform; `the_test_binary_carries_a_build_marker` in
//! `tests/server_integration.rs` holds the linker to that.

#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__suru"))]
#[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".suru"))]
static BUILD_ID: [u8; 32] = suru_build_id::generate!();
