//! The user's `/relay` command: the list of their own Server's Relays, adding
//! one by address, logging in there, and removing one.

use std::time::Duration;

use crate::support::{
    application_looking_at_studio, grace_elapses, key, rendered_application_rows,
    studio_stops_answering, type_terminal_text,
};
use crossterm::event::KeyCode;
use suru::{
    protocol::{
        Outlook, Relay, RelayAccount, RelayLogin, RelayLoginOutcome, RelayLoginRefusal,
        RelayRemoval, RelayState, RelayUnreachable,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

const COMPANY: &str = "https://relay.example.com";
const HOME: &str = "https://home.example.net";
const LAPSED: &str = "https://lapsed.example.org";
const VISIT: &str = "https://github.com/login/device";
const CODE: &str = "WDJB-MJHT";

#[test]
fn relay_opens_the_list_of_the_servers_relays_with_each_ones_state() {
    let mut application = Application::default();

    type_terminal_text(&mut application, "/relay");
    let completion = rendered_application_rows(&application).join("\n");
    assert!(completion.contains("/relay"), "{completion}");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::ListRelays
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Loading Relays…")
    );

    list(
        &mut application,
        vec![
            logged_in(COMPANY),
            relay(HOME),
            Relay {
                state: RelayState::Unreachable,
                unreachable: Some(RelayUnreachable {
                    behind: None,
                    message: "the Relay did not answer".to_owned(),
                }),
                ..relay(LAPSED)
            },
        ],
    );

    let list = rendered_application_rows(&application).join("\n");
    assert!(list.contains("Relays"), "{list}");
    assert!(
        entry(&list, COMPANY).contains("Logged in as octocat (github)"),
        "{list}"
    );
    assert!(entry(&list, HOME).contains("Login needed"), "{list}");
    assert!(
        entry(&list, LAPSED).contains("Unreachable · the Relay did not answer"),
        "{list}"
    );
    assert!(
        list.contains("a add · Enter log in · x remove · Esc close"),
        "{list}"
    );

    assert_eq!(
        key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains(COMPANY)
    );
}

#[test]
fn a_listing_that_fails_says_why_in_place_of_the_relays() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/relay");
    key(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RelayListingFailed(
            "send Relay listing".to_owned(),
        ))
        .expect("take the failed listing");

    for _ in 0..2 {
        let list = rendered_application_rows(&application).join("\n");
        assert!(list.contains("send Relay listing"), "{list}");
        assert!(
            !list.contains("No Relays added"),
            "a listing that failed says nothing of what the Server holds: {list}"
        );
        // It stands however the reader moves, until a listing lands.
        key(&mut application, KeyCode::Down);
    }
}

#[test]
fn relay_lists_the_clients_own_server_whichever_way_the_outlook_is_turned() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    grace_elapses(&mut application, Outlook::Remote("studio".to_owned()));

    type_terminal_text(&mut application, "/relay");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::ListRelays,
        "the Relays are the Client's own Server's, so a Remote that has stopped \
         answering refuses nothing"
    );
    list(&mut application, vec![logged_in(COMPANY)]);
    let list = rendered_application_rows(&application).join("\n");
    assert!(entry(&list, COMPANY).contains("Logged in"), "{list}");
}

#[test]
fn a_relay_is_added_by_its_address() {
    let mut application = open_on(Vec::new());
    let empty = rendered_application_rows(&application).join("\n");
    assert!(empty.contains("No Relays added"), "{empty}");

    assert_eq!(
        key(&mut application, KeyCode::Char('a')),
        ApplicationTransition::Continue
    );
    let address_entry = rendered_application_rows(&application).join("\n");
    assert!(address_entry.contains("Add a Relay"), "{address_entry}");
    assert!(
        address_entry.contains("Enter add · Esc back"),
        "{address_entry}"
    );

    type_terminal_text(&mut application, COMPANY);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains(COMPANY)
    );
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::AddRelay(COMPANY.to_owned())
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Adding Relay…")
    );

    application
        .handle_event(ApplicationEvent::RelayAdded(relay(COMPANY)))
        .expect("take the added Relay");
    let list = rendered_application_rows(&application).join("\n");
    assert!(entry(&list, COMPANY).contains("Login needed"), "{list}");
    assert!(
        entry(&list, COMPANY).contains('›'),
        "the Relay just added is the one the keys are on, ready to log in: {list}"
    );
    assert!(list.contains(&format!("Added {COMPANY}")), "{list}");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::BeginRelayLogin(COMPANY.to_owned())
    );
}

