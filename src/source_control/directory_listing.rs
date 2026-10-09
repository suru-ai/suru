//! One directory of the owning Server read for the Directory Browser.

use std::{cmp::Ordering, io, path::Path};

use crate::protocol::{ChildDirectory, DirectoryListing};

/// The directory `path` names, listed: read from `base` when relative and
/// from `home` when it begins with `~`, resolved to its root, and answered
/// with that root's parent and the directories directly within it. A path
/// naming nothing, a file, or a directory this Server cannot read is refused
/// with the reason a reader is told.
pub(crate) fn list_directory(
    path: &Path,
    base: &Path,
    home: Option<&Path>,
) -> Result<DirectoryListing, String> {
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
    let mut children = std::fs::read_dir(&root)
        .map_err(|error| unreadable(&error))?
        .flatten()
        .filter(is_directory)
        // A name the wire cannot spell names nothing a Client could ask for.
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some(ChildDirectory {
                path: entry.path(),
                name,
            })
        })
        .collect::<Vec<_>>();
    children.sort_by(|left, right| natural_order(&left.name, &right.name));
    Ok(DirectoryListing {
        parent: root.parent().map(Path::to_owned),
        root,
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
