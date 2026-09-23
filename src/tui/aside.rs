//! The Aside: the collapsible column on the far side of the main view from the
//! Sidebar, answering for the open Session through a stack of Sections.
//!
//! The Aside owns its column chrome (through the shared [`SideColumn`]), the
//! tree the per-tree subscription last delivered, and the geometry the last
//! frame drew its rows at. What each Section says is the Section's own; see
//! [`section`].

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
};

use ratatui::{
    Frame,
    layout::{Position, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::managed_client::SubagentTreeEvent;
use crate::protocol::{
    Outlook, SessionId, SessionReference, SubagentTreeChange, SubagentTreeEntry,
    SubagentTreeSnapshot, SubagentTreeTopLevel,
};
use crate::theme::Theme;

use super::{
    commands::SemanticInvocation,
    render::{horizontally_inset, side_column_block},
    side_column::{Side, SideColumn, ToggleStep},
    slots::truncate_to_width,
};

mod section;
mod subagents;

use section::{SectionContext, SectionView, built_in_sections};

/// Whether the Aside begins shown, until its Setting arrives (#367).
const INITIAL_VISIBILITY_SHOWN: bool = true;
/// How wide the Aside begins, until its Setting arrives (#367).
const INITIAL_WIDTH: u64 = 32;

#[derive(Clone, Debug)]
pub(super) struct Aside {
    column: SideColumn,
    tree: TreeFollow,
    /// Where the last frame drew each pointable row, and what choosing it
    /// does. A frame that drew no Aside leaves nothing to press.
    rows: RefCell<Vec<AsideRowHit>>,
    /// Whether the last frame drew live presentation in the Aside.
    animating: Cell<bool>,
}

#[derive(Clone, Debug)]
struct AsideRowHit {
    row: u16,
    columns: Range<u16>,
    invocation: Option<SemanticInvocation>,
}

/// What a press over the Aside's body landed on.
#[derive(Clone, Debug)]
pub(super) enum AsidePress {
    /// Somewhere the Aside did not draw.
    Elsewhere,
    /// Inside the Aside, but on nothing that acts — its header, blank space,
    /// or the open Session's own entry.
    Inert,
    /// An entry, standing for this invocation.
    Invoke(SemanticInvocation),
}

impl Aside {
    pub(super) fn new() -> Self {
        Self {
            column: SideColumn::new(Side::Right),
            tree: TreeFollow::default(),
            rows: RefCell::new(Vec::new()),
            animating: Cell::new(false),
        }
    }

    /// Takes the launch visibility and width from the first Settings
    /// snapshot; later snapshots leave the reader's own choices alone.
    pub(super) fn adopt_settings(&mut self) {
        if self.column.seed(INITIAL_WIDTH) {
            self.column.set_revealed(INITIAL_VISIBILITY_SHOWN);
        }
    }

    /// The width a reset returns the Aside to.
    pub(super) const fn initial_width(&self) -> u64 {
        INITIAL_WIDTH
    }

    pub(super) const fn column(&self) -> &SideColumn {
        &self.column
    }

    pub(super) fn column_mut(&mut self) -> &mut SideColumn {
        &mut self.column
    }

    /// The Aside's show/hide act, the same three-way act as the Sidebar's:
    /// hidden, it shows and takes the keys; shown without them, it takes
    /// them; holding them, it hides and hands them back. Answers whether the
    /// Aside now claims the keys, so the caller can take them from the
    /// Sidebar.
    pub(super) fn toggle(&mut self) -> bool {
        match self.column.toggle_step() {
            ToggleStep::Show => {
                self.column.set_revealed(true);
                self.column.take_keys();
            }
            ToggleStep::TakeKeys => self.column.take_keys(),
            ToggleStep::Hide => {
                self.column.set_revealed(false);
                self.hand_back_keys();
            }
        }
        self.column.claims_keys()
    }

    pub(super) fn hand_back_keys(&mut self) {
        self.column.hand_back_keys();
    }

    /// Whether the reader is driving the Aside: it claims the keys and the
    /// last frame had the columns to draw it.
    pub(super) fn has_focus(&self) -> bool {
        self.column.has_keys()
    }

    /// Gives up what the last frame recorded.
    pub(super) fn forget_frame(&self) {
        self.column.forget_frame();
        self.rows.borrow_mut().clear();
        self.animating.set(false);
    }

    /// Whether the Aside on screen draws anything live.
    pub(super) fn shows_live_work(&self) -> bool {
        self.animating.get() && self.column.is_on_screen()
    }

    /// Resolves a press against the rows the last frame drew.
    pub(super) fn press_at(&self, position: Position) -> AsidePress {
        let Some(area) = self.column.drawn_area() else {
            return AsidePress::Elsewhere;
        };
        if !area.contains(position) {
            return AsidePress::Elsewhere;
        }
        self.rows
            .borrow()
            .iter()
            .find(|hit| hit.row == position.y && hit.columns.contains(&position.x))
            .and_then(|hit| hit.invocation.clone())
            .map_or(AsidePress::Inert, AsidePress::Invoke)
    }

    /// The Session the per-tree subscription should be asked through, or
    /// `None` when there is nothing for the Aside to answer for. Moving
    /// between Sessions of the tree already in hand keeps the subscription
    /// it came from, so the Section stands as it is.
    pub(super) fn tree_request(&self, open: Option<&SessionReference>) -> Option<SessionReference> {
        let open = open?;
        if !self.column.is_revealed() {
            return None;
        }
        if let Some(through) = &self.tree.through
            && self.tree.covers(open)
        {
            return Some(through.clone());
        }
        Some(open.clone())
    }

    /// Takes one event from the subscription asked through `through`,
    /// answering whether anything the Aside draws may have moved. An event
    /// from a subscription the Aside no longer wants is dropped.
    pub(super) fn receive_tree(
        &mut self,
        through: SessionReference,
        event: SubagentTreeEvent,
        open: Option<&SessionReference>,
    ) -> bool {
        if self.tree_request(open).as_ref() != Some(&through) {
            return false;
        }
        match event {
            SubagentTreeEvent::Snapshot(snapshot) => {
                self.tree.reading =
                    Some(SubagentTreeReading::new(through.origin.clone(), snapshot));
                self.tree.through = Some(through);
                true
            }
            SubagentTreeEvent::Changed(change) => {
                if self.tree.through.as_ref() != Some(&through) {
                    return false;
                }
                let Some(reading) = &mut self.tree.reading else {
                    return false;
                };
                reading.apply(change);
                true
            }
            // The last tree in hand stands. Saying the tree could not be
            // read, and reading it again, is #370's.
            SubagentTreeEvent::Failed(_) => false,
        }
    }

    /// The tree the open Session belongs to, where the Aside holds it.
    pub(super) fn tree_for(&self, open: &SessionReference) -> Option<&SubagentTreeReading> {
        self.tree
            .covers(open)
            .then_some(())
            .and(self.tree.reading.as_ref())
    }

    /// Draws the Aside in the column the frame's layout gave it, stacking
    /// every built-in Section and recording its rows for the pointer.
    pub(super) fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        open: &SessionReference,
        owns_input: bool,
        theme: &Theme,
        spinner_frame: usize,
    ) {
        let block = side_column_block(&self.column, owns_input, theme);
        let inside = block.inner(area);
        let content = horizontally_inset(inside, 1);
        frame.render_widget(block, area);
        let context = SectionContext {
            open,
            subagent_tree: self.tree_for(open),
            width: content.width,
            theme,
            spinner_frame,
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut hits = Vec::new();
        let mut animates = false;
        for section in built_in_sections() {
            let view = match section.view(&context) {
                Ok(view) => view,
                Err(message) => {
                    // A failing Section is drawn as failed, in its own place,
                    // and the Sections around it go on as they were.
                    lines.push(header_line(section.name(), None, content.width, theme));
                    lines.push(Line::styled(
                        truncate_to_width(
                            &format!("Could not show: {message}"),
                            usize::from(content.width),
                        ),
                        theme.feedback.error,
                    ));
                    continue;
                }
            };
            let SectionView {
                header,
                rows,
                current,
                animates: section_animates,
            } = view;
            animates |= section_animates;
            lines.push(header_line(header.name, header.count, content.width, theme));
            // Rows past the column's foot are dropped from the top where that
            // is what keeps the open Session's entry in view.
            let room = usize::from(content.height).saturating_sub(lines.len());
            let skipped = current.map_or(0, |current| (current + 1).saturating_sub(room));
            for row in rows.into_iter().skip(skipped) {
                let y = content
                    .y
                    .saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
                if y < content.bottom() {
                    hits.push(AsideRowHit {
                        row: y,
                        columns: inside.x..inside.right(),
                        invocation: row.invocation,
                    });
                }
                lines.push(row.line);
            }
        }
        frame.render_widget(Paragraph::new(lines).style(theme.surface.elevated), content);
        *self.rows.borrow_mut() = hits;
        self.animating.set(animates);
    }
}

/// A Section's header: its name, and its count beside it where it knows one.
fn header_line(name: &str, count: Option<usize>, width: u16, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(
        truncate_to_width(name, usize::from(width)),
        theme.text.primary,
    )];
    if let Some(count) = count {
        spans.push(Span::styled(format!(" {count}"), theme.text.subdued));
    }
    Line::from(spans)
}

/// The per-tree subscription as the Aside follows it.
#[derive(Clone, Debug, Default)]
struct TreeFollow {
    /// The Session the subscription whose tree is in hand was asked through.
    through: Option<SessionReference>,
    reading: Option<SubagentTreeReading>,
}

impl TreeFollow {
    /// Whether the tree in hand is the one `open` belongs to.
    fn covers(&self, open: &SessionReference) -> bool {
        self.reading
            .as_ref()
            .is_some_and(|reading| reading.contains(open))
    }
}

/// One tree as the per-tree subscription has described it: its snapshot with
/// every change since applied.
#[derive(Clone, Debug)]
pub(super) struct SubagentTreeReading {
    origin: Outlook,
    top_level: SubagentTreeTopLevel,
    /// Every Subagent, in the order the subscription announced them; the
    /// tree order is read from each entry's parent and spawn order.
    subagents: Vec<SubagentTreeEntry>,
}

/// One Subagent in depth-first order, with what its tree guides need.
pub(super) struct TreeEntry<'a> {
    pub(super) entry: &'a SubagentTreeEntry,
    /// For each level above this entry's own, below the top-level Session:
    /// whether a later sibling still follows at that level.
    continues: Vec<bool>,
    /// Whether this entry is the last its spawner spawned.
    last: bool,
}

