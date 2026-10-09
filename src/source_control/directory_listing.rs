//! One directory of the owning Server read for the Directory Browser.

use std::{
    cmp::Ordering,
    io,
    path::{Path, PathBuf},
};

use super::git::listed_source_control;
use crate::protocol::{ChildDirectory, DirectoryListing};
#[cfg(windows)]
use crate::protocol::{DRIVE_LIST, DirectorySourceControl};

/// The directory `path` names, listed: read from `base` when relative and
/// from `home` when it begins with `~`, resolved to its root, and answered
/// with that root's parent and the directories directly within it, the root
/// and each child with what it is to source control as Git's metadata on
/// disk says. A path
/// naming nothing, a file, a directory this Server cannot read, or one whose
/// path is not Unicode is refused with the reason a reader is told.
///
/// Every path crosses the wire as Unicode text, so a directory whose name
/// is not Unicode can be neither named to a Client nor chosen by one; like a
/// file, it is left out of the children. A hidden directory is not: it is
/// listed in its place, flagged hidden, for the Client to leave out.
///
/// On Windows a drive root's parent is the drive list, which goes by
/// [`DRIVE_LIST`](crate::protocol::DRIVE_LIST): that path is answered with
/// the drive list rather than read from `base`.
pub(crate) fn list_directory(
    path: &Path,
    base: &Path,
    home: Option<&Path>,
) -> Result<DirectoryListing, String> {
    #[cfg(windows)]
    if path.as_os_str() == DRIVE_LIST {
        return list_drives();
    }
    let named = match path.strip_prefix("~") {
        Ok(beneath_home) => home
            .ok_or("The Server's home is unknown")?
            .join(beneath_home),
        Err(_) => base.join(path),
    };
    let root = crate::paths::canonical(&named).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => "No directory there".to_owned(),
        _ => unreadable(&error),
    })?;
    if !root.is_dir() {
        return Err("Not a directory".to_owned());
    }
    if root.to_str().is_none() {
        return Err("This directory's path is not Unicode".to_owned());
    }
    let mut children = std::fs::read_dir(&root)
        .map_err(|error| unreadable(&error))?
        .flatten()
        .filter(is_directory)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let path = entry.path();
            Some(ChildDirectory {
                hidden: name.starts_with('.') || carries_hidden_attribute(&entry),
                source_control: listed_source_control(&path),
                path,
                name,
            })
        })
        .collect::<Vec<_>>();
    children.sort_by(|left, right| natural_order(&left.name, &right.name));
    Ok(DirectoryListing {
        parent: parent(&root),
        source_control: listed_source_control(&root),
        root,
        children,
    })
}

/// The directory above `root`, which above a drive on Windows is the drive
/// list.
#[cfg(windows)]
fn parent(root: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};

    let mut components = root.components();
    let is_drive_root = matches!(
        components.next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    ) && components.next() == Some(Component::RootDir)
        && components.next().is_none();
    root.parent()
        .map(Path::to_owned)
        .or_else(|| is_drive_root.then(|| PathBuf::from(DRIVE_LIST)))
}

/// The directory above `root`; the filesystem's own root has none.
#[cfg(not(windows))]
fn parent(root: &Path) -> Option<PathBuf> {
    root.parent().map(Path::to_owned)
}

/// The drive list: parentless, with a child for each drive this Server can
/// see, named and spelled as the drive's root, in the order of their
/// letters. A drive is listed whether or not it can be read now — a drive
/// with no disc in it, or a network drive out of reach — since reading each
/// could keep the reader waiting on the slowest, and opening one says why it
/// cannot be read as any directory does. Nor is a drive read for what it is
/// to source control, for the same reason.
#[cfg(windows)]
fn list_drives() -> Result<DirectoryListing, String> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

    // SAFETY: GetLogicalDrives takes nothing and answers a bitmask of the
    // drives present, bit 0 for A.
    let drives = unsafe { GetLogicalDrives() };
    if drives == 0 {
        return Err(format!(
            "Could not read the drives: {}",
            io::Error::last_os_error()
        ));
    }
    let children = (b'A'..=b'Z')
        .enumerate()
        .filter(|(bit, _)| drives & (1 << bit) != 0)
        .map(|(_, letter)| {
            let root = format!("{}:\\", char::from(letter));
            ChildDirectory {
                path: PathBuf::from(&root),
                name: root,
                source_control: DirectorySourceControl::Plain,
                hidden: false,
            }
        })
        .collect();
    Ok(DirectoryListing {
        root: PathBuf::from(DRIVE_LIST),
        parent: None,
        source_control: DirectorySourceControl::Plain,
        children,
    })
}

fn unreadable(error: &io::Error) -> String {
    format!("Could not read this directory: {error}")
}

/// Whether `entry` is a directory, a symlink counting as what it names.
fn is_directory(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|kind| {
        kind.is_dir()
            || (kind.is_symlink()
                && std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir()))
    })
}

/// Whether `entry` carries Windows' hidden attribute, which the directory's
/// own listing already read, so asking costs nothing more.
#[cfg(windows)]
fn carries_hidden_attribute(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;

    entry
        .metadata()
        .is_ok_and(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

/// Elsewhere a directory is hidden by its name alone.
#[cfg(not(windows))]
fn carries_hidden_attribute(_entry: &std::fs::DirEntry) -> bool {
    false
}

/// Names as a file manager orders them: case set aside, and a run of digits
/// read as the number it spells, so `a2` comes before `a10`. Names alike in
/// that reading fall back to their own order, so the listing is the same
/// however the directory hands its entries out.
fn natural_order(left: &str, right: &str) -> Ordering {
    let mut left_chars = left.chars().peekable();
    let mut right_chars = right.chars().peekable();
    loop {
        let order = match (left_chars.peek(), right_chars.peek()) {
            (None, None) => return left.cmp(right),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left_char), Some(right_char))
                if left_char.is_ascii_digit() && right_char.is_ascii_digit() =>
            {
                let left_number = digits(&mut left_chars);
                let right_number = digits(&mut right_chars);
                let left_number = left_number.trim_start_matches('0');
                let right_number = right_number.trim_start_matches('0');
                left_number
                    .len()
                    .cmp(&right_number.len())
                    .then_with(|| left_number.cmp(right_number))
            }
            (Some(_), Some(_)) => {
                let lowered = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    chars.next().into_iter().flat_map(char::to_lowercase)
                };
                lowered(&mut left_chars).cmp(lowered(&mut right_chars))
            }
        };
        if order.is_ne() {
            return order;
        }
    }
}

/// The run of digits `chars` stands at, taken from it.
fn digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut run = String::new();
    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
        run.push(digit);
    }
    run
}
