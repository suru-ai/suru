//! The Notice the Application carries when startup found configuration
//! problems, or when something the reader asked of the Client itself failed.
//!
//! The server hands every client the startup diagnostics on the
//! effective-settings snapshot, each one already naming its file, its key path,
//! and why the value was ignored. The Log keeps all of that; a Notice keeps
//! only what a reader needs to know something went wrong and where to look —
//! so it collapses the whole set into one line: the loudest failures first,
//! whole files ahead of single keys, the keys merely counted, and a pointer to
//! the Log for the rest. A failed paste says why in a Notice of its own, as
//! does a draft whose Attachment labels were demoted to plain text.
//!
//! A Relay coming to need a login raises a Notice too. Nothing of a Relay
//! reaches the Log (ADR-0008), so that Notice points at where its reader logs
//! in instead.

use std::cell::Cell;
use std::path::Path;

use uuid::Uuid;

use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{SettingsDiagnostic, SettingsDiagnosticSeverity},
    theme::{Theme, ThemeDiagnostic},
};

use super::{clipboard::PasteId, slots::truncate_to_width};

/// Leads the Notice so severity reads before the words do.
const ERROR_GLYPH: &str = "×";
const WARNING_GLYPH: &str = "!";

/// Where the diagnostics the Notice had no room for live.
const LOG_POINTER: &str = "see the Log";

/// Where a reader told a Relay needs a login goes to log in there.
pub(super) const RELAY_LOGIN_POINTER: &str = "/relay to log in";

/// What the Application has to say, and whether the reader has seen it. The
/// identity of a dismissed Notice keeps that condition from returning without
/// suppressing a distinct runtime problem.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ApplicationNotice {
    showing: Option<Notice>,
    /// Conditions whose Notice the reader already saw and dismissed. This is
    /// cumulative so A, then B, cannot make A new again.
    dismissed: Vec<NoticeIdentity>,
    settings_diagnostics: Vec<SettingsDiagnostic>,
    theme_diagnostics: Vec<ThemeDiagnostic>,
    /// How many times a draft's Attachment labels have been demoted, which
    /// tells each demotion's Notice apart from the last.
    demotions: u64,
}

impl ApplicationNotice {
    pub(super) fn showing(&self) -> Option<&Notice> {
        self.showing.as_ref()
    }

    /// Takes what a freshly received effective-settings snapshot found at
    /// startup. The same startup Notice stays dismissed.
    pub(super) fn receive(&mut self, diagnostics: &[SettingsDiagnostic]) {
        self.settings_diagnostics = diagnostics.to_vec();
        self.refresh_diagnostics();
    }

    /// Replaces the result of the Client's latest scan of its own Theme
    /// directory. A repaired file disappears from the Notice, while a newly
    /// broken file can still speak after an earlier one was dismissed.
    pub(super) fn receive_theme_diagnostics(&mut self, diagnostics: Vec<ThemeDiagnostic>) {
        self.theme_diagnostics = diagnostics;
        self.refresh_diagnostics();
    }

    /// Reports a valid open Setting whose runtime value names no available
    /// Theme. The pin remains valid configuration; only this Client's
    /// resolution falls back for the run.
    pub(super) fn receive_theme_fallback(&mut self, name: &str) {
        self.raise(
            NoticeIdentity::ThemeFallback(name.to_owned()),
            SettingsDiagnosticSeverity::Warning,
            format!("Theme {name:?} was not found; using System"),
        );
    }

    /// Reports why a paste inserted nothing. Each paste is its own condition,
    /// so dismissing one failure never silences the next.
    pub(super) fn receive_paste_failure(
        &mut self,
        paste: PasteId,
        failure: PasteFailure,
        summary: String,
    ) {
        self.raise(
            NoticeIdentity::PasteFailed { paste, failure },
            SettingsDiagnosticSeverity::Error,
            summary,
        );
    }

    /// Reports the Attachments, named as the reader knows them, whose labels
    /// a draft now carries as plain text, and why. Each demotion is its own
    /// condition, so dismissing one never silences the next.
    pub(super) fn receive_demoted_attachments(
        &mut self,
        names: &[&str],
        demotion: AttachmentDemotion,
    ) {
        if names.is_empty() {
            return;
        }
        self.demotions += 1;
        self.raise(
            NoticeIdentity::AttachmentsDemoted(self.demotions),
            SettingsDiagnosticSeverity::Warning,
            demotion.summary(names),
        );
    }

