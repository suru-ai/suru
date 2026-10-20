//! The rename dialog: a centered, single-line field a reader opens over one
//! Session to rename it — by writing its Title, or by asking the Session's
//! Origin to propose one derived from its Prompts so far.
//!
//! A proposal is only a proposal. It arrives in the field, where the reader
//! may change it, keep it, or abandon it, and nothing about the Session moves
//! until they save. While one is being derived the field is held, so typing
//! cannot be lost under the proposal when it lands; stopping the derivation
//! gives the field back the text it held.
//!
//! Whether the Origin would derive one at all is its own Setting's answer,
//! which a Client reaching a Remote's Session cannot read for itself, so the
//! dialog asks when it opens and offers deriving only once the Origin says it
//! would. A Setting that turns derivation off hides the offer rather than
//! greying it out: that was the user's own choice.

use std::cell::RefCell;

use ratatui::layout::{Position, Rect};
use uuid::Uuid;

use crate::protocol::{MAX_TITLE_CHARS, SessionReference, one_line_title, title_too_long};

/// The open dialog, if any, and where its last frame drew it.
#[derive(Clone, Debug, Default)]
pub(super) struct RenameDialog {
    draft: Option<Draft>,
    geometry: RefCell<Option<RenameGeometry>>,
}

/// Where a frame drew the dialog, so a press can be told apart: on its derive
/// affordance, elsewhere inside it, or outside it.
#[derive(Clone, Copy, Debug)]
pub(super) struct RenameGeometry {
    pub(super) dialog: Rect,
    pub(super) derive: Option<Rect>,
}

/// What a press over the dialog means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RenamePress {
    Derive,
    Inside,
    Outside,
}

/// The Title a reader is writing for one Session.
#[derive(Clone, Debug)]
struct Draft {
    session: SessionReference,
    /// This opening's identity, named by the Origin's answer to whether it
    /// would derive a proposal, so an answer for an earlier opening moves
    /// nothing.
    opening: Uuid,
    text: String,
    /// Whether the reader has made the field's text their own — by writing
    /// in it or by taking a proposal into it. Until they have, the field
    /// follows the Session's Title wherever it changes.
    touched: bool,
    offer: Offer,
    deriving: Option<Derivation>,
    saving: bool,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Offer {
    Asking,
    Offered,
    Withheld,
}

/// A proposal on its way: which request it answers, and what the field held
/// before it was asked for.
#[derive(Clone, Debug)]
struct Derivation {
    request: Uuid,
    before: String,
}

/// The dialog as a frame draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct RenameDialogView<'a> {
    pub(super) text: &'a str,
    /// How long the Title is as it will be kept, which is what the limit is
    /// measured against.
    pub(super) kept_chars: usize,
    pub(super) offered: bool,
    pub(super) deriving: bool,
    pub(super) saving: bool,
    pub(super) error: Option<&'a str>,
}

/// What stopping asked for: the derivation it abandoned, or the dialog
/// closing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RenameCancel {
    StoppedDeriving(Uuid),
    Closed,
}

impl RenameDialog {
    /// Opens the dialog over `session`, its field holding `title` — or
    /// `written`, what the reader already wrote for it elsewhere, which is
    /// theirs from the start and judged against the limit at once. Answers
    /// the opening's identity, which the question of whether a proposal is
    /// offered carries. Opening it again over any Session replaces whatever
    /// stood.
    pub(super) fn open(
        &mut self,
        session: SessionReference,
        title: &str,
        written: Option<String>,
    ) -> Uuid {
        let opening = Uuid::new_v4();
        let touched = written.is_some();
        let text = written.unwrap_or_else(|| title.to_owned());
        let error = touched.then(|| too_long(&text)).flatten();
        self.draft = Some(Draft {
            session,
            opening,
            text,
            touched,
            offer: Offer::Asking,
            deriving: None,
            saving: false,
            error,
        });
        opening
    }

    pub(super) fn is_open(&self) -> bool {
        self.draft.is_some()
    }

    pub(super) fn close(&mut self) {
        self.draft = None;
    }

    /// The Session the dialog renames, while one stands.
    pub(super) fn session(&self) -> Option<&SessionReference> {
        self.draft.as_ref().map(|draft| &draft.session)
    }

    /// The proposal still being derived, whose request a closing dialog
    /// abandons.
    pub(super) fn derivation(&self) -> Option<Uuid> {
        self.draft
            .as_ref()
            .and_then(|draft| draft.deriving.as_ref())
            .map(|deriving| deriving.request)
    }

