//! Provider-neutral suggested CLI version checks.
//!
//! Providers own the version they have verified against and how they learn the installed CLI's
//! version. This module owns everything shared after that: parsing the semantic version shape,
//! comparing prereleases correctly, and writing one consistent advisory warning.

use std::fmt;

/// A CLI version Suru suggests without making older, readable versions unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SuggestedCliVersion {
    cli: &'static str,
    minimum: CliVersion,
}

impl SuggestedCliVersion {
    pub(crate) const fn new(cli: &'static str, major: u64, minor: u64, patch: u64) -> Self {
        Self {
            cli,
            minimum: CliVersion {
                major,
                minor,
                patch,
                prerelease: false,
            },
        }
    }

    /// Returns the compatibility guidance for a readable version below the suggestion.
    ///
    /// An error means `reported` is not a semantic version this policy can compare. The Provider
    /// decides whether an unreadable version is a hard wire incompatibility or merely means no
    /// advisory can be made.
    pub(crate) fn warning_for(
        self,
        reported: &str,
    ) -> Result<Option<String>, UnreadableCliVersion> {
        let reported = reported.trim();
        let version = CliVersion::parse(reported).ok_or(UnreadableCliVersion)?;
        Ok(version.is_below(self.minimum).then(|| {
            format!(
                "{} {reported} may have compatibility issues; update to {} {} or newer",
                self.cli, self.cli, self.minimum
            )
        }))
    }
}

/// A version that is not the comparable semantic shape Provider CLI releases use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UnreadableCliVersion;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CliVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: bool,
}

impl CliVersion {
    fn parse(reported: &str) -> Option<Self> {
        let (without_build, build) = reported
            .split_once('+')
            .map_or((reported, None), |(version, build)| (version, Some(build)));
        let (core, prerelease) = without_build
            .split_once('-')
            .map_or((without_build, None), |(core, prerelease)| {
                (core, Some(prerelease))
            });
        let mut parts = core.split('.');
        let version = Self {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next()?.parse().ok()?,
            prerelease: prerelease.is_some(),
        };
        (parts.next().is_none()
            && prerelease.is_none_or(|suffix| !suffix.is_empty())
            && build.is_none_or(|suffix| !suffix.is_empty()))
        .then_some(version)
    }

    fn is_below(self, minimum: Self) -> bool {
        (self.major, self.minor, self.patch) < (minimum.major, minimum.minor, minimum.patch)
            || ((self.major, self.minor, self.patch)
                == (minimum.major, minimum.minor, minimum.patch)
                && self.prerelease
                && !minimum.prerelease)
    }
}

impl fmt::Display for CliVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::{SuggestedCliVersion, UnreadableCliVersion};

    const SUGGESTED: SuggestedCliVersion = SuggestedCliVersion::new("Fixture CLI", 1, 2, 3);

    #[test]
    fn versions_below_the_suggestion_receive_shared_guidance() {
        for reported in ["0.99.99", "1.2.2", "1.2.3-rc.1"] {
            assert_eq!(
                SUGGESTED.warning_for(reported),
                Ok(Some(format!(
                    "Fixture CLI {reported} may have compatibility issues; update to Fixture CLI \
                     1.2.3 or newer"
                )))
            );
        }
    }

    #[test]
    fn versions_at_or_above_the_suggestion_receive_no_guidance() {
        for reported in ["1.2.3", "1.2.3+linux-x64", " 1.2.3 ", "1.2.4", "2.0.0"] {
            assert_eq!(SUGGESTED.warning_for(reported), Ok(None));
        }
    }

    #[test]
    fn versions_outside_the_comparable_shape_are_unreadable() {
        for reported in ["", "1.2", "v1.2.3", "1.2.x", "1.2.3-"] {
            assert_eq!(SUGGESTED.warning_for(reported), Err(UnreadableCliVersion));
        }
    }
}
