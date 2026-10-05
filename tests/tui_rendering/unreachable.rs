//! A Remote that stops answering while the Outlook is turned toward it.
//!
//! Losing a Remote is scoped to that Remote: everything the Client offers that
//! does not touch it stays as live as it ever was, and only the local Server's
//! own loss still stands the whole frame down.

use crate::support::{
    application_looking_at_studio, grace_elapses, navigable_session_snapshot,
    rendered_application_rows, rendered_application_rows_at, studio_stops_answering,
    studio_stops_answering_because, type_terminal_text,
};
use crossterm::event::KeyCode;
use std::time::Duration;
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        Outlook, Relay, RelayAccount, RelayListing, RelayLogin, RelayLoginOutcome, RelayState,
        SessionId, UnreachableReason,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

fn studio() -> Outlook {
    Outlook::Remote("studio".to_owned())
}

#[test]
fn a_remote_that_stops_answering_never_raises_the_whole_frame_modal() {
    let mut application = application_looking_at_studio();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), std::path::Path::new("."), 1),
        ))
        .expect("attach the Remote Session");
    type_terminal_text(&mut application, "unfinished thought");

    studio_stops_answering(&mut application, 3, Duration::from_secs(4));
    grace_elapses(&mut application, studio());

    let screen = rendered_application_rows(&application).join("\n");
    assert!(
        !screen.contains("Reconnecting to Suru"),
        "only the local Server's own loss stands the whole frame down: {screen}"
    );
    assert!(
        screen.contains("unfinished thought"),
        "the composer keeps what the reader wrote: {screen}"
    );
}

#[test]
fn the_banner_names_the_remote_above_the_composer_and_leaves_when_it_answers() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 3, Duration::from_secs(4));

    let before_grace = rendered_application_rows(&application).join("\n");
    assert!(
        !before_grace.contains("unreachable"),
        "a loss inside the grace period is not drawn at all: {before_grace}"
    );

    grace_elapses(&mut application, studio());
    let rows = rendered_application_rows_at(&application, 80, 15);
    let banner = rows
        .iter()
        .position(|row| row.contains("studio is unreachable"))
        .unwrap_or_else(|| panic!("draw the unreachable banner: {rows:?}"));
    assert!(
        rows[banner].contains("retrying in 4s (attempt 3)") && rows[banner].contains("Try again"),
        "the banner reports the schedule and offers a retry: {:?}",
        rows[banner]
    );
    let composer_top = rows
        .iter()
        .position(|row| row.contains('┌'))
        .expect("draw the composer");
    assert_eq!(
        banner + 1,
        composer_top,
        "the banner stands directly above the composer: {rows:?}"
    );

    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::RemoteRecovered,
        })
        .expect("take the Remote answering again");
    let recovered = rendered_application_rows(&application).join("\n");
    assert!(
        !recovered.contains("is unreachable"),
        "reconnection is silent: the banner simply leaves: {recovered}"
    );
}

/// Why `studio` cannot be reached: the Relay it is reached through joins no
/// more connections at once for the Account.
fn relay_cap_reached() -> UnreachableReason {
    UnreachableReason::RelayCapReached {
        relay: "https://relay.company.example".to_owned(),
        limit: 256,
    }
}

/// `rows` as one run of prose, each trimmed of what frames it.
fn prose(rows: &[String]) -> String {
    rows.iter()
        .map(|row| row.trim().trim_matches('│').trim())
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn a_remote_its_relay_joins_nothing_more_for_says_which_cap_and_what_to_do_until_it_answers() {
    let mut application = application_looking_at_studio();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), std::path::Path::new("."), 1),
        ))
        .expect("attach the Remote Session");
    studio_stops_answering_because(
        &mut application,
        3,
        Duration::from_secs(4),
        Some(relay_cap_reached()),
    );
    grace_elapses(&mut application, studio());

    for width in [80, 56] {
        let rows = rendered_application_rows_at(&application, width, 15);
        let banner = rows
            .iter()
            .position(|row| row.contains("studio is unreachable"))
            .unwrap_or_else(|| panic!("draw the unreachable banner: {rows:?}"));
        let composer_top = rows
            .iter()
            .position(|row| row.contains('┌'))
            .expect("draw the composer");
        assert_eq!(banner + 1, composer_top, "{rows:?}");
        let why = prose(&rows[..banner]);
        assert!(
            why.contains(
                "Your Account has reached the Relay's cap of 256 connections joined at once at \
                 https://relay.company.example"
            ) && why.contains("ask the Relay's operator to raise the cap"),
            "the cap, its limit, the Relay and what to do are said above the banner: {rows:?}"
        );
    }
    let rows = rendered_application_rows_at(&application, 80, 15);
    assert!(
        rows.iter()
            .any(|row| row
                .contains("studio is unreachable · retrying in 4s (attempt 3) · Try again")),
        "the banner itself reads as it does for any loss: {rows:?}"
    );

    // What the reader asks of the Remote meanwhile is refused naming the cap.
    type_terminal_text(&mut application, "words worth keeping");
    application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("refuse the Prompt");
    let screen = prose(&rendered_application_rows_at(&application, 120, 15));
    assert!(
        screen.contains("Error: studio is unreachable · Relay cap reached: 256 joined connections"),
        "{screen}"
    );

    // A later loss for no reason the reader can act on says no more than
    // that, and the Remote answering again takes everything away.
    studio_stops_answering(&mut application, 4, Duration::from_secs(8));
    let rows = rendered_application_rows_at(&application, 80, 15);
    let banner = rows
        .iter()
        .position(|row| row.contains("studio is unreachable · retrying in 8s (attempt 4)"))
        .unwrap_or_else(|| panic!("draw the unreachable banner: {rows:?}"));
    assert!(!prose(&rows[..=banner]).contains("Relay's cap"), "{rows:?}");
    studio_stops_answering_because(
        &mut application,
        5,
        Duration::from_secs(8),
        Some(relay_cap_reached()),
    );
    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::RemoteRecovered,
        })
        .expect("take the Remote answering again");
    let recovered = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(
        !recovered.contains("retrying in") && !recovered.contains("Relay's cap"),
        "the banner and why it stood leave together: {recovered}"
    );
}

