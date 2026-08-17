use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    for variable in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    if let Ok(target) = std::env::var("TARGET") {
        let target_rustflags = format!(
            "CARGO_TARGET_{}_RUSTFLAGS",
            target.replace('-', "_").to_ascii_uppercase()
        );
        println!("cargo:rerun-if-env-changed={target_rustflags}");
    }

    let compiled_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_nanos();
    println!("cargo:rustc-env=CHIDORI_COMPILE_ID={compiled_at:x}");
}
