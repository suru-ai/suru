//! The directories Workspace Roots name, told apart as directories rather
//! than by how the Setting spells them (ADR 0051): `~/projects` spelled out
//! in full beneath the home is the same Root, and so is a link and where it
//! leads. Each Root is read as written — `~` beneath the home and its
//! separators made plain — and as the filesystem resolves it, read apart
//! (see [`super::reads_apart`]) and waited on no longer than a bound, so a
//! Root on a mount gone quiet is not resolved at all, and is told apart
//! only as far as its spelling and its writing tell.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use futures_util::{StreamExt, stream};

use super::{reads_apart::ReadsApart, root_discovery};

/// How many Roots are resolved at once.
const CONCURRENT_RESOLUTIONS: usize = 8;

/// The directory a Workspace Root names, as far as it could be told.
pub(super) struct NamedDirectory {
    /// The Root as the Setting spells it.
    spelled: String,
    /// The path it names, `~` read beneath the home and its separators made
    /// plain; `None` for one beneath a home this Server does not know.
    written: Option<PathBuf>,
    /// What the filesystem made of that path.
    resolved: Resolution,
}

/// What the filesystem made of the path a Root names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Resolution {
    /// The directory it leads to, links and all.
    Resolved(PathBuf),
    /// Nothing is there, nor at the end of any link along it.
    Missing,
    /// Nothing could be told: it was not read in time, could not be read,
    /// or names a home this Server does not know.
    Untold,
}

/// Whether two Roots name the same directory, as far as can be told.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Likeness {
    Same,
    Different,
    Untold,
}

impl NamedDirectory {
    /// Whether `other` names the same directory: surely so where they are
    /// spelled alike, written alike, or resolved alike; surely not where
    /// both resolve, elsewhere; and otherwise untold. Nothing being there
    /// for one tells nothing: a link on a drive not mounted may lead where
    /// the other names, so a Root whose drive is gone is told apart only
    /// once the drive is back.
    pub(super) fn likeness(&self, other: &Self) -> Likeness {
        if self.spelled == other.spelled || self.surely_written_as(other.written.as_deref()) {
            return Likeness::Same;
        }
        match (&self.resolved, &other.resolved) {
            (Resolution::Resolved(left), Resolution::Resolved(right)) => {
                if left == right {
                    Likeness::Same
                } else {
                    Likeness::Different
                }
            }
            _ => Likeness::Untold,
        }
    }

    /// Whether this Root surely names `directory`, one standing now and
    /// read canonically: written as it, or resolved to it. A Root not
    /// resolved names it only by its writing.
    pub(super) fn names(&self, directory: &Path) -> bool {
        self.surely_written_as(Some(directory))
            || self.resolved == Resolution::Resolved(directory.to_owned())
    }

    fn surely_written_as(&self, path: Option<&Path>) -> bool {
        self.written.is_some() && self.written.as_deref() == path
    }
}

