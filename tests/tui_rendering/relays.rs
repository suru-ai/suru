//! The user's `/relay` command: the list of their own Server's Relays, adding
//! one by address, logging in there, and removing one.
//!
//! Keys are taken the whole way the run loop takes them, through
//! [`Application::take_terminal_event`], and every request the list sends
//! names itself, so an answer is delivered here to the request it answers —
//! or, where a test says so, to one the reader has since moved past.

use std::time::Duration;

use crate::support::{
    application_looking_at_studio, grace_elapses, rendered_application_rows,
    rendered_application_rows_at, studio_stops_answering, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    protocol::{
        Outlook, Relay, RelayAccount, RelayListing, RelayLogin, RelayLoginOutcome,
        RelayLoginRefusal, RelayRemoval, RelayState, RelayUnreachable,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, RelayLoginFollow,
        RelayRequest, SemanticCommandId,
    },
};

const COMPANY: &str = "https://relay.example.com";
const HOME: &str = "https://home.example.net";
const LAPSED: &str = "https://lapsed.example.org";
const VISIT: &str = "https://github.com/login/device";
const CODE: &str = "WDJB-MJHT";
const LATER_CODE: &str = "KQTR-VXZB";
/// The revision a listing answers at where a test pushes nothing to tell it
/// apart from: every such listing stands for the same moment, and the newest
/// the list has heard.
const LISTED: u64 = 1;
/// The run of the Client's own Server the tests' listings come from.
const SERVER: uuid::Uuid = uuid::Uuid::from_u128(7);

#[test]
fn relay_opens_the_list_of_the_servers_relays_with_each_ones_state() {
    let mut application = Application::default();

    type_terminal_text(&mut application, "/relay");
    let completion = rendered_application_rows(&application).join("\n");
    assert!(completion.contains("/relay"), "{completion}");
    let ApplicationTransition::ListRelays(listing) = press(&mut application, KeyCode::Enter) else {
        panic!("/relay asks the Client's own Server for its Relays");
    };
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Loading Relays…")
    );

    list(
        &mut application,
        listing,
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
    // A login is offered at the Relay the keys are on only where it needs
    // one: not where it stands, nor where it is merely Unreachable.
    assert!(
        list.contains("↑↓ choose · a add · x remove · Esc close"),
        "{list}"
    );
    press(&mut application, KeyCode::Down);
    let list = rendered_application_rows(&application).join("\n");
    assert!(
        list.contains("↑↓ choose · a add · Enter log in · x remove · Esc close"),
        "{list}"
    );
    press(&mut application, KeyCode::Down);
    let list = rendered_application_rows(&application).join("\n");
    assert!(
        list.contains("↑↓ choose · a add · x remove · Esc close"),
        "{list}"
    );

    assert_eq!(
        press(&mut application, KeyCode::Esc),
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
    let listing = open(&mut application);
    application
        .handle_event(ApplicationEvent::RelayListingFailed {
            request: listing,
            error: "send Relay listing".to_owned(),
        })
        .expect("take the failed listing");

    for _ in 0..2 {
        let list = rendered_application_rows(&application).join("\n");
        assert!(list.contains("send Relay listing"), "{list}");
        assert!(
            !list.contains("No Relays added"),
            "a listing that failed says nothing of what the Server holds: {list}"
        );
        // It stands however the reader moves, until a listing lands.
        press(&mut application, KeyCode::Down);
    }
}

#[test]
fn a_relay_added_after_a_failed_listing_is_listed_afresh() {
    let mut application = Application::default();
    let listing = open(&mut application);
    application
        .handle_event(ApplicationEvent::RelayListingFailed {
            request: listing,
            error: "send Relay listing".to_owned(),
        })
        .expect("take the failed listing");

    press(&mut application, KeyCode::Char('a'));
    type_terminal_text(&mut application, COMPANY);
    let addition = addition(press(&mut application, KeyCode::Enter), COMPANY);
    let ApplicationTransition::ListRelays(refresh) = application
        .handle_event(ApplicationEvent::RelayAdded {
            request: addition,
            relay: relay(COMPANY),
        })
        .expect("take the added Relay")
    else {
        panic!("the Server answered again, so the Relays are asked for afresh");
    };
    let recovering = prose(&application);
    assert!(
        recovering.contains(&format!("Added {COMPANY}")),
        "the addition is said at once: {recovering}"
    );

    list(&mut application, refresh, vec![relay(HOME), relay(COMPANY)]);
    let recovered = rendered_application_rows(&application).join("\n");
    assert!(!recovered.contains("send Relay listing"), "{recovered}");
    assert!(
        entry(&recovered, HOME).contains("Login needed"),
        "{recovered}"
    );
    assert!(
        entry(&recovered, COMPANY).contains('›'),
        "the keys stay on the Relay just added: {recovered}"
    );
}

#[test]
fn a_listing_asked_for_before_a_login_began_leaves_that_login_standing() {
    let mut application = open_on(vec![relay(COMPANY)]);
    press(&mut application, KeyCode::Char('a'));
    type_terminal_text(&mut application, HOME);
    let request = addition(press(&mut application, KeyCode::Enter), HOME);
    let ApplicationTransition::ListRelays(refresh) = application
        .handle_event(ApplicationEvent::RelayAdded {
            request,
            relay: relay(HOME),
        })
        .expect("take the added Relay")
    else {
        panic!("an addition answered asks for the Relays afresh");
    };

    // A login begins at the other Relay before that listing has landed, and
    // it lands picturing the Relays from before the login began.
    press(&mut application, KeyCode::Up);
    let follower = log_in(&mut application, COMPANY, pending());
    list(&mut application, refresh, vec![relay(COMPANY), relay(HOME)]);
    let display = rendered_application_rows(&application).join("\n");
    assert!(
        display.contains(CODE),
        "the listing that predates the login leaves its display standing: {display}"
    );

    // Back on the list, the login is still under way there, and shown again
    // rather than begun anew.
    press(&mut application, KeyCode::Esc);
    let listed = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&listed, COMPANY).contains(&format!("Logging in · enter {CODE}")),
        "{listed}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "the login is still followed by what began following it"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains(CODE)
    );

    settle_and_list(
        &mut application,
        follower,
        done(pending()),
        vec![logged_in_by(COMPANY, pending()), relay(HOME)],
    );
    let resolved = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&resolved, COMPANY).contains("Logged in as octocat"),
        "{resolved}"
    );
    assert!(
        entry(&resolved, HOME).contains("Login needed"),
        "{resolved}"
    );
}