/// Why `studio` cannot be reached: the Relay it is reached through joins
/// this Server, logged in there as `octocat`, to nothing but a Server logged
/// in there under the same Account, and `studio`'s stands under another.
fn relay_accounts_differ() -> UnreachableReason {
    UnreachableReason::RelayDifferentAccounts {
        relay: "https://relay.company.example".to_owned(),
        account: RelayAccount {
            provider: "github".to_owned(),
            username: "octocat".to_owned(),
        },
    }
}

#[test]
fn a_remote_its_relay_joins_under_another_account_says_so_and_what_to_do_until_it_answers() {
    let mut application = application_looking_at_studio();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), std::path::Path::new("."), 1),
        ))
        .expect("attach the Remote Session");
    studio_stops_answering_because(
        &mut application,
        3,
        Duration::from_secs(4),
        Some(relay_accounts_differ()),
    );
    grace_elapses(&mut application, studio());

    // Why, and what to do, are said in full above the banner however narrow
    // the terminal, the Account this Server stands under among them.
    for width in [80, 48] {
        let rows = rendered_application_rows_at(&application, width, 15);
        let banner = rows
            .iter()
            .position(|row| row.contains("studio is unreachable"))
            .unwrap_or_else(|| panic!("draw the unreachable banner: {rows:?}"));
        let why = prose(&rows[..banner]);
        assert!(
            why.contains(
                "Accounts differ at the Relay at https://relay.company.example: this Server is \
                 logged in there as octocat (github) and the Remote under another Account, and a \
                 Relay joins only Servers logged in under the same one, so log this Server in \
                 there as the user the Remote is logged in as, or pair the two directly"
            ),
            "the Relay, the Account and what to do are said at {width} columns: {rows:?}"
        );
    }
    // Its offer tries again, as for any loss: no login is to be begun where
    // this Server already stands.
    let rows = rendered_application_rows_at(&application, 80, 15);
    assert!(
        rows.iter()
            .any(|row| row
                .contains("studio is unreachable · retrying in 4s (attempt 3) · Try again")),
        "{rows:?}"
    );
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::RemoteRetry,
            )))
            .expect("take the offer by key"),
        ApplicationTransition::RetryCatalogOrigin(_)
    ));

    // What the reader asks of the Remote meanwhile is refused saying so.
    type_terminal_text(&mut application, "words worth keeping");
    application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("refuse the Prompt");
    let screen = prose(&rendered_application_rows_at(&application, 120, 15));
    assert!(
        screen.contains(
            "Error: studio is unreachable · Accounts differ at https://relay.company.example"
        ),
        "{screen}"
    );

    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::RemoteRecovered,
        })
        .expect("take the Remote answering again");
    let recovered = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(
        !recovered.contains("retrying in") && !recovered.contains("Accounts differ at the Relay"),
        "the banner and why it stood leave together: {recovered}"
    );
}

#[test]
fn the_status_line_says_the_accounts_differ_at_the_relay_a_remote_is_unreachable_through() {
    let mut application = application_looking_at_studio();
    studio_stops_answering_because(
        &mut application,
        2,
        Duration::from_secs(7),
        Some(relay_accounts_differ()),
    );
    grace_elapses(&mut application, studio());
    let reported = rendered_application_rows_at(&application, 120, 15);
    let status = reported.last().expect("draw a status line");
    assert!(
        status.contains(
            "studio is unreachable · Accounts differ at https://relay.company.example (attempt 2, \
             retry in 7s)"
        ),
        "{status}"
    );
}

