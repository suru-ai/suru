//! The Aside: the collapsible column on the far side of the main view from the
//! Sidebar, answering for the open Session through a stack of Sections.
//!
//! The Aside owns its column chrome (through the shared [`SideColumn`]), the
//! tree the per-tree subscription last delivered, and the geometry the last
//! frame drew its rows at. What each Section says is the Section's own; see
//! [`section`].

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ops::Range,
    time::{Duration, Instant},
};

use ratatui::{
    Frame,
    layout::{Position, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use unicode_width::UnicodeWidthStr;

use crate::managed_client::SubagentTreeEvent;
use crate::protocol::{
    ActivityStatus, AsideVisibility, EffectiveSettings, Outlook, SessionId, SessionReference,
    SessionTimestamp, SubagentTreeChange, SubagentTreeEntry, SubagentTreeSnapshot,
    SubagentTreeTopLevel,
};
use crate::theme::Theme;

use super::{
    commands::SemanticInvocation,
    list_window::{ListWindow, WindowEntry, furthest_opening},
    render::{horizontally_inset, side_column_block},
    shimmer,
    side_column::{Side, SideColumn, ToggleStep},
    slots::truncate_to_width,
};

mod section;
mod subagents;

use section::{
    Section, SectionContext, SectionHeader, SectionRowKey, SectionView, SubagentTreeView,
    built_in_sections,
};

/// How long a tree not yet in hand is drawn blank before it says Loading: the
/// quiet period a Session still opening keeps, so a fast arrival never
/// flashes.
const TREE_LOADING_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone, Debug)]
pub(super) struct Aside {
    column: SideColumn,
    tree: TreeFollow,
    /// Where the last frame drew each pointable row, and what choosing it
    /// does. A frame that drew no Aside leaves nothing to press.
    rows: RefCell<Vec<AsideRowHit>>,
    /// Whether the last frame drew live presentation in the Aside.
    animating: Cell<bool>,
    /// The tree the Aside has been waiting on since a frame first wanted it,
    /// named by the Session it is asked through, and the moment it may say
    /// Loading. Only a frame can start the wait, because the wait is about
    /// what the reader has been looking at.
    waiting: RefCell<Option<(SessionReference, Instant)>>,
    /// The entry the keys stand on while the Aside holds them: named by its
    /// Section and key, so it follows the entry however the rows around it
    /// move, and by where it stood, so a vanished entry hands focus to the
    /// nearest one left. Resolved again by every frame, hence the cell.
    focus: RefCell<Option<AsideFocus>>,
    /// Each Section's scroll: the window its entries are read through, and
    /// the open Session's entry as the last frame drew it.
    scrolls: RefCell<HashMap<&'static str, SectionScroll>>,
    /// Whether a press has landed on the Aside since the last frame. What it
    /// opens was already in view, so it moves no window.
    pressed: Cell<bool>,
    /// Where the last frame drew each Section's rows, for the wheel.
    drawn_sections: RefCell<Vec<DrawnSection>>,
    /// The Session the Aside stands in a top-level entry for, drawn from what
    /// the client already knows, while no tree of it is in hand: the
    /// Provisional Session, then the real Session that answers it until its
    /// tree lands. It is why the Aside appears once, at submission, rather
    /// than blanking when the Session arrives.
    stand_in: Option<StandIn>,
}

#[derive(Clone, Debug)]
struct AsideFocus {
    section: &'static str,
    key: SectionRowKey,
    /// Where the entry stood in the Aside's focus order.
    position: usize,
}

/// One Section's window, which holds where it stands — the wheel's doing
/// included, because wheeling is looking rather than choosing — until row
/// focus walks within the Section or another Session opens and carries its
/// entry into view.
#[derive(Clone, Debug, Default)]
struct SectionScroll {
    window: ListWindow,
    /// The open Session's entry as the last frame drew it, so the next can
    /// tell another Session opening.
    current: Option<SectionRowKey>,
}

#[derive(Clone, Debug)]
struct DrawnSection {
    name: &'static str,
    /// The screen rows the Section's window was drawn across.
    rows: Range<u16>,
    /// The furthest entry the window can begin at: the first from which the
    /// rest of the Section fits the window's lines.
    furthest: usize,
}

