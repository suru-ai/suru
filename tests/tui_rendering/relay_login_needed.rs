//! How the Client's own Server's Relays stand, as that Server pushes them:
//! the `/relay` list following each Relay live as logged in, login needed or
//! Unreachable; the Notice raised once when a Relay comes to need a login,
//! leading to that login; and a Relay merely Unreachable calling for none.
//!
//! The Server says which lapse of a Relay's Login a Notice is to be raised
//! of, and gives that Notice to the first Client to claim it and to none
//! after — so one Client raises it, and neither a Client opened later, nor
//! one that hears of it late, nor the same state pushed again, raises it
//! more.

use std::time::Duration;

use uuid::Uuid;

use crate::support::{
    application_looking_at_studio, buffer_rows, click_mouse, grace_elapses,
    navigable_session_snapshot, rendered_application_buffer, rendered_application_rows,
    rendered_application_rows_at, studio_stops_answering_because, text_position,
    type_terminal_text,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, Outlook, Relay, RelayAccount, RelayListing, RelayLogin,
        RelayLoginOutcome, RelayLoginRefusal, RelayRemoval, RelayState, RelayUnreachable,
        SessionId, SettingsSnapshot, SidebarSettings, SidebarVisibility, UnreachableReason,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, RelayRequest,
        SemanticCommandId,
    },
};

const COMPANY: &str = "https://relay.company.example";
const HOME: &str = "https://home.example.net";
const CODE: &str = "WDJB-MJHT";
/// Two lapses of one Relay's Login, the second after it stood again.
const FIRST: Uuid = Uuid::from_u128(1);
const SECOND: Uuid = Uuid::from_u128(2);
/// Two runs of the Client's own Server, the later the one now running.
const EARLIER: Uuid = Uuid::from_u128(10);
const LATER: Uuid = Uuid::from_u128(11);

#[test]
fn the_relay_list_follows_each_relays_state_as_the_server_pushes_it() {
    let mut application = Application::default();
    push(
        &mut application,
        10,
        vec![logged_in(COMPANY), needing_login(HOME)],
    );

    let listing = open(&mut application);
    // The listing asked for as the list opened was answered before the push
    // that came after it, and is not taken over it.
    list(&mut application, listing, 9, vec![needing_login(COMPANY)]);
    let shown = prose(&application);
    assert!(
        shown.contains(&format!("{COMPANY} Logged in as octocat (github)"))
            && shown.contains(&format!("{HOME} Login needed")),
        "the pushed state stands over the older listing: {shown}"
    );

    // While the list is open, each Relay's state moves with the Server's.
    push(
        &mut application,
        11,
        vec![unreachable(COMPANY), needing_login(HOME)],
    );
    assert!(
        prose(&application).contains(&format!("{COMPANY} Unreachable · the Relay did not answer")),
        "{}",
        prose(&application)
    );
    push(
        &mut application,
        12,
        vec![lapsed(COMPANY, None), needing_login(HOME)],
    );
    assert!(
        prose(&application).contains(&format!("{COMPANY} Login needed")),
        "{}",
        prose(&application)
    );

    // A push the Server made before one already taken moves nothing.
    push(&mut application, 11, vec![unreachable(COMPANY)]);
    assert!(
        prose(&application).contains(&format!("{COMPANY} Login needed")),
        "{}",
        prose(&application)
    );

    // And a listing newer than every push is taken, as it is the newest.
    press(&mut application, KeyCode::Esc);
    let listing = open(&mut application);
    list(&mut application, listing, 13, vec![logged_in(COMPANY)]);
    let shown = prose(&application);
    assert!(
        shown.contains(&format!("{COMPANY} Logged in")) && !shown.contains(HOME),
        "{shown}"
    );
}