/// Why `studio` cannot be reached: the Relay it is reached through joins
/// nothing for this Server until it logs in there.
fn relay_login_needed() -> UnreachableReason {
    UnreachableReason::RelayLoginNeeded {
        relay: "https://relay.company.example".to_owned(),
    }
}

/// The Relay `studio` is reached through, as the Server pushes it.
fn company_relay(state: RelayState) -> Relay {
    Relay {
        address: "https://relay.company.example".to_owned(),
        state,
        unreachable: None,
        account: None,
        login: None,
        serve_through: false,
        login_needed_notice: None,
    }
}

/// The run of the Client's own Server the tests' listings come from.
const SERVER: uuid::Uuid = uuid::Uuid::from_u128(7);

fn push_relays(application: &mut Application, revision: u64, relays: Vec<Relay>) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Relays(
            RelayListing {
                instance: SERVER,
                revision,
                relays,
            },
        )))
        .expect("take the pushed Relays");
}

#[test]
fn a_remote_out_of_reach_for_want_of_a_login_says_so_and_its_offer_leads_to_the_login() {
    let mut application = application_looking_at_studio();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), std::path::Path::new("."), 1),
        ))
        .expect("attach the Remote Session");
    studio_stops_answering_because(
        &mut application,
        3,
        Duration::from_secs(4),
        Some(relay_login_needed()),
    );
    grace_elapses(&mut application, studio());

    // Unreachable like any other, its offer to try again saying a login is
    // needed, and why said in full above it however narrow the terminal.
    for width in [80, 56] {
        let rows = rendered_application_rows_at(&application, width, 15);
        let banner = rows
            .iter()
            .position(|row| row.contains("studio is unreachable"))
            .unwrap_or_else(|| panic!("draw the unreachable banner: {rows:?}"));
        let why = prose(&rows[..banner]);
        assert!(
            why.contains(
                "Login needed at the Relay at https://relay.company.example: it joins this \
                 Server to nothing until this Server logs in there, so log in to try again"
            ),
            "{rows:?}"
        );
    }
    let rows = rendered_application_rows_at(&application, 100, 15);
    assert!(
        rows.iter().any(|row| row
            .contains("studio is unreachable · retrying in 4s (attempt 3) · Log in to try again")),
        "{rows:?}"
    );

    // The offer, by key, opens the Relay list and logs in there once it
    // lands, rather than trying again at once.
    let ApplicationTransition::ListRelays(listing) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::RemoteRetry,
        )))
        .expect("take the offer by key")
    else {
        panic!("the offer leads to the Relay list, not a retry");
    };
    let transition = application
        .handle_event(ApplicationEvent::RelaysListed {
            request: listing,
            listing: RelayListing {
                instance: SERVER,
                revision: 1,
                relays: vec![company_relay(RelayState::LoginNeeded)],
            },
        })
        .expect("list the Relays");
    let ApplicationTransition::BeginRelayLogin { address, .. } = transition else {
        panic!("the list logs in at the Relay the Remote waits on: {transition:?}");
    };
    assert_eq!(address, "https://relay.company.example");

    // And by pointer, the same.
    crate::support::key(&mut application, KeyCode::Esc);
    let buffer = crate::support::rendered_application_buffer(&application, 100, 15);
    let (column, row) = crate::support::text_position(&buffer, "Log in to try again");
    let pressed = crate::support::click_mouse(
        &mut application,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
    )
    .expect("take the offer by pointer");
    assert!(
        matches!(pressed, ApplicationTransition::ListRelays(_)),
        "{pressed:?}"
    );

    // Anything asked of the Remote meanwhile is refused saying so too.
    crate::support::key(&mut application, KeyCode::Esc);
    type_terminal_text(&mut application, "words worth keeping");
    application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("refuse the Prompt");
    let screen = prose(&rendered_application_rows_at(&application, 120, 15));
    assert!(
        screen.contains(
            "Error: studio is unreachable · login needed at https://relay.company.example"
        ),
        "{screen}"
    );
}

#[test]
fn the_status_line_says_a_login_is_needed_at_the_relay_a_remote_waits_on() {
    let mut application = application_looking_at_studio();
    studio_stops_answering_because(
        &mut application,
        2,
        Duration::from_secs(7),
        Some(relay_login_needed()),
    );
    grace_elapses(&mut application, studio());
    let reported = rendered_application_rows_at(&application, 140, 15);
    let status = reported.last().expect("draw a status line");
    assert!(
        status.contains(
            "studio is unreachable · login needed at https://relay.company.example (attempt 2, \
             retry in 7s)"
        ),
        "{status}"
    );
}

