use std::{
    fs::File,
    io::{Read, Seek},
    path::Path,
};

use anyhow::{Context, Result};
use blake3::Hasher;

const HASH_BUFFER_SIZE: usize = 64 * 1024;

/// Read the compilation marker from the configured executable's small Suru section.
///
/// Every compilation of the binary mints a new marker; copying, signing or
/// stripping debug information preserves it. This is build compatibility metadata,
/// not an integrity check for post-link patches. Missing or malformed markers
/// fall back to a full BLAKE3 hash, without trusting timestamps or file lengths.
/// Each call opens the file afresh so atomic same-path replacements are observed.
pub fn for_executable(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let mut executable = File::open(path)
        .with_context(|| format!("open Suru executable for build identity: {path:?}"))?;
    if let Some(identity) = embedded_identity(&mut executable) {
        return Ok(identity);
    }
    executable
        .rewind()
        .context("rewind executable for build identity fallback")?;
    let mut hasher = Hasher::new();
    let mut buffer = [0; HASH_BUFFER_SIZE];
    loop {
        let bytes_read = executable
            .read(&mut buffer)
            .with_context(|| format!("read Suru executable for build identity: {path:?}"))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

pub fn for_current_executable() -> Result<String> {
    let executable = std::env::current_exe().context("find current Suru executable")?;
    for_executable(executable)
}

// Use the format-specific header APIs: object::File::parse also loads symbol
// tables, which can themselves be large in a debug executable.
fn embedded_identity(file: &mut File) -> Option<String> {
    use object::{FileKind, macho};
    let data = object::read::ReadCache::new(file);
    match FileKind::parse(&data).ok()? {
        FileKind::MachOFat32 => universal_identity::<macho::FatArch32>(&data),
        FileKind::MachOFat64 => universal_identity::<macho::FatArch64>(&data),
        _ => thin_identity(&data),
    }
}

fn thin_identity<'a>(data: impl object::read::ReadRef<'a>) -> Option<String> {
    use object::{Endianness, FileKind, elf, macho, pe};
    match FileKind::parse(data).ok()? {
        FileKind::Elf32 => elf_identity::<elf::FileHeader32<Endianness>>(data),
        FileKind::Elf64 => elf_identity::<elf::FileHeader64<Endianness>>(data),
        FileKind::MachO32 => macho_identity::<macho::MachHeader32<Endianness>>(data),
        FileKind::MachO64 => macho_identity::<macho::MachHeader64<Endianness>>(data),
        FileKind::Pe32 => pe_identity::<pe::ImageNtHeaders32>(data),
        FileKind::Pe64 => pe_identity::<pe::ImageNtHeaders64>(data),
        _ => None,
    }
}

fn section_identity<'a>(
    data: impl object::read::ReadRef<'a>,
    (offset, size): (u64, u64),
) -> Option<String> {
    if size < 32 {
        return None;
    }
    let record = data.read_bytes_at(offset, 32).ok()?;
    if &record[..16] != b"SURU_BUILD_ID_V1" || record[16..].iter().all(|byte| *byte == 0) {
        return None;
    }
    let id = uuid::Uuid::from_slice(&record[16..]).ok()?;
    Some(format!("suru-build-v1:{}", id.simple()))
}

fn elf_identity<'a, Elf: object::read::elf::FileHeader>(
    data: impl object::read::ReadRef<'a>,
) -> Option<String> {
    use object::read::elf::SectionHeader;
    let header = Elf::parse(data).ok()?;
    let endian = header.endian().ok()?;
    let sections = header.sections(endian, data).ok()?;
    let (_, section) = sections.section_by_name(endian, b".suru")?;
    section_identity(data, section.file_range(endian)?)
}

fn macho_identity<'a, Mach: object::read::macho::MachHeader>(
    data: impl object::read::ReadRef<'a>,
) -> Option<String> {
    use object::read::macho::{Section, Segment};
    let header = Mach::parse(data, 0).ok()?;
    let endian = header.endian().ok()?;
    let mut commands = header.load_commands(endian, data, 0).ok()?;
    while let Some(command) = commands.next().ok()? {
        if let Some((segment, bytes)) = Mach::Segment::from_command(command).ok()? {
            for section in segment.sections(endian, bytes).ok()? {
                if section.name() == b"__suru" {
                    return section_identity(data, section.file_range(endian)?);
                }
            }
        }
    }
    None
}

fn pe_identity<'a, Pe: object::read::pe::ImageNtHeaders>(
    data: impl object::read::ReadRef<'a>,
) -> Option<String> {
    use object::{LittleEndian as LE, pe};
    let dos = pe::ImageDosHeader::parse(data).ok()?;
    let mut offset = dos.nt_headers_offset().into();
    let (header, _) = Pe::parse(data, &mut offset).ok()?;
    for section in header.sections(data, offset).ok()?.iter() {
        if &section.name == b".suru\0\0\0" {
            return section_identity(
                data,
                (
                    section.pointer_to_raw_data.get(LE).into(),
                    section.size_of_raw_data.get(LE).into(),
                ),
            );
        }
    }
    None
}

fn universal_identity<Fat: object::read::macho::FatArch>(
    data: &object::read::ReadCache<&mut File>,
) -> Option<String> {
    let fat = object::read::macho::MachOFatFile::<Fat>::parse(data).ok()?;
    if fat.arches().is_empty() {
        return None;
    }
    let mut hasher = Hasher::new();
    for arch in fat.arches() {
        let (offset, size) = arch.file_range();
        // Every slice must have an identity; otherwise hash the whole container.
        let identity = thin_identity(data.range(offset, size))?;
        hasher.update(&arch.cputype().to_le_bytes());
        hasher.update(&arch.cpusubtype().to_le_bytes());
        hasher.update(identity.as_bytes());
    }
    Some(format!("universal:{}", hasher.finalize().to_hex()))
}