/// One entry row focus can stand on, in the order the Aside draws them.
#[derive(Clone, Debug)]
pub(super) struct FocusEntry {
    section: &'static str,
    key: SectionRowKey,
    invocation: Option<SemanticInvocation>,
    /// Whether this is the open Session's own entry.
    current: bool,
}

/// A top-level entry the client draws before the Server has said anything
/// about its tree.
#[derive(Clone, Debug)]
struct StandIn {
    session: SessionReference,
    title: String,
    working: bool,
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
            waiting: RefCell::new(None),
            focus: RefCell::new(None),
            scrolls: RefCell::new(HashMap::new()),
            pressed: Cell::new(false),
            drawn_sections: RefCell::new(Vec::new()),
            stand_in: None,
        }
    }

    /// Stands a top-level entry in for `session` until its tree is in hand:
    /// its Title, and whether it is Working as far as the client can say.
    /// Calling it again for the same Session updates what the entry says.
    pub(super) fn stand_in_for(&mut self, session: SessionReference, title: String, working: bool) {
        self.stand_in = Some(StandIn {
            session,
            title,
            working,
        });
    }

    /// Gives up the stand-in entry, as a Provisional Session given up does.
    pub(super) fn drop_stand_in(&mut self) {
        self.stand_in = None;
    }

    /// The stand-in entry for `open`, where the Aside draws one.
    fn stand_in_of(&self, open: &SessionReference) -> Option<&StandIn> {
        self.stand_in
            .as_ref()
            .filter(|stand_in| stand_in.session == *open)
    }

    /// Takes the launch visibility and width from the first Settings
    /// snapshot; later snapshots leave the reader's own showing, hiding and
    /// dragging alone. The reveal takes no keys: a reader who has not touched
    /// the Aside is typing their first Prompt.
    pub(super) fn adopt_settings(&mut self, settings: &EffectiveSettings) {
        if self.column.seed(settings.aside.initial_width) {
            self.column
                .set_revealed(settings.aside.initial_visibility == AsideVisibility::Shown);
        }
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
    ///
    /// Where the Aside is not `present` — on the Landing, with no Session for
    /// it to answer for — there is nothing to hold the keys, so the act only
    /// flips whether the reader wants it shown, and leaves no claim behind
    /// to take the keys once a Session opens.
    pub(super) fn toggle(&mut self, present: bool) -> bool {
        if !present {
            let revealed = self.column.is_revealed();
            self.column.set_revealed(!revealed);
            self.hand_back_keys();
            return false;
        }
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

    /// Hands the keys back, and row focus with them: it says what Enter would
    /// act on, and nothing here answers to Enter any more.
    pub(super) fn hand_back_keys(&mut self) {
        self.column.hand_back_keys();
        self.focus.replace(None);
    }

    /// Puts row focus where the reader is — on the open Session's entry, or
    /// the first entry where none stands for it — as the Aside takes the
    /// keys.
    pub(super) fn seed_focus(&mut self, entries: &[FocusEntry]) {
        let seeded = entries
            .iter()
            .position(|entry| entry.current)
            .or((!entries.is_empty()).then_some(0))
            .map(|position| focus_at(entries, position));
        self.focus.replace(seeded);
        self.reveal_focus();
    }

    /// Asks the window of the Section row focus stands in to carry it into
    /// view, which the keys moving it do.
    fn reveal_focus(&self) {
        let Some(section) = self.focus.borrow().as_ref().map(|focus| focus.section) else {
            return;
        };
        self.scrolls
            .borrow_mut()
            .entry(section)
            .or_default()
            .window
            .reveal();
    }

    /// Walks row focus one entry, wrapping past either end.
    pub(super) fn move_focus(&mut self, entries: &[FocusEntry], forward: bool) {
        if entries.is_empty() {
            return;
        }
        let position = match self.resolve_focus(entries) {
            Some(position) if forward => (position + 1) % entries.len(),
            Some(position) => (position + entries.len() - 1) % entries.len(),
            None => entries.iter().position(|entry| entry.current).unwrap_or(0),
        };
        self.focus.replace(Some(focus_at(entries, position)));
        self.reveal_focus();
    }

    /// What Enter on the focused entry does, if anything.
    pub(super) fn focused_invocation(&self, entries: &[FocusEntry]) -> Option<SemanticInvocation> {
        let position = self.resolve_focus(entries)?;
        entries[position].invocation.clone()
    }

    /// Finds the focused entry in `entries`: the same entry where it
    /// survives, the nearest one left where it does not. Focus with nothing
    /// to stand on — a tree still arriving — is kept for when it arrives.
    fn resolve_focus(&self, entries: &[FocusEntry]) -> Option<usize> {
        let mut focus = self.focus.borrow_mut();
        let held = focus.as_ref()?;
        if let Some(position) = entries
            .iter()
            .position(|entry| entry.section == held.section && entry.key == held.key)
        {
            focus.as_mut().expect("focus is held").position = position;
            return Some(position);
        }
        if entries.is_empty() {
            return None;
        }
        let position = held.position.min(entries.len() - 1);
        *focus = Some(focus_at(entries, position));
        Some(position)
    }

    /// Every entry row focus can stand on, as the Sections say for `open`.
    pub(super) fn focus_entries(
        &self,
        open: &SessionReference,
        presentation: AsidePresentation<'_>,
    ) -> Vec<FocusEntry> {
        let width = self.column.drawn_area().map_or(0, |area| area.width);
        self.views(open, presentation, width)
            .into_iter()
            .filter_map(|(section, view)| Some((section.name(), view.ok()?)))
            .flat_map(|(section, view)| {
                let current = view.current;
                view.rows
                    .into_iter()
                    .enumerate()
                    .filter_map(move |(index, row)| {
                        Some(FocusEntry {
                            section,
                            key: row.key?,
                            invocation: row.invocation,
                            current: current == Some(index),
                        })
                    })
            })
            .collect()
    }

    fn views(
        &self,
        open: &SessionReference,
        presentation: AsidePresentation<'_>,
        width: u16,
    ) -> Vec<(&'static dyn Section, Result<SectionView, String>)> {
        let context = SectionContext {
            open,
            subagent_tree: self.tree_view(open, presentation.now),
            width,
            theme: presentation.theme,
            spinner_frame: presentation.spinner_frame,
            shimmer: presentation.shimmer,
            truecolor: presentation.truecolor,
            now: presentation.session_now,
        };
        built_in_sections()
            .into_iter()
            .map(|section| (section, section.view(&context)))
            .collect()
    }

    /// Moves the window of the Section under the pointer by the wheel's
    /// step of `rows` — whole entries, however many lines each takes —
    /// answering whether the pointer stood over the Aside at all: wheeling
    /// over it never reaches the Transcript. It takes neither the keys nor
    /// row focus.
    pub(super) fn wheel_at(&self, position: Position, scrolling_down: bool, rows: usize) -> bool {
        let Some(area) = self.column.drawn_area() else {
            return false;
        };
        if !area.contains(position) {
            return false;
        }
        let drawn = self.drawn_sections.borrow();
        let Some(section) = drawn
            .iter()
            .find(|section| section.rows.contains(&position.y))
            .or_else(|| drawn.first())
        else {
            return true;
        };
        let mut scrolls = self.scrolls.borrow_mut();
        let window = &scrolls.entry(section.name).or_default().window;
        let first = window.first();
        let moved = if scrolling_down {
            first.saturating_add(rows).min(section.furthest)
        } else {
            first.saturating_sub(rows)
        };
        if moved != first {
            window.scroll_to(moved);
        }
        true
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
        self.pressed.set(true);
        self.rows
            .borrow()
            .iter()
            .find(|hit| hit.row == position.y && hit.columns.contains(&position.x))
            .and_then(|hit| hit.invocation.clone())
            .map_or(AsidePress::Inert, AsidePress::Invoke)
    }

    /// The Session the per-tree subscription should be asked through, or
    /// `None` when no Session is open. Moving between Sessions of the tree
    /// already in hand keeps the subscription it came from, so the Section
    /// stands as it is.
    ///
    /// The tree is followed whether or not the Aside is shown, because the
    /// Sidebar reads the top-level Session it heads from it too (see
    /// [`Self::top_level_of`]).
    ///
    /// A tree whose subscription ended — it could not be read, or it was
    /// deleted — is not asked for again until a Session of it is next opened
    /// (see [`Self::note_opened`]), so the run loop lets the ended
    /// subscription go and starts a fresh one then.
    pub(super) fn tree_request(&self, open: Option<&SessionReference>) -> Option<SessionReference> {
        let open = open?;
        if self.tree.ended_for(open).is_some() {
            return None;
        }
        Some(self.tree.wanted(open))
    }

    /// The top-level Session heading the tree `open` belongs to, where a tree
    /// the per-tree subscription delivered says — including one whose
    /// subscription has since failed, which still names it. `None` while no
    /// such tree is in hand.
    pub(super) fn top_level_of(&self, open: &SessionReference) -> Option<SessionReference> {
        let reading = self
            .tree_for(open)
            .or_else(|| self.tree.ended_for(open).and_then(|end| end.tree.as_ref()))?;
        Some(SessionReference::new(
            reading.origin().clone(),
            reading.top_level().session_id,
        ))
    }

    /// Notes that the reader opened `opened`. Opening a Session of a tree
    /// whose subscription ended is what asks for that tree again.
    pub(super) fn note_opened(&mut self, opened: &SessionReference) {
        if self.tree.ended_for(opened).is_some() {
            self.tree.ended = None;
        }
        if self.stand_in_of(opened).is_none() {
            self.stand_in = None;
        }
    }

    /// When the tree the open Session wants may first say Loading, while that
    /// moment is still ahead: the one wakeup the run loop owes the Aside.
    pub(super) fn loading_deadline(
        &self,
        open: Option<&SessionReference>,
        now: Instant,
    ) -> Option<Instant> {
        // Only a shown Aside says Loading, so only it is owed the wakeup.
        if !self.column.is_revealed() {
            return None;
        }
        let wanted = self.tree_request(open)?;
        // A stand-in entry never says Loading: the tree replaces it in place.
        if self.tree.covers(open?) || self.stand_in_of(open?).is_some() {
            return None;
        }
        self.waiting
            .borrow()
            .as_ref()
            .filter(|(waited_for, deadline)| *waited_for == wanted && *deadline > now)
            .map(|(_, deadline)| *deadline)
    }

    /// What the Aside knows of the tree `open` belongs to, as of `now`. A
    /// tree not yet in hand starts its quiet period the first time a frame
    /// asks.
    fn tree_view(&self, open: &SessionReference, now: Instant) -> SubagentTreeView<'_> {
        if let Some(end) = self.tree.ended_for(open) {
            return end
                .failure
                .as_deref()
                .map_or(SubagentTreeView::Gone, SubagentTreeView::Failed);
        }
        if let Some(reading) = self.tree_for(open) {
            return SubagentTreeView::Ready(reading);
        }
        if let Some(stand_in) = self.stand_in_of(open) {
            return SubagentTreeView::StandIn {
                title: &stand_in.title,
                working: stand_in.working,
            };
        }
        let wanted = self.tree.wanted(open);
        let mut waiting = self.waiting.borrow_mut();
        let deadline = match waiting.as_ref() {
            Some((waited_for, deadline)) if *waited_for == wanted => *deadline,
            _ => {
                let deadline = now
                    .checked_add(TREE_LOADING_DELAY)
                    .expect("the tree loading delay fits a monotonic clock");
                *waiting = Some((wanted, deadline));
                deadline
            }
        };
        SubagentTreeView::Arriving {
            loading: now >= deadline,
        }
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
                // The tree has landed, replacing any stand-in in place.
                self.stand_in = None;
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
            // The subscription has ended. What it was answering for — the
            // tree in hand, or the Session it was asked through — says so
            // until one of its Sessions is next opened.
            SubagentTreeEvent::Failed(message) => {
                self.end_tree(through, Some(message));
                true
            }
            // A deleted tree is dropped without complaint: deleting the open
            // top-level Session has already moved the view away.
            SubagentTreeEvent::Deleted => {
                self.end_tree(through, None);
                true
            }
        }
    }

    fn end_tree(&mut self, through: SessionReference, failure: Option<String>) {
        let tree = if self.tree.through.as_ref() == Some(&through) {
            self.tree.through = None;
            self.tree.reading.take()
        } else {
            None
        };
        self.tree.ended = Some(TreeEnd {
            through,
            tree,
            failure,
        });
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
        presentation: AsidePresentation<'_>,
    ) {
        let theme = presentation.theme;
        let block = side_column_block(&self.column, owns_input, theme);
        let inside = block.inner(area);
        let content = horizontally_inset(inside, 1);
        frame.render_widget(block, area);
        let views = self.views(open, presentation, content.width);
        // Row focus is resolved against every entry the Sections offer, not
        // only the ones the windows show, so it survives scrolling out of
        // view and the tree moving beneath it.
        let focused = owns_input
            .then(|| {
                let entries = views
                    .iter()
                    .filter_map(|(section, view)| Some((section.name(), view.as_ref().ok()?)))
                    .flat_map(|(section, view)| {
                        view.rows.iter().filter_map(move |row| {
                            Some(FocusEntry {
                                section,
                                key: row.key.clone()?,
                                invocation: None,
                                current: false,
                            })
                        })
                    })
                    .collect::<Vec<_>>();
                let position = self.resolve_focus(&entries)?;
                let entry = entries.into_iter().nth(position)?;
                Some((entry.section, entry.key))
            })
            .flatten();
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut hits = Vec::new();
        let mut drawn_sections = Vec::new();
        let mut animates = false;
        let mut scrolls = self.scrolls.borrow_mut();
        let pressed = self.pressed.take();
        for (section, view) in views {
            let view = match view {
                Ok(view) => view,
                Err(message) => {
                    // A failing Section is drawn as failed, in its own place,
                    // and the Sections around it go on as they were.
                    let header = SectionHeader {
                        name: section.name(),
                        count: None,
                        working: None,
                    };
                    lines.push(header_line(&header, content.width, theme));
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
            lines.push(header_line(&header, content.width, theme));
            // The window is anchored on the focused entry while the keys stand
            // on one here, and on the open Session's entry otherwise. It is
            // carried when the keys move focus or another Session opens, a
            // whole entry at a time however many lines each takes, and only
            // as far as keeps two entries beyond the anchor in view.
            let room = usize::from(content.height).saturating_sub(lines.len());
            let entries = rows
                .iter()
                .map(|row| WindowEntry {
                    rows: row.lines.len(),
                    focusable: row.key.is_some(),
                })
                .collect::<Vec<_>>();
            let anchor = focused
                .as_ref()
                .filter(|(focused_section, _)| *focused_section == section.name())
                .and_then(|(_, key)| rows.iter().position(|row| row.key.as_ref() == Some(key)))
                .or(current);
            let scroll = scrolls.entry(section.name()).or_default();
            let current = current.and_then(|index| rows[index].key.clone());
            let opened = scroll.current != current;
            scroll.current = current;
            if pressed {
                scroll.window.hold();
            } else if opened {
                scroll.window.reveal();
            }
            let offset = scroll.window.settle(&entries, room, anchor).start;
            let first_row = content
                .y
                .saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
            let mut left = room;
            for (index, row) in rows.into_iter().enumerate().skip(offset) {
                // A row is never cut, as the Sidebar never cuts one, unless
                // it heads the window and the window is shorter than it.
                if left == 0 || (row.lines.len() > left && index > offset) {
                    break;
                }
                let is_focused = focused.as_ref().is_some_and(|(focused_section, key)| {
                    *focused_section == section.name() && row.key.as_ref() == Some(key)
                });
                for line in row.lines.into_iter().take(left) {
                    let y = content
                        .y
                        .saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
                    hits.push(AsideRowHit {
                        row: y,
                        columns: inside.x..inside.right(),
                        invocation: row.invocation.clone(),
                    });
                    lines.push(if is_focused {
                        focus_painted(line, content.width, theme)
                    } else {
                        line
                    });
                    left -= 1;
                }
            }
            let last_row = content
                .y
                .saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
            drawn_sections.push(DrawnSection {
                name: section.name(),
                rows: first_row..last_row.max(first_row.saturating_add(1)),
                furthest: furthest_opening(&entries, room),
            });
        }
        drop(scrolls);
        *self.drawn_sections.borrow_mut() = drawn_sections;
        frame.render_widget(Paragraph::new(lines).style(theme.surface.elevated), content);
        *self.rows.borrow_mut() = hits;
        self.animating.set(animates);
    }
}

/// What the Aside draws with beyond its own state: the Theme and the run
/// loop's presentation clock, frame, and colour depth.
#[derive(Clone, Copy)]
pub(super) struct AsidePresentation<'a> {
    pub(super) theme: &'a Theme,
    pub(super) spinner_frame: usize,
    pub(super) shimmer: &'a shimmer::Clock,
    pub(super) truecolor: bool,
    pub(super) now: Instant,
    /// The moment now on the clock the Server's timestamps are read against,
    /// for ticking how long live work has been running.
    pub(super) session_now: SessionTimestamp,
}

