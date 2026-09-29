//! Which files an `apply_patch` touches, and what it does to each, read off its patch envelope.
//!
//! Copilot hands `apply_patch` the patch as text in the envelope Codex's apply-patch grammar
//! defines:
//!
//! ```text
//! *** Begin Patch
//! *** Add File: <path>
//! +<line>
//! *** Update File: <path>
//! *** Move to: <path>
//! @@ <context>
//! -<line>
//! +<line>
//! *** Delete File: <path>
//! *** End Patch
//! ```
//!
//! Only the headers are read: a File Change records which files changed and how, never what changed
//! in them, so the hunks are passed over rather than checked. A header stands at the start of its
//! line, where no hunk line can — every hunk line opens with `+`, `-`, a space, or `@@` — so a file
//! the patch merely mentions in its content is never mistaken for one it touches.
//!
//! Text Suru cannot be sure names every file it touches reads as nothing at all, which leaves the
//! execution to be recorded some other way: text in no envelope, an envelope cut off before its
//! end, one naming no file, and one holding a line in the shape of a marker the grammar does not
//! place there — a header naming no path, a move following anything but an update's header, or
//! content before the first header.

use std::path::PathBuf;

use crate::protocol::FileChange;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
/// The one marker a hunk may hold: an update's last lines are the end of the file.
const END_OF_FILE: &str = "*** End of File";
/// What every marker opens with.
const MARKER: &str = "***";

/// The changes the patch `text` makes, one per file header in the order the patch names them, or
/// nothing when `text` is no complete patch envelope naming at least one file.
pub(super) fn patch_changes(text: &str) -> Option<Vec<FileChange>> {
    let lines = text.trim().lines().map(str::trim_end).collect::<Vec<_>>();
    let [begin, body @ .., end] = lines.as_slice() else {
        return None;
    };
    if begin.trim_start() != BEGIN_PATCH || end.trim_start() != END_PATCH {
        return None;
    }
    let mut changes = Vec::new();
    // Whether the latest line is an update's header, which is the one place a move may follow.
    let mut after_update = false;
    for line in body {
        let at_update = line.starts_with(UPDATE_FILE);
        if let Some(path) = line.strip_prefix(ADD_FILE) {
            changes.push(FileChange::Add { path: named(path)? });
        } else if let Some(path) = line.strip_prefix(DELETE_FILE) {
            changes.push(FileChange::Delete { path: named(path)? });
        } else if let Some(path) = line.strip_prefix(UPDATE_FILE) {
            changes.push(FileChange::Update {
                path: named(path)?,
                moved_to: None,
            });
        } else if let Some(path) = line.strip_prefix(MOVE_TO) {
            let (true, Some(FileChange::Update { moved_to, .. })) =
                (after_update, changes.last_mut())
            else {
                return None;
            };
            *moved_to = Some(named(path)?);
        } else if (line.starts_with(MARKER) && *line != END_OF_FILE)
            || (changes.is_empty() && !line.is_empty())
        {
            // A marker the grammar places nowhere here, or content before the first header.
            return None;
        }
        after_update = at_update;
    }
    (!changes.is_empty()).then_some(changes)
}

