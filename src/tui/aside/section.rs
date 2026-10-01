//! The Section seam: how one reading of the open Session contributes to the
//! Aside.
//!
//! A Section is handed a typed context for the open Session and answers with
//! what it has to say — a header, its rows as drawn content, the semantic
//! invocation each row stands for, and which row is the open Session's own.
//! Everything else is the Aside's: where the Section stands, drawing its
//! header, resolving a press to a row, and standing in for a Section that
//! failed. Unlike a render slot, a Section's rows are pointable, so this seam
//! carries the invocations beside the content rather than content alone.

use ratatui::text::Line;

use crate::protocol::{SessionReference, SessionTimestamp, WorkspacePaths};
use crate::theme::Theme;

use super::super::{commands::SemanticInvocation, shimmer};
use super::SubagentTreeReading;

/// What a Section is told about the open Session. Every field is a reading
/// the client already holds; a Section asks the server for nothing itself.
pub(in crate::tui) struct SectionContext<'a> {
    /// The Session the main view has open.
    pub(in crate::tui) open: &'a SessionReference,
    /// What the Aside knows of the tree the open Session belongs to.
    pub(in crate::tui) subagent_tree: SubagentTreeView<'a>,
    /// The columns a row may take.
    pub(in crate::tui) width: u16,
    pub(in crate::tui) theme: &'a Theme,
    /// The run loop's presentation frame, for a live Marker's Spinner.
    pub(in crate::tui) spinner_frame: usize,
    /// The shimmer's clock and the colour depth it draws at, for Loading.
    pub(in crate::tui) shimmer: &'a shimmer::Clock,
    pub(in crate::tui) truecolor: bool,
    /// The moment now on the clock the Server's timestamps are read against,
    /// for ticking how long live work has been running.
    pub(in crate::tui) now: SessionTimestamp,
    /// Whether Icons are drawn at all.
    pub(in crate::tui) show_icons: bool,
    /// How the open Session's Server spells and names its paths, where it
    /// has said; a Workspace is named by the last part of its path where it
    /// has not.
    pub(in crate::tui) workspace_paths: Option<&'a WorkspacePaths>,
}

/// The tree the open Session belongs to, as far as the Aside knows it.
#[derive(Clone, Copy, Debug)]
pub(in crate::tui) enum SubagentTreeView<'a> {
    /// The per-tree subscription has delivered it.
    Ready(&'a SubagentTreeReading),
    /// Not yet in hand: drawn blank, then `loading` once the quiet period
    /// has passed.
    Arriving { loading: bool },
    /// It could not be read, for this reason.
    Failed(&'a str),
    /// It was deleted, and there is nothing to say about it.
    Gone,
    /// Not yet in hand for a Session the client already knows enough about
    /// to stand in its top-level entry: the Provisional Session, and the real
    /// Session answering it until its tree lands. Only the Server knows when
    /// Working began, so the entry carries no time.
    StandIn { title: &'a str, working: bool },
}

/// A Section's header: its name, how many things it holds where it knows,
/// and how many of those are working where any are.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::tui) struct SectionHeader {
    pub(in crate::tui) name: &'static str,
    pub(in crate::tui) count: Option<usize>,
    pub(in crate::tui) working: Option<usize>,
}

/// One drawn row, and what choosing it does. A row is one entry however many
/// lines it takes: every line is pressed, focused, and scrolled as the one
/// entry. A row that stands for nothing — the open Session's own entry, whose
/// choosing would change nothing — carries no invocation.
#[derive(Clone, Debug)]
pub(in crate::tui) struct SectionRow {
    pub(in crate::tui) lines: Vec<Line<'static>>,
    pub(in crate::tui) invocation: Option<SemanticInvocation>,
    /// What the row is an entry for, so row focus and the scroll anchor can
    /// follow it however the rows around it move. A row with no key — a
    /// Loading line, an error — takes no row focus.
    pub(in crate::tui) key: Option<SectionRowKey>,
}

/// The identity of a Section's entry, stable across frames.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(in crate::tui) enum SectionRowKey {
    /// An entry standing for one Session.
    Session(SessionReference),
}

/// Everything one Section says for one frame.
#[derive(Clone, Debug)]
pub(in crate::tui) struct SectionView {
    pub(in crate::tui) header: SectionHeader,
    pub(in crate::tui) rows: Vec<SectionRow>,
    /// The row standing for the open Session, which the Aside keeps in view.
    pub(in crate::tui) current: Option<usize>,
    /// Whether any row draws live presentation, so the run loop keeps
    /// ticking while it is on screen.
    pub(in crate::tui) animates: bool,
}

/// One reading of the open Session the Aside can present.
pub(in crate::tui) trait Section {
    /// The name the Aside heads the Section with, also where the Section
    /// could not say anything itself.
    fn name(&self) -> &'static str;

    /// What the Section has to say for this frame, or why it could not say
    /// it. A failure is drawn in the Section's place and takes nothing else
    /// in the Aside with it.
    fn view(&self, context: &SectionContext<'_>) -> Result<SectionView, String>;
}

/// The Sections the Aside stacks, in the order it stacks them. Customising
/// the Aside later changes this list, not the seam.
pub(in crate::tui) fn built_in_sections() -> [&'static dyn Section; 1] {
    [&super::subagents::SubagentsSection]
}
