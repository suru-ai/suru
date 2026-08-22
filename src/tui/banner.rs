//! The one-line notice the launch view carries when startup found problems.
//!
//! The server hands every client the startup diagnostics on the
//! effective-settings snapshot, each one already naming its file, its key path,
//! and why the value was ignored. The Log keeps all of that; the banner keeps
//! only what a reader needs to know something went wrong and where to look —
//! so it collapses the whole set into one line: the loudest failures first,
//! whole files ahead of single keys, the keys merely counted, and a pointer to
//! the Log for the rest.

use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{SettingsDiagnostic, SettingsDiagnosticSeverity},
    theme::Theme,
};

use super::slots::truncate_to_width;

/// Leads the banner so severity reads before the words do.
const ERROR_GLYPH: &str = "×";
const WARNING_GLYPH: &str = "!";

/// Where the diagnostics the banner had no room for live.
const LOG_POINTER: &str = "see the Log";

/// What the launch view has to say about startup, and whether it has already
/// said it. Modelled as one value because the two facts constrain each other:
/// a banner the reader dismissed is gone for the run, so a snapshot arriving
/// later — after a reconnect, or after an edit of a Setting — cannot put the
/// same startup problems back in front of them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum LaunchNotice {
    /// Startup found nothing to report, or the snapshot has not landed yet.
    #[default]
    Quiet,
    Showing(LaunchBanner),
    /// The reader saw a banner and moved on.
    Dismissed,
}

impl LaunchNotice {
    pub(super) fn showing(&self) -> Option<&LaunchBanner> {
        match self {
            Self::Showing(banner) => Some(banner),
            Self::Quiet | Self::Dismissed => None,
        }
    }

    /// Takes what a freshly received effective-settings snapshot found at
    /// startup. A notice already dismissed stays dismissed.
    pub(super) fn receive(&mut self, diagnostics: &[SettingsDiagnostic]) {
        if matches!(self, Self::Dismissed) {
            return;
        }
        *self = LaunchBanner::for_diagnostics(diagnostics).map_or(Self::Quiet, Self::Showing);
    }

    /// Takes the banner away on the reader's first interaction with it, and
    /// reports whether there was one to take. Dismissal is recorded only once
    /// a banner was actually showing, so interacting before the snapshot lands
    /// cannot suppress a notice the reader never got.
    pub(super) fn dismiss(&mut self) -> bool {
        if !matches!(self, Self::Showing(_)) {
            return false;
        }
        *self = Self::Dismissed;
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LaunchBanner {
    severity: SettingsDiagnosticSeverity,
    summary: String,
}

impl LaunchBanner {
    /// The banner a startup's diagnostics earn, or `None` when it found
    /// nothing to report and the launch view stays as it was.
    fn for_diagnostics(diagnostics: &[SettingsDiagnostic]) -> Option<Self> {
        // A whole Config Document being ignored is worded first and in full,
        // loudest severity leading; the keys ignored one at a time are worded
        // once, as a count.
        let mut documents = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.key.is_none())
            .collect::<Vec<_>>();
        documents.sort_by_key(|diagnostic| {
            u8::from(diagnostic.severity == SettingsDiagnosticSeverity::Warning)
        });
        let mut clauses = documents
            .into_iter()
            .map(|diagnostic| {
                format!(
                    "{} {}",
                    file_name(diagnostic),
                    headline(&diagnostic.message)
                )
            })
            .collect::<Vec<_>>();
        clauses.extend(ignored_keys_clause(diagnostics));
        if clauses.is_empty() {
            return None;
        }
        // The loudest problem the startup found is the one the reader has to
        // act on, so it sets the severity the whole banner reads at.
        let any_error = diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == SettingsDiagnosticSeverity::Error);
        Some(Self {
            severity: if any_error {
                SettingsDiagnosticSeverity::Error
            } else {
                SettingsDiagnosticSeverity::Warning
            },
            summary: clauses.join("; "),
        })
    }

    /// The banner's one line at the width it has. The glyph and the pointer at
    /// the Log are what the reader acts on, so a summary too long for the
    /// terminal is what gives way — never the pointer telling them where the
    /// rest of it is.
    pub(super) fn text(&self, width: u16) -> String {
        let glyph = self.glyph();
        let reserved = glyph.width() + " ".width() + " · ".width() + LOG_POINTER.width();
        let summary = truncate_to_width(&self.summary, usize::from(width).saturating_sub(reserved));
        if summary.is_empty() {
            return format!("{glyph} {LOG_POINTER}");
        }
        format!("{glyph} {summary} · {LOG_POINTER}")
    }

    pub(super) fn style(&self, theme: &Theme) -> Style {
        match self.severity {
            SettingsDiagnosticSeverity::Error => theme.feedback.error,
            SettingsDiagnosticSeverity::Warning => theme.feedback.warning,
        }
    }

    fn glyph(&self) -> &'static str {
        match self.severity {
            SettingsDiagnosticSeverity::Error => ERROR_GLYPH,
            SettingsDiagnosticSeverity::Warning => WARNING_GLYPH,
        }
    }
}

/// How many keys a startup ignored one at a time, named with their Config
/// Document while they all come from the same one.
fn ignored_keys_clause(diagnostics: &[SettingsDiagnostic]) -> Option<String> {
    let keyed = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.key.is_some())
        .collect::<Vec<_>>();
    let (first, rest) = keyed.split_first()?;
    let noun = if rest.is_empty() { "key" } else { "keys" };
    let count = keyed.len();
    if rest.iter().all(|diagnostic| diagnostic.file == first.file) {
        return Some(format!("{count} {noun} ignored in {}", file_name(first)));
    }
    Some(format!("{count} {noun} ignored"))
}

/// A diagnostic's message up to the detail it trails: the loader words every
/// one as "ignored because …", with whatever the parser or the filesystem said
/// after a colon. The clause before that colon is the banner's sentence; the
/// detail is the Log's.
fn headline(message: &str) -> &str {
    message
        .split_once(':')
        .map_or(message, |(headline, _)| headline)
        .trim_end()
}

/// Config Documents are named by their file alone. The Log carries the path
/// the banner has no room for.
fn file_name(diagnostic: &SettingsDiagnostic) -> String {
    diagnostic
        .file
        .file_name()
        .unwrap_or(diagnostic.file.as_os_str())
        .to_string_lossy()
        .into_owned()
}