/// A login the offer led to, done, has the Remote tried again at once rather
/// than on its schedule, which may be seconds off yet.
#[test]
fn a_login_the_offer_led_to_has_the_remote_tried_again_at_once_once_done() {
    let mut application = application_looking_at_studio();
    studio_stops_answering_because(
        &mut application,
        5,
        Duration::from_secs(5),
        Some(relay_login_needed()),
    );
    let ApplicationTransition::ListRelays(listing) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::RemoteRetry,
        )))
        .expect("take the offer")
    else {
        panic!("the offer leads to the Relay list");
    };
    let ApplicationTransition::BeginRelayLogin { request, .. } = application
        .handle_event(ApplicationEvent::RelaysListed {
            request: listing,
            listing: RelayListing {
                instance: SERVER,
                revision: 1,
                relays: vec![company_relay(RelayState::LoginNeeded)],
            },
        })
        .expect("list the Relays")
    else {
        panic!("the list logs in at the Relay");
    };
    let login = RelayLogin {
        verification_uri: "https://github.com/login/device".to_owned(),
        user_code: "WDJB-MJHT".to_owned(),
        outcome: RelayLoginOutcome::Pending,
    };
    let ApplicationTransition::FollowRelayLogins(follows) = application
        .handle_event(ApplicationEvent::RelayLoginBegun {
            request,
            login: login.clone(),
        })
        .expect("take the begun login")
    else {
        panic!("the login is followed");
    };
    assert!(
        application.take_relay_retries().is_empty(),
        "nothing is tried again while the login is under way"
    );

    let settled = application
        .handle_event(ApplicationEvent::RelayLoginSettled {
            request: follows[0].request,
            login: RelayLogin {
                outcome: RelayLoginOutcome::Done {
                    account: RelayAccount {
                        provider: "github".to_owned(),
                        username: "octocat".to_owned(),
                    },
                },
                ..login
            },
        })
        .expect("take the login done");
    // The Client hears the Login stands again as the Relays, asked for
    // afresh, are listed — or as they are pushed, whichever comes first.
    let ApplicationTransition::ListRelays(refresh) = settled else {
        panic!("a login ended asks for the Relays afresh: {settled:?}");
    };
    application
        .handle_event(ApplicationEvent::RelaysListed {
            request: refresh,
            listing: RelayListing {
                instance: SERVER,
                revision: 2,
                relays: vec![company_relay(RelayState::LoggedIn)],
            },
        })
        .expect("list the Relays");
    let retries = application.take_relay_retries();
    let [ApplicationTransition::RetryCatalogOrigin(retry)] = retries.as_slice() else {
        panic!("the Remote waiting on the Relay is tried again at once: {retries:?}");
    };
    assert_eq!(retry.outlook(), &studio());
    assert!(application.take_relay_retries().is_empty(), "and once only");
}

#[test]
fn once_its_relay_is_logged_in_at_again_the_remote_offers_to_try_again() {
    let mut application = application_looking_at_studio();
    push_relays(
        &mut application,
        1,
        vec![company_relay(RelayState::LoginNeeded)],
    );
    studio_stops_answering_because(
        &mut application,
        3,
        Duration::from_secs(4),
        Some(relay_login_needed()),
    );
    grace_elapses(&mut application, studio());
    assert!(
        rendered_application_rows_at(&application, 100, 15)
            .iter()
            .any(|row| row.contains("Log in to try again"))
    );

    // Logged in at again — from this Client or any Server of the Account —
    // the Relay is no longer why, though the Remote has yet to be tried.
    push_relays(
        &mut application,
        2,
        vec![company_relay(RelayState::LoggedIn)],
    );
    let rows = rendered_application_rows_at(&application, 100, 15);
    let banner = rows
        .iter()
        .position(|row| row.contains("studio is unreachable"))
        .unwrap_or_else(|| panic!("draw the unreachable banner: {rows:?}"));
    assert!(
        rows[banner].contains("· Try again") && !rows[banner].contains("Log in"),
        "{rows:?}"
    );
    assert!(!prose(&rows).contains("Login needed"), "{rows:?}");
    let ApplicationTransition::RetryCatalogOrigin(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::RemoteRetry,
        )))
        .expect("try the Remote again")
    else {
        panic!("with the Login standing again, the offer tries the Remote again at once");
    };
    assert_eq!(request.outlook(), &studio());
}

#[test]
fn a_relay_the_server_holds_no_entry_for_is_offered_to_be_added_and_logged_in_at() {
    let mut application = application_looking_at_studio();
    studio_stops_answering_because(
        &mut application,
        1,
        Duration::from_secs(5),
        Some(relay_login_needed()),
    );
    let ApplicationTransition::ListRelays(listing) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::RemoteRetry,
        )))
        .expect("take the offer")
    else {
        panic!("the offer leads to the Relay list");
    };
    application
        .handle_event(ApplicationEvent::RelaysListed {
            request: listing,
            listing: RelayListing {
                instance: SERVER,
                revision: 1,
                relays: Vec::new(),
            },
        })
        .expect("list the Relays");
    let screen = rendered_application_rows(&application).join("\n");
    assert!(
        screen.contains("Add a Relay") && screen.contains("> https://relay.company.example"),
        "the Relay is ready to be added, its address typed in: {screen}"
    );
    let added = crate::support::key(&mut application, KeyCode::Enter);
    assert!(
        matches!(
            &added,
            ApplicationTransition::AddRelay { address, .. }
                if address == "https://relay.company.example"
        ),
        "{added:?}"
    );
}