impl TreeEntry<'_> {
    /// The guides leading this entry's line: a rule for each level above it
    /// with more to come, and its own branch.
    pub(super) fn guides(&self) -> String {
        let mut guides = String::new();
        for continues in &self.continues {
            guides.push_str(if *continues { "│ " } else { "  " });
        }
        guides.push_str(if self.last { "└ " } else { "├ " });
        guides
    }
}

impl SubagentTreeReading {
    fn new(origin: Outlook, snapshot: SubagentTreeSnapshot) -> Self {
        Self {
            origin,
            top_level: snapshot.top_level,
            subagents: snapshot.subagents,
        }
    }

    pub(super) const fn origin(&self) -> &Outlook {
        &self.origin
    }

    pub(super) const fn top_level(&self) -> &SubagentTreeTopLevel {
        &self.top_level
    }

    pub(super) fn subagent_count(&self) -> usize {
        self.subagents.len()
    }

    /// Whether `reference` names a Session in this tree.
    fn contains(&self, reference: &SessionReference) -> bool {
        reference.origin == self.origin
            && (reference.session_id == self.top_level.session_id
                || self
                    .subagents
                    .iter()
                    .any(|entry| entry.session_id == reference.session_id))
    }

    fn apply(&mut self, change: SubagentTreeChange) {
        match change {
            SubagentTreeChange::SubagentSpawned { entry } => {
                if let Some(held) = self.entry_mut(entry.session_id) {
                    *held = entry;
                } else {
                    self.subagents.push(entry);
                }
            }
            SubagentTreeChange::SubagentSettled {
                session_id,
                status,
                duration_ms,
            } => {
                if let Some(entry) = self.entry_mut(session_id) {
                    entry.status = status;
                    entry.duration_ms = duration_ms;
                }
            }
            SubagentTreeChange::SubagentRetitled {
                session_id,
                name,
                title,
            } => {
                if let Some(entry) = self.entry_mut(session_id) {
                    entry.name = name;
                    entry.title = title;
                }
            }
            SubagentTreeChange::TopLevelRetitled { title } => self.top_level.title = title,
        }
    }