    /// Reports that the Relay at `address` has come to need a login, in
    /// `lapse`. Each lapse is its own condition, so a Relay that needs a
    /// login again after it was logged in at is said to anew.
    pub(super) fn receive_relay_login_needed(&mut self, address: &str, lapse: Uuid) {
        self.raise(
            NoticeIdentity::RelayLoginNeeded {
                address: address.to_owned(),
                lapse,
            },
            SettingsDiagnosticSeverity::Warning,
            format!("Login needed at {address}"),
        );
    }

    /// Takes back the Notice of each Relay coming to need a login in a lapse
    /// `ended` says is over, where no frame has drawn it yet: a lapse that
    /// ended before its reader could see it is nothing to tell them. One
    /// drawn stands until the reader dismisses it, as any Notice does.
    pub(super) fn withdraw_unshown_relay_login_needed(
        &mut self,
        ended: impl Fn(&str, Uuid) -> bool,
    ) {
        let Some(notice) = self.showing.as_mut() else {
            return;
        };
        if notice.shown.get() {
            return;
        }
        notice.parts.retain(|part| {
            !part.identities.iter().any(|identity| {
                matches!(
                    identity,
                    NoticeIdentity::RelayLoginNeeded { address, lapse } if ended(address, *lapse)
                )
            })
        });
        if notice.parts.is_empty() {
            self.showing = None;
        }
    }