#[test]
fn the_status_line_names_the_cap_a_remote_is_unreachable_for() {
    let mut application = application_looking_at_studio();
    studio_stops_answering_because(
        &mut application,
        2,
        Duration::from_secs(7),
        Some(relay_cap_reached()),
    );
    grace_elapses(&mut application, studio());
    let reported = rendered_application_rows_at(&application, 120, 15);
    let status = reported.last().expect("draw a status line");
    assert!(
        status.contains(
            "studio is unreachable · Relay cap reached: 256 joined connections (attempt 2, \
             retry in 7s)"
        ),
        "the status line names the cap before the schedule: {status}"
    );
}

#[test]
fn a_prompt_to_an_unreachable_origin_is_refused_in_view_and_keeps_its_draft() {
    let mut application = application_looking_at_studio();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), std::path::Path::new("."), 1),
        ))
        .expect("attach the Remote Session");
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    type_terminal_text(&mut application, "words worth keeping");

    // Inside the grace period, before anything is drawn about the loss.
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("refuse the Prompt"),
        ApplicationTransition::Continue,
        "a Prompt bound for an Unreachable Origin never leaves"
    );
    let screen = rendered_application_rows(&application).join("\n");
    assert!(
        screen.contains("studio") && screen.contains("unreachable"),
        "the refusal names the Remote where the reader can see it: {screen}"
    );
    assert!(
        screen.contains("words worth keeping"),
        "the draft stays in the composer: {screen}"
    );
}

#[test]
fn the_client_stays_live_around_an_unreachable_remote() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    grace_elapses(&mut application, studio());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SidebarToggle,
        )))
        .expect("the Sidebar stays live");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SettingsOpen,
        )))
        .expect("Settings stays live");
    let settings = rendered_application_rows(&application).join("\n");
    assert!(
        settings.contains("Settings"),
        "the settings panel opens over an Unreachable Remote: {settings}"
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SettingsClose,
        )))
        .expect("close Settings");

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::ApplicationExit,
            )))
            .expect("exit stays live"),
        ApplicationTransition::Exit,
    );
}

#[test]
fn exit_passes_even_under_the_local_servers_own_modal() {
    let mut application = suru::tui::Application::default();
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            suru::managed_client::RecoveryStatus {
                attempt: 1,
                retry_in: std::time::Duration::from_secs(1),
                unreachable: None,
            },
        )))
        .expect("lose the local Server");
    grace_elapses(&mut application, Outlook::Local);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Reconnecting to Suru"),
        "the local Server's own loss still stands the whole frame down"
    );

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::ApplicationExit,
            )))
            .expect("exit under the modal"),
        ApplicationTransition::Exit,
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
            .expect("clear-or-exit under the modal"),
        ApplicationTransition::Exit,
    );
}

#[test]
fn turning_the_outlook_away_from_an_unreachable_remote_still_works() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    grace_elapses(&mut application, studio());

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
                ways: vec![suru::protocol::Way::Direct(
                    "10.0.0.8:7777".parse().expect("parse the address"),
                )],
                status: suru::protocol::RemoteStatus::Available,
            },
        ]))
        .expect("list the paired Remotes");
    let picker = rendered_application_rows(&application).join("\n");
    assert!(
        picker.contains("studio"),
        "the Connect picker is drawn over an Unreachable Remote: {picker}"
    );
    crate::support::key(&mut application, KeyCode::Up);
    let transition = crate::support::key(&mut application, KeyCode::Enter);
    assert!(
        matches!(transition, ApplicationTransition::TurnOutlook { .. }),
        "turning the Outlook back to Local is the reader's own to do: {transition:?}"
    );
}

#[test]
fn the_banners_try_again_invokes_the_command_the_sidebar_row_invokes() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 2, Duration::from_secs(3));
    grace_elapses(&mut application, studio());

    // A key naming no Remote means the one the Outlook is turned toward.
    let ApplicationTransition::RetryCatalogOrigin(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::RemoteRetry,
        )))
        .expect("retry by key")
    else {
        panic!("a retry restarts the Remote's catalog stream and asks it afresh");
    };
    assert_eq!(request.outlook(), &studio());

    studio_stops_answering(&mut application, 3, Duration::from_secs(3));
    grace_elapses(&mut application, studio());
    let buffer = crate::support::rendered_application_buffer(&application, 80, 15);
    let (column, row) = crate::support::text_position(&buffer, "Try again");
    let ApplicationTransition::RetryCatalogOrigin(clicked) = crate::support::click_mouse(
        &mut application,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
    )
    .expect("retry by pointer") else {
        panic!("a press on the banner's affordance retries the same Remote");
    };
    assert_eq!(clicked.outlook(), &studio());
}