fn focus_at(entries: &[FocusEntry], position: usize) -> AsideFocus {
    AsideFocus {
        section: entries[position].section,
        key: entries[position].key.clone(),
        position,
    }
}

/// A row painted with row focus: the focus block the whole width across,
/// each span keeping its own colour except where that colour would vanish
/// into the block, as the Sidebar paints its row focus.
fn focus_painted(line: Line<'static>, width: u16, theme: &Theme) -> Line<'static> {
    let focused = theme.selection.focused;
    let mut spans = line
        .spans
        .into_iter()
        .map(|mut span| {
            let vanishes = span.style.fg.is_none() || span.style.fg == focused.bg;
            span.style = span.style.bg(focused.bg.unwrap_or_default());
            if vanishes {
                span.style = span.style.fg(focused.fg.unwrap_or_default());
            }
            span
        })
        .collect::<Vec<_>>();
    let drawn: usize = spans.iter().map(|span| span.content.width()).sum();
    let gap = usize::from(width).saturating_sub(drawn);
    if gap > 0 {
        spans.push(Span::styled(" ".repeat(gap), focused));
    }
    Line::from(spans)
}

/// A Section's header: its name, its count beside it where it knows one,
/// and how many of those are working, in the working Marker's colour.
fn header_line(header: &SectionHeader, width: u16, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(
        truncate_to_width(header.name, usize::from(width)),
        theme.text.primary,
    )];
    if let Some(count) = header.count {
        spans.push(Span::styled(format!(" {count}"), theme.text.subdued));
    }
    if let Some(working) = header.working {
        spans.push(Span::styled(
            format!(" ({working} active)"),
            theme.accent.primary,
        ));
    }
    Line::from(spans)
}