#[test]
fn a_listing_asked_for_before_a_relay_was_added_or_removed_does_not_undo_it() {
    let mut application = open_on(vec![relay(COMPANY)]);

    // Another Relay is added and the first removed, each answer asking for
    // the Relays afresh in place of the listing asked for before.
    press(&mut application, KeyCode::Char('a'));
    type_terminal_text(&mut application, HOME);
    let request = addition(press(&mut application, KeyCode::Enter), HOME);
    let ApplicationTransition::ListRelays(before_removal) = application
        .handle_event(ApplicationEvent::RelayAdded {
            request,
            relay: relay(HOME),
        })
        .expect("take the added Relay")
    else {
        panic!("an addition answered asks for the Relays afresh");
    };
    press(&mut application, KeyCode::Char('x'));
    let request = removal(press(&mut application, KeyCode::Char('x')), COMPANY);
    let after_removal = removed(&mut application, request, COMPANY, true);

    // The listing asked for before the removal lands nowhere, the one after
    // it is taken.
    list(
        &mut application,
        before_removal,
        vec![relay(COMPANY), relay(HOME)],
    );
    press(&mut application, KeyCode::Down);
    let held = rendered_application_rows(&application).join("\n");
    assert!(
        !held.contains(HOME),
        "the listing from before the removal is not taken: {held}"
    );
    list(
        &mut application,
        after_removal,
        vec![relay(HOME), relay(LAPSED)],
    );
    let listed = rendered_application_rows(&application).join("\n");
    assert!(entry(&listed, HOME).contains("Login needed"), "{listed}");
    assert!(entry(&listed, LAPSED).contains("Login needed"), "{listed}");
    assert!(!listed.contains(COMPANY), "{listed}");
}

