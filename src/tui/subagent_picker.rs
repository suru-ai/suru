//! The Subagent Picker: the list a reader docks over the composer to browse
//! the open Session's working Subagents, drawn as the tree they spawned in.
//!
//! The picker owns no listing of its own: its entries are read live from the
//! open Session's snapshot, the same rows the Transcript draws, so a Subagent
//! spawning or settling moves the picker the moment the session stream says
//! so. All the picker holds is that it is open, the entry the reader is on,
//! and the geometry the frame in force drew so the pointer can answer a row.

use std::cell::RefCell;
use std::ops::Range;

use ratatui::layout::Position;

use crate::protocol::{
    Activity, ActivityStatus, ModelId, SessionId, SessionReference, SessionSnapshot,
};

use super::list_window::ListWindow;

/// One working Subagent on offer: the child Session its entry opens, the name
/// and description its row is drawn from, and whether Suru spawned it through
/// the Broker, which lets it be stopped whatever the open Session's Provider
/// allows.
#[derive(Clone, Copy, Debug)]
pub(super) struct WorkingSubagent<'a> {
    pub(super) session_id: SessionId,
    pub(super) name: &'a str,
    pub(super) pending_questionnaires: usize,
    pub(super) pending_approvals: usize,
    pub(super) description: &'a str,
    pub(super) model: Option<&'a ModelId>,
    pub(super) brokered: bool,
}

/// The open Session's working Subagents, in the order they spawned — the tree
/// the picker draws. A settled Subagent is not the picker's concern: its row
/// in the Transcript is the way back into it. A resumed Subagent works only
/// through the row of its latest stretch — the rows before it have settled,
/// and no resume begins while one is open — so it is offered once, by that
/// row, and stopping it stops that stretch.
pub(super) fn working_subagents(snapshot: &SessionSnapshot) -> Vec<WorkingSubagent<'_>> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Subagent {
                status: ActivityStatus::Active,
                name,
                description,
                model,
                session_id,
                brokered,
                ..
            } => Some(WorkingSubagent {
                pending_questionnaires: snapshot.pending_questionnaires_in_subagent(*session_id),
                pending_approvals: snapshot.pending_approvals_in_subagent(*session_id),
                session_id: *session_id,
                name,
                description,
                model: model.as_ref(),
                brokered: *brokered,
            }),
            _ => None,
        })
        .collect()
}

#[derive(Clone, Debug, Default)]
pub(super) struct SubagentPicker {
    open: bool,
    /// The entry the reader stands on, by the Session it opens — on the open
    /// Session's Server, or on a Remote of it.
    selected: Option<SessionReference>,
    /// Where the frame in force drew each entry, recorded at draw time and
    /// resolved against a press, so the pointer answers the rows the reader
    /// can see rather than the entries the state would draw next.
    geometry: RefCell<Vec<SubagentPickerSpan>>,
    window: ListWindow,
}

#[derive(Clone, Debug)]
struct SubagentPickerSpan {
    row: u16,
    columns: Range<u16>,
    session: SessionReference,
}

impl SubagentPicker {
    /// Opens over the working Subagents on offer, standing on the first. With
    /// nothing to browse the picker stays closed, which is what keeps the key
    /// that asks for it inert.
    pub(super) fn open_over(&mut self, working: &[SessionReference]) {
        let Some(first) = working.first() else {
            return;
        };
        self.open = true;
        self.selected = Some(first.clone());
        self.window.open();
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.selected = None;
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn selected(&self) -> Option<&SessionReference> {
        self.selected.as_ref()
    }

    /// Keeps the picker true to the working Subagents still on offer: the
    /// selection moves off an entry that settled, and a picker with nothing
    /// left to browse closes rather than standing over nothing.
    pub(super) fn reconcile(&mut self, working: &[SessionReference]) {
        if !self.open {
            return;
        }
        if working.is_empty() {
            self.close();
            return;
        }
        if !self
            .selected
            .as_ref()
            .is_some_and(|selected| working.contains(selected))
        {
            self.selected = working.first().cloned();
        }
    }

    pub(super) fn move_selection(&mut self, working: &[SessionReference], distance: isize) {
        // An open picker always has entries — reconciling closes it over
        // none — so an empty offering leaves the selection alone.
        if working.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| working.iter().position(|entry| entry == selected))
            .unwrap_or(0);
        let len = working.len() as isize;
        let next = (current as isize + distance).rem_euclid(len) as usize;
        self.selected = Some(working[next].clone());
        self.window.reveal();
    }

    /// The window over the working Subagents as the frame draws them.
    pub(super) fn window(&self) -> &ListWindow {
        &self.window
    }

    /// Gives up the last frame's geometry, called as every frame begins, so a
    /// press resolves only against rows actually on screen.
    pub(super) fn forget_frame(&self) {
        self.geometry.borrow_mut().clear();
    }

    /// Records where the frame in force drew one entry's row.
    pub(super) fn record_row(&self, row: u16, columns: Range<u16>, session: SessionReference) {
        self.geometry.borrow_mut().push(SubagentPickerSpan {
            row,
            columns,
            session,
        });
    }

    /// The Subagent whose row the reader pressed, or `None` for a press
    /// outside every row — which is how a reader puts the picker away.
    pub(super) fn hit(&self, position: Position) -> Option<SessionReference> {
        self.geometry
            .borrow()
            .iter()
            .find(|span| span.row == position.y && span.columns.contains(&position.x))
            .map(|span| span.session.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Outlook;

    fn entry() -> SessionReference {
        SessionReference::new(Outlook::Local, SessionId::new())
    }

    #[test]
    fn opening_over_nothing_stays_closed() {
        let mut picker = SubagentPicker::default();
        picker.open_over(&[]);
        assert!(!picker.is_open());
        assert_eq!(picker.selected(), None);
    }

    #[test]
    fn opening_stands_on_the_first_entry() {
        let working = [entry(), entry()];
        let mut picker = SubagentPicker::default();
        picker.open_over(&working);
        assert!(picker.is_open());
        assert_eq!(picker.selected(), Some(&working[0]));
    }

    #[test]
    fn the_selection_wraps_both_ways() {
        let working = [entry(), entry(), entry()];
        let mut picker = SubagentPicker::default();
        picker.open_over(&working);
        picker.move_selection(&working, -1);
        assert_eq!(picker.selected(), Some(&working[2]));
        picker.move_selection(&working, 1);
        assert_eq!(picker.selected(), Some(&working[0]));
    }

    #[test]
    fn reconciling_moves_off_a_settled_entry_and_closes_over_nothing() {
        let working = [entry(), entry()];
        let mut picker = SubagentPicker::default();
        picker.open_over(&working);
        picker.move_selection(&working, 1);
        picker.reconcile(&working[..1]);
        assert_eq!(picker.selected(), Some(&working[0]));
        picker.reconcile(&[]);
        assert!(!picker.is_open());
    }

    #[test]
    fn a_press_resolves_against_the_recorded_frame() {
        let session = entry();
        let picker = SubagentPicker::default();
        picker.record_row(4, 2..40, session.clone());
        assert_eq!(picker.hit(Position::new(10, 4)), Some(session));
        assert_eq!(picker.hit(Position::new(10, 5)), None);
        picker.forget_frame();
        assert_eq!(picker.hit(Position::new(10, 4)), None);
    }
}