/// The per-tree subscription as the Aside follows it.
#[derive(Clone, Debug, Default)]
struct TreeFollow {
    /// The Session the subscription whose tree is in hand was asked through.
    through: Option<SessionReference>,
    reading: Option<SubagentTreeReading>,
    /// The subscription that ended without a tree to go on showing.
    ended: Option<TreeEnd>,
}

/// A subscription that ended, and what it answered for.
#[derive(Clone, Debug)]
struct TreeEnd {
    through: SessionReference,
    /// The tree it had delivered, which names the Sessions the end stands for.
    tree: Option<SubagentTreeReading>,
    /// Why the tree could not be read, or `None` for a tree deleted.
    failure: Option<String>,
}

impl TreeFollow {
    /// The Session the tree `open` belongs to is asked through: the
    /// subscription already delivering it, or `open` itself.
    fn wanted(&self, open: &SessionReference) -> SessionReference {
        match &self.through {
            Some(through) if self.covers(open) => through.clone(),
            _ => open.clone(),
        }
    }

    /// The ended subscription `open` belongs to, if one does.
    fn ended_for(&self, open: &SessionReference) -> Option<&TreeEnd> {
        self.ended.as_ref().filter(|end| {
            end.through == *open || end.tree.as_ref().is_some_and(|tree| tree.contains(open))
        })
    }

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
    /// tree order is read from each entry's parent, status, and spawn order.
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
    /// Whether this entry spawned Subagents of its own, which hang beneath
    /// its lines.
    spawned: bool,
}

