//! What a Claude Session and the projection of its conversation must agree on about the Turn in
//! flight.
//!
//! Two things are known on one side of that seam and needed on the other. The Session must know
//! whether a Turn is running at all, because steering and interrupting are Session-level requests
//! that say nothing about the Turn they meant — and it must know what background work the agent has
//! spawned, because an interrupt stops that before it stops the loop. The projection must know
//! which terminal `result` Settles the Turn, because a steer does not always add one.
//!
//! Every user message the Session writes into a Turn — its Prompt, and each steer — goes under a
//! uuid minted here, and the CLI reports by that uuid when a loop takes the message up. A message
//! queued while the loop runs is taken up at the loop's next tool round and answered by that
//! loop's own `result`; one still queued when the loop ends begins a loop of its own once that
//! `result` is out, which ends with a `result` of its own
//! (docs/validation/0407-claude-folded-steer.md). So a `result` Settles the Turn unless a message
//! written into it is still waiting for a loop to take it up, and Suru keeps the whole stretch
//! inside the one Turn the steer joined. A CLI that has never reported a message's fate gives no
//! such account: there every `result` Settles the Turn, and a loop a message still queued begins
//! afterwards is a native Continuation — never a Turn left waiting on a `result` that a message
//! folded into the loop before it will not get.
//!
//! The roster of background work outlives any one Turn, as the CLI's own does; it is only ever read
//! while a Turn is in flight, because stopping that work is something only an interrupt does. It
//! does not outlive the process, though, any more than the CLI's own does: the work on it runs in
//! the CLI's process group and dies with it, and a replacement CLI announces nothing it inherited.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::protocol::AgentSelection;

#[derive(Default)]
pub(super) struct TurnInFlight {
    state: Mutex<TurnState>,
}

#[derive(Default)]
struct TurnState {
    /// The running CLI's selection, retained for work it resumes without a Prompt.
    selection: Option<AgentSelection>,
    /// Whether a Turn runs: from its Prompt, or from the native loop that began a Continuation,
    /// until the `result` that Settles it or until it is abandoned.
    running: bool,
    /// The messages written into the running Turn that no loop has taken up yet, by the uuid each
    /// was written under.
    untaken: BTreeSet<String>,
    /// Whether a `result` has ended the running Turn's last loop while messages written into it
    /// were still waiting for a loop, so that until one is taken up no loop of the Turn's runs.
    between_loops: bool,
    /// Whether the CLI has reported the fate of any message, which is what makes a message it has
    /// not reported taken up one to wait for. The CLI does not change under a Session, so this is
    /// never forgotten.
    reports_lifecycle: bool,
    /// The tasks the CLI has reported started and not yet reported settled.
    tasks: BTreeSet<String>,
    /// Of those tasks, the ones running Subagents. A Subagent is known to the rest of Suru by its
    /// task id — the identity a resume starts again, and the one the CLI's stop request takes —
    /// so this is the lookup a per-Subagent stop resolves through, whichever stretch is running.
    subagents: BTreeSet<String>,
}

impl TurnState {
    fn settle(&mut self) {
        self.running = false;
        self.untaken.clear();
        self.between_loops = false;
    }
}