#[test]
fn a_listing_asked_for_earlier_lands_nowhere() {
    let mut application = Application::default();
    let earlier = open(&mut application);
    press(&mut application, KeyCode::Esc);
    let later = open(&mut application);

    list(&mut application, earlier, vec![relay(COMPANY)]);
    application
        .handle_event(ApplicationEvent::RelayListingFailed {
            request: earlier,
            error: "send Relay listing".to_owned(),
        })
        .expect("take the earlier failure");
    let waiting = rendered_application_rows(&application).join("\n");
    assert!(waiting.contains("Loading Relays…"), "{waiting}");

    list(&mut application, later, vec![relay(HOME)]);
    let list = rendered_application_rows(&application).join("\n");
    assert!(entry(&list, HOME).contains("Login needed"), "{list}");
    assert!(!list.contains(COMPANY), "{list}");
}

#[test]
fn relay_lists_the_clients_own_server_whichever_way_the_outlook_is_turned() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));
    grace_elapses(&mut application, Outlook::Remote("studio".to_owned()));

    // The Relays are the Client's own Server's, so a Remote that has stopped
    // answering refuses nothing here.
    let listing = open(&mut application);
    list(&mut application, listing, vec![logged_in(COMPANY)]);
    let list = rendered_application_rows(&application).join("\n");
    assert!(entry(&list, COMPANY).contains("Logged in"), "{list}");
}

#[test]
fn a_relay_is_added_by_its_address() {
    let mut application = open_on(Vec::new());
    let empty = rendered_application_rows(&application).join("\n");
    assert!(empty.contains("No Relays added"), "{empty}");

    assert_eq!(
        press(&mut application, KeyCode::Char('a')),
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
    let request = addition(press(&mut application, KeyCode::Enter), COMPANY);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Adding Relay…")
    );

    let ApplicationTransition::ListRelays(refresh) = application
        .handle_event(ApplicationEvent::RelayAdded {
            request,
            relay: relay(COMPANY),
        })
        .expect("take the added Relay")
    else {
        panic!("an addition answered asks for the Relays afresh");
    };
    list(&mut application, refresh, vec![relay(COMPANY)]);
    let list = rendered_application_rows(&application).join("\n");
    assert!(entry(&list, COMPANY).contains("Login needed"), "{list}");
    assert!(
        entry(&list, COMPANY).contains('›'),
        "the Relay just added is the one the keys are on, ready to log in: {list}"
    );
    assert!(list.contains(&format!("Added {COMPANY}")), "{list}");
    assert!(matches!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::BeginRelayLogin { address, .. } if address == COMPANY
    ));
}

