//! The Notice the Application carries when startup found configuration problems.
//!
//! The server hands every client the startup diagnostics on the
//! effective-settings snapshot, each one already naming its file, its key path,
//! and why the value was ignored. The Log keeps all of that; a Notice keeps
//! only what a reader needs to know something went wrong and where to look —
//! so it collapses the whole set into one line: the loudest failures first,
//! whole files ahead of single keys, the keys merely counted, and a pointer to
//! the Log for the rest.

use std::cell::Cell;

use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{SettingsDiagnostic, SettingsDiagnosticSeverity},
    theme::Theme,
};

use super::slots::truncate_to_width;

/// Leads the Notice so severity reads before the words do.
const ERROR_GLYPH: &str = "×";
const WARNING_GLYPH: &str = "!";

/// Where the diagnostics the Notice had no room for live.
const LOG_POINTER: &str = "see the Log";

/// What the Application has to say, and whether the reader has seen it. The
/// identity of a dismissed Notice keeps that condition from returning without
/// suppressing a distinct runtime problem.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ApplicationNotice {
    showing: Option<Notice>,
    /// Conditions whose Notice the reader already saw and dismissed. This is
    /// cumulative so A, then B, cannot make A new again.
    dismissed: Vec<NoticeIdentity>,
}

impl ApplicationNotice {
    pub(super) fn showing(&self) -> Option<&Notice> {
        self.showing.as_ref()
    }

    /// Takes what a freshly received effective-settings snapshot found at
    /// startup. The same startup Notice stays dismissed.
    pub(super) fn receive(&mut self, diagnostics: &[SettingsDiagnostic]) {
        let Some(notice) = Notice::for_diagnostics(diagnostics) else {
            self.showing = None;
            return;
        };
        if self.dismissed.contains(&NoticeIdentity::StartupDiagnostics) {
            self.showing = None;
            return;
        }
        self.showing = Some(notice);
    }

    /// Reports a valid open Setting whose runtime value names no available
    /// Theme. The pin remains valid configuration; only this Client's
    /// resolution falls back for the run.
    pub(super) fn receive_theme_fallback(&mut self, name: &str) {
        let identity = NoticeIdentity::ThemeFallback(name.to_owned());
        if self.dismissed.contains(&identity) {
            return;
        }
        let fallback = format!("Theme {name:?} was not found; using System");
        match self.showing.as_mut() {
            Some(notice) => {
                if notice.identities.contains(&identity) {
                    return;
                }
                notice.summary.push_str("; ");
                notice.summary.push_str(&fallback);
                notice.identities.push(identity);
                notice.shown.set(false);
            }
            None => {
                self.showing = Some(Notice {
                    severity: SettingsDiagnosticSeverity::Warning,
                    summary: fallback,
                    identities: vec![identity],
                    shown: Cell::new(false),
                });
            }
        }
    }

    /// Takes the Notice away on the reader's first interaction with it, and
    /// reports whether there was one to take. Dismissal is recorded only once
    /// a Notice was actually showing, so interacting before the snapshot lands
    /// cannot suppress one the reader never got.
    pub(super) fn dismiss(&mut self) -> bool {
        let Some(notice) = self.showing.as_ref() else {
            return false;
        };
        if !notice.shown.get() {
            return false;
        }
        for identity in &notice.identities {
            if !self.dismissed.contains(identity) {
                self.dismissed.push(identity.clone());
            }
        }
        self.showing = None;
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NoticeIdentity {
    StartupDiagnostics,
    ThemeFallback(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Notice {
    severity: SettingsDiagnosticSeverity,
    summary: String,
    identities: Vec<NoticeIdentity>,
    shown: Cell<bool>,
}

impl Notice {
    /// The Notice a startup's diagnostics earn, or `None` when it found
    /// nothing to report and the Landing stays as it was.
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
        // act on, so it sets the severity the whole Notice reads at.
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
            identities: vec![NoticeIdentity::StartupDiagnostics],
            shown: Cell::new(false),
        })
    }

    /// Records that a frame carried this Notice. Input coalesced behind the
    /// event that created it cannot dismiss words that never reached screen.
    pub(super) fn mark_shown(&self) {
        self.shown.set(true);
    }

    /// The Notice's one line at the width it has. The glyph and the pointer at
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
/// after a colon. The clause before that colon is the Notice's sentence; the
/// detail is the Log's.
fn headline(message: &str) -> &str {
    message
        .split_once(':')
        .map_or(message, |(headline, _)| headline)
        .trim_end()
}

/// Config Documents are named by their file alone. The Log carries the path
/// the Notice has no room for.
fn file_name(diagnostic: &SettingsDiagnostic) -> String {
    diagnostic
        .file
        .file_name()
        .unwrap_or(diagnostic.file.as_os_str())
        .to_string_lossy()
        .into_owned()
}
