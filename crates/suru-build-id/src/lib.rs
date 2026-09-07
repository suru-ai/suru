use proc_macro::TokenStream;

/// Mint an identity when the executable is compiled, not when this macro crate
/// is built. Cargo tracks source, dependencies, features and compiler options;
/// every executable compilation gets a new ID without duplicating that tracking.
/// An unchanged artifact (including copies of it) retains its identity.
#[proc_macro]
pub fn generate(_input: TokenStream) -> TokenStream {
    let mut record = *b"SURU_BUILD_ID_V10000000000000000";
    record[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    format!("{record:?}")
        .parse()
        .expect("byte array is a Rust expression")
}