/// The path a header names, or nothing when it names none.
fn named(path: &str) -> Option<PathBuf> {
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(path: &str) -> FileChange {
        FileChange::Add { path: path.into() }
    }

    fn delete(path: &str) -> FileChange {
        FileChange::Delete { path: path.into() }
    }

    fn update(path: &str) -> FileChange {
        FileChange::Update {
            path: path.into(),
            moved_to: None,
        }
    }

    fn moved(path: &str, to: &str) -> FileChange {
        FileChange::Update {
            path: path.into(),
            moved_to: Some(to.into()),
        }
    }

    #[test]
    fn a_patch_touching_several_files_names_each_in_the_order_it_touches_them() {
        let patch = "*** Begin Patch\n\
                     *** Update File: src/lib.rs\n\
                     @@ fn main() {\n\
                     -    old();\n\
                     +    new();\n\
                     *** Add File: docs/notes.md\n\
                     +# Notes\n\
                     +\n\
                     *** Delete File: src/obsolete.rs\n\
                     *** Update File: src/main.rs\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** End Patch\n";

        assert_eq!(
            patch_changes(patch),
            Some(vec![
                update("src/lib.rs"),
                add("docs/notes.md"),
                delete("src/obsolete.rs"),
                update("src/main.rs"),
            ])
        );
    }

    #[test]
    fn an_update_moved_elsewhere_carries_where_it_moved_to() {
        let patch = "*** Begin Patch\n\
                     *** Update File: src/old.rs\n\
                     *** Move to: src/new.rs\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** Update File: src/kept.rs\n\
                     @@\n\
                     -c\n\
                     +d\n\
                     *** End Patch";

        assert_eq!(
            patch_changes(patch),
            Some(vec![
                moved("src/old.rs", "src/new.rs"),
                update("src/kept.rs")
            ])
        );
    }

    #[test]
    fn a_pure_rename_and_a_lone_delete_are_whole_patches() {
        assert_eq!(
            patch_changes(
                "*** Begin Patch\n*** Update File: a.rs\n*** Move to: b.rs\n*** End Patch"
            ),
            Some(vec![moved("a.rs", "b.rs")])
        );
        assert_eq!(
            patch_changes("*** Begin Patch\n*** Delete File: gone.rs\n*** End Patch"),
            Some(vec![delete("gone.rs")])
        );
    }

    #[test]
    fn absolute_paths_are_kept_as_the_patch_names_them() {
        let root = if cfg!(windows) { r"C:\work" } else { "/work" };
        let patch = format!(
            "*** Begin Patch\n*** Add File: {root}/new.rs\n+fn new() {{}}\n*** End Patch\n"
        );

        assert_eq!(
            patch_changes(&patch),
            Some(vec![FileChange::Add {
                path: PathBuf::from(format!("{root}/new.rs")),
            }])
        );
    }

    #[test]
    fn crlf_line_endings_and_space_around_the_envelope_read_alike() {
        let patch = "\r\n  *** Begin Patch\r\n\
                     *** Update File: src/lib.rs  \r\n\
                     *** Move to: src/core.rs\r\n\
                     @@\r\n\
                     -a\r\n\
                     +b\r\n\
                     *** Delete File: src/old.rs\r\n\
                     *** End Patch  \r\n\r\n";

        assert_eq!(
            patch_changes(patch),
            Some(vec![
                moved("src/lib.rs", "src/core.rs"),
                delete("src/old.rs")
            ])
        );
    }

    #[test]
    fn a_header_inside_a_hunk_is_content_not_a_file_the_patch_touches() {
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Update File: tests/patches.rs\n",
            "@@\n",
            "-*** Add File: a.rs\n",
            "+*** Add File: b.rs\n",
            " *** Update File: c.rs\n",
            " *** Move to: d.rs\n",
            "*** End of File\n",
            "*** End Patch",
        );

        assert_eq!(patch_changes(patch), Some(vec![update("tests/patches.rs")]));
    }

    #[test]
    fn a_patch_cut_off_before_its_end_reads_as_nothing() {
        assert_eq!(
            patch_changes("*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n"),
            None
        );
        assert_eq!(
            patch_changes("*** Begin Patch\n*** Add File: src/lib.rs"),
            None
        );
        assert_eq!(patch_changes("*** Begin Patch"), None);
    }

    #[test]
    fn a_patch_with_no_beginning_reads_as_nothing() {
        assert_eq!(
            patch_changes("*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch"),
            None
        );
    }

    #[test]
    fn an_envelope_naming_no_file_reads_as_nothing() {
        assert_eq!(patch_changes("*** Begin Patch\n*** End Patch"), None);
        assert_eq!(patch_changes("*** Begin Patch\n\n*** End Patch\n"), None);
    }

    #[test]
    fn a_header_naming_no_path_reads_as_nothing() {
        for header in [
            "*** Add File: ",
            "*** Add File:",
            "*** Delete File:   ",
            "*** Update File:",
        ] {
            assert_eq!(
                patch_changes(&format!(
                    "*** Begin Patch\n*** Update File: kept.rs\n{header}\n*** End Patch"
                )),
                None,
                "{header:?}"
            );
        }
        assert_eq!(
            patch_changes("*** Begin Patch\n*** Update File: a.rs\n*** Move to: \n*** End Patch"),
            None
        );
    }

    #[test]
    fn a_move_anywhere_but_right_after_an_updates_header_reads_as_nothing() {
        for patch in [
            "*** Begin Patch\n*** Move to: b.rs\n*** End Patch",
            "*** Begin Patch\n*** Add File: a.rs\n*** Move to: b.rs\n+x\n*** End Patch",
            "*** Begin Patch\n*** Delete File: a.rs\n*** Move to: b.rs\n*** End Patch",
            "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Move to: b.rs\n*** End Patch",
            "*** Begin Patch\n*** Update File: a.rs\n*** Move to: b.rs\n*** Move to: c.rs\n\
             *** End Patch",
        ] {
            assert_eq!(patch_changes(patch), None, "{patch:?}");
        }
    }

    #[test]
    fn content_before_the_first_header_reads_as_nothing() {
        assert_eq!(
            patch_changes("*** Begin Patch\n+orphan\n*** Add File: a.rs\n+x\n*** End Patch"),
            None
        );
    }

    #[test]
    fn a_marker_the_grammar_does_not_place_there_reads_as_nothing() {
        for patch in [
            "*** Begin Patch\n*** Update File: a.rs\n*** End Patch\n*** Add File: b.rs\n\
             *** End Patch",
            "*** Begin Patch\n*** Begin Patch\n*** Add File: a.rs\n*** End Patch",
            "*** Begin Patch\n*** Rename File: a.rs\n*** End Patch",
            "*** Begin Patch\n*** Environment ID: remote\n*** Add File: a.rs\n*** End Patch",
        ] {
            assert_eq!(patch_changes(patch), None, "{patch:?}");
        }
    }

    #[test]
    fn text_that_is_no_patch_reads_as_nothing() {
        for text in [
            "",
            "   \n",
            "Update src/lib.rs so it compiles",
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n",
        ] {
            assert_eq!(patch_changes(text), None, "{text:?}");
        }
    }
}