#[test]
fn an_address_that_cannot_be_used_is_refused_where_the_reader_can_see_it() {
    let mut application = open_on(Vec::new());
    press(&mut application, KeyCode::Char('a'));

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "an empty address is never sent"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Type a Relay's address first")
    );

    type_terminal_text(&mut application, "relay.example.com");
    let request = addition(press(&mut application, KeyCode::Enter), "relay.example.com");
    let refusal = "a Relay's address is an https:// or http:// address naming its host";
    application
        .handle_event(ApplicationEvent::RelayAdditionFailed {
            request,
            error: refusal.to_owned(),
        })
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

    press(&mut application, KeyCode::Backspace);
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains(refusal),
        "editing the address puts the refusal down"
    );
    assert_eq!(
        press(&mut application, KeyCode::Esc),
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
fn a_late_answer_to_an_earlier_addition_leaves_a_newer_one_alone() {
    let mut application = open_on(Vec::new());
    press(&mut application, KeyCode::Char('a'));
    type_terminal_text(&mut application, COMPANY);
    let earlier = addition(press(&mut application, KeyCode::Enter), COMPANY);
    // The reader leaves while it is under way, and comes back to add another.
    press(&mut application, KeyCode::Esc);
    let listing = open(&mut application);
    list(&mut application, listing, Vec::new());
    press(&mut application, KeyCode::Char('a'));
    type_terminal_text(&mut application, HOME);
    let later = addition(press(&mut application, KeyCode::Enter), HOME);

    application
        .handle_event(ApplicationEvent::RelayAdditionFailed {
            request: earlier,
            error: "the Relay at that address is already added".to_owned(),
        })
        .expect("take the earlier refusal");
    let adding = rendered_application_rows(&application).join("\n");
    assert!(adding.contains("Adding Relay…"), "{adding}");
    assert!(!adding.contains("already added"), "{adding}");

    application
        .handle_event(ApplicationEvent::RelayAdded {
            request: earlier,
            relay: relay(COMPANY),
        })
        .expect("take the earlier addition");
    let adding = rendered_application_rows(&application).join("\n");
    assert!(
        adding.contains("Adding Relay…"),
        "an earlier addition resolves nothing the reader asked since: {adding}"
    );

    let ApplicationTransition::ListRelays(refresh) = application
        .handle_event(ApplicationEvent::RelayAdded {
            request: later,
            relay: relay(HOME),
        })
        .expect("take the later addition")
    else {
        panic!("an addition answered asks for the Relays afresh");
    };
    list(&mut application, refresh, vec![relay(COMPANY), relay(HOME)]);
    let list = rendered_application_rows(&application).join("\n");
    assert!(list.contains(&format!("Added {HOME}")), "{list}");
    assert!(entry(&list, HOME).contains('›'), "{list}");
    assert!(
        entry(&list, COMPANY).contains("Login needed"),
        "the Server did add the earlier one, so it is listed: {list}"
    );
}

#[test]
fn the_login_display_shows_where_to_go_and_the_code_each_copyable_and_resolves_on_its_own() {
    let mut application = open_on(vec![relay(COMPANY)]);

    let beginning = beginning(press(&mut application, KeyCode::Enter), COMPANY);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Beginning login…")
    );
    let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);

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
        press(&mut application, KeyCode::Char('a')),
        ApplicationTransition::CopyToClipboard(VISIT.into())
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('c')),
        ApplicationTransition::CopyToClipboard(CODE.into())
    );

    settle_and_list(
        &mut application,
        follower,
        done(pending()),
        vec![logged_in_by(COMPANY, pending())],
    );
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
    let follower = log_in(&mut application, COMPANY, pending());
    let ended = RelayLogin {
        outcome: not_admitted(),
        ..pending()
    };

    settle_and_list(
        &mut application,
        follower,
        ended.clone(),
        vec![with_login(relay(COMPANY), ended)],
    );

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
fn a_login_past_the_relays_cap_says_which_cap_and_what_makes_room() {
    let mut application = open_on(vec![relay(COMPANY)]);
    let follower = log_in(&mut application, COMPANY, pending());
    let ended = RelayLogin {
        outcome: RelayLoginOutcome::Refused {
            reason: RelayLoginRefusal::LoginsCapReached { limit: 64 },
            message: "the Server's own account of the cap".to_owned(),
        },
        ..pending()
    };

    settle_and_list(
        &mut application,
        follower,
        ended.clone(),
        vec![with_login(relay(COMPANY), ended)],
    );

    let refused = rendered_application_rows(&application).join("\n");
    assert!(!refused.contains(CODE), "{refused}");
    assert!(
        prose(&application).contains(&format!(
            "Your Account already has 64 Servers logged in at {COMPANY}, as many as the \
             Relay's operator allows; remove the Relay from a Server that no longer needs it, \
             or ask the operator to raise the cap"
        )),
        "{refused}"
    );
    assert!(
        entry(&refused, COMPANY)
            .contains("Login needed · at the cap of 64 Servers; ask the Relay's operator"),
        "{refused}"
    );
}