/// A uuid for one user message, which the CLI reports that message's fate under.
fn message_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl TurnInFlight {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A Turn is beginning, and its Prompt is to be written under the uuid this answers. Suru runs
    /// one Turn at a time, so this replaces what was in flight rather than adding to it: a message
    /// left over from a Turn that ended some way this Session never heard about belongs to nothing
    /// now.
    pub(super) fn begin_turn(&self, selection: AgentSelection) -> String {
        let mut state = self.state();
        let uuid = message_uuid();
        state.selection = Some(selection);
        state.settle();
        state.running = true;
        state.untaken.insert(uuid.clone());
        uuid
    }

    /// A fresh native message after a result begins another loop, including when a background
    /// command (rather than a Subagent) woke Claude. Its result and interrupt belong to that
    /// loop before any of its blocks are projected. No message of Suru's began it, so none is
    /// waiting on it.
    pub(super) fn begin_continuation(&self) -> Option<AgentSelection> {
        let mut state = self.state();
        if state.running {
            return None;
        }
        let selection = state.selection.clone()?;
        state.settle();
        state.running = true;
        Some(selection)
    }

    /// Accepts a steer into the running Turn, answering the uuid to write it under. Answers `None`
    /// when there is no Turn to steer.
    pub(super) fn accept_steer(&self) -> Option<String> {
        let mut state = self.state();
        if !state.running {
            return None;
        }
        let uuid = message_uuid();
        state.untaken.insert(uuid.clone());
        Some(uuid)
    }

    /// Takes back a steer the CLI never received, so a delivery that failed on its way out leaves
    /// the Turn waiting on nothing for it.
    pub(super) fn withdraw_steer(&self, uuid: &str) {
        self.state().untaken.remove(uuid);
    }

    pub(super) fn is_running(&self) -> bool {
        self.state().running
    }

    /// The CLI has queued a message, which says only that it reports the fate of the messages it
    /// is sent.
    pub(super) fn message_queued(&self) {
        self.state().reports_lifecycle = true;
    }

    /// A loop has taken up the message written under `uuid`: folded into the loop already running,
    /// whose `result` answers it, or begun as a loop of its own.
    pub(super) fn message_started(&self, uuid: &str) {
        let mut state = self.state();
        state.reports_lifecycle = true;
        if state.untaken.remove(uuid) {
            state.between_loops = false;
        }
    }

    /// The message written under `uuid` has ended, and one no loop took up never will. `true` when
    /// that leaves nothing for the running Turn to wait on after the `result` that ended its last
    /// loop, so that nothing else will ever Settle it and it Settles now.
    pub(super) fn message_ended(&self, uuid: &str) -> bool {
        let mut state = self.state();
        state.reports_lifecycle = true;
        if !state.untaken.remove(uuid) {
            return false;
        }
        if state.running && state.between_loops && state.untaken.is_empty() {
            state.settle();
            return true;
        }
        false
    }

    /// A loop ended with a successful terminal result: `true` when that Settles the Turn, `false`
    /// while a message written into the Turn still waits for a loop of its own and the Turn goes on
    /// — and `false` for a result no Turn was waiting on at all, which is the answer to a message
    /// that outlived the Turn it was queued into and has nothing left to settle.
    pub(super) fn result_settles_turn(&self) -> bool {
        let mut state = self.state();
        if !state.running {
            return false;
        }
        if state.reports_lifecycle && !state.untaken.is_empty() {
            state.between_loops = true;
            return false;
        }
        state.settle();
        true
    }

    /// The Turn is over whatever it was still waiting on — interrupted, or failed — so nothing the
    /// CLI writes afterwards is its to settle.
    pub(super) fn abandon_turn(&self) {
        self.state().settle();
    }

    pub(super) fn task_started(&self, task_id: String) {
        self.state().tasks.insert(task_id);
    }

    /// Remembers that a started task runs a Subagent — spawned, or resumed — so a stop asking for
    /// the Subagent can find the task to stop.
    pub(super) fn subagent_task_started(&self, task_id: String) {
        self.state().subagents.insert(task_id);
    }

    pub(super) fn task_settled(&self, task_id: &str) {
        let mut state = self.state();
        state.tasks.remove(task_id);
        state.subagents.remove(task_id);
    }

    /// The CLI process this roster was kept for has ended — stopped by the Session, or replaced
    /// by one spawned under another Agent Selection — and every task on it died with that process.
    /// None will ever report settling, and the process that replaces it has never heard of them,
    /// so a stop asking it for one would be refused and leave the task on the roster for good.
    /// The roster starts empty again, the way the new CLI's own does.
    pub(super) fn tasks_died_with_process(&self) {
        let mut state = self.state();
        state.tasks.clear();
        state.subagents.clear();
    }

    /// The background work the CLI has reported running, which an interrupt stops before the loop.
    pub(super) fn live_tasks(&self) -> Vec<String> {
        self.state().tasks.iter().cloned().collect()
    }

    /// The task running the Subagent this identity names, while it is still on the roster.
    pub(super) fn subagent_task(&self, subagent: &str) -> Option<String> {
        self.state().subagents.get(subagent).cloned()
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
    use crate::protocol::{AgentSelection, ModelId, ProviderId};

    fn selection() -> AgentSelection {
        AgentSelection {
            provider: ProviderId::new("claude"),
            model: ModelId::new("default"),
            options: Vec::new(),
        }
    }

    /// A Turn begun on a CLI that reports lifecycles, whose Prompt a loop has taken up: the
    /// ground every steer below is written onto.
    fn prompt_taken_up() -> std::sync::Arc<TurnInFlight> {
        let turn = TurnInFlight::new();
        let prompt = turn.begin_turn(selection());
        turn.message_queued();
        turn.message_started(&prompt);
        turn
    }

    #[test]
    fn a_turn_settles_on_the_result_that_answers_its_prompt() {
        let turn = TurnInFlight::new();
        assert!(!turn.is_running(), "a Session with no Turn runs none");
        let prompt = turn.begin_turn(selection());
        assert!(turn.is_running());
        turn.message_queued();
        turn.message_started(&prompt);
        assert!(turn.result_settles_turn());
        assert!(!turn.is_running());
    }

    #[test]
    fn each_message_is_written_under_a_uuid_of_its_own() {
        let turn = TurnInFlight::new();
        let prompt = turn.begin_turn(selection());
        let first = turn.accept_steer().expect("the running Turn takes a steer");
        let second = turn.accept_steer().expect("and another");
        assert_ne!(prompt, first);
        assert_ne!(first, second);
        assert!(
            uuid::Uuid::parse_str(&prompt).is_ok(),
            "the CLI is handed a uuid: {prompt}"
        );
    }

    #[test]
    fn a_steer_folded_into_the_running_loop_is_answered_by_that_loops_one_result() {
        let turn = prompt_taken_up();
        let steer = turn.accept_steer().expect("the running Turn takes a steer");
        turn.message_queued();
        turn.message_started(&steer);
        assert!(
            turn.result_settles_turn(),
            "the loop that took the steer up at its tool round answers it with its own result"
        );
        assert!(!turn.is_running());
    }

    #[test]
    fn a_steer_still_queued_when_its_loop_ends_holds_the_turn_for_the_loop_it_begins() {
        let turn = prompt_taken_up();
        let steer = turn.accept_steer().expect("the running Turn takes a steer");
        turn.message_queued();
        assert!(
            !turn.result_settles_turn(),
            "the stretch the steer joined ends without ending the Turn"
        );
        assert!(turn.is_running());
        turn.message_started(&steer);
        assert!(
            turn.result_settles_turn(),
            "the loop the steer began Settles the Turn with its result"
        );
    }

    #[test]
    fn a_steer_taken_up_after_the_result_it_raced_still_holds_the_turn() {
        let turn = prompt_taken_up();
        let steer = turn.accept_steer().expect("the running Turn takes a steer");
        assert!(
            !turn.result_settles_turn(),
            "a steer the CLI has not yet reported queued is still one no loop has taken up"
        );
        turn.message_queued();
        turn.message_started(&steer);
        assert!(turn.result_settles_turn());
    }

    #[test]
    fn a_prompt_queued_behind_a_native_loop_is_not_answered_by_that_loops_result() {
        let turn = prompt_taken_up();
        assert!(turn.result_settles_turn());
        assert!(
            turn.begin_continuation().is_some(),
            "a background task wakes the loop"
        );
        assert!(
            turn.result_settles_turn(),
            "the woken loop's result settles its Continuation"
        );

        let prompt = turn.begin_turn(selection());
        turn.message_queued();
        assert!(
            !turn.result_settles_turn(),
            "a loop the Prompt never joined does not answer it"
        );
        turn.message_started(&prompt);
        assert!(turn.result_settles_turn());
    }

    #[test]
    fn without_a_reported_lifecycle_every_result_settles_the_turn() {
        let turn = TurnInFlight::new();
        turn.begin_turn(selection());
        assert!(turn.accept_steer().is_some());
        assert!(
            turn.result_settles_turn(),
            "nothing says the steer was not folded into the loop that just ended"
        );
        assert!(
            turn.begin_continuation().is_some(),
            "a loop the steer begins afterwards is a native Continuation"
        );
    }

    #[test]
    fn a_steer_that_ends_untaken_after_the_last_loop_settles_the_turn_then() {
        let turn = prompt_taken_up();
        let steer = turn.accept_steer().expect("the running Turn takes a steer");
        turn.message_queued();
        assert!(!turn.result_settles_turn());
        assert!(
            turn.message_ended(&steer),
            "no loop will take the steer up, so nothing else will Settle the Turn"
        );
        assert!(!turn.is_running());
    }

    #[test]
    fn a_steer_that_ends_untaken_while_a_loop_runs_leaves_the_result_to_settle_the_turn() {
        let turn = prompt_taken_up();
        let steer = turn.accept_steer().expect("the running Turn takes a steer");
        assert!(
            !turn.message_ended(&steer),
            "the running loop's own result is still to come"
        );
        assert!(turn.is_running());
        assert!(turn.result_settles_turn());
    }

    #[test]
    fn a_session_with_no_turn_running_has_nothing_to_steer() {
        let turn = TurnInFlight::new();
        assert!(turn.accept_steer().is_none());
        turn.begin_turn(selection());
        turn.abandon_turn();
        assert!(turn.accept_steer().is_none());
    }

    #[test]
    fn a_steer_the_cli_never_received_leaves_the_turn_waiting_on_nothing_for_it() {
        let turn = prompt_taken_up();
        let steer = turn.accept_steer().expect("the running Turn takes a steer");
        turn.withdraw_steer(&steer);
        assert!(
            turn.result_settles_turn(),
            "the Turn waits only on its Prompt"
        );
    }

    #[test]
    fn a_result_an_abandoned_turn_never_waited_on_settles_nothing() {
        let turn = prompt_taken_up();
        let steer = turn.accept_steer().expect("the running Turn takes a steer");
        turn.abandon_turn();
        turn.message_started(&steer);
        assert!(
            !turn.result_settles_turn(),
            "the queued steer's own result has no Turn left to Settle"
        );
        assert!(!turn.is_running());
        assert!(
            !turn.message_ended(&steer),
            "nor does the steer's end, once the Turn it joined is over"
        );
    }

    #[test]
    fn the_task_roster_holds_what_the_cli_has_reported_running() {
        let turn = TurnInFlight::new();
        turn.task_started("task-one".to_owned());
        turn.task_started("task-two".to_owned());
        turn.task_settled("task-one");
        assert_eq!(turn.live_tasks(), ["task-two"]);
    }

    #[test]
    fn a_subagents_task_resolves_by_its_identity_until_the_task_settles() {
        let turn = TurnInFlight::new();
        turn.task_started("task-one".to_owned());
        turn.subagent_task_started("task-one".to_owned());
        assert_eq!(turn.subagent_task("task-one"), Some("task-one".to_owned()));
        turn.task_settled("task-one");
        assert_eq!(
            turn.subagent_task("task-one"),
            None,
            "a settled task leaves nothing for a stop to resolve"
        );
    }

    #[test]
    fn the_roster_starts_empty_again_once_its_process_has_ended() {
        let turn = TurnInFlight::new();
        turn.task_started("task-bash".to_owned());
        turn.task_started("task-agent".to_owned());
        turn.subagent_task_started("task-agent".to_owned());

        turn.tasks_died_with_process();

        assert!(
            turn.live_tasks().is_empty(),
            "no task outlives the process it ran in"
        );
        assert_eq!(
            turn.subagent_task("task-agent"),
            None,
            "a Subagent whose task died has nothing left for a stop to resolve"
        );
        turn.task_started("task-next".to_owned());
        assert_eq!(
            turn.live_tasks(),
            ["task-next"],
            "the replacement process's own work joins an empty roster"
        );
    }

    #[test]
    fn a_resumed_subagents_task_resolves_again_while_the_resume_runs() {
        let turn = TurnInFlight::new();
        turn.task_started("task-one".to_owned());
        turn.subagent_task_started("task-one".to_owned());
        turn.task_settled("task-one");

        turn.task_started("task-one".to_owned());
        turn.subagent_task_started("task-one".to_owned());
        assert_eq!(
            turn.subagent_task("task-one"),
            Some("task-one".to_owned()),
            "a stop reaches the resumed task under the Subagent's same identity"
        );
        assert_eq!(turn.live_tasks(), ["task-one"]);
    }
}