#[test]
fn a_relay_that_needs_a_login_offers_one_and_one_merely_unreachable_does_not() {
    let mut application = Application::default();
    push(
        &mut application,
        1,
        vec![
            needing_login(COMPANY),
            unreachable(HOME),
            logged_in("https://third.example.org"),
        ],
    );
    let listing = open(&mut application);
    list(
        &mut application,
        listing,
        1,
        vec![
            needing_login(COMPANY),
            unreachable(HOME),
            logged_in("https://third.example.org"),
        ],
    );

    let keys = keys_taught(&application);
    assert!(keys.contains("Enter log in"), "{keys}");
    press(&mut application, KeyCode::Down);
    let keys = keys_taught(&application);
    assert!(
        !keys.contains("log in"),
        "a Relay merely Unreachable is offered no login: {keys}"
    );
    press(&mut application, KeyCode::Down);
    assert!(!keys_taught(&application).contains("log in"));

    // A login under way is shown again rather than begun anew.
    push(
        &mut application,
        2,
        vec![
            Relay {
                login: Some(pending()),
                ..needing_login(COMPANY)
            },
            unreachable(HOME),
            logged_in("https://third.example.org"),
        ],
    );
    press(&mut application, KeyCode::Down);
    assert!(keys_taught(&application).contains("Enter show login"));

    // The keys wrap on a narrow terminal rather than run off the box.
    let narrow = rendered_application_rows_at(&application, 40, 24)
        .iter()
        .map(|row| row.trim().trim_matches('│').trim().to_owned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        narrow.contains("Enter show login · s Serve through · x remove · Esc close"),
        "{narrow}"
    );
}

#[test]
fn a_notice_is_raised_once_for_each_lapse_this_client_is_given() {
    let mut application = Application::default();
    push(&mut application, 1, vec![logged_in(COMPANY)]);
    assert!(application.take_relay_notice_claims().is_empty());

    // A lapse is claimed of the Server, and raised once the Server gives it
    // this Client.
    push(&mut application, 2, vec![lapsed(COMPANY, Some(FIRST))]);
    assert_eq!(
        application.take_relay_notice_claims(),
        vec![(COMPANY.to_owned(), FIRST)],
        "the Notice of the lapse is claimed of the Server"
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Login needed"),
        "nothing is raised before the Server gives it"
    );
    answer_claim(&mut application, COMPANY, FIRST, true);
    let notice = rendered_application_rows(&application)[0].trim().to_owned();
    assert_eq!(
        notice,
        format!("! Login needed at {COMPANY} · /relay to log in"),
        "the Notice points at where to log in, not at the Log, which holds nothing of a Relay"
    );

    // Pushed again before the Server has said otherwise, or by a second
    // stream, the same lapse is claimed no more and raises nothing more
    // once the reader has seen it.
    interact(&mut application);
    push(&mut application, 3, vec![lapsed(COMPANY, Some(FIRST))]);
    push(&mut application, 3, vec![lapsed(COMPANY, Some(FIRST))]);
    assert!(application.take_relay_notice_claims().is_empty());
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Login needed at https"),
        "{:?}",
        rendered_application_rows(&application)
    );

    // A later lapse is another, even where the push saying the earlier one
    // was given never reached this Client.
    push(&mut application, 6, vec![lapsed(COMPANY, Some(SECOND))]);
    assert_eq!(
        application.take_relay_notice_claims(),
        vec![(COMPANY.to_owned(), SECOND)]
    );
    answer_claim(&mut application, COMPANY, SECOND, true);
    assert!(
        rendered_application_rows(&application)[0].contains(&format!("Login needed at {COMPANY}")),
        "{:?}",
        rendered_application_rows(&application)
    );

    // The Server, having given it, asks no more: still login needed,
    // nothing claimed or raised.
    interact(&mut application);
    push(&mut application, 7, vec![lapsed(COMPANY, None)]);
    assert!(application.take_relay_notice_claims().is_empty());
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Login needed at https")
    );
}