#[test]
fn a_login_ended_any_other_way_says_why_in_the_servers_words() {
    let mut application = open_on(vec![relay(COMPANY)]);
    let follower = log_in(&mut application, COMPANY, pending());
    settle(
        &mut application,
        follower,
        RelayLogin {
            outcome: RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::Expired,
                message: "nobody finished the login before it expired".to_owned(),
            },
            ..pending()
        },
    );

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
    let request = beginning(press(&mut application, KeyCode::Enter), COMPANY);
    application
        .handle_event(ApplicationEvent::RelayLoginNotBegun {
            request,
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
fn a_late_beginning_lands_nowhere() {
    let mut application = open_on(vec![relay(COMPANY)]);
    let earlier = beginning(press(&mut application, KeyCode::Enter), COMPANY);
    press(&mut application, KeyCode::Esc);
    let listing = open(&mut application);
    list(&mut application, listing, vec![relay(COMPANY)]);
    let later = beginning(press(&mut application, KeyCode::Enter), COMPANY);

    assert_eq!(
        begun(&mut application, earlier, pending()),
        ApplicationTransition::Continue
    );
    application
        .handle_event(ApplicationEvent::RelayLoginNotBegun {
            request: earlier,
            error: "the Relay did not answer".to_owned(),
        })
        .expect("take the earlier failure");
    let waiting = rendered_application_rows(&application).join("\n");
    assert!(waiting.contains("Beginning login…"), "{waiting}");

    let follower = followed_at(begun(&mut application, later, later_login()), COMPANY);
    let display = rendered_application_rows(&application).join("\n");
    assert!(display.contains(LATER_CODE), "{display}");
    settle_and_list(
        &mut application,
        follower,
        done(later_login()),
        vec![logged_in_by(COMPANY, later_login())],
    );
    assert!(
        entry(&rendered_application_rows(&application).join("\n"), COMPANY).contains("Logged in")
    );
}

#[test]
fn a_login_outlives_the_client_and_reopening_shows_where_it_stands() {
    // A fresh Client, as after closing the one that began these logins.
    let mut application = Application::default();
    let listing = open(&mut application);
    let follower = followed_at(
        list(
            &mut application,
            listing,
            vec![
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
            ],
        ),
        COMPANY,
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
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "the login under way is shown again rather than begun anew"
    );
    let display = rendered_application_rows(&application).join("\n");
    assert!(display.contains(CODE), "{display}");
    assert_eq!(
        press(&mut application, KeyCode::Char('c')),
        ApplicationTransition::CopyToClipboard(CODE.into())
    );

    settle_and_list(
        &mut application,
        follower,
        done(pending()),
        vec![
            logged_in_by(COMPANY, pending()),
            with_login(
                relay(HOME),
                RelayLogin {
                    outcome: not_admitted(),
                    ..pending()
                },
            ),
        ],
    );
    let resolved = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&resolved, COMPANY).contains("Logged in as octocat"),
        "{resolved}"
    );
}

#[test]
fn a_login_already_followed_is_not_followed_twice() {
    let mut application = open_on(vec![relay(COMPANY)]);
    let follower = log_in(&mut application, COMPANY, pending());
    press(&mut application, KeyCode::Esc);
    press(&mut application, KeyCode::Esc);

    let listing = open(&mut application);
    assert_eq!(
        list(
            &mut application,
            listing,
            vec![Relay {
                login: Some(pending()),
                ..relay(COMPANY)
            }],
        ),
        ApplicationTransition::Continue,
        "the Client is still following the login it began"
    );

    // Following can be lost — the Client's own Server restarting, say — and
    // then showing the login again follows it afresh.
    application
        .handle_event(ApplicationEvent::RelayLoginLost {
            request: follower,
            error: "the Relay login was given up before it ended".to_owned(),
        })
        .expect("lose the login's progress");
    let again = followed_at(press(&mut application, KeyCode::Enter), COMPANY);
    assert_ne!(again, follower);
}

#[test]
fn a_follower_left_behind_by_a_later_login_is_replaced_and_its_late_answer_discarded() {
    let mut application = open_on(vec![relay(COMPANY)]);
    let earlier = log_in(&mut application, COMPANY, pending());
    press(&mut application, KeyCode::Esc);
    press(&mut application, KeyCode::Esc);

    // The list, opened again, already says the login ended, before what was
    // following it has answered.
    let listing = open(&mut application);
    assert_eq!(
        list(
            &mut application,
            listing,
            vec![Relay {
                login: Some(RelayLogin {
                    outcome: RelayLoginOutcome::Refused {
                        reason: RelayLoginRefusal::Expired,
                        message: "nobody finished the login before it expired".to_owned(),
                    },
                    ..pending()
                }),
                ..relay(COMPANY)
            }],
        ),
        ApplicationTransition::Continue
    );

    let later = log_in(&mut application, COMPANY, later_login());
    assert_ne!(later, earlier, "the later login has a follower of its own");

    settle(
        &mut application,
        earlier,
        RelayLogin {
            outcome: RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::Interrupted,
                message: "the login was given up for a later one at the same Relay".to_owned(),
            },
            ..pending()
        },
    );
    let display = rendered_application_rows(&application).join("\n");
    assert!(
        display.contains(LATER_CODE),
        "the earlier login's late end leaves the later one's display standing: {display}"
    );
    assert!(!display.contains("given up"), "{display}");

    settle_and_list(
        &mut application,
        later,
        done(later_login()),
        vec![logged_in_by(COMPANY, later_login())],
    );
    let resolved = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&resolved, COMPANY).contains("Logged in as octocat"),
        "{resolved}"
    );
}