#[test]
fn an_address_that_cannot_be_used_is_refused_where_the_reader_can_see_it() {
    let mut application = open_on(Vec::new());
    key(&mut application, KeyCode::Char('a'));

    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "an empty address is never sent"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Type a Relay's address first")
    );

    type_terminal_text(&mut application, "relay.example.com");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::AddRelay("relay.example.com".to_owned())
    );
    let refusal = "a Relay's address is an https:// or http:// address naming its host";
    application
        .handle_event(ApplicationEvent::RelayAdditionFailed(refusal.to_owned()))
        .expect("take the refusal");

    let rows = rendered_application_rows(&application);
    let field = rows
        .iter()
        .position(|row| row.contains("relay.example.com"))
        .unwrap_or_else(|| panic!("the address stays as the reader typed it: {rows:#?}"));
    assert!(
        rows[field + 1].contains(refusal),
        "the refusal stands beneath the address it refuses: {rows:#?}"
    );

    key(&mut application, KeyCode::Backspace);
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains(refusal),
        "editing the address puts the refusal down"
    );
    assert_eq!(
        key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("No Relays added"),
        "Esc steps back to the list"
    );
}

#[test]
fn the_login_display_shows_where_to_go_and_the_code_each_copyable_and_resolves_on_its_own() {
    let mut application = open_on(vec![relay(COMPANY)]);

    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::BeginRelayLogin(COMPANY.to_owned())
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Beginning login…")
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::RelayLoginBegun {
                address: COMPANY.to_owned(),
                login: pending(),
            })
            .expect("take the begun login"),
        ApplicationTransition::FollowRelayLogins(vec![COMPANY.to_owned()]),
        "the Client follows the login the Server carries out"
    );

    let display = rendered_application_rows(&application).join("\n");
    assert!(
        display.contains(&format!("Log in at {COMPANY}")),
        "{display}"
    );
    assert!(display.contains(VISIT), "{display}");
    assert!(display.contains(CODE), "{display}");
    assert!(display.contains("Waiting for the login…"), "{display}");
    assert!(
        display.contains("a copy address · c copy code · Esc back"),
        "{display}"
    );

    assert_eq!(
        key(&mut application, KeyCode::Char('a')),
        ApplicationTransition::CopyToClipboard(VISIT.into())
    );
    assert_eq!(
        key(&mut application, KeyCode::Char('c')),
        ApplicationTransition::CopyToClipboard(CODE.into())
    );

    application
        .handle_event(ApplicationEvent::RelayLoginSettled {
            address: COMPANY.to_owned(),
            login: RelayLogin {
                outcome: RelayLoginOutcome::Done { account: octocat() },
                ..pending()
            },
        })
        .expect("take the finished login");
    let resolved = rendered_application_rows(&application).join("\n");
    assert!(
        !resolved.contains(CODE),
        "the display resolves on its own: {resolved}"
    );
    assert!(
        entry(&resolved, COMPANY).contains("Logged in as octocat (github)"),
        "{resolved}"
    );
    assert!(
        resolved.contains(&format!("Logged in at {COMPANY} as octocat")),
        "{resolved}"
    );
}