/// Two Clients hear of a lapse, and the other is given its Notice first:
/// this one, slower, raises nothing, however late it hears of the lapse or
/// of the Notice given.
#[test]
fn a_notice_another_client_was_given_is_raised_by_none_however_late_this_one_hears() {
    let mut application = Application::default();
    push(&mut application, 1, vec![logged_in(COMPANY)]);
    push(&mut application, 2, vec![lapsed(COMPANY, Some(FIRST))]);
    assert_eq!(
        application.take_relay_notice_claims(),
        vec![(COMPANY.to_owned(), FIRST)]
    );
    answer_claim(&mut application, COMPANY, FIRST, false);
    push(&mut application, 3, vec![lapsed(COMPANY, None)]);

    assert!(application.take_relay_notice_claims().is_empty());
    let screen = rendered_application_rows(&application).join("\n");
    assert!(!screen.contains("Login needed"), "{screen}");
}

/// A lapse pushed late — once the Client has heard its Relay logged in at
/// again since — is no lapse to claim a Notice of.
#[test]
fn a_lapse_pushed_after_its_relay_was_heard_logged_in_again_raises_nothing() {
    let mut application = Application::default();
    push(&mut application, 5, vec![logged_in(COMPANY)]);
    push(&mut application, 3, vec![lapsed(COMPANY, Some(FIRST))]);

    assert!(
        application.take_relay_notice_claims().is_empty(),
        "nothing is claimed of a lapse older than what the Client holds"
    );
    let screen = prose(&application);
    assert!(!screen.contains("Login needed"), "{screen}");
}