#[test]
fn a_login_begun_elsewhere_replaces_the_follower_of_the_one_it_supersedes() {
    let mut application = Application::default();
    let listing = open(&mut application);
    let earlier = followed_at(
        list(
            &mut application,
            listing,
            vec![Relay {
                login: Some(pending()),
                ..relay(COMPANY)
            }],
        ),
        COMPANY,
    );
    press(&mut application, KeyCode::Esc);

    // Another Client began a later login there meanwhile.
    let listing = open(&mut application);
    let later = followed_at(
        list(
            &mut application,
            listing,
            vec![Relay {
                login: Some(later_login()),
                ..relay(COMPANY)
            }],
        ),
        COMPANY,
    );
    assert_ne!(later, earlier);

    settle(
        &mut application,
        earlier,
        RelayLogin {
            outcome: RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::Interrupted,
                message: "the login was given up for a later one at the same Relay".to_owned(),
            },
            ..pending()
        },
    );
    let list = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&list, COMPANY).contains(&format!("Logging in · enter {LATER_CODE}")),
        "{list}"
    );
}

#[test]
fn a_relay_is_removed_from_the_list_with_a_second_press() {
    let mut application = open_on(vec![relay(COMPANY), logged_in(HOME)]);

    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("x confirm removal · any other key cancels")
    );
    press(&mut application, KeyCode::Down);
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("x confirm removal"),
        "any other key puts the removal down"
    );
    press(&mut application, KeyCode::Char('x'));
    let request = removal(press(&mut application, KeyCode::Char('x')), HOME);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Removing Relay…")
    );
    let refresh = removed(&mut application, request, HOME, true);
    list(&mut application, refresh, vec![relay(COMPANY)]);
    let shown = rendered_application_rows(&application).join("\n");
    assert!(shown.contains(&format!("Removed {HOME}")), "{shown}");
    assert_eq!(
        shown.matches(HOME).count(),
        1,
        "only the note names the Relay removed: {shown}"
    );
    assert!(entry(&shown, COMPANY).contains('›'), "{shown}");

    press(&mut application, KeyCode::Char('x'));
    let request = removal(press(&mut application, KeyCode::Char('x')), COMPANY);
    let refresh = removed(&mut application, request, COMPANY, false);
    list(&mut application, refresh, Vec::new());
    let list = rendered_application_rows(&application).join("\n");
    assert!(
        prose(&application).contains(&format!("Removed {COMPANY} here; the Relay did not answer")),
        "{list}"
    );
    assert!(list.contains("No Relays added"), "{list}");
}

#[test]
fn a_removal_is_put_down_by_any_other_key_the_run_loop_takes() {
    let mut application = open_on(vec![relay(COMPANY), relay(HOME)]);

    press(&mut application, KeyCode::Char('x'));
    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::Continue,
        "moving to another Relay put the first removal down, so this only arms one"
    );

    // A key the list has no use for puts it down too, and that is something
    // on screen for the run loop to draw.
    let unbound = application
        .take_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::NONE,
        )))
        .expect("take the key");
    assert_eq!(unbound.transition, ApplicationTransition::Continue);
    assert!(unbound.changed, "putting a removal down is drawn");
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("x confirm removal")
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::Continue
    );
    removal(press(&mut application, KeyCode::Char('x')), HOME);
}

#[test]
fn a_refused_removal_keeps_the_relay_listed_with_why() {
    let mut application = open_on(vec![relay(COMPANY)]);
    press(&mut application, KeyCode::Char('x'));
    let request = removal(press(&mut application, KeyCode::Char('x')), COMPANY);
    application
        .handle_event(ApplicationEvent::RelayRemoved {
            request,
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
fn a_late_removal_answer_resolves_nothing_newer() {
    let mut application = open_on(vec![relay(COMPANY), relay(HOME)]);
    press(&mut application, KeyCode::Char('x'));
    let earlier = removal(press(&mut application, KeyCode::Char('x')), COMPANY);
    press(&mut application, KeyCode::Esc);
    let listing = open(&mut application);
    list(&mut application, listing, vec![relay(COMPANY), relay(HOME)]);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Char('x'));
    let later = removal(press(&mut application, KeyCode::Char('x')), HOME);

    application
        .handle_event(ApplicationEvent::RelayRemoved {
            request: earlier,
            result: Err("the Server could not store its Relays".to_owned()),
        })
        .expect("take the earlier refusal");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Removing Relay…")
    );
    removed(&mut application, earlier, COMPANY, true);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Removing Relay…"),
        "an earlier removal resolves nothing the reader asked since"
    );

    let refresh = removed(&mut application, later, HOME, true);
    list(&mut application, refresh, Vec::new());
    let list = rendered_application_rows(&application).join("\n");
    assert!(list.contains(&format!("Removed {HOME}")), "{list}");
    assert!(
        list.contains("No Relays added"),
        "the Server did remove the earlier one, so it is gone: {list}"
    );
}

