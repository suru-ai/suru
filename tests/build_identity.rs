use suru::build_identity::for_executable;

fn record(id: u8) -> Vec<u8> {
    let mut bytes = b"SURU_BUILD_ID_V1".to_vec();
    bytes.extend_from_slice(&[id; 16]);
    bytes
}

// Minimal ELF64 section table with .shstrtab and the allocated .suru section.
fn elf(id: u8) -> Vec<u8> {
    let mut bytes = vec![0; 280];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[40..48].copy_from_slice(&64u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
    bytes[58..60].copy_from_slice(&64u16.to_le_bytes());
    bytes[60..62].copy_from_slice(&3u16.to_le_bytes());
    bytes[62..64].copy_from_slice(&1u16.to_le_bytes());
    bytes[128..132].copy_from_slice(&1u32.to_le_bytes());
    bytes[132..136].copy_from_slice(&3u32.to_le_bytes()); // SHT_STRTAB
    bytes[152..160].copy_from_slice(&256u64.to_le_bytes());
    bytes[160..168].copy_from_slice(&17u64.to_le_bytes());
    bytes[192..196].copy_from_slice(&11u32.to_le_bytes());
    bytes[196..200].copy_from_slice(&1u32.to_le_bytes()); // SHT_PROGBITS
    bytes[200..208].copy_from_slice(&2u64.to_le_bytes()); // SHF_ALLOC
    bytes[216..224].copy_from_slice(&280u64.to_le_bytes());
    bytes[224..232].copy_from_slice(&32u64.to_le_bytes());
    bytes[256..273].copy_from_slice(b"\0.shstrtab\0.suru\0");
    bytes.extend_from_slice(&record(id));
    bytes
}

// Mach-O 64 header, LC_SEGMENT_64 and its __suru section.
fn macho(id: u8) -> Vec<u8> {
    let mut bytes = vec![0; 184];
    bytes[..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
    bytes[4..8].copy_from_slice(&0x0100_0007u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&2u32.to_le_bytes());
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    bytes[20..24].copy_from_slice(&152u32.to_le_bytes());
    bytes[32..36].copy_from_slice(&0x19u32.to_le_bytes());
    bytes[36..40].copy_from_slice(&152u32.to_le_bytes());
    bytes[40..46].copy_from_slice(b"__DATA");
    bytes[96..100].copy_from_slice(&1u32.to_le_bytes());
    bytes[104..110].copy_from_slice(b"__suru");
    bytes[120..126].copy_from_slice(b"__DATA");
    bytes[144..152].copy_from_slice(&32u64.to_le_bytes());
    bytes[152..156].copy_from_slice(&184u32.to_le_bytes());
    bytes.extend_from_slice(&record(id));
    bytes
}

fn pe(id: u8, generation: u8) -> Vec<u8> {
    let mut bytes = vec![0; 512];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[60..64].copy_from_slice(&128u32.to_le_bytes());
    bytes[128..132].copy_from_slice(b"PE\0\0");
    bytes[132..134].copy_from_slice(&0x8664u16.to_le_bytes());
    bytes[134..136].copy_from_slice(&1u16.to_le_bytes());
    bytes[148..150].copy_from_slice(&240u16.to_le_bytes());
    bytes[152..154].copy_from_slice(&0x20bu16.to_le_bytes());
    bytes[260..264].copy_from_slice(&16u32.to_le_bytes());
    bytes[392..397].copy_from_slice(b".suru");
    bytes[400..404].copy_from_slice(&32u32.to_le_bytes());
    bytes[404..408].copy_from_slice(&0x1000u32.to_le_bytes());
    bytes[408..412].copy_from_slice(&32u32.to_le_bytes());
    bytes[412..416].copy_from_slice(&512u32.to_le_bytes());
    bytes.extend_from_slice(&record(id));
    if id != 0 {
        bytes[543] = generation;
    }
    bytes
}

fn universal(first: u8, second: u8) -> Vec<u8> {
    let mut bytes = vec![0; 48];
    bytes[..4].copy_from_slice(&0xcafe_babeu32.to_be_bytes());
    bytes[4..8].copy_from_slice(&2u32.to_be_bytes());
    for (index, cpu) in [0x0100_0007u32, 0x0100_000c].into_iter().enumerate() {
        let arch = 8 + index * 20;
        bytes[arch..arch + 4].copy_from_slice(&cpu.to_be_bytes());
        bytes[arch + 8..arch + 12].copy_from_slice(&(48 + index as u32 * 216).to_be_bytes());
        bytes[arch + 12..arch + 16].copy_from_slice(&216u32.to_be_bytes());
    }
    bytes.extend_from_slice(&macho(first));
    bytes.extend_from_slice(&macho(second));
    bytes
}
#[test]
fn embedded_identity_survives_changes_to_unmapped_packaging_data() {
    for contents in [elf(1), macho(1), pe(1, 1)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server");
        std::fs::write(&path, &contents).unwrap();
        let identity = for_executable(&path).unwrap();
        let mut packaged = contents;
        packaged.extend_from_slice(b"packaging data outside the linked image");
        std::fs::write(&path, packaged).unwrap();
        assert_eq!(for_executable(&path).unwrap(), identity);
    }
}
#[test]
fn universal_identity_covers_every_architecture_and_ignores_packaging() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server");
    std::fs::write(&path, universal(1, 2)).unwrap();
    let identity = for_executable(&path).unwrap();
    let mut packaged = universal(1, 2);
    packaged.extend_from_slice(b"packaging data outside the linked image");
    std::fs::write(&path, packaged).unwrap();
    assert_eq!(for_executable(&path).unwrap(), identity);
    std::fs::write(&path, universal(1, 3)).unwrap();
    assert_ne!(for_executable(&path).unwrap(), identity);
}

#[test]
fn same_size_same_timestamp_rebuilds_change_every_supported_identity() {
    for (first, rebuilt) in [
        (elf(1), elf(2)),
        (macho(1), macho(2)),
        (pe(1, 1), pe(2, 1)),
        (pe(1, 1), pe(1, 2)), // content changes with a newly compiled marker
        (universal(1, 2), universal(3, 2)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server");
        std::fs::write(&path, &first).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let identity = for_executable(&path).unwrap();
        assert_eq!(first.len(), rebuilt.len());
        std::fs::write(&path, rebuilt).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_ne!(for_executable(&path).unwrap(), identity);
    }
}

#[test]
fn missing_or_malformed_markers_fall_back_to_content_identity() {
    let mut truncated = elf(1);
    truncated.truncate(290);
    for mut contents in [
        elf(0),
        macho(0),
        pe(0, 1),
        universal(1, 0),
        truncated,
        b"not an executable".to_vec(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server");
        std::fs::write(&path, &contents).unwrap();
        let identity = for_executable(&path).unwrap();
        assert!(identity.starts_with("blake3:"));
        contents.push(1);
        std::fs::write(&path, contents).unwrap();
        assert_ne!(for_executable(&path).unwrap(), identity);
    }
}

#[test]
fn packaged_suru_has_an_embedded_identity_in_this_profile() {
    let identity = for_executable(env!("CARGO_BIN_EXE_suru")).unwrap();
    assert!(
        identity.starts_with("suru-build-v1:"),
        "Suru should not need a full-file hash: {identity}"
    );
}

#[test]
fn cargo_rebuilds_invalidate_the_marker_and_unchanged_artifacts_reuse_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("dependency/src")).unwrap();
    let macro_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/suru-build-id");
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"
[package]
name = "identity-fixture"
version = "0.0.0"
edition = "2024"
[features]
extra = []
[dependencies]
suru-build-id = {{ path = {macro_path:?} }}
fixture-dependency = {{ path = "dependency" }}
"#
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("dependency/Cargo.toml"),
        "[package]\nname = \"fixture-dependency\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("dependency/src/lib.rs"),
        "pub fn value() -> u8 { 1 }",
    )
    .unwrap();
    std::fs::write(root.join("src/other.rs"), "pub fn value() -> u8 { 1 }").unwrap();
    let main = r#"
mod other;
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__suru"))]
#[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".suru"))]
static BUILD_ID: [u8; 32] = suru_build_id::generate!();
fn foo() -> u8 { 1 }
fn bar() -> u8 { 2 }
static SELECTED: fn() -> u8 = foo;
fn main() {
    std::hint::black_box(&BUILD_ID);
    std::hint::black_box((foo as fn() -> u8, bar as fn() -> u8));
    println!("{}", SELECTED() + other::value() + fixture_dependency::value());
}
"#;
    std::fs::write(root.join("src/main.rs"), main).unwrap();
    let executable = root
        .join("target/debug")
        .join(format!("identity-fixture{}", std::env::consts::EXE_SUFFIX));
    let build = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO"))
            .current_dir(root)
            .args(["build", "--offline"])
            .args(args)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .output()
            .expect("compile the fixture through Cargo");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let identity = for_executable(&executable).unwrap();
        assert!(identity.starts_with("suru-build-v1:"));
        identity
    };
    let mut previous = build(&[]);
    assert_eq!(
        build(&[]),
        previous,
        "unchanged Cargo build must retain its marker"
    );
    for (path, contents) in [
        ("src/main.rs", main.replace("= foo;", "= bar;")), // relocation-only source change
        ("src/other.rs", "pub fn value() -> u8 { 2 }".to_owned()),
        (
            "dependency/src/lib.rs",
            "pub fn value() -> u8 { 2 }".to_owned(),
        ),
    ] {
        std::fs::write(root.join(path), contents).unwrap();
        let rebuilt = build(&[]);
        assert_ne!(
            rebuilt, previous,
            "{path} must invalidate the compilation marker"
        );
        previous = rebuilt;
    }
    let featured = build(&["--features", "extra"]);
    assert_ne!(featured, previous);
    assert_eq!(build(&["--features", "extra"]), featured);
    let optimized = build(&["--features", "extra", "--config", "profile.dev.opt-level=1"]);
    assert_ne!(optimized, featured);
    // The marker lives outside debug information and must survive stripping.
    let stripped = build(&["--config", "profile.dev.strip=\"symbols\""]);
    assert_ne!(stripped, optimized);
}