    fn entry_mut(&mut self, session_id: SessionId) -> Option<&mut SubagentTreeEntry> {
        self.subagents
            .iter_mut()
            .find(|entry| entry.session_id == session_id)
    }

    /// Every Subagent depth-first: each after the Session that spawned it,
    /// its own descendants after it, and siblings in spawn order. An entry
    /// never moves because its work settled, since nothing here reads its
    /// status.
    pub(super) fn depth_first(&self) -> Vec<TreeEntry<'_>> {
        let mut children: HashMap<SessionId, Vec<&SubagentTreeEntry>> = HashMap::new();
        for entry in &self.subagents {
            children
                .entry(entry.parent_session_id)
                .or_default()
                .push(entry);
        }
        for siblings in children.values_mut() {
            siblings.sort_by_key(|entry| entry.spawn_order);
        }
        let mut ordered = Vec::with_capacity(self.subagents.len());
        let mut visited = std::collections::HashSet::new();
        // Each frame: the siblings still to visit at one level, and the
        // continuation guides above that level.
        let mut stack: Vec<(std::vec::IntoIter<&SubagentTreeEntry>, Vec<bool>)> = vec![(
            children
                .remove(&self.top_level.session_id)
                .unwrap_or_default()
                .into_iter(),
            Vec::new(),
        )];
        while let Some((siblings, continues)) = stack.last_mut() {
            let Some(entry) = siblings.next() else {
                stack.pop();
                continue;
            };
            let last = siblings.len() == 0;
            let continues = continues.clone();
            ordered.push(TreeEntry {
                entry,
                continues: continues.clone(),
                last,
            });
            if visited.insert(entry.session_id)
                && let Some(below) = children.remove(&entry.session_id)
            {
                let mut deeper = continues;
                deeper.push(!last);
                stack.push((below.into_iter(), deeper));
            }
        }
        ordered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ActivityStatus;