#[test]
fn a_relay_notice_stands_through_a_settings_snapshot() {
    let mut application = Application::default();
    push(&mut application, 1, vec![lapsed(COMPANY, Some(FIRST))]);
    assert_eq!(
        application.take_relay_notice_claims(),
        vec![(COMPANY.to_owned(), FIRST)]
    );
    answer_claim(&mut application, COMPANY, FIRST, true);

    // A terminal too small to draw it draws nothing, and the Notice stands.
    rendered_application_rows_at(&application, 12, 4);

    // Another Client changing a Setting pushes a snapshot to every Client:
    // the Notice stands through it, untouched by the reader.
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    // The Notice is the Landing's own row, so the Sidebar
                    // stays off the frame rather than sharing it.
                    sidebar: SidebarSettings {
                        initial_visibility: SidebarVisibility::Hidden,
                        ..SidebarSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("take the Settings snapshot");
    let screen = rendered_application_rows(&application).join("\n");
    assert!(
        screen.contains(&format!("Login needed at {COMPANY} · /relay to log in")),
        "{screen}"
    );
}

#[test]
fn a_late_answer_to_an_addition_brings_back_no_relay_removed_since() {
    let mut application = Application::default();
    let listing = open(&mut application);
    list(&mut application, listing, 1, vec![logged_in(HOME)]);
    press(&mut application, KeyCode::Char('a'));
    type_terminal_text(&mut application, COMPANY);
    let ApplicationTransition::AddRelay { request, .. } = press(&mut application, KeyCode::Enter)
    else {
        panic!("Enter adds the Relay typed");
    };

    // Another Client removed it before this one heard it was added.
    push(&mut application, 3, vec![logged_in(HOME)]);
    let refresh = application
        .handle_event(ApplicationEvent::RelayAdded {
            request,
            relay: needing_login(COMPANY),
        })
        .expect("take the late addition");
    assert!(!listed(&application, COMPANY), "{}", prose(&application));
    let ApplicationTransition::ListRelays(refresh) = refresh else {
        panic!("the list is asked for again rather than patched: {refresh:?}");
    };
    list(&mut application, refresh, 3, vec![logged_in(HOME)]);
    assert!(!listed(&application, COMPANY), "{}", prose(&application));
    assert!(listed(&application, HOME));
}

#[test]
fn a_late_answer_to_a_removal_takes_away_no_relay_added_again_since() {
    let mut application = Application::default();
    let listing = open(&mut application);
    list(&mut application, listing, 1, vec![logged_in(COMPANY)]);
    press(&mut application, KeyCode::Char('x'));
    let ApplicationTransition::RemoveRelay { request, address } =
        press(&mut application, KeyCode::Char('x'))
    else {
        panic!("a second x removes the Relay");
    };

    // Another Client added it again before this one heard it was removed.
    push(&mut application, 3, vec![needing_login(COMPANY)]);
    let refresh = application
        .handle_event(ApplicationEvent::RelayRemoved {
            request,
            result: Ok(RelayRemoval {
                address,
                acknowledged: true,
            }),
        })
        .expect("take the late removal");
    assert!(listed(&application, COMPANY), "{}", prose(&application));
    let ApplicationTransition::ListRelays(refresh) = refresh else {
        panic!("the list is asked for again rather than patched: {refresh:?}");
    };
    list(&mut application, refresh, 3, vec![needing_login(COMPANY)]);
    assert!(listed(&application, COMPANY), "{}", prose(&application));
}

#[test]
fn a_relay_removed_elsewhere_while_its_login_is_shown_resolves_the_display_as_the_login_ends() {
    let mut application = Application::default();
    let listing = open(&mut application);
    list(&mut application, listing, 1, vec![needing_login(COMPANY)]);
    let ApplicationTransition::BeginRelayLogin { request, .. } =
        press(&mut application, KeyCode::Enter)
    else {
        panic!("Enter logs in at a Relay that needs a login");
    };
    let ApplicationTransition::FollowRelayLogins(follows) = application
        .handle_event(ApplicationEvent::RelayLoginBegun {
            request,
            login: pending(),
        })
        .expect("take the begun login")
    else {
        panic!("the begun login is followed");
    };
    let follower = follows[0].request;

    // Another Client removes the Relay, and that is pushed before the login
    // is heard to end.
    push(&mut application, 3, Vec::new());
    assert!(
        prose(&application).contains(CODE),
        "{}",
        prose(&application)
    );
    application
        .handle_event(ApplicationEvent::RelayLoginSettled {
            request: follower,
            login: RelayLogin {
                outcome: RelayLoginOutcome::Refused {
                    reason: RelayLoginRefusal::Interrupted,
                    message: "the login was given up as its Relay was being removed".to_owned(),
                },
                ..pending()
            },
        })
        .expect("take how the login ended");
    let shown = prose(&application);
    assert!(
        !shown.contains(CODE)
            && shown.contains(&format!(
                "The login at {COMPANY} ended: the login was given up as its Relay was being \
                 removed"
            )),
        "{shown}"
    );
}

#[test]
fn a_server_started_again_is_heard_whatever_its_revisions_and_an_earlier_ones_listing_is_not() {
    let mut application = Application::default();
    push_from(&mut application, EARLIER, 500, vec![logged_in(COMPANY)]);
    let listing = open(&mut application);

    // The Server restarts, counting its revisions afresh.
    push_from(&mut application, LATER, 1, vec![needing_login(COMPANY)]);
    list_from(
        &mut application,
        listing,
        EARLIER,
        900,
        vec![logged_in(COMPANY)],
    );
    let shown = prose(&application);
    assert!(
        shown.contains(&format!("{COMPANY} Login needed")),
        "the earlier run's listing is not taken over the Server now running: {shown}"
    );
    push_from(&mut application, LATER, 2, vec![logged_in(COMPANY)]);
    assert!(
        prose(&application).contains(&format!("{COMPANY} Logged in")),
        "{}",
        prose(&application)
    );
}

#[test]
fn a_client_opened_after_the_notice_was_raised_raises_nothing() {
    // The Server says no Notice is to be raised any more, though the Relay
    // still needs a login, as it does to a Client opened after another
    // raised it.
    let mut application = Application::default();
    push(&mut application, 1, vec![lapsed(COMPANY, None)]);
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Login needed at https")
    );
    assert!(application.take_relay_notice_claims().is_empty());
}

