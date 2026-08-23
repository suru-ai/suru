//! What a Claude Session and the projection of its conversation must agree on about the Turn in
//! flight.
//!
//! Two things are known on one side of that seam and needed on the other. The Session must know
//! whether a Turn is running at all, because steering and interrupting are Session-level requests
//! that say nothing about the Turn they meant — and it must know what background work the agent has
//! spawned, because an interrupt stops that before it stops the loop. The projection must know how
//! many terminal results the Turn is still owed, because the CLI answers every user message queued
//! into a running loop with a `result` of its own, while Suru keeps the whole stretch inside the one
//! Turn the steer joined.
//!
//! The roster of background work outlives any one Turn, as the CLI's own does; it is only ever read
//! while a Turn is in flight, because stopping that work is something only an interrupt does.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, MutexGuard},
};

#[derive(Default)]
pub(super) struct TurnInFlight {
    state: Mutex<TurnState>,
}

#[derive(Default)]
struct TurnState {
    /// Terminal results the CLI still owes the running Turn: one for the Prompt that began it, and
    /// one more for every steer delivered into it. None owed means no Turn is running.
    owed_results: usize,
    /// The tasks the CLI has reported started and not yet reported settled.
    tasks: BTreeSet<String>,
}

impl TurnInFlight {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A Turn is beginning, and the CLI owes it the one result its Prompt will produce. Suru runs
    /// one Turn at a time, so this is what the Turn is owed rather than something added to it: a
    /// count left over from a Turn that ended some way this Session never heard about belongs to
    /// nothing now.
    pub(super) fn begin_turn(&self) {
        self.state().owed_results = 1;
    }

    /// Accepts a steer into the running Turn, which will owe one result more. Answers `false` when
    /// there is no Turn to steer, leaving the count alone.
    pub(super) fn accept_steer(&self) -> bool {
        let mut state = self.state();
        if state.owed_results == 0 {
            return false;
        }
        state.owed_results += 1;
        true
    }

    /// Takes back a Prompt the CLI never received, so a delivery that failed on its way out leaves
    /// the Turn waiting on nothing.
    pub(super) fn withdraw_prompt(&self) {
        let mut state = self.state();
        state.owed_results = state.owed_results.saturating_sub(1);
    }

    pub(super) fn is_running(&self) -> bool {
        self.state().owed_results > 0
    }

    /// One terminal result arrived: `true` when it is the last the Turn was owed and so Settles it,
    /// `false` while a steer's own result is still to come and the Turn goes on — and `false` for a
    /// result no Turn was waiting on at all, which is the answer to a message that outlived the Turn
    /// it was queued into and has nothing left to settle.
    pub(super) fn result_settles_turn(&self) -> bool {
        let mut state = self.state();
        if state.owed_results == 0 {
            return false;
        }
        state.owed_results -= 1;
        state.owed_results == 0
    }

    /// The Turn is over whatever it was still owed — interrupted, or failed — so nothing the CLI
    /// writes afterwards is its to settle.
    pub(super) fn abandon_turn(&self) {
        self.state().owed_results = 0;
    }

    pub(super) fn task_started(&self, task_id: String) {
        self.state().tasks.insert(task_id);
    }

    pub(super) fn task_settled(&self, task_id: &str) {
        self.state().tasks.remove(task_id);
    }

    /// The background work the CLI has reported running, which an interrupt stops before the loop.
    pub(super) fn live_tasks(&self) -> Vec<String> {
        self.state().tasks.iter().cloned().collect()
    }

    fn state(&self) -> MutexGuard<'_, TurnState> {
        self.state
            .lock()
            .expect("Claude Turn-in-flight lock is not poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::TurnInFlight;

    #[test]
    fn a_turn_settles_on_the_one_result_its_prompt_owes() {
        let turn = TurnInFlight::new();
        assert!(!turn.is_running(), "a Session with no Turn runs none");
        turn.begin_turn();
        assert!(turn.is_running());
        assert!(turn.result_settles_turn());
        assert!(!turn.is_running());
    }

    #[test]
    fn a_steered_turn_settles_only_on_the_result_its_steer_owes() {
        let turn = TurnInFlight::new();
        turn.begin_turn();
        assert!(turn.accept_steer());
        assert!(
            !turn.result_settles_turn(),
            "the stretch the steer joined ends without ending the Turn"
        );
        assert!(turn.is_running());
        assert!(
            turn.result_settles_turn(),
            "the steered stretch's own result Settles the Turn"
        );
    }

    #[test]
    fn a_session_with_no_turn_running_has_nothing_to_steer() {
        let turn = TurnInFlight::new();
        assert!(!turn.accept_steer());
        turn.begin_turn();
        turn.abandon_turn();
        assert!(!turn.accept_steer());
    }

    #[test]
    fn a_steer_the_cli_never_received_leaves_the_turn_owed_what_it_was() {
        let turn = TurnInFlight::new();
        turn.begin_turn();
        assert!(turn.accept_steer());
        turn.withdraw_prompt();
        assert!(turn.result_settles_turn(), "the Turn owes only its Prompt");
    }

    #[test]
    fn a_result_an_abandoned_turn_never_waited_on_settles_nothing() {
        let turn = TurnInFlight::new();
        turn.begin_turn();
        assert!(turn.accept_steer());
        turn.abandon_turn();
        assert!(
            !turn.result_settles_turn(),
            "the queued steer's own result has no Turn left to Settle"
        );
        assert!(!turn.is_running());
    }

    #[test]
    fn the_task_roster_holds_what_the_cli_has_reported_running() {
        let turn = TurnInFlight::new();
        turn.task_started("task-one".to_owned());
        turn.task_started("task-two".to_owned());
        turn.task_settled("task-one");
        assert_eq!(turn.live_tasks(), ["task-two"]);
    }
}