    pub(super) fn view(&self) -> Option<RenameDialogView<'_>> {
        self.draft.as_ref().map(|draft| RenameDialogView {
            text: &draft.text,
            kept_chars: one_line_title(&draft.text).chars().count(),
            offered: draft.offer == Offer::Offered,
            deriving: draft.deriving.is_some(),
            saving: draft.saving,
            error: draft.error.as_deref(),
        })
    }

    pub(super) fn record_geometry(&self, geometry: RenameGeometry) {
        *self.geometry.borrow_mut() = Some(geometry);
    }

    pub(super) fn forget_frame(&self) {
        *self.geometry.borrow_mut() = None;
    }

    pub(super) fn press_at(&self, position: Position) -> RenamePress {
        let Some(geometry) = *self.geometry.borrow() else {
            return RenamePress::Outside;
        };
        if geometry
            .derive
            .is_some_and(|derive| derive.contains(position))
        {
            RenamePress::Derive
        } else if geometry.dialog.contains(position) {
            RenamePress::Inside
        } else {
            RenamePress::Outside
        }
    }

    /// The field the reader may write in: the dialog stands, and neither a
    /// save nor a proposal is on its way.
    fn writable(&mut self) -> Option<&mut Draft> {
        self.draft
            .as_mut()
            .filter(|draft| !draft.saving && draft.deriving.is_none())
    }

    /// Takes typed or pasted text into the field whole, a line break written
    /// as the space it will be kept as. Nothing is cut: a Title running past
    /// the limit says so as it is written.
    pub(super) fn insert(&mut self, text: &str) {
        let Some(draft) = self.writable() else {
            return;
        };
        draft.touched = true;
        draft.text.extend(
            text.chars()
                .map(|character| {
                    if character.is_whitespace() {
                        ' '
                    } else {
                        character
                    }
                })
                .filter(|character| !character.is_control()),
        );
        draft.error = too_long(&draft.text);
    }

    pub(super) fn delete_backward(&mut self) {
        if let Some(draft) = self.writable() {
            draft.touched = true;
            draft.text.pop();
            draft.error = too_long(&draft.text);
        }
    }

    pub(super) fn clear(&mut self) {
        if let Some(draft) = self.writable() {
            draft.touched = true;
            draft.text.clear();
            draft.error = None;
        }
    }

    /// Asks for a proposal, where the Origin offers one and the field is not
    /// already waiting on something: answers the Session and the request the
    /// proposal will name, and holds the field until it arrives.
    pub(super) fn derive(&mut self) -> Option<(SessionReference, Uuid)> {
        let draft = self
            .writable()
            .filter(|draft| draft.offer == Offer::Offered)?;
        let request = Uuid::new_v4();
        draft.deriving = Some(Derivation {
            request,
            before: draft.text.clone(),
        });
        draft.error = None;
        Some((draft.session.clone(), request))
    }

    /// Stops a proposal still being derived, giving the field back the text
    /// it held, or closes the dialog where none is.
    pub(super) fn cancel(&mut self) -> RenameCancel {
        if let Some(draft) = self.draft.as_mut()
            && let Some(deriving) = draft.deriving.take()
        {
            draft.text = deriving.before;
            return RenameCancel::StoppedDeriving(deriving.request);
        }
        self.draft = None;
        RenameCancel::Closed
    }

    /// Answers the Title to rename the Session to, kept on one line, and holds
    /// the dialog until the Origin answers. A blank Title, or one longer than
    /// the limit, is not sent at all: the dialog says why instead.
    pub(super) fn save(&mut self) -> Option<(SessionReference, String)> {
        let draft = self.writable()?;
        let title = one_line_title(&draft.text);
        if title.is_empty() {
            draft.error = Some("A Title cannot be blank".to_owned());
            return None;
        }
        if let Some(error) = too_long(&title) {
            draft.error = Some(error);
            return None;
        }
        draft.saving = true;
        draft.error = None;
        Some((draft.session.clone(), title))
    }

    /// Takes the Origin's word on whether it would derive a proposal for the
    /// opening it names.
    pub(super) fn offer_answered(&mut self, opening: Uuid, offered: bool) {
        if let Some(draft) = self.draft.as_mut().filter(|draft| draft.opening == opening) {
            draft.offer = if offered {
                Offer::Offered
            } else {
                Offer::Withheld
            };
        }
    }

    /// Takes a proposal — or why there is none — into the field waiting on
    /// it. A proposal the reader stopped waiting for moves nothing.
    pub(super) fn proposal_answered(&mut self, request: Uuid, answer: Result<String, String>) {
        let Some(draft) = self.draft.as_mut().filter(|draft| {
            draft
                .deriving
                .as_ref()
                .is_some_and(|deriving| deriving.request == request)
        }) else {
            return;
        };
        draft.deriving = None;
        match answer {
            Ok(title) => {
                draft.text = title;
                draft.touched = true;
                draft.error = None;
            }
            Err(error) => draft.error = Some(format!("Could not derive a Title: {error}")),
        }
    }

    /// Follows the Session's Title where it changed while the dialog stands,
    /// as long as the reader has not made the field their own.
    pub(super) fn title_changed(&mut self, session: &SessionReference, title: &str) {
        if let Some(draft) = self.draft.as_mut().filter(|draft| {
            &draft.session == session && !draft.touched && !draft.saving && draft.deriving.is_none()
        }) {
            title.clone_into(&mut draft.text);
        }
    }

    /// Takes the Origin's word that the rename landed: the dialog waiting on
    /// it closes.
    pub(super) fn saved(&mut self, session: &SessionReference) {
        if self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.saving && &draft.session == session)
        {
            self.draft = None;
        }
    }

    /// Takes why a rename did not land and gives the reader their text back
    /// beside it, answering `false` where no dialog waits on it.
    pub(super) fn save_failed(&mut self, session: &SessionReference, error: String) -> bool {
        match self
            .draft
            .as_mut()
            .filter(|draft| draft.saving && &draft.session == session)
        {
            Some(draft) => {
                draft.saving = false;
                draft.error = Some(error);
                true
            }
            None => false,
        }
    }
}