#[test]
fn a_relay_that_stops_answering_or_was_never_logged_in_at_raises_no_notice() {
    let mut application = Application::default();
    push(
        &mut application,
        1,
        vec![logged_in(COMPANY), needing_login(HOME)],
    );
    push(
        &mut application,
        2,
        vec![unreachable(COMPANY), needing_login(HOME)],
    );
    let screen = rendered_application_rows(&application).join("\n");
    assert!(
        !screen.contains("Login needed") && !screen.contains("log in"),
        "{screen}"
    );
    assert!(application.take_relay_notice_claims().is_empty());
}

#[test]
fn the_notice_leads_to_the_login_at_its_relay() {
    let mut application = Application::default();
    push(&mut application, 1, vec![lapsed(COMPANY, Some(FIRST))]);
    answer_claim(&mut application, COMPANY, FIRST, true);
    let buffer = rendered_application_buffer(&application, 120, 20);
    let (column, row) = text_position(&buffer, "/relay to log in");

    // A press on it dismisses the Notice and opens the list on its Relay,
    // logging in there once it is listed.
    let ApplicationTransition::ListRelays(listing) = press_at(&mut application, column, row) else {
        panic!("a press on the Notice's pointer opens the Relay list");
    };
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("/relay to log in")
    );
    let transition = list(
        &mut application,
        listing,
        1,
        vec![logged_in(HOME), lapsed(COMPANY, None)],
    );
    let ApplicationTransition::BeginRelayLogin { request, address } = transition else {
        panic!("the list, landed, logs in at the Relay the Notice named: {transition:?}");
    };
    assert_eq!(address, COMPANY);
    application
        .handle_event(ApplicationEvent::RelayLoginBegun {
            request,
            login: pending(),
        })
        .expect("take the begun login");
    let display = prose(&application);
    assert!(
        display.contains(&format!("Log in at {COMPANY}")) && display.contains(CODE),
        "{display}"
    );

    // Typing /relay, which the Notice also names, opens the same list.
    press(&mut application, KeyCode::Esc);
    press(&mut application, KeyCode::Esc);
    type_terminal_text(&mut application, "/relay");
    assert!(matches!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::ListRelays(_)
    ));
}

#[test]
fn the_login_on_display_ends_only_with_that_login_however_the_relay_is_pushed() {
    let mut application = Application::default();
    push(&mut application, 5, vec![lapsed(COMPANY, None)]);
    let listing = open(&mut application);
    list(&mut application, listing, 5, vec![lapsed(COMPANY, None)]);
    let ApplicationTransition::BeginRelayLogin { request, .. } =
        press(&mut application, KeyCode::Enter)
    else {
        panic!("Enter logs in at a Relay that needs a login");
    };
    application
        .handle_event(ApplicationEvent::RelayLoginBegun {
            request,
            login: pending(),
        })
        .expect("take the begun login");

    // A push made before the login began, landing after it, leaves the
    // display standing.
    push(&mut application, 6, vec![lapsed(COMPANY, None)]);
    assert!(
        prose(&application).contains(CODE),
        "{}",
        prose(&application)
    );

    // A push showing the login under way leaves it too, and one showing it
    // done resolves it, saying so, with nothing following it needed.
    push(
        &mut application,
        7,
        vec![Relay {
            login: Some(pending()),
            ..lapsed(COMPANY, None)
        }],
    );
    assert!(prose(&application).contains(CODE));
    push(
        &mut application,
        8,
        vec![Relay {
            login: Some(RelayLogin {
                outcome: RelayLoginOutcome::Done { account: octocat() },
                ..pending()
            }),
            ..logged_in(COMPANY)
        }],
    );
    let shown = prose(&application);
    assert!(
        shown.contains(&format!("Logged in at {COMPANY} as octocat"))
            && shown.contains("Logged in as octocat (github)"),
        "{shown}"
    );
}