#[test]
fn an_unreachable_remote_is_marked_and_dimmed_outside_everywhere() {
    let mut application = application_looking_at_studio();
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SidebarToggle,
        )))
        .expect("reveal the Sidebar")
    else {
        panic!("revealing the Sidebar asks its Origin for Sessions");
    };
    assert_eq!(
        request.scope(),
        &suru::tui::SessionListScope::AllWorkspaces,
        "the Sidebar opens on a scope narrower than Everywhere"
    );
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![listed("Studio work")],
        })
        .expect("hydrate the Sidebar");

    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    let buffer = crate::support::rendered_application_buffer(&application, 120, 20);
    let rows = crate::support::buffer_rows(&buffer);
    assert!(
        rows.iter().any(|row| row.contains("studio [unreachable]")),
        "the unreachable row stands in every scope, not only Everywhere: {rows:?}"
    );
    let work = crate::support::text_position(&buffer, "Studio work");
    assert_eq!(
        buffer.cell(work).expect("draw the Studio row").fg,
        ratatui::style::Color::DarkGray,
        "the Remote's cached rows are dimmed in every scope"
    );
}

#[test]
fn the_sidebar_still_walks_and_reads_while_its_remote_is_unreachable() {
    let mut application = application_looking_at_studio();
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SidebarToggle,
        )))
        .expect("reveal the Sidebar")
    else {
        panic!("revealing the Sidebar asks its Origin for Sessions");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![listed("Studio work")],
        })
        .expect("hydrate the Sidebar");
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    grace_elapses(&mut application, studio());

    crate::support::key(&mut application, KeyCode::Down);
    crate::support::key(&mut application, KeyCode::Down);
    let opened = crate::support::key(&mut application, KeyCode::Enter);
    assert!(
        matches!(
            opened,
            ApplicationTransition::ViewSession(_) | ApplicationTransition::ViewAndAttachSession(_)
        ),
        "a Session of an Unreachable Remote may still be opened and read: {opened:?}"
    );
}

#[test]
fn local_work_is_untouched_while_a_remote_is_unreachable() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    grace_elapses(&mut application, studio());

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
                ways: vec![suru::protocol::Way::Direct(
                    "10.0.0.8:7777".parse().expect("parse the address"),
                )],
                status: suru::protocol::RemoteStatus::Available,
            },
        ]))
        .expect("list the paired Remotes");
    crate::support::key(&mut application, KeyCode::Up);
    crate::support::key(&mut application, KeyCode::Enter);

    type_terminal_text(&mut application, "local work begins");
    let begun = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit on the local Landing");
    assert_ne!(
        begun,
        ApplicationTransition::Continue,
        "a Remote that stopped answering blocks nothing on another Server"
    );
}

#[test]
fn an_intervention_waits_out_of_sight_until_its_remote_answers() {
    let workspace = crate::support::workspace_dir();
    let mut application = application_looking_at_studio();
    let (_, mut snapshot, turn_id) =
        crate::support::enter_active_session(&mut application, workspace.path());
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));

    pend_an_approval(&mut snapshot, turn_id);
    application
        .handle_event(ApplicationEvent::Session(
            suru::managed_client::SessionEvent::snapshot(snapshot.clone()),
        ))
        .expect("deliver the Session snapshot");

    let held = rendered_application_rows(&application).join("\n");
    assert!(
        !held.contains("Approval · Choose Decision"),
        "an Intervention whose Origin cannot be reached waits out of sight: {held}"
    );

    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::RemoteRecovered,
        })
        .expect("take the Remote answering again");
    let presented = rendered_application_rows(&application).join("\n");
    assert!(
        presented.contains("Approval · Choose Decision"),
        "the Intervention presents itself as soon as the Remote answers: {presented}"
    );
}

#[test]
fn the_outlook_stays_turned_toward_a_remote_that_merely_stops_answering() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    grace_elapses(&mut application, studio());

    let landing = rendered_application_rows(&application).join("\n");
    assert!(
        landing.contains("studio · ."),
        "a Remote that becomes Unreachable does not turn the Outlook: {landing}"
    );
}

fn listed(title: &str) -> suru::protocol::SessionListItem {
    crate::support::listed_session(
        SessionId::new(),
        title,
        &crate::support::named_workspace_path("studio"),
        1,
        1,
    )
}