#[test]
fn a_login_the_relay_refuses_says_the_user_is_not_admitted_and_to_ask_its_operator() {
    let mut application = open_on(vec![relay(COMPANY)]);
    key(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RelayLoginBegun {
            address: COMPANY.to_owned(),
            login: pending(),
        })
        .expect("take the begun login");

    application
        .handle_event(ApplicationEvent::RelayLoginSettled {
            address: COMPANY.to_owned(),
            login: RelayLogin {
                outcome: not_admitted(),
                ..pending()
            },
        })
        .expect("take the refused login");

    let refused = rendered_application_rows(&application).join("\n");
    assert!(!refused.contains(CODE), "{refused}");
    assert!(
        prose(&application).contains(&format!(
            "You are not admitted to {COMPANY}; ask the Relay's operator to admit you"
        )),
        "{refused}"
    );
    assert!(
        entry(&refused, COMPANY).contains("Login needed · not admitted"),
        "{refused}"
    );
}

#[test]
fn a_login_ended_any_other_way_says_why_in_the_servers_words() {
    let mut application = open_on(vec![relay(COMPANY)]);
    key(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RelayLoginBegun {
            address: COMPANY.to_owned(),
            login: pending(),
        })
        .expect("take the begun login");
    application
        .handle_event(ApplicationEvent::RelayLoginSettled {
            address: COMPANY.to_owned(),
            login: RelayLogin {
                outcome: RelayLoginOutcome::Refused {
                    reason: RelayLoginRefusal::Expired,
                    message: "nobody finished the login before it expired".to_owned(),
                },
                ..pending()
            },
        })
        .expect("take the expired login");

    let ended = rendered_application_rows(&application).join("\n");
    assert!(
        prose(&application).contains(&format!(
            "The login at {COMPANY} ended: nobody finished the login before it expired"
        )),
        "{ended}"
    );
    assert!(!ended.contains("not admitted"), "{ended}");
}

#[test]
fn a_login_that_cannot_begin_is_said_on_the_list() {
    let mut application = open_on(vec![relay(COMPANY)]);
    key(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RelayLoginNotBegun {
            address: COMPANY.to_owned(),
            error: "the Relay did not answer".to_owned(),
        })
        .expect("take the failed beginning");

    let list = rendered_application_rows(&application).join("\n");
    assert!(
        prose(&application).contains(&format!(
            "Could not begin a login at {COMPANY}: the Relay did not answer"
        )),
        "{list}"
    );
    assert!(entry(&list, COMPANY).contains("Login needed"), "{list}");
}

#[test]
fn a_login_outlives_the_client_and_reopening_shows_where_it_stands() {
    // A fresh Client, as after closing the one that began these logins.
    let mut application = Application::default();
    type_terminal_text(&mut application, "/relay");
    key(&mut application, KeyCode::Enter);
    assert_eq!(
        application
            .handle_event(ApplicationEvent::RelaysListed(vec![
                Relay {
                    login: Some(pending()),
                    ..relay(COMPANY)
                },
                Relay {
                    login: Some(RelayLogin {
                        outcome: not_admitted(),
                        ..pending()
                    }),
                    ..relay(HOME)
                },
            ]))
            .expect("list the Relays"),
        ApplicationTransition::FollowRelayLogins(vec![COMPANY.to_owned()]),
        "a login still under way is followed again, so it resolves on its own"
    );

    let list = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&list, COMPANY).contains(&format!("Logging in · enter {CODE} at {VISIT}")),
        "{list}"
    );
    assert!(
        entry(&list, HOME).contains("Login needed · not admitted; ask the Relay's operator"),
        "{list}"
    );

    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "the login under way is shown again rather than begun anew"
    );
    let display = rendered_application_rows(&application).join("\n");
    assert!(display.contains(CODE), "{display}");
    assert_eq!(
        key(&mut application, KeyCode::Char('c')),
        ApplicationTransition::CopyToClipboard(CODE.into())
    );

    application
        .handle_event(ApplicationEvent::RelayLoginSettled {
            address: COMPANY.to_owned(),
            login: RelayLogin {
                outcome: RelayLoginOutcome::Done { account: octocat() },
                ..pending()
            },
        })
        .expect("take the finished login");
    let resolved = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&resolved, COMPANY).contains("Logged in as octocat"),
        "{resolved}"
    );
}