#[test]
fn a_wrapped_note_keeps_its_rows_and_the_keys_beneath_it_on_a_narrow_terminal() {
    let mut relays = vec![relay(COMPANY), relay(HOME), relay(LAPSED)];
    relays.extend(
        [
            "https://r4.example.com",
            "https://r5.example.com",
            "https://r6.example.com",
        ]
        .map(relay),
    );
    let mut application = open_on(relays);
    let follower = log_in(&mut application, COMPANY, pending());
    settle(
        &mut application,
        follower,
        RelayLogin {
            outcome: not_admitted(),
            ..pending()
        },
    );

    let rows = rendered_application_rows_at(&application, 40, 24);
    let inside = rows
        .iter()
        .map(|row| row.trim().trim_matches('│').trim())
        .collect::<Vec<_>>();
    let top = rows
        .iter()
        .position(|row| row.contains("┌ Relay"))
        .unwrap_or_else(|| panic!("the box is drawn: {rows:#?}"));
    let bottom = top
        + rows[top..]
            .iter()
            .position(|row| row.contains("└─"))
            .unwrap_or_else(|| panic!("the box is closed: {rows:#?}"));
    assert!(
        inside[bottom - 2].starts_with("↑↓ choose")
            && inside[bottom - 2..bottom].join(" ")
                == "↑↓ choose · a add · Enter log in · x remove · Esc close",
        "the keys are taught, whole, on the box's last Rows: {rows:#?}"
    );
    assert!(
        inside.join(" ").contains(&format!(
            "You are not admitted to {COMPANY}; ask the Relay's operator to admit you"
        )),
        "every Row of the note is shown: {rows:#?}"
    );
    assert!(
        inside.iter().any(|row| row.contains(COMPANY)),
        "the list keeps the selected Relay in view: {rows:#?}"
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
    let ApplicationTransition::ListRelays(listing) =
        invoke(&mut application, SemanticCommandId::RelayOpen)
    else {
        panic!("opening asks for the Relays");
    };
    list(&mut application, listing, vec![relay(COMPANY)]);
    let request = beginning(
        invoke(&mut application, SemanticCommandId::RelayLogin),
        COMPANY,
    );
    begun(&mut application, request, pending());
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
    let request = removal(
        invoke(&mut application, SemanticCommandId::RelayRemove),
        COMPANY,
    );
    removed(&mut application, request, COMPANY, true);

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
    addition(invoke(&mut application, SemanticCommandId::RelayAdd), HOME);
}

/// Takes a key the whole way the run loop takes it.
fn press(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .take_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("take the key")
        .transition
}

fn invoke(application: &mut Application, command: SemanticCommandId) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            command,
        )))
        .expect("invoke the semantic command")
}

/// Types `/relay`, answering the listing it asks for.
fn open(application: &mut Application) -> RelayRequest {
    type_terminal_text(application, "/relay");
    let ApplicationTransition::ListRelays(request) = press(application, KeyCode::Enter) else {
        panic!("/relay asks the Client's own Server for its Relays");
    };
    request
}

/// Opens `/relay` and lists `relays` there.
fn open_on(relays: Vec<Relay>) -> Application {
    let mut application = Application::default();
    let request = open(&mut application);
    list(&mut application, request, relays);
    application
}

fn list(
    application: &mut Application,
    request: RelayRequest,
    relays: Vec<Relay>,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::RelaysListed {
            request,
            listing: RelayListing {
                instance: SERVER,
                revision: LISTED,
                relays,
            },
        })
        .expect("list the Relays")
}