#[test]
fn what_reaches_into_an_unreachable_sessions_turn_is_refused_too() {
    for command in [
        CommandId::RequestInterrupt,
        CommandId::ConfirmInterrupt,
        CommandId::PromoteSelectedPrompt,
        CommandId::CancelSelectedPrompt,
    ] {
        let workspace = crate::support::workspace_dir();
        let mut application = application_looking_at_studio();
        crate::support::enter_active_session(&mut application, workspace.path());
        studio_stops_answering(&mut application, 1, Duration::from_secs(5));

        assert_eq!(
            application
                .handle_event(ApplicationEvent::Command(command.clone()))
                .expect("refuse work bound for the Session's own Server"),
            ApplicationTransition::Continue,
            "{command:?} is asked of the Session's Server"
        );
        let screen = rendered_application_rows(&application).join("\n");
        assert!(
            screen.contains("studio is unreachable"),
            "{command:?} is refused where the reader can see it: {screen}"
        );
    }
}

#[test]
fn a_held_intervention_does_not_advertise_a_key_that_would_be_refused() {
    let workspace = crate::support::workspace_dir();
    let mut application = application_looking_at_studio();
    let (_, mut snapshot, turn_id) =
        crate::support::enter_active_session(&mut application, workspace.path());
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    pend_an_approval(&mut snapshot, turn_id);
    application
        .handle_event(ApplicationEvent::Session(
            suru::managed_client::SessionEvent::snapshot(snapshot.clone()),
        ))
        .expect("deliver the Session snapshot");

    let held = rendered_application_rows(&application).join("\n");
    assert!(
        !held.contains("Ctrl+Y decide"),
        "an Intervention waiting out of sight names no key to answer it: {held}"
    );

    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::RemoteRecovered,
        })
        .expect("take the Remote answering again");
    // The panel presents itself rather than leaving a notice standing, which
    // is the whole of what the notice was offering.
    let answered = rendered_application_rows(&application).join("\n");
    assert!(
        answered.contains("Approval · Choose Decision"),
        "the Intervention presents itself once the Remote answers: {answered}"
    );
}

#[test]
fn the_banners_retry_works_with_the_sidebar_narrowed_away_from_the_remote() {
    let mut application = application_looking_at_studio();
    // No Sidebar has been revealed and no listing asked for, so the column
    // holds nothing at all about this Remote.
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    grace_elapses(&mut application, studio());

    let ApplicationTransition::RetryCatalogOrigin(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::RemoteRetry,
        )))
        .expect("retry the Remote the Outlook is turned toward")
    else {
        panic!("the banner's retry does not depend on the Sidebar listing the Remote");
    };
    assert_eq!(request.outlook(), &studio());

    // A Remote that is answering has nothing to retry.
    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::RemoteRecovered,
        })
        .expect("take the Remote answering again");
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::RemoteRetry,
            )))
            .expect("nothing to retry"),
        ApplicationTransition::Continue,
    );
}

#[test]
fn the_status_line_says_nothing_until_the_grace_runs_out() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 2, Duration::from_secs(7));

    let inside_grace = rendered_application_rows(&application).join("\n");
    assert!(
        !inside_grace.contains("unreachable"),
        "a loss inside its grace is reported nowhere, the status line least of all: {inside_grace}"
    );

    grace_elapses(&mut application, studio());
    let reported = rendered_application_rows_at(&application, 80, 15);
    let status = reported.last().expect("draw a status line");
    assert!(
        status.contains("studio is unreachable (attempt 2, retry in 7s)"),
        "the status line names the Remote and its schedule: {status}"
    );
}

fn pend_an_approval(
    snapshot: &mut suru::protocol::SessionSnapshot,
    turn_id: suru::protocol::TurnId,
) {
    let activity = crate::support::approval_activity(
        turn_id,
        suru::protocol::ApprovalSubject::Network {
            host_or_url: "https://api.example.test/v1".to_owned(),
        },
        None,
        suru::protocol::ApprovalOutcome::Pending,
        None,
    );
    let suru::protocol::Activity::Approval { approval, .. } = &activity else {
        unreachable!("an Approval Activity carries an Approval")
    };
    snapshot.pending_approvals.push(approval.id);
    snapshot.pending_approvals_revision = snapshot.revision;
    crate::support::add_activity(snapshot, activity.clone());
}

#[test]
fn a_fatal_error_that_lowers_the_modal_is_not_overruled_by_a_later_grace() {
    let mut application = application_looking_at_studio();
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            suru::managed_client::RecoveryStatus {
                attempt: 1,
                retry_in: Duration::from_secs(1),
                unreachable: None,
            },
        )))
        .expect("lose the local Server");
    grace_elapses(&mut application, Outlook::Local);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Reconnecting to Suru"),
        "the local Server's own loss stands the whole frame down"
    );

    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::Fatal("the connection cannot be made".to_owned()),
        })
        .expect("take the fatal error");
    let failed = rendered_application_rows(&application).join("\n");
    assert!(
        !failed.contains("Reconnecting to Suru"),
        "a fatal error takes the modal down: {failed}"
    );

    // The recovery goes on retrying beneath it. A grace coming due again must
    // not put the modal back over what the reader is being told.
    grace_elapses(&mut application, Outlook::Local);
    let still_failed = rendered_application_rows(&application).join("\n");
    assert!(
        !still_failed.contains("Reconnecting to Suru"),
        "a grace already served cannot raise the modal over a fatal error: {still_failed}"
    );
    assert!(
        still_failed.contains("the connection cannot be made"),
        "the reader goes on being told what actually happened: {still_failed}"
    );
}