#[test]
fn a_login_already_followed_is_not_followed_twice() {
    let mut application = open_on(vec![relay(COMPANY)]);
    key(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RelayLoginBegun {
            address: COMPANY.to_owned(),
            login: pending(),
        })
        .expect("take the begun login");
    key(&mut application, KeyCode::Esc);
    key(&mut application, KeyCode::Esc);

    type_terminal_text(&mut application, "/relay");
    key(&mut application, KeyCode::Enter);
    assert_eq!(
        application
            .handle_event(ApplicationEvent::RelaysListed(vec![Relay {
                login: Some(pending()),
                ..relay(COMPANY)
            }]))
            .expect("list the Relays"),
        ApplicationTransition::Continue,
        "the Client is still following the login it began"
    );

    // Following can be lost — the Client's own Server restarting, say — and
    // then showing the login again follows it afresh.
    application
        .handle_event(ApplicationEvent::RelayLoginLost {
            address: COMPANY.to_owned(),
            error: "the Relay login was given up before it ended".to_owned(),
        })
        .expect("lose the login's progress");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::FollowRelayLogins(vec![COMPANY.to_owned()])
    );
}

#[test]
fn a_relay_is_removed_from_the_list_with_a_second_press() {
    let mut application = open_on(vec![relay(COMPANY), logged_in(HOME)]);

    assert_eq!(
        key(&mut application, KeyCode::Char('x')),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("x confirm removal · any other key cancels")
    );
    key(&mut application, KeyCode::Down);
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("x confirm removal"),
        "any other key puts the removal down"
    );
    key(&mut application, KeyCode::Char('x'));
    assert_eq!(
        key(&mut application, KeyCode::Char('x')),
        ApplicationTransition::RemoveRelay(HOME.to_owned())
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Removing Relay…")
    );
    application
        .handle_event(ApplicationEvent::RelayRemoved {
            address: HOME.to_owned(),
            result: Ok(RelayRemoval {
                address: HOME.to_owned(),
                acknowledged: true,
            }),
        })
        .expect("take the removal");
    let list = rendered_application_rows(&application).join("\n");
    assert!(list.contains(&format!("Removed {HOME}")), "{list}");
    assert_eq!(
        list.matches(HOME).count(),
        1,
        "only the note names the Relay removed: {list}"
    );
    assert!(entry(&list, COMPANY).contains('›'), "{list}");

    key(&mut application, KeyCode::Char('x'));
    assert_eq!(
        key(&mut application, KeyCode::Char('x')),
        ApplicationTransition::RemoveRelay(COMPANY.to_owned())
    );
    application
        .handle_event(ApplicationEvent::RelayRemoved {
            address: COMPANY.to_owned(),
            result: Ok(RelayRemoval {
                address: COMPANY.to_owned(),
                acknowledged: false,
            }),
        })
        .expect("take the unanswered removal");
    let list = rendered_application_rows(&application).join("\n");
    assert!(
        prose(&application).contains(&format!("Removed {COMPANY} here; the Relay did not answer")),
        "{list}"
    );
    assert!(list.contains("No Relays added"), "{list}");
}

#[test]
fn a_refused_removal_keeps_the_relay_listed_with_why() {
    let mut application = open_on(vec![relay(COMPANY)]);
    key(&mut application, KeyCode::Char('x'));
    key(&mut application, KeyCode::Char('x'));
    application
        .handle_event(ApplicationEvent::RelayRemoved {
            address: COMPANY.to_owned(),
            result: Err("the Server could not store its Relays".to_owned()),
        })
        .expect("take the refused removal");
    let list = rendered_application_rows(&application).join("\n");
    assert!(entry(&list, COMPANY).contains("Login needed"), "{list}");
    assert!(
        list.contains("the Server could not store its Relays"),
        "{list}"
    );
}

