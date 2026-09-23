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

use crate::protocol::SessionReference;
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
}

/// A Section's header: its name, and how many things it holds where it
/// knows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::tui) struct SectionHeader {
    pub(in crate::tui) name: &'static str,
    pub(in crate::tui) count: Option<usize>,
}

/// One drawn row, and what choosing it does. A row that stands for nothing —
/// the open Session's own entry, whose choosing would change nothing — carries
/// no invocation.
#[derive(Clone, Debug)]
pub(in crate::tui) struct SectionRow {
    pub(in crate::tui) line: Line<'static>,
    pub(in crate::tui) invocation: Option<SemanticInvocation>,
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