    /// Raises a runtime condition's Notice, joining any Notice already
    /// showing, unless the reader already dismissed that very condition.
    fn raise(
        &mut self,
        identity: NoticeIdentity,
        severity: SettingsDiagnosticSeverity,
        summary: String,
    ) {
        if self.dismissed.contains(&identity) {
            return;
        }
        let part = NoticePart {
            identities: vec![identity],
            severity,
            summary,
        };
        match self.showing.as_mut() {
            Some(notice) => {
                if notice.holds(&part.identities[0]) {
                    return;
                }
                notice.parts.push(part);
                notice.shown.set(false);
            }
            None => {
                self.showing = Some(Notice {
                    parts: vec![part],
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
        for identity in notice.identities() {
            if !self.dismissed.contains(identity) {
                self.dismissed.push(identity.clone());
            }
        }
        self.showing = None;
        true
    }

    fn refresh_diagnostics(&mut self) {
        let mut diagnostics = Vec::new();
        let mut identities = Vec::new();
        if !self.settings_diagnostics.is_empty()
            && !self.dismissed.contains(&NoticeIdentity::StartupDiagnostics)
        {
            diagnostics.extend(self.settings_diagnostics.iter().map(NoticeDiagnostic::from));
            identities.push(NoticeIdentity::StartupDiagnostics);
        }
        for diagnostic in &self.theme_diagnostics {
            let identity = NoticeIdentity::ThemeFile(diagnostic.clone());
            if self.dismissed.contains(&identity) {
                continue;
            }
            diagnostics.push(NoticeDiagnostic::from(diagnostic));
            identities.push(identity);
        }
        // Only what the diagnostics say is replaced: a runtime condition the
        // Notice carries — a failed paste, a Relay needing a login — stands
        // until the reader dismisses it, whatever another Client's Setting
        // change pushes meanwhile.
        let (before, shown) = self.showing.take().map_or_else(Default::default, |notice| {
            (notice.parts, notice.shown.get())
        });
        let parts = NoticePart::for_diagnostics(&diagnostics, identities)
            .into_iter()
            .chain(before.iter().filter(|part| part.is_runtime()).cloned())
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return;
        }
        self.showing = Some(Notice {
            shown: Cell::new(shown && parts == before),
            parts,
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NoticeIdentity {
    StartupDiagnostics,
    ThemeFile(ThemeDiagnostic),
    ThemeFallback(String),
    PasteFailed {
        paste: PasteId,
        failure: PasteFailure,
    },
    AttachmentsDemoted(u64),
    /// The Relay at `address` came to need a login, in `lapse`.
    RelayLoginNeeded {
        address: String,
        lapse: Uuid,
    },
}

impl NoticeIdentity {
    /// Whether this is a condition of the run, which stands until the
    /// reader dismisses it, rather than what loading configuration found.
    fn is_runtime(&self) -> bool {
        !matches!(self, Self::StartupDiagnostics | Self::ThemeFile(_))
    }
}

/// Why a draft's Attachment labels were demoted to plain text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttachmentDemotion {
    /// The Server the draft goes to no longer stores them: a Prompt recalled
    /// from history bound them, and they were reclaimed since.
    NoLongerStored,
    /// They were pasted to the Server the Outlook has since turned away from.
    LeftBehind,
}

impl AttachmentDemotion {
    /// The Notice's words for `names`, as in `Image 1 is no longer on the
    /// Server; its label stays as text`.
    fn summary(self, names: &[&str]) -> String {
        let one = names.len() == 1;
        let names = match names {
            [only] => (*only).to_owned(),
            [first, second] => format!("{first} and {second}"),
            [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
            [] => String::new(),
        };
        let what = match (self, one) {
            (Self::NoLongerStored, true) => "is no longer on the Server",
            (Self::NoLongerStored, false) => "are no longer on the Server",
            (Self::LeftBehind, true) => "is on another Server",
            (Self::LeftBehind, false) => "are on another Server",
        };
        let labels = if one {
            "its label stays"
        } else {
            "their labels stay"
        };
        format!("{names} {what}; {labels} as text")
    }
}

/// Why a paste from the clipboard inserted nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PasteFailure {
    /// The clipboard could not be read.
    Unreadable,
    /// The image is larger than one Attachment may be.
    TooLarge,
    /// The image is in a format that cannot be attached.
    UnsupportedFormat,
    /// The draft already binds as many Attachments as a Prompt may carry.
    TooMany,
    /// The Server refused the upload, or could not be reached for it.
    Refused,
}

#[derive(Clone, Copy)]
struct NoticeDiagnostic<'a> {
    severity: SettingsDiagnosticSeverity,
    file: &'a Path,
    key: Option<&'a str>,
    message: &'a str,
}

impl<'a> From<&'a SettingsDiagnostic> for NoticeDiagnostic<'a> {
    fn from(diagnostic: &'a SettingsDiagnostic) -> Self {
        Self {
            severity: diagnostic.severity,
            file: &diagnostic.file,
            key: diagnostic.key.as_deref(),
            message: &diagnostic.message,
        }
    }
}

impl<'a> From<&'a ThemeDiagnostic> for NoticeDiagnostic<'a> {
    fn from(diagnostic: &'a ThemeDiagnostic) -> Self {
        Self {
            severity: SettingsDiagnosticSeverity::Error,
            file: &diagnostic.file,
            key: None,
            message: &diagnostic.message,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Notice {
    /// What it says: the diagnostics' part first, where there is one, then
    /// each runtime condition in the order raised.
    parts: Vec<NoticePart>,
    shown: Cell<bool>,
}

/// One condition's part of a Notice: what it says and how loudly — or,
/// for the diagnostics, all of theirs in one.
#[derive(Clone, Debug, Eq, PartialEq)]
struct NoticePart {
    identities: Vec<NoticeIdentity>,
    severity: SettingsDiagnosticSeverity,
    summary: String,
}

impl NoticePart {
    fn is_runtime(&self) -> bool {
        self.identities.iter().all(NoticeIdentity::is_runtime)
    }

    /// The part a startup's diagnostics earn, or `None` when it found
    /// nothing to report.
    fn for_diagnostics(
        diagnostics: &[NoticeDiagnostic<'_>],
        identities: Vec<NoticeIdentity>,
    ) -> Option<Self> {
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
            .map(|diagnostic| format!("{} {}", file_name(diagnostic), headline(diagnostic.message)))
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
            identities,
        })
    }
}

impl Notice {
    fn identities(&self) -> impl Iterator<Item = &NoticeIdentity> {
        self.parts.iter().flat_map(|part| &part.identities)
    }

    fn holds(&self, identity: &NoticeIdentity) -> bool {
        self.identities().any(|held| held == identity)
    }

    /// The loudest condition the Notice carries sets the severity the whole
    /// Notice reads at.
    fn severity(&self) -> SettingsDiagnosticSeverity {
        if self
            .parts
            .iter()
            .any(|part| part.severity == SettingsDiagnosticSeverity::Error)
        {
            SettingsDiagnosticSeverity::Error
        } else {
            SettingsDiagnosticSeverity::Warning
        }
    }

    /// Records that a frame carried this Notice. Input coalesced behind the
    /// event that created it cannot dismiss words that never reached screen.
    pub(super) fn mark_shown(&self) {
        self.shown.set(true);
    }

    /// The Notice's one line at the width it has. The glyph and the pointers
    /// are what the reader acts on, so a summary too long for the terminal is
    /// what gives way — never a pointer telling them where the rest of it is,
    /// or where to act on it.
    pub(super) fn text(&self, width: u16) -> String {
        let glyph = self.glyph();
        let pointers = self.pointers();
        let reserved = glyph.width() + " ".width() + " · ".width() + pointers.width();
        let summary = self
            .parts
            .iter()
            .map(|part| part.summary.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let summary = truncate_to_width(&summary, usize::from(width).saturating_sub(reserved));
        if summary.is_empty() {
            return format!("{glyph} {pointers}");
        }
        format!("{glyph} {summary} · {pointers}")
    }

    /// The Relays this Notice says need a login, in the order it said so.
    pub(super) fn relays_needing_login(&self) -> Vec<&str> {
        self.identities()
            .filter_map(|identity| match identity {
                NoticeIdentity::RelayLoginNeeded { address, .. } => Some(address.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Where the Notice points its reader: at the Log for whatever it keeps
    /// the detail of, and at where to log in for a Relay needing a login.
    fn pointers(&self) -> String {
        let logged = self
            .identities()
            .any(|identity| !matches!(identity, NoticeIdentity::RelayLoginNeeded { .. }));
        let relay = !self.relays_needing_login().is_empty();
        [
            logged.then_some(LOG_POINTER),
            relay.then_some(RELAY_LOGIN_POINTER),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
    }

    pub(super) fn style(&self, theme: &Theme) -> Style {
        match self.severity() {
            SettingsDiagnosticSeverity::Error => theme.feedback.error,
            SettingsDiagnosticSeverity::Warning => theme.feedback.warning,
        }
    }

    fn glyph(&self) -> &'static str {
        match self.severity() {
            SettingsDiagnosticSeverity::Error => ERROR_GLYPH,
            SettingsDiagnosticSeverity::Warning => WARNING_GLYPH,
        }
    }
}

/// How many keys a startup ignored one at a time, named with their Config
/// Document while they all come from the same one.
fn ignored_keys_clause(diagnostics: &[NoticeDiagnostic<'_>]) -> Option<String> {
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
fn file_name(diagnostic: &NoticeDiagnostic<'_>) -> String {
    diagnostic
        .file
        .file_name()
        .unwrap_or(diagnostic.file.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_demotion_names_each_image_and_agrees_with_how_many_there_are() {
        assert_eq!(
            AttachmentDemotion::NoLongerStored.summary(&["Image 2"]),
            "Image 2 is no longer on the Server; its label stays as text"
        );
        assert_eq!(
            AttachmentDemotion::NoLongerStored.summary(&["Image 1", "Image 3"]),
            "Image 1 and Image 3 are no longer on the Server; their labels stay as text"
        );
        assert_eq!(
            AttachmentDemotion::LeftBehind.summary(&["Image 1"]),
            "Image 1 is on another Server; its label stays as text"
        );
        assert_eq!(
            AttachmentDemotion::LeftBehind.summary(&["Image 1", "Image 2", "Image 4"]),
            "Image 1, Image 2, and Image 4 are on another Server; their labels stay as text"
        );
    }

    #[test]
    fn each_demotion_raises_its_own_notice_after_the_last_was_dismissed() {
        let mut notice = ApplicationNotice::default();
        notice.receive_demoted_attachments(&[], AttachmentDemotion::NoLongerStored);
        assert!(notice.showing().is_none(), "nothing demoted says nothing");

        notice.receive_demoted_attachments(&["Image 1"], AttachmentDemotion::NoLongerStored);
        notice
            .showing()
            .expect("the demotion is shown")
            .mark_shown();
        assert!(notice.dismiss());
        notice.receive_demoted_attachments(&["Image 1"], AttachmentDemotion::NoLongerStored);
        assert!(
            notice
                .showing()
                .is_some_and(|shown| shown.text(120).contains("Image 1 is no longer")),
            "the same image demoted again is a new condition"
        );
    }
}