#[test]
fn opening_adding_logging_in_and_removing_are_each_a_semantic_command() {
    for (command, id) in [
        (SemanticCommandId::RelayOpen, "relay.open"),
        (SemanticCommandId::RelayAdd, "relay.add"),
        (SemanticCommandId::RelayLogin, "relay.login"),
        (SemanticCommandId::RelayRemove, "relay.remove"),
        (
            SemanticCommandId::RelayCopyAddress,
            "relay.login.copy-address",
        ),
        (SemanticCommandId::RelayCopyCode, "relay.login.copy-code"),
    ] {
        assert_eq!(command.as_str(), id);
    }

    // Invoked by no key at all, as a plugin or the pointer would.
    let mut application = Application::default();
    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayOpen),
        ApplicationTransition::ListRelays
    );
    list(&mut application, vec![relay(COMPANY)]);
    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayLogin),
        ApplicationTransition::BeginRelayLogin(COMPANY.to_owned())
    );
    application
        .handle_event(ApplicationEvent::RelayLoginBegun {
            address: COMPANY.to_owned(),
            login: pending(),
        })
        .expect("take the begun login");
    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayCopyAddress),
        ApplicationTransition::CopyToClipboard(VISIT.into())
    );
    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayCopyCode),
        ApplicationTransition::CopyToClipboard(CODE.into())
    );
    invoke(&mut application, SemanticCommandId::RelayClose);

    invoke(&mut application, SemanticCommandId::RelayRemove);
    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayRemove),
        ApplicationTransition::RemoveRelay(COMPANY.to_owned())
    );
    application
        .handle_event(ApplicationEvent::RelayRemoved {
            address: COMPANY.to_owned(),
            result: Ok(RelayRemoval {
                address: COMPANY.to_owned(),
                acknowledged: true,
            }),
        })
        .expect("take the removal");

    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayAdd),
        ApplicationTransition::Continue
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemanticText(
            SemanticCommandId::RelayAddressInsert,
            HOME.to_owned(),
        )))
        .expect("type the address");
    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayAdd),
        ApplicationTransition::AddRelay(HOME.to_owned())
    );
}

/// Opens `/relay` and lists `relays` there.
fn open_on(relays: Vec<Relay>) -> Application {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/relay");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::ListRelays
    );
    list(&mut application, relays);
    application
}

fn list(application: &mut Application, relays: Vec<Relay>) {
    application
        .handle_event(ApplicationEvent::RelaysListed(relays))
        .expect("list the Relays");
}

fn invoke(application: &mut Application, command: SemanticCommandId) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            command,
        )))
        .expect("invoke the semantic command")
}

/// What the screen says, its wrapped Rows read on as one line of prose.
fn prose(application: &Application) -> String {
    rendered_application_rows(application)
        .iter()
        .map(|row| row.trim().trim_matches('│').trim())
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The rendered entry for the Relay at `address`: the row naming it, and how
/// it stands on the row beneath.
fn entry(screen: &str, address: &str) -> String {
    let rows = screen.lines().collect::<Vec<_>>();
    let named = rows
        .iter()
        .position(|row| row.contains(address))
        .unwrap_or_else(|| panic!("a row names {address}: {screen}"));
    rows[named..=(named + 1).min(rows.len() - 1)].join("\n")
}

fn relay(address: &str) -> Relay {
    Relay {
        address: address.to_owned(),
        state: RelayState::LoginNeeded,
        unreachable: None,
        account: None,
        login: None,
    }
}

fn logged_in(address: &str) -> Relay {
    Relay {
        state: RelayState::LoggedIn,
        account: Some(octocat()),
        ..relay(address)
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
        verification_uri: VISIT.to_owned(),
        user_code: CODE.to_owned(),
        outcome: RelayLoginOutcome::Pending,
    }
}

fn not_admitted() -> RelayLoginOutcome {
    RelayLoginOutcome::Refused {
        reason: RelayLoginRefusal::NotAdmitted,
        message: "the Relay does not admit this Account".to_owned(),
    }
}
