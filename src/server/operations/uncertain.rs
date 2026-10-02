//! Acts carried to a Remote whose outcome was never learned: the Remote was
//! asked, and its answer did not come back whole. Nothing is recorded of such
//! an act as though it had been done, and nothing is kept to do again; a
//! Sidekick told so reads what the Remote holds to find out. What can be told
//! from such a read is taken up then: a Prompt it sent, or the first Prompt of
//! a Session it asked to begin, found among a Session's Prompts when that
//! Session is next read there shows the act was done, and it is recorded as
//! any act a Remote took.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crate::protocol::{Prompt, PromptId, SessionId};

/// The acts whose outcome at a Remote is not known yet, held in memory, the
/// oldest given up once more than [`UncertainActs::HELD`] wait.
#[derive(Clone, Default)]
pub(crate) struct UncertainActs {
    waiting: Arc<Mutex<VecDeque<UncertainAct>>>,
}

/// One act whose outcome at a Remote is not known: the Prompt it carried,
/// which the Remote holds where it was done.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UncertainAct {
    pub(super) sidekick: SessionId,
    pub(super) remote: String,
    pub(super) prompt: PromptId,
    /// Whether the Prompt was to begin a Session there.
    pub(super) began: bool,
}

impl UncertainActs {
    /// The most acts held waiting at once.
    const HELD: usize = 64;

    pub(super) fn note(&self, act: UncertainAct) {
        let mut waiting = self
            .waiting
            .lock()
            .expect("uncertain acts are not poisoned");
        if waiting.len() == Self::HELD {
            waiting.pop_front();
        }
        waiting.push_back(act);
    }

    /// The acts a Session just read from the Remote `remote`, holding
    /// `prompts`, shows were done there — each one whose Prompt it holds —
    /// given up as uncertain.
    pub(super) fn done_in(&self, remote: &str, prompts: &[Prompt]) -> Vec<UncertainAct> {
        let mut waiting = self
            .waiting
            .lock()
            .expect("uncertain acts are not poisoned");
        let (done, still): (Vec<_>, Vec<_>) = waiting.drain(..).partition(|act| {
            act.remote == remote && prompts.iter().any(|prompt| prompt.id == act.prompt)
        });
        waiting.extend(still);
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PromptDelivery, PromptOrder, PromptStatus};

    fn act(remote: &str, prompt: PromptId) -> UncertainAct {
        UncertainAct {
            sidekick: SessionId::new(),
            remote: remote.to_owned(),
            prompt,
            began: false,
        }
    }

    #[test]
    fn an_uncertain_act_is_done_once_its_prompt_is_found_where_it_was_sent() {
        let (sent, other) = (PromptId::new(), PromptId::new());
        let uncertain = UncertainActs::default();
        uncertain.note(act("workstation", sent));
        uncertain.note(act("laptop", sent));
        uncertain.note(act("workstation", other));
        let prompts = [Prompt {
            id: sent,
            text: "Go on.".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder(1),
            status: PromptStatus::Delivered,
            author: None,
            withdrawal: None,
        }];
        let done = uncertain.done_in("workstation", &prompts);
        assert_eq!(
            done.iter()
                .map(|act| (act.remote.as_str(), act.prompt))
                .collect::<Vec<_>>(),
            [("workstation", sent)],
            "only the Remote read, and only a Prompt it holds"
        );
        assert!(
            uncertain.done_in("workstation", &prompts).is_empty(),
            "and an act found done is waited on no longer"
        );
        assert_eq!(uncertain.waiting.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_oldest_uncertain_act_is_given_up_past_the_most_held() {
        let uncertain = UncertainActs::default();
        let first = PromptId::new();
        uncertain.note(act("workstation", first));
        for _ in 0..UncertainActs::HELD {
            uncertain.note(act("workstation", PromptId::new()));
        }
        let waiting = uncertain.waiting.lock().unwrap();
        assert_eq!(waiting.len(), UncertainActs::HELD);
        assert!(waiting.iter().all(|act| act.prompt != first));
    }
}