/// The addition `transition` asks for, of the Relay at `address`.
fn addition(transition: ApplicationTransition, address: &str) -> RelayRequest {
    match transition {
        ApplicationTransition::AddRelay {
            request,
            address: asked,
        } if asked == address => request,
        other => panic!("expected the Relay at {address} added, got {other:?}"),
    }
}

/// The login `transition` asks to begin, at the Relay at `address`.
fn beginning(transition: ApplicationTransition, address: &str) -> RelayRequest {
    match transition {
        ApplicationTransition::BeginRelayLogin {
            request,
            address: asked,
        } if asked == address => request,
        other => panic!("expected a login begun at {address}, got {other:?}"),
    }
}

/// The removal `transition` asks for, of the Relay at `address`.
fn removal(transition: ApplicationTransition, address: &str) -> RelayRequest {
    match transition {
        ApplicationTransition::RemoveRelay {
            request,
            address: asked,
        } if asked == address => request,
        other => panic!("expected the Relay at {address} removed, got {other:?}"),
    }
}

/// The one follower `transition` asks for, of the login at `address`.
fn followed_at(transition: ApplicationTransition, address: &str) -> RelayRequest {
    match transition {
        ApplicationTransition::FollowRelayLogins(follows) => match follows.as_slice() {
            [
                RelayLoginFollow {
                    request,
                    address: asked,
                },
            ] if asked == address => *request,
            follows => panic!("expected the login at {address} followed, got {follows:?}"),
        },
        other => panic!("expected the login at {address} followed, got {other:?}"),
    }
}

fn begun(
    application: &mut Application,
    request: RelayRequest,
    login: RelayLogin,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::RelayLoginBegun { request, login })
        .expect("take the begun login")
}

/// Presses Enter on the selected Relay, at `address`, and has the Server
/// begin `login` there, answering what follows it.
fn log_in(application: &mut Application, address: &str, login: RelayLogin) -> RelayRequest {
    let request = beginning(press(application, KeyCode::Enter), address);
    followed_at(begun(application, request, login), address)
}

/// Has the login `follower` follows end as `login`, answering what that
/// asks of the Server.
fn settle(
    application: &mut Application,
    follower: RelayRequest,
    login: RelayLogin,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::RelayLoginSettled {
            request: follower,
            login,
        })
        .expect("take how the login ended")
}

/// Has the login `follower` follows end as `login`, and answers the listing
/// that asks for afresh with `relays`, as the Server then holds them.
fn settle_and_list(
    application: &mut Application,
    follower: RelayRequest,
    login: RelayLogin,
    relays: Vec<Relay>,
) {
    let ApplicationTransition::ListRelays(refresh) = settle(application, follower, login) else {
        panic!("a login ended asks for the Relays afresh");
    };
    list(application, refresh, relays);
}

/// Has the Server answer the removal `request` asked for, answering the
/// listing that asks for afresh.
fn removed(
    application: &mut Application,
    request: RelayRequest,
    address: &str,
    acknowledged: bool,
) -> RelayRequest {
    let ApplicationTransition::ListRelays(refresh) = application
        .handle_event(ApplicationEvent::RelayRemoved {
            request,
            result: Ok(RelayRemoval {
                address: address.to_owned(),
                acknowledged,
            }),
        })
        .expect("take the removal")
    else {
        panic!("a removal answered asks for the Relays afresh");
    };
    refresh
}

/// The Relay at `address` with `login` the latest begun there.
fn with_login(relay: Relay, login: RelayLogin) -> Relay {
    Relay {
        login: Some(login),
        ..relay
    }
}

/// The Relay at `address` logged in by `login`, done.
fn logged_in_by(address: &str, login: RelayLogin) -> Relay {
    with_login(logged_in(address), done(login))
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
        serve_through: false,
        login_needed_notice: None,
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

/// A login begun after [`pending`]'s, with a code of its own.
fn later_login() -> RelayLogin {
    RelayLogin {
        user_code: LATER_CODE.to_owned(),
        ..pending()
    }
}

fn done(login: RelayLogin) -> RelayLogin {
    RelayLogin {
        outcome: RelayLoginOutcome::Done { account: octocat() },
        ..login
    }
}

fn not_admitted() -> RelayLoginOutcome {
    RelayLoginOutcome::Refused {
        reason: RelayLoginRefusal::NotAdmitted,
        message: "the Relay does not admit this Account".to_owned(),
    }
}