#[test]
fn everything_else_stays_live_while_a_relay_needs_a_login_or_is_unreachable() {
    let mut application = application_looking_at_studio();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), std::path::Path::new("."), 1),
        ))
        .expect("attach a Session on the Remote");
    push(
        &mut application,
        1,
        vec![lapsed(COMPANY, Some(FIRST)), unreachable(HOME)],
    );
    answer_claim(&mut application, COMPANY, FIRST, true);
    studio_stops_answering_because(
        &mut application,
        2,
        Duration::from_secs(4),
        Some(UnreachableReason::RelayLoginNeeded {
            relay: COMPANY.to_owned(),
        }),
    );
    grace_elapses(&mut application, Outlook::Remote("studio".to_owned()));

    // The keystroke that dismisses the Notice still types, and the composer
    // holds what the reader writes.
    type_terminal_text(&mut application, "unfinished thought");
    let screen = rendered_application_rows(&application).join("\n");
    assert!(screen.contains("unfinished thought"), "{screen}");
    assert!(
        !screen.contains("Reconnecting to Suru"),
        "neither a Relay nor a Remote takes the Client down: {screen}"
    );

    // A surface opened over the Remote's Session owns its keys, though the
    // Session's own panels wait out of sight: Settings, and the Relay list
    // the Session's offer to try again opens.
    for open in [
        SemanticCommandId::SettingsOpen,
        SemanticCommandId::RemoteRetry,
    ] {
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(open)))
            .expect("open stays live");
        let opened = rendered_application_rows(&application).join("\n");
        press(&mut application, KeyCode::Esc);
        let closed = rendered_application_rows(&application).join("\n");
        assert_ne!(opened, closed, "Esc reaches what {open:?} opened");
        assert!(
            !closed.contains("is unreachable · login needed"),
            "and is not taken for work bound for the Remote: {closed}"
        );
    }
    for _ in 0..2 {
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SidebarToggle,
            )))
            .expect("the Sidebar stays live");
    }

    // Turning the Outlook back to this machine's own Server, work begins
    // there as ever.
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ConnectOpen,
        )))
        .expect("open the Connect picker");
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![
            suru::protocol::Remote {
                name: "studio".to_owned(),
                fingerprint: "studio-fingerprint".to_owned(),
                ways: vec![suru::protocol::Way::Relay(COMPANY.to_owned())],
                status: suru::protocol::RemoteStatus::Available,
            },
        ]))
        .expect("list the paired Remotes");
    press(&mut application, KeyCode::Up);
    press(&mut application, KeyCode::Enter);
    type_terminal_text(&mut application, "local work begins");
    assert_ne!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("submit on the local Landing"),
        ApplicationTransition::Continue,
        "a Relay needing a login blocks nothing on this machine's own Server"
    );

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::ApplicationExit,
            )))
            .expect("exit stays live"),
        ApplicationTransition::Exit
    );
}

/// The Server's answer to the Client's claim of the Notice of the Relay at
/// `address` coming to need a login in `lapse`: whether it gave the Client
/// that Notice.
fn answer_claim(application: &mut Application, address: &str, lapse: Uuid, claimed: bool) {
    application
        .handle_event(ApplicationEvent::RelayLoginNeededNoticeClaimed {
            address: address.to_owned(),
            lapse,
            claimed,
        })
        .expect("take the answer to the claim");
}

/// The Server pushes its Relays as they stood at `revision`.
fn push(application: &mut Application, revision: u64, relays: Vec<Relay>) {
    push_from(application, LATER, revision, relays);
}

/// The run of the Server that is `instance` pushes its Relays as they stood
/// at `revision`.
fn push_from(application: &mut Application, instance: Uuid, revision: u64, relays: Vec<Relay>) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Relays(
            RelayListing {
                instance,
                revision,
                relays,
            },
        )))
        .expect("take the pushed Relays");
}