    fn entry(session_id: SessionId, parent: SessionId, spawn_order: u32) -> SubagentTreeEntry {
        SubagentTreeEntry {
            session_id,
            parent_session_id: parent,
            spawn_order,
            name: "Explore".to_owned(),
            title: "Map".to_owned(),
            status: ActivityStatus::Active,
            duration_ms: None,
        }
    }

    #[test]
    fn the_subscription_is_kept_while_the_reader_moves_within_its_tree() {
        let mut aside = Aside::new();
        aside.adopt_settings();
        let top = SessionId::new();
        let child = SessionId::new();
        let reference = |id| SessionReference::new(Outlook::Local, id);
        assert_eq!(
            aside.tree_request(None),
            None,
            "nothing open, nothing to ask"
        );
        assert_eq!(
            aside.tree_request(Some(&reference(child))),
            Some(reference(child)),
            "a tree not yet in hand is asked for through the open Session"
        );

        assert!(aside.receive_tree(
            reference(child),
            SubagentTreeEvent::Snapshot(SubagentTreeSnapshot {
                revision: crate::protocol::SubagentTreeRevision::INITIAL,
                top_level: SubagentTreeTopLevel {
                    session_id: top,
                    title: "Delegate".to_owned(),
                },
                subagents: vec![entry(child, top, 0)],
            }),
            Some(&reference(child)),
        ));
        assert_eq!(
            aside.tree_request(Some(&reference(top))),
            Some(reference(child)),
            "moving to another Session of the tree keeps the subscription it came from"
        );
        let elsewhere = SessionId::new();
        assert_eq!(
            aside.tree_request(Some(&reference(elsewhere))),
            Some(reference(elsewhere)),
            "a Session of another tree asks afresh"
        );
        assert_eq!(
            aside.tree_request(Some(&SessionReference::new(
                Outlook::Remote("studio".to_owned()),
                top
            ))),
            Some(SessionReference::new(
                Outlook::Remote("studio".to_owned()),
                top
            )),
            "the same id on another Server is another tree"
        );

        aside.toggle();
        aside.toggle();
        assert_eq!(
            aside.tree_request(Some(&reference(top))),
            None,
            "a hidden Aside asks for nothing"
        );
    }

    #[test]
    fn a_late_nested_spawn_stands_beneath_its_spawner_with_guides_that_say_so() {
        let top = SessionId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let nested = SessionId::new();
        let mut reading = SubagentTreeReading::new(
            Outlook::Local,
            SubagentTreeSnapshot {
                revision: crate::protocol::SubagentTreeRevision::INITIAL,
                top_level: SubagentTreeTopLevel {
                    session_id: top,
                    title: "Delegate".to_owned(),
                },
                subagents: vec![entry(first, top, 0), entry(second, top, 1)],
            },
        );
        reading.apply(SubagentTreeChange::SubagentSpawned {
            entry: entry(nested, first, 0),
        });

        let ordered = reading.depth_first();
        assert_eq!(
            ordered
                .iter()
                .map(|entry| (entry.entry.session_id, entry.guides()))
                .collect::<Vec<_>>(),
            vec![
                (first, "├ ".to_owned()),
                (nested, "│ └ ".to_owned()),
                (second, "└ ".to_owned()),
            ]
        );
    }
}