/// Why `text` cannot be saved for its length, where it cannot.
fn too_long(text: &str) -> Option<String> {
    let chars = one_line_title(text).chars().count();
    (chars > MAX_TITLE_CHARS).then(|| title_too_long(chars))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Outlook, SessionId};

    fn session() -> SessionReference {
        SessionReference::new(Outlook::Local, SessionId::new())
    }

    fn offered(dialog: &mut RenameDialog, session: SessionReference) {
        let opening = dialog.open(session, "Explain the seam", None);
        dialog.offer_answered(opening, true);
    }

    #[test]
    fn a_proposal_replaces_the_field_and_stopping_one_gives_the_field_back() {
        let mut dialog = RenameDialog::default();
        offered(&mut dialog, session());
        dialog.insert(" too");
        let (_, request) = dialog.derive().expect("a proposal is offered");
        dialog.insert("lost");
        assert_eq!(dialog.view().unwrap().text, "Explain the seam too");
        assert_eq!(dialog.cancel(), RenameCancel::StoppedDeriving(request));
        assert_eq!(dialog.view().unwrap().text, "Explain the seam too");
        dialog.proposal_answered(request, Ok("Late".to_owned()));
        assert_eq!(
            dialog.view().unwrap().text,
            "Explain the seam too",
            "a proposal the reader stopped waiting for moves nothing"
        );

        let (_, request) = dialog.derive().expect("a proposal is offered again");
        dialog.proposal_answered(request, Ok("Map the Provider seam".to_owned()));
        assert_eq!(dialog.view().unwrap().text, "Map the Provider seam");
        assert_eq!(dialog.cancel(), RenameCancel::Closed);
        assert!(!dialog.is_open());
    }

    #[test]
    fn deriving_waits_for_the_origin_to_offer_it() {
        let mut dialog = RenameDialog::default();
        let opening = dialog.open(session(), "Explain the seam", None);
        assert!(dialog.derive().is_none(), "nothing is offered yet");
        dialog.offer_answered(Uuid::new_v4(), true);
        assert!(dialog.derive().is_none(), "an answer for another opening");
        dialog.offer_answered(opening, false);
        assert!(dialog.derive().is_none(), "the Origin offers none");
        assert!(!dialog.view().unwrap().offered);
    }

    #[test]
    fn a_blank_or_overlong_title_is_not_sent() {
        let mut dialog = RenameDialog::default();
        dialog.open(session(), "Explain the seam", None);
        dialog.clear();
        dialog.insert("   ");
        assert!(dialog.save().is_none());
        assert_eq!(
            dialog.view().unwrap().error,
            Some("A Title cannot be blank")
        );
        dialog.insert(&"x".repeat(81));
        assert!(
            dialog.view().unwrap().error.is_some(),
            "said as it is written"
        );
        assert!(dialog.save().is_none());
        dialog.delete_backward();
        assert_eq!(dialog.view().unwrap().error, None);
        let (_, title) = dialog.save().expect("80 characters stand");
        assert_eq!(title, "x".repeat(80));
    }

    #[test]
    fn the_field_follows_the_title_until_the_reader_makes_it_their_own() {
        let mut dialog = RenameDialog::default();
        let session = session();
        dialog.open(session.clone(), "Explain the seam", None);
        dialog.title_changed(&session, "Map the Provider seam");
        assert_eq!(dialog.view().unwrap().text, "Map the Provider seam");
        dialog.insert("!");
        dialog.title_changed(&session, "Something else");
        assert_eq!(dialog.view().unwrap().text, "Map the Provider seam!");
    }

    #[test]
    fn text_written_elsewhere_is_judged_at_once() {
        let mut dialog = RenameDialog::default();
        dialog.open(session(), "Explain the seam", Some("y".repeat(90)));
        let view = dialog.view().unwrap();
        assert_eq!(view.text, "y".repeat(90));
        assert!(view.error.is_some());
    }
}