/// The directory each Root among `roots` names, by how it is spelled: as
/// written, with `~` read beneath `home`, and as the filesystem resolves it
/// — read through `reads`, and waited on no longer than `wait`, so a Root on
/// a mount gone quiet is not resolved at all.
pub(super) async fn resolve<'a>(
    reads: &ReadsApart,
    home: Option<&Path>,
    wait: Duration,
    roots: impl Iterator<Item = &'a String>,
) -> HashMap<String, NamedDirectory> {
    let mut named = roots
        .map(|root| {
            let written = root_discovery::expand(root, home)
                .map(|directory| directory.components().collect::<PathBuf>());
            (
                root.clone(),
                NamedDirectory {
                    spelled: root.clone(),
                    written,
                    resolved: Resolution::Untold,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let written = named
        .iter()
        .filter_map(|(root, directory)| Some((root.clone(), directory.written.clone()?)))
        .collect::<Vec<_>>();
    let mut resolved = HashMap::new();
    let resolutions = stream::iter(written.into_iter().map(|(root, directory)| async move {
        let read = directory.clone();
        let resolution = reads
            .read(
                &directory,
                move || async move { crate::paths::canonical(&read) },
            )
            .await;
        (root, resolution)
    }))
    .buffer_unordered(CONCURRENT_RESOLUTIONS)
    .for_each(|(root, resolution)| {
        let resolution = match resolution {
            Ok(Ok(directory)) => Resolution::Resolved(directory),
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => Resolution::Missing,
            Ok(Err(_)) | Err(_) => Resolution::Untold,
        };
        resolved.insert(root, resolution);
        std::future::ready(())
    });
    if tokio::time::timeout(wait, resolutions).await.is_err() {
        tracing::debug!("told Workspace Roots apart before every one was resolved");
    }
    for (root, resolution) in resolved {
        if let Some(named) = named.get_mut(&root) {
            named.resolved = resolution;
        }
    }
    named
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path rooted as this platform roots absolute paths.
    fn absolute(components: &[&str]) -> PathBuf {
        components.iter().fold(
            PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }),
            |path, component| path.join(component),
        )
    }

    fn named(spelled: &str, written: &[&str], resolved: Resolution) -> NamedDirectory {
        NamedDirectory {
            spelled: spelled.to_owned(),
            written: (!written.is_empty()).then(|| absolute(written)),
            resolved,
        }
    }

    fn resolved(components: &[&str]) -> Resolution {
        Resolution::Resolved(absolute(components))
    }

    /// Two Roots are the same directory where spelled, written or resolved
    /// alike, surely not where both resolve elsewhere, and untold otherwise:
    /// nothing being there for one tells nothing.
    #[test]
    fn roots_are_told_apart_only_as_far_as_they_can_be() {
        let projects = named(
            "~/projects",
            &["home", "projects"],
            resolved(&["home", "projects"]),
        );
        for (other, likeness, why) in [
            (
                named("~/projects/", &["home", "projects"], Resolution::Untold),
                Likeness::Same,
                "written alike",
            ),
            (
                named("/link", &["link"], resolved(&["home", "projects"])),
                Likeness::Same,
                "resolved alike",
            ),
            (
                named("/notes", &["notes"], resolved(&["notes"])),
                Likeness::Different,
                "resolved elsewhere",
            ),
            (
                named("/unplugged", &["unplugged"], Resolution::Missing),
                Likeness::Untold,
                "nothing there, as for a link on a drive not mounted",
            ),
            (
                named("/quiet", &["quiet"], Resolution::Untold),
                Likeness::Untold,
                "not resolved in time",
            ),
            (
                named("~someone/projects", &[], Resolution::Untold),
                Likeness::Untold,
                "beneath an unknown home",
            ),
        ] {
            assert_eq!(projects.likeness(&other), likeness, "{why}");
            assert_eq!(
                other.likeness(&projects),
                likeness,
                "{why}, either way round"
            );
        }
        let dangling = named("/link", &["link"], Resolution::Missing);
        let target = named(
            "/mount/projects",
            &["mount", "projects"],
            Resolution::Missing,
        );
        assert_eq!(
            dangling.likeness(&target),
            Likeness::Untold,
            "a dangling link may lead where a path with nothing there names"
        );
    }

    /// A Root names a directory standing now where it is written as it or
    /// resolves to it, and one it cannot be resolved to by its writing
    /// alone.
    #[test]
    fn a_root_names_a_directory_where_written_or_resolved_as_it() {
        let atlas = absolute(&["home", "projects", "atlas"]);
        for (root, names, why) in [
            (
                named(
                    "~/projects/atlas/",
                    &["home", "projects", "atlas"],
                    Resolution::Untold,
                ),
                true,
                "written as it",
            ),
            (
                named("/link", &["link"], resolved(&["home", "projects", "atlas"])),
                true,
                "resolved to it",
            ),
            (
                named("/notes", &["notes"], resolved(&["notes"])),
                false,
                "resolved elsewhere",
            ),
            (
                named("/unplugged", &["unplugged"], Resolution::Missing),
                false,
                "nothing there",
            ),
            (
                named("/quiet", &["quiet"], Resolution::Untold),
                false,
                "not resolved, and written otherwise",
            ),
            (
                named("~someone/atlas", &[], Resolution::Untold),
                false,
                "beneath an unknown home",
            ),
        ] {
            assert_eq!(root.names(&atlas), names, "{why}");
        }
    }

    #[tokio::test]
    async fn a_root_is_read_beneath_the_home_and_resolved_where_something_is_there() {
        let home = tempfile::tempdir().expect("create a home");
        let home = crate::paths::canonical(home.path()).expect("read the home canonically");
        let projects = home.join("projects");
        std::fs::create_dir(&projects).expect("create a Root");
        let roots = [
            "~/projects".to_owned(),
            home.join("unplugged").to_str().expect("UTF-8").to_owned(),
            "~someone/projects".to_owned(),
        ];
        let reads = ReadsApart::new("test-resolution", 8);

        let named = resolve(&reads, Some(&home), Duration::from_secs(10), roots.iter()).await;

        assert!(named["~/projects"].names(&projects));
        assert_eq!(named[&roots[1]].resolved, Resolution::Missing);
        assert_eq!(named["~someone/projects"].written, None);
        assert_eq!(named["~someone/projects"].resolved, Resolution::Untold);
    }
}