#[test]
fn a_remote_whose_pairing_ends_stops_being_merely_unreachable() {
    let mut application = suru::tui::Application::default();
    // The Outlook is Local throughout; studio is reached only as a background
    // Origin, the way Everywhere keeps it.
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::RemoteFailed {
                status: suru::protocol::RemoteStatus::Revoked,
                message: "the Pairing was ended".to_owned(),
            },
        })
        .expect("take the ended Pairing");

    // Pairing afresh and turning toward it must find a Remote like any other,
    // not one this Client is still holding a lost connection for.
    turn_toward_studio(&mut application);
    assert_ne!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::ModelList,
            )))
            .expect("ask the re-paired Remote for its Models"),
        ApplicationTransition::Continue,
        "a Remote whose Pairing ended is not left standing as Unreachable"
    );
}

#[test]
fn an_open_panel_leaves_the_frame_when_its_origin_stops_answering() {
    let workspace = crate::support::workspace_dir();
    let mut application = application_looking_at_studio();
    let (_, mut snapshot, turn_id) =
        crate::support::enter_active_session(&mut application, workspace.path());
    pend_an_approval(&mut snapshot, turn_id);
    application
        .handle_event(ApplicationEvent::Session(
            suru::managed_client::SessionEvent::snapshot(snapshot.clone()),
        ))
        .expect("deliver the Session snapshot");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Approval · Choose Decision"),
        "the panel presents itself while the Remote is answering"
    );

    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    let held = rendered_application_rows(&application).join("\n");
    assert!(
        !held.contains("Approval · Choose Decision"),
        "a panel no key reaches and no Decision leaves does not stand: {held}"
    );

    // The keys the panel would have taken belong to the composer again, and
    // nothing is typed under a panel that is no longer there.
    type_terminal_text(&mut application, "back in the composer");
    let composing = rendered_application_rows(&application).join("\n");
    assert!(composing.contains("back in the composer"), "{composing}");
    assert!(
        !composing.contains("Approval · Choose Decision"),
        "{composing}"
    );

    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio(),
            event: ManagedEvent::RemoteRecovered,
        })
        .expect("take the Remote answering again");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Approval · Choose Decision"),
        "the Intervention was hidden, not dismissed: it presents itself again"
    );
}

/// Turns the Outlook toward `studio` through the Connect picker, the way a
/// reader does.
fn turn_toward_studio(application: &mut Application) {
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
                ways: vec![suru::protocol::Way::Direct(
                    "10.0.0.8:7777".parse().expect("parse the address"),
                )],
                status: suru::protocol::RemoteStatus::Available,
            },
        ]))
        .expect("list the paired Remotes");
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(suru::protocol::RemoteHealth {
                protocol_version: Some(suru::protocol::PROTOCOL_VERSION),
                status: suru::protocol::RemoteStatus::Available,
                unreachable: None,
            }),
        })
        .expect("probe the Remote");
    crate::support::key(application, KeyCode::Down);
    let turned = crate::support::key(application, KeyCode::Enter);
    assert!(
        matches!(
            turned,
            ApplicationTransition::TurnOutlook {
                outlook: Outlook::Remote(_),
                ..
            }
        ),
        "the fixture turns the Outlook toward studio: {turned:?}"
    );
}

#[test]
fn choosing_an_icon_or_a_worktree_goes_to_the_origin_and_is_refused_with_it() {
    for command in [
        SemanticCommandId::IconPickerChoose,
        SemanticCommandId::WorktreeSelect,
    ] {
        let mut application = application_looking_at_studio();
        studio_stops_answering(&mut application, 1, Duration::from_secs(5));
        assert_eq!(
            application
                .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                    command
                )))
                .expect("refuse work bound for the Remote"),
            ApplicationTransition::Continue,
            "{command:?} is applied on the Origin"
        );
        let screen = rendered_application_rows(&application).join("\n");
        assert!(
            screen.contains("studio is unreachable"),
            "{command:?} is refused where the reader can see it: {screen}"
        );
    }
}

#[test]
fn a_refused_semantic_command_says_so_exactly_once() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelList,
        )))
        .expect("refuse the Remote-bound command");

    let screen = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert_eq!(
        screen.matches("studio is unreachable").count(),
        1,
        "the refusal is decided at one choke point and said once: {screen}"
    );
}