impl TreeEntry<'_> {
    /// The guides leading this entry's first line: a rule for each level
    /// above it with more to come, and its own branch.
    pub(super) fn guides(&self) -> String {
        let mut guides = String::new();
        for continues in &self.continues {
            guides.push_str(if *continues { "│ " } else { "  " });
        }
        guides.push_str(if self.last { "└ " } else { "├ " });
        guides
    }

    /// The guides leading the lines beneath the entry's first: the rules
    /// above it as its first line draws them, its own branch's rule carried
    /// on to the sibling that follows it, and — in the column its Marker
    /// took — the rule its own Subagents hang from, where it spawned any.
    pub(super) fn continuation_guides(&self) -> String {
        let mut guides = String::new();
        for continues in &self.continues {
            guides.push_str(if *continues { "│ " } else { "  " });
        }
        guides.push_str(if self.last { "  " } else { "│ " });
        guides.push_str(if self.spawned { "│ " } else { "  " });
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
            SubagentTreeChange::SubagentWorkingChanged {
                session_id,
                status,
                worked_ms,
                working_since,
                monitoring_since,
            } => {
                if let Some(entry) = self.entry_mut(session_id) {
                    entry.status = status;
                    entry.worked_ms = worked_ms;
                    entry.working_since = working_since;
                    entry.monitoring_since = monitoring_since;
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
            SubagentTreeChange::SubagentModelChanged { session_id, model } => {
                if let Some(entry) = self.entry_mut(session_id) {
                    entry.model = Some(model);
                }
            }
            SubagentTreeChange::TopLevelRetitled { title } => self.top_level.title = title,
            SubagentTreeChange::TopLevelWorkingChanged {
                working_since,
                monitoring_since,
            } => {
                self.top_level.working_since = working_since;
                self.top_level.monitoring_since = monitoring_since;
            }
            SubagentTreeChange::NeedsInterventionChanged {
                session_id,
                needs_intervention,
            } => {
                if session_id == self.top_level.session_id {
                    self.top_level.needs_intervention = needs_intervention;
                } else if let Some(entry) = self.entry_mut(session_id) {
                    entry.needs_intervention = needs_intervention;
                }
            }
            // The managed client ends a deleted tree's subscription with
            // `SubagentTreeEvent::Deleted` rather than forwarding this.
            SubagentTreeChange::TreeDeleted => {}
        }
    }

    fn entry_mut(&mut self, session_id: SessionId) -> Option<&mut SubagentTreeEntry> {
        self.subagents
            .iter_mut()
            .find(|entry| entry.session_id == session_id)
    }

    /// How many Subagents, at any depth, are working now.
    pub(super) fn working_count(&self) -> usize {
        self.subagents
            .iter()
            .filter(|entry| entry.status == ActivityStatus::Active)
            .count()
    }

    /// The Sessions whose branch holds work still going: every working
    /// Subagent, and each Session above it.
    fn live_branches(&self) -> HashSet<SessionId> {
        let parents = self
            .subagents
            .iter()
            .map(|entry| (entry.session_id, entry.parent_session_id))
            .collect::<HashMap<_, _>>();
        let mut live = HashSet::new();
        for entry in &self.subagents {
            if entry.status != ActivityStatus::Active {
                continue;
            }
            // A Session already marked had the Sessions above it marked with
            // it, which also ends a walk that runs round a cycle.
            let mut session = entry.session_id;
            while live.insert(session) {
                let Some(parent) = parents.get(&session) else {
                    break;
                };
                session = *parent;
            }
        }
        live
    }

    /// Every Subagent depth-first: each after the Session that spawned it,
    /// its own descendants after it. Among siblings, a branch with work still
    /// going anywhere in it comes before the settled ones, and within each
    /// the most recently spawned comes first, so a branch moves down when
    /// the last work in it settles and back up when any of it works again.
    pub(super) fn depth_first(&self) -> Vec<TreeEntry<'_>> {
        let live = self.live_branches();
        let mut children: HashMap<SessionId, Vec<&SubagentTreeEntry>> = HashMap::new();
        for entry in &self.subagents {
            children
                .entry(entry.parent_session_id)
                .or_default()
                .push(entry);
        }
        for siblings in children.values_mut() {
            siblings.sort_by_key(|entry| {
                (
                    std::cmp::Reverse(live.contains(&entry.session_id)),
                    std::cmp::Reverse(entry.spawn_order),
                )
            });
        }
        let mut ordered = Vec::with_capacity(self.subagents.len());
        let mut visited = HashSet::new();
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
                spawned: children.contains_key(&entry.session_id),
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

    fn entry(session_id: SessionId, parent: SessionId, spawn_order: u32) -> SubagentTreeEntry {
        SubagentTreeEntry {
            session_id,
            parent_session_id: parent,
            spawn_order,
            name: "Explore".to_owned(),
            title: "Map".to_owned(),
            model: None,
            status: ActivityStatus::Active,
            worked_ms: Some(0),
            working_since: None,
            monitoring_since: None,
            needs_intervention: false,
        }
    }

    #[test]
    fn the_subscription_is_kept_while_the_reader_moves_within_its_tree() {
        let mut aside = Aside::new();
        aside.adopt_settings(&EffectiveSettings::default());
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
                    working_since: None,
                    monitoring_since: None,
                    needs_intervention: false,
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

        aside.toggle(true);
        aside.toggle(true);
        assert_eq!(
            aside.tree_request(Some(&reference(top))),
            Some(reference(child)),
            "a hidden Aside still follows the tree, which the Sidebar reads too"
        );
        assert_eq!(
            aside.top_level_of(&reference(child)),
            Some(reference(top)),
            "and names its top-level Session"
        );
    }

    #[test]
    fn an_ended_subscription_is_let_go_and_asked_for_again_when_its_tree_is_next_opened() {
        let mut aside = Aside::new();
        aside.adopt_settings(&EffectiveSettings::default());
        let top = SessionId::new();
        let child = SessionId::new();
        let reference = |id| SessionReference::new(Outlook::Local, id);
        let snapshot = SubagentTreeSnapshot {
            revision: crate::protocol::SubagentTreeRevision::INITIAL,
            top_level: SubagentTreeTopLevel {
                session_id: top,
                title: "Delegate".to_owned(),
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
            },
            subagents: vec![entry(child, top, 0)],
        };
        let open = reference(top);
        aside.receive_tree(
            open.clone(),
            SubagentTreeEvent::Snapshot(snapshot),
            Some(&open),
        );
        aside.receive_tree(
            open.clone(),
            SubagentTreeEvent::Failed("revoked".to_owned()),
            Some(&open),
        );
        assert_eq!(
            aside.tree_request(Some(&open)),
            None,
            "the run loop lets the ended subscription go"
        );
        assert_eq!(
            aside.tree_request(Some(&reference(child))),
            None,
            "for every Session of the tree it was delivering"
        );

        aside.note_opened(&reference(child));
        assert_eq!(
            aside.tree_request(Some(&reference(child))),
            Some(reference(child)),
            "opening a Session of the tree asks for it afresh"
        );

        aside.receive_tree(
            reference(child),
            SubagentTreeEvent::Deleted,
            Some(&reference(child)),
        );
        assert_eq!(aside.tree_request(Some(&reference(child))), None);
        aside.note_opened(&reference(SessionId::new()));
        assert_eq!(
            aside.tree_request(Some(&reference(child))),
            None,
            "opening another tree's Session asks nothing of this one"
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
                    working_since: None,
                    monitoring_since: None,
                    needs_intervention: false,
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
                (second, "├ ".to_owned()),
                (first, "└ ".to_owned()),
                (nested, "  └ ".to_owned()),
            ]
        );
        assert_eq!(
            ordered
                .iter()
                .map(|entry| entry.continuation_guides())
                .collect::<Vec<_>>(),
            vec!["│   ".to_owned(), "  │ ".to_owned(), "      ".to_owned()],
            "beneath its first line an entry carries its branch's rule on to the sibling \
             after it, and hangs the rule its own spawns branch from where its Marker stood"
        );
    }

    #[test]
    fn siblings_run_working_branches_first_then_newest_spawn_first() {
        let top = SessionId::new();
        let [oldest, settled_parent, working, newest, deep] =
            std::array::from_fn(|_| SessionId::new());
        let settled = |mut entry: SubagentTreeEntry| {
            entry.status = ActivityStatus::Completed;
            entry
        };
        let reading = SubagentTreeReading::new(
            Outlook::Local,
            SubagentTreeSnapshot {
                revision: crate::protocol::SubagentTreeRevision::INITIAL,
                top_level: SubagentTreeTopLevel {
                    session_id: top,
                    title: "Delegate".to_owned(),
                    working_since: None,
                    monitoring_since: None,
                    needs_intervention: false,
                },
                subagents: vec![
                    settled(entry(oldest, top, 0)),
                    settled(entry(settled_parent, top, 1)),
                    entry(working, top, 2),
                    settled(entry(newest, top, 3)),
                    entry(deep, settled_parent, 0),
                ],
            },
        );

        assert_eq!(
            reading
                .depth_first()
                .iter()
                .map(|entry| entry.entry.session_id)
                .collect::<Vec<_>>(),
            vec![working, settled_parent, deep, newest, oldest],
            "a branch with work still going anywhere in it stands before the settled \
             ones, the most recently spawned first within each"
        );
    }
}