/// Types `/relay`, answering the listing it asks for.
fn open(application: &mut Application) -> RelayRequest {
    type_terminal_text(application, "/relay");
    let ApplicationTransition::ListRelays(request) = press(application, KeyCode::Enter) else {
        panic!("/relay asks the Client's own Server for its Relays");
    };
    request
}

fn list(
    application: &mut Application,
    request: RelayRequest,
    revision: u64,
    relays: Vec<Relay>,
) -> ApplicationTransition {
    list_from(application, request, LATER, revision, relays)
}

fn list_from(
    application: &mut Application,
    request: RelayRequest,
    instance: Uuid,
    revision: u64,
    relays: Vec<Relay>,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::RelaysListed {
            request,
            listing: RelayListing {
                instance,
                revision,
                relays,
            },
        })
        .expect("list the Relays")
}

/// Whether the list holds an entry for the Relay at `address`: a row naming
/// it alone, apart from any note that mentions it.
fn listed(application: &Application, address: &str) -> bool {
    rendered_application_rows(application).iter().any(|row| {
        let row = row.trim().trim_matches('│').trim();
        row == address || row == format!("› {address}")
    })
}

/// Takes a key the whole way the run loop takes it.
fn press(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .take_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("take the key")
        .transition
}

/// A press the run loop takes at `column`, `row`.
fn press_at(application: &mut Application, column: u16, row: u16) -> ApplicationTransition {
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("take the press")
}

/// The reader touches the terminal, as any interaction does.
fn interact(application: &mut Application) {
    application.note_interaction(&InputEvent::Key(KeyEvent::new(
        KeyCode::F(12),
        KeyModifiers::NONE,
    )));
}

/// What the screen says, its wrapped Rows read on as one line of prose.
fn prose(application: &Application) -> String {
    buffer_rows(&rendered_application_buffer(application, 120, 24))
        .iter()
        .map(|row| row.trim().trim_matches('│').trim())
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The keys the open Relay list teaches, which stand last in its box.
/// The keys the Relay list teaches, however many Rows they wrap across.
fn keys_taught(application: &Application) -> String {
    let rows = rendered_application_rows(application)
        .iter()
        .map(|row| row.trim().trim_matches('│').trim().to_owned())
        .collect::<Vec<_>>();
    let Some(first) = rows.iter().rposition(|row| row.starts_with("↑↓ choose")) else {
        return String::new();
    };
    let last = first
        + rows[first..]
            .iter()
            .position(|row| row.ends_with("close"))
            .unwrap_or(0);
    rows[first..=last].join(" ")
}

fn needing_login(address: &str) -> Relay {
    Relay {
        address: address.to_owned(),
        state: RelayState::LoginNeeded,
        unreachable: None,
        account: None,
        login: None,
        serve_through: false,
        login_needed_notice: None,
    }
}

/// A Relay whose Login stood and is now refused, the Server asking a Notice
/// of the lapse `notice` names, where it names one.
fn lapsed(address: &str, notice: Option<Uuid>) -> Relay {
    Relay {
        login_needed_notice: notice,
        ..needing_login(address)
    }
}

fn logged_in(address: &str) -> Relay {
    Relay {
        state: RelayState::LoggedIn,
        account: Some(octocat()),
        ..needing_login(address)
    }
}

fn unreachable(address: &str) -> Relay {
    Relay {
        state: RelayState::Unreachable,
        unreachable: Some(RelayUnreachable {
            behind: None,
            message: "the Relay did not answer".to_owned(),
        }),
        ..logged_in(address)
    }
}

fn octocat() -> RelayAccount {
    RelayAccount {
        provider: "github".to_owned(),
        username: "octocat".to_owned(),
    }
}

fn pending() -> RelayLogin {
    RelayLogin {
        verification_uri: "https://github.com/login/device".to_owned(),
        user_code: CODE.to_owned(),
        outcome: RelayLoginOutcome::Pending,
    }
}
