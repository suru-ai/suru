//! Pairing two machines through a Relay from the Client: choosing in `/relay`
//! whether the Server Serves through each Relay, offering the Relays it
//! Serves through among the ways `/serve` issues an Invite with, seeing in
//! the Invite preview the Relay an Invite travels through, and redeeming an
//! Invite through a Relay this Server is not logged in at — which begins that
//! login and carries on with the same redemption once it is done, so pairing
//! a new machine comes to one login and one paste.
//!
//! Keys are taken the whole way the run loop takes them, and every request
//! names itself, so an answer is delivered here to the request it answers —
//! or, where a test says so, to one the reader has since moved past. How the
//! Server's Relays stand reaches the Client as the Server pushes them.

use std::net::{Ipv4Addr, SocketAddr};

use crate::support::{
    deliver_settings, rendered_application_rows, rendered_application_rows_at, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, InvitePreview, IssueInviteRequest, IssuedInvite, RedeemInviteRequest,
        Relay, RelayAccount, RelayListing, RelayLogin, RelayLoginOutcome, RelayLoginRefusal,
        RelayState, Remote, RemoteStatus, SidebarVisibility, Way,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, ConnectRequest,
        RelayLoginFollow, RelayRequest, SemanticCommandId, ServeRequest,
    },
};

const COMPANY: &str = "https://relay.company.example";
const HOME: &str = "https://home.example.net";
const OTHER: &str = "https://other.example.org";
const VISIT: &str = "https://github.com/login/device";
const CODE: &str = "WDJB-MJHT";
const LATER_CODE: &str = "KQTR-VXZB";
const INVITE: &str = "suru-v1-through-the-relay";
const FINGERPRINT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// The run of the Client's own Server the tests' Relays come from.
const SERVER: uuid::Uuid = uuid::Uuid::from_u128(7);
/// What `/serve` says in place of the machine's addresses while the Serving
/// listener is off.
const LISTENER_OFF: &str =
    "The Serving listener is off, so an Invite offers none of this machine's addresses";
/// What `/serve` says where the listener is off and no Relay can be offered.
const NO_WAY: &str = "An Invite has no way to offer: turn the Serving listener on in the \
                      settings panel, or Serve through a Relay from /relay";
const NO_LOGIN: &str = "this Server holds no Login at the Relay at https://relay.company.example; \
                        log in there, then try again";
const DIFFERENT_ACCOUNTS: &str = "this Server is logged in at the Relay at \
                                  https://relay.company.example as someone-else (github), and \
                                  the Server it would reach there under another Account; a \
                                  Relay joins only Servers logged in under the same one, so log \
                                  this Server in there as the user that Server is logged in as, \
                                  or pair the two directly";

// The Serve-through choice, in `/relay`.

#[test]
fn relay_lets_the_user_choose_per_relay_whether_the_server_serves_through_it() {
    let mut application = serving_application(true);
    push(&mut application, 1, vec![logged_in(COMPANY), relay(HOME)]);
    let listing = open_relays(&mut application);
    list(
        &mut application,
        listing,
        1,
        vec![logged_in(COMPANY), relay(HOME)],
    );

    let shown = rendered_application_rows(&application).join("\n");
    assert!(
        !entry(&shown, COMPANY).contains("Serv"),
        "a Relay is Served through only once its user chooses to: {shown}"
    );
    assert!(
        prose(&application).contains("↑↓ choose · a add · s Serve through · x remove · Esc close"),
        "{}",
        prose(&application)
    );

    let request = serve_through(press(&mut application, KeyCode::Char('s')), COMPANY, true);
    let pending = rendered_application_rows(&application).join("\n");
    assert!(
        pending.contains("Turning Serve through on…"),
        "the choice is pending until the Server answers it: {pending}"
    );
    let ApplicationTransition::ListRelays(refresh) = chosen(
        &mut application,
        request,
        Relay {
            serve_through: true,
            ..logged_in(COMPANY)
        },
    ) else {
        panic!("what the Server answered is pictured afresh");
    };
    assert!(
        prose(&application).contains(&format!("This Server Serves through {COMPANY}")),
        "{}",
        prose(&application)
    );
    list(
        &mut application,
        refresh,
        2,
        vec![serving_through(logged_in(COMPANY)), relay(HOME)],
    );
    let shown = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&shown, COMPANY).contains("Logged in as octocat (github) · Serving through"),
        "{shown}"
    );
    assert!(
        prose(&application)
            .contains("↑↓ choose · a add · s stop Serving through · x remove · Esc close"),
        "{}",
        prose(&application)
    );

    // The choice is the entry's, and stored whether or not the Server is
    // logged in there — though it opens nothing until it is.
    press(&mut application, KeyCode::Down);
    let request = serve_through(press(&mut application, KeyCode::Char('s')), HOME, true);
    let ApplicationTransition::ListRelays(refresh) =
        chosen(&mut application, request, serving_through(relay(HOME)))
    else {
        panic!("what the Server answered is pictured afresh");
    };
    assert!(
        prose(&application).contains(&format!(
            "This Server Serves through {HOME} once it is logged in there"
        )),
        "{}",
        prose(&application)
    );
    list(
        &mut application,
        refresh,
        3,
        vec![
            serving_through(logged_in(COMPANY)),
            serving_through(relay(HOME)),
        ],
    );
    let shown = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&shown, HOME).contains("Login needed · Serves through once logged in"),
        "{shown}"
    );
    assert!(
        prose(&application).contains(
            "↑↓ choose · a add · Enter log in · s stop Serving through · x remove · Esc close"
        ),
        "{}",
        prose(&application)
    );

    // Turned off again, from the row as the Server pictures it.
    let request = serve_through(press(&mut application, KeyCode::Char('s')), HOME, false);
    chosen(&mut application, request, relay(HOME));
    assert!(
        prose(&application).contains(&format!("This Server no longer Serves through {HOME}")),
        "{}",
        prose(&application)
    );
}

#[test]
fn a_serve_through_choice_while_serving_is_off_says_serving_must_be_on_too() {
    let mut application = serving_application(false);
    push(
        &mut application,
        1,
        vec![serving_through(logged_in(COMPANY)), logged_in(HOME)],
    );
    let listing = open_relays(&mut application);
    list(
        &mut application,
        listing,
        1,
        vec![serving_through(logged_in(COMPANY)), logged_in(HOME)],
    );
    let shown = rendered_application_rows(&application).join("\n");
    assert!(
        entry(&shown, COMPANY).contains("Serves through once Serving is on"),
        "{shown}"
    );

    press(&mut application, KeyCode::Down);
    let request = serve_through(press(&mut application, KeyCode::Char('s')), HOME, true);
    chosen(&mut application, request, serving_through(logged_in(HOME)));
    assert!(
        prose(&application).contains(&format!(
            "This Server Serves through {HOME} once Serving is on; /serve turns Serving on"
        )),
        "{}",
        prose(&application)
    );
}

#[test]
fn the_serve_through_choice_is_only_ever_shown_as_the_server_pictures_it() {
    let mut application = serving_application(true);
    push(&mut application, 1, vec![logged_in(COMPANY)]);
    let listing = open_relays(&mut application);
    list(&mut application, listing, 1, vec![logged_in(COMPANY)]);

    let request = serve_through(press(&mut application, KeyCode::Char('s')), COMPANY, true);
    let ApplicationTransition::ListRelays(_) = chosen(
        &mut application,
        request,
        Relay {
            serve_through: true,
            ..logged_in(COMPANY)
        },
    ) else {
        panic!("what the Server answered is pictured afresh");
    };
    // Another Client turns it off again before the listing asked for lands,
    // and the Server pushes that: the row says what the Server holds.
    push(&mut application, 3, vec![logged_in(COMPANY)]);
    let shown = rendered_application_rows(&application).join("\n");
    assert!(
        !entry(&shown, COMPANY).contains("Serving through"),
        "nothing the list was answered is written over the Server's picture: {shown}"
    );
    assert!(
        prose(&application).contains("↑↓ choose · a add · s Serve through · x remove · Esc close"),
        "{}",
        prose(&application)
    );
}

#[test]
fn a_late_serve_through_answer_resolves_nothing_newer() {
    let mut application = serving_application(true);
    push(&mut application, 1, vec![logged_in(COMPANY)]);
    let listing = open_relays(&mut application);
    list(&mut application, listing, 1, vec![logged_in(COMPANY)]);
    let earlier = serve_through(press(&mut application, KeyCode::Char('s')), COMPANY, true);

    // The reader closes the list and opens it again before the Server
    // answers, and makes the choice afresh.
    press(&mut application, KeyCode::Esc);
    let listing = open_relays(&mut application);
    list(&mut application, listing, 1, vec![logged_in(COMPANY)]);
    let later = serve_through(press(&mut application, KeyCode::Char('s')), COMPANY, true);
    assert_ne!(earlier, later);

    application
        .handle_event(ApplicationEvent::RelayServeThroughFailed {
            request: earlier,
            error: "send Relay Serve-through choice".to_owned(),
        })
        .expect("take the late refusal");
    chosen(
        &mut application,
        earlier,
        serving_through(logged_in(COMPANY)),
    );
    let shown = prose(&application);
    assert!(
        shown.contains("Turning Serve through on…"),
        "the choice made afresh still waits on its own answer: {shown}"
    );

    application
        .handle_event(ApplicationEvent::RelayServeThroughFailed {
            request: later,
            error: "the Server could not store its Relays".to_owned(),
        })
        .expect("take the refusal");
    let shown = prose(&application);
    assert!(
        shown.contains(&format!(
            "Could not change whether this Server Serves through {COMPANY}: the Server could not \
             store its Relays"
        )),
        "{shown}"
    );
}

#[test]
fn the_serve_through_choice_is_a_semantic_command() {
    assert_eq!(
        SemanticCommandId::RelayServeThroughToggle.as_str(),
        "relay.serve-through.toggle"
    );
    let mut application = serving_application(true);
    let ApplicationTransition::ListRelays(listing) =
        invoke(&mut application, SemanticCommandId::RelayOpen)
    else {
        panic!("opening asks for the Relays");
    };
    list(&mut application, listing, 1, vec![logged_in(COMPANY)]);
    serve_through(
        invoke(&mut application, SemanticCommandId::RelayServeThroughToggle),
        COMPANY,
        true,
    );
}

// Relays among the ways `/serve` offers.

#[test]
fn serve_offers_each_relay_served_through_and_logged_in_at_and_the_invite_carries_exactly_the_ways_chosen()
 {
    let mut application = serving_application(true);
    push(
        &mut application,
        1,
        vec![
            serving_through(logged_in(COMPANY)),
            serving_through(relay(HOME)),
            logged_in(OTHER),
        ],
    );
    open_serve(&mut application, vec![direct()]);

    let ways = rendered_application_rows(&application).join("\n");
    assert!(ways.contains("Choose Invite addresses"), "{ways}");
    assert!(ways.contains("[x] 10.0.0.8:7777"), "{ways}");
    assert!(ways.contains(&format!("[x] Relay {COMPANY}")), "{ways}");
    assert!(
        ways.contains(&format!("[-] Relay {HOME} · login needed")),
        "a Relay the Server needs a login at is shown, and why it is not offered: {ways}"
    );
    assert!(
        ways.contains(&format!("[-] Relay {OTHER} · not Served through")),
        "{ways}"
    );

    // A Relay that cannot be offered says what would offer it.
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Char(' '));
    assert!(
        prose(&application).contains(&format!(
            "An Invite offers the Relay at {HOME} only once this Server is logged in there; log \
             in from /relay"
        )),
        "{}",
        prose(&application)
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Char(' '));
    assert!(
        prose(&application).contains(&format!(
            "An Invite offers the Relay at {OTHER} only once this Server Serves through it; \
             choose that from /relay"
        )),
        "{}",
        prose(&application)
    );

    // The address left out, the Invite offers the Relay alone.
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Char(' '));
    let (request, issuance) = issuance(press(&mut application, KeyCode::Enter));
    assert_eq!(
        issuance,
        IssueInviteRequest {
            ways: vec![Way::Relay(COMPANY.to_owned())],
        }
    );
    application
        .handle_event(ApplicationEvent::InviteIssued {
            request,
            invite: IssuedInvite {
                invite: INVITE.to_owned(),
                ways: vec![Way::Relay(COMPANY.to_owned())],
            },
            peers: Vec::new(),
        })
        .expect("show the fresh Invite");
    let issued = rendered_application_rows(&application).join("\n");
    assert!(issued.contains("Fresh Invite"), "{issued}");
    assert!(issued.contains(INVITE), "{issued}");
}

#[test]
fn serve_follows_the_relays_as_the_server_pushes_them_while_the_ways_are_chosen() {
    let mut application = serving_application(true);
    push(&mut application, 1, vec![logged_in(COMPANY)]);
    open_serve(&mut application, vec![direct()]);
    assert!(
        prose(&application).contains(&format!("[-] Relay {COMPANY} · not Served through")),
        "{}",
        prose(&application)
    );

    push(
        &mut application,
        2,
        vec![serving_through(logged_in(COMPANY))],
    );
    assert!(
        prose(&application).contains(&format!("[x] Relay {COMPANY}")),
        "{}",
        prose(&application)
    );

    // Its Login lapses before the Invite is issued, so the Invite does not
    // offer it.
    push(
        &mut application,
        3,
        vec![serving_through(Relay {
            state: RelayState::LoginNeeded,
            ..logged_in(COMPANY)
        })],
    );
    assert!(
        prose(&application).contains(&format!("[-] Relay {COMPANY} · login needed")),
        "{}",
        prose(&application)
    );
    let (_, issuance) = issuance(press(&mut application, KeyCode::Enter));
    assert_eq!(issuance.ways, vec![direct()]);
}

/// While the Serving listener is off, `/serve` lists none of the machine's
/// addresses and says why, following the Setting as it changes while the
/// ways are chosen as it follows the Relays, and an Invite then offers the
/// Relays alone.
#[test]
fn serve_offers_no_address_while_the_listener_is_off_and_follows_it_as_it_turns() {
    let mut application = listening_application(false);
    push(
        &mut application,
        1,
        vec![serving_through(logged_in(COMPANY))],
    );
    open_serve(&mut application, vec![direct()]);
    let ways = prose(&application);
    assert!(ways.contains(LISTENER_OFF), "{ways}");
    assert!(
        !ways.contains("10.0.0.8"),
        "no address of the machine's is offered while nothing listens there: {ways}"
    );
    assert!(ways.contains(&format!("[x] Relay {COMPANY}")), "{ways}");
    assert!(
        !ways.contains(NO_WAY),
        "the Relay is a way to offer: {ways}"
    );

    // Turned on while the ways are chosen, the listener offers the address.
    deliver_settings(&mut application, listening(true));
    let ways = prose(&application);
    assert!(ways.contains("[x] 10.0.0.8:7777"), "{ways}");
    assert!(!ways.contains(LISTENER_OFF), "{ways}");

    // Turned off again before the Invite is issued, it offers it no more.
    deliver_settings(&mut application, listening(false));
    let (_, issuance) = issuance(press(&mut application, KeyCode::Enter));
    assert_eq!(issuance.ways, vec![Way::Relay(COMPANY.to_owned())]);
}

/// With the Serving listener off and no Relay an Invite can offer, there is
/// no way to offer at all: `/serve` says so, and what would give it one,
/// wrapped whole however narrow, and asks for no Invite — until a Relay is
/// Served through.
#[test]
fn serve_says_an_invite_has_no_way_to_offer_with_the_listener_off_and_no_relay_served_through() {
    for width in [80, 48] {
        let mut application = listening_application(false);
        push(&mut application, 1, vec![logged_in(OTHER)]);
        open_serve(&mut application, vec![direct()]);
        let shown = prose_at(&application, width);
        assert!(
            shown.contains(LISTENER_OFF)
                && shown.contains(NO_WAY)
                && shown.contains(&format!("[-] Relay {OTHER}"))
                && shown.contains("not Served through"),
            "at {width} columns: {shown}"
        );
        assert!(
            shown.contains("Enter issue Invite · Esc close"),
            "the keys stay in the box: {shown}"
        );
        assert_eq!(
            press(&mut application, KeyCode::Enter),
            ApplicationTransition::Continue,
            "no Invite is asked for with no way to offer"
        );
        assert!(prose_at(&application, width).contains(NO_WAY));

        push(&mut application, 2, Vec::new());
        let shown = prose_at(&application, width);
        assert!(
            shown.contains(LISTENER_OFF) && shown.contains(NO_WAY),
            "with no Relay at all, the same: {shown}"
        );

        push(&mut application, 3, vec![serving_through(logged_in(OTHER))]);
        assert!(!prose_at(&application, width).contains(NO_WAY));
        let (_, issuance) = issuance(press(&mut application, KeyCode::Enter));
        assert_eq!(issuance.ways, vec![Way::Relay(OTHER.to_owned())]);
    }
}

#[test]
fn an_invite_the_server_refuses_says_why_and_keeps_the_ways_chosen() {
    let refusal = format!(
        "an Invite offers a Relay only where this Server Serves through it and is logged in \
         there, and the Relay at {COMPANY} is not one"
    );
    for width in [80, 48] {
        let mut application = serving_application(true);
        push(
            &mut application,
            1,
            vec![serving_through(logged_in(COMPANY))],
        );
        open_serve(&mut application, vec![direct()]);
        press(&mut application, KeyCode::Char(' '));
        let (request, _) = issuance(press(&mut application, KeyCode::Enter));
        application
            .handle_event(ApplicationEvent::InviteIssuanceFailed {
                request,
                error: refusal.clone(),
            })
            .expect("take the refusal");

        let shown = prose_at(&application, width);
        assert!(shown.contains("Choose Invite addresses"), "{shown}");
        assert!(
            shown.contains(&refusal),
            "the refusal is wrapped whole at {width} columns: {shown}"
        );
        assert!(
            shown.contains("[ ] 10.0.0.8:7777") && shown.contains(&format!("[x] Relay {COMPANY}")),
            "the ways stay as the reader chose them: {shown}"
        );
        assert!(
            shown.contains("Enter issue Invite · Esc close"),
            "the keys stay in the box: {shown}"
        );
    }
}

#[test]
fn a_late_invite_lands_nowhere() {
    let mut application = serving_application(true);
    push(
        &mut application,
        1,
        vec![serving_through(logged_in(COMPANY))],
    );
    open_serve(&mut application, vec![direct()]);
    let (earlier, _) = issuance(press(&mut application, KeyCode::Enter));

    press(&mut application, KeyCode::Esc);
    open_serve(&mut application, vec![direct()]);
    let (later, _) = issuance(press(&mut application, KeyCode::Enter));
    assert_ne!(earlier, later);
    application
        .handle_event(ApplicationEvent::InviteIssued {
            request: earlier,
            invite: IssuedInvite {
                invite: "suru-v1-superseded".to_owned(),
                ways: vec![direct()],
            },
            peers: Vec::new(),
        })
        .expect("take the late Invite");
    let shown = prose(&application);
    assert!(!shown.contains("suru-v1-superseded"), "{shown}");
    assert!(shown.contains("Preparing Serving…"), "{shown}");
}

// The Relay in the Invite preview.

#[test]
fn the_invite_preview_shows_the_relay_it_travels_through_beside_the_fingerprint() {
    let mut application = Application::default();
    push(&mut application, 1, vec![logged_in(COMPANY)]);
    begin_pairing(&mut application);
    preview(
        &mut application,
        vec![
            direct(),
            Way::Relay(COMPANY.to_owned()),
            Way::Relay(HOME.to_owned()),
        ],
    );

    let shown = rendered_application_rows(&application).join("\n");
    assert!(shown.contains("Confirm Serving Server"), "{shown}");
    assert!(shown.contains(FINGERPRINT), "{shown}");
    assert!(shown.contains("Reached by"), "{shown}");
    assert!(shown.contains("10.0.0.8:7777"), "{shown}");
    assert!(
        shown.contains(&format!("Relay {COMPANY} · logged in")),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("Relay {HOME} · login needed")),
        "a Relay this Server holds no Login at says so before it is trusted: {shown}"
    );
    assert!(
        prose(&application).contains(
            "To pair through a Relay that needs a login, this Server logs in there first"
        ),
        "{}",
        prose(&application)
    );
    assert!(shown.contains("Enter trust · Esc cancel"), "{shown}");

    // However narrow, each Relay is shown whole, and what is said of it
    // beneath where there is no room beside it.
    let narrow = unbroken_at(&application, 40);
    assert!(
        narrow.contains(COMPANY) && narrow.contains(HOME),
        "{narrow}"
    );
    let narrow = prose_at(&application, 40);
    assert!(
        narrow.contains(&format!("{COMPANY} logged in"))
            && narrow.contains(&format!("{HOME} login needed")),
        "{narrow}"
    );
    assert!(narrow.contains("Enter trust · Esc cancel"), "{narrow}");

    // It follows the Relays as the Server pushes them.
    push(
        &mut application,
        2,
        vec![logged_in(COMPANY), logged_in(HOME)],
    );
    let shown = rendered_application_rows(&application).join("\n");
    assert!(
        shown.contains(&format!("Relay {HOME} · logged in")),
        "{shown}"
    );
    assert!(
        !prose(&application).contains("this Server logs in there first"),
        "{}",
        prose(&application)
    );
}

// Redeeming an Invite that begins a login.

#[test]
fn redeeming_through_a_relay_this_server_is_not_logged_in_at_logs_in_there_and_carries_on_pairing()
{
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, redemption) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    assert_eq!(
        redemption,
        RedeemInviteRequest {
            invite: INVITE.to_owned(),
            name: Some("studio".to_owned()),
            ways: vec![Way::Relay(COMPANY.to_owned())],
        }
    );

    let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
    assert!(
        prose(&application).contains("Beginning login…"),
        "{}",
        prose(&application)
    );
    let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);
    let shown = prose(&application);
    for expected in [
        format!("Log in at {COMPANY}"),
        "To pair through this Relay, this Server logs in there first".to_owned(),
        "Visit this address on any device".to_owned(),
        VISIT.to_owned(),
        "and enter this code".to_owned(),
        CODE.to_owned(),
        "Waiting for the login…".to_owned(),
        "a copy address · c copy code · Esc back".to_owned(),
    ] {
        assert!(shown.contains(&expected), "{expected}: {shown}");
    }
    assert_eq!(
        press(&mut application, KeyCode::Char('a')),
        ApplicationTransition::CopyToClipboard(VISIT.into())
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('c')),
        ApplicationTransition::CopyToClipboard(CODE.into())
    );
    assert!(
        application.take_relay_retries().is_empty(),
        "nothing carries on while the login is under way"
    );

    settle(&mut application, follower, done(pending()));
    let (second, resumed) = resumed(&mut application);
    assert_ne!(first, second, "the redemption carried on is asked afresh");
    assert_eq!(resumed, redemption, "and is the same redemption");
    assert!(
        application.take_relay_retries().is_empty(),
        "and is carried on once only"
    );
    assert!(
        prose(&application).contains("Pairing Remote…"),
        "{}",
        prose(&application)
    );
    redeemed(&mut application, second);
    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Paired Remotes"), "{picker}");
    assert!(picker.contains("studio  Available"), "{picker}");
}

#[test]
fn a_relay_this_server_holds_no_entry_for_is_added_before_its_login_begins() {
    let mut application = Application::default();
    push(&mut application, 1, Vec::new());
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);

    let ApplicationTransition::AddRelay { request, address } =
        refused_for_login(&mut application, first)
    else {
        panic!("a Relay the Server holds no entry for is added first");
    };
    assert_eq!(address, COMPANY);
    assert!(
        prose(&application).contains("Adding Relay…"),
        "{}",
        prose(&application)
    );
    let beginning = beginning(
        application
            .handle_event(ApplicationEvent::RelayAdded {
                request,
                relay: relay(COMPANY),
            })
            .expect("take the added Relay"),
        COMPANY,
    );
    let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);

    // The Server pushes the login done before the follower answers: the
    // redemption carries on all the same, once.
    push(
        &mut application,
        3,
        vec![with_login(logged_in(COMPANY), done(pending()))],
    );
    let (second, _) = resumed(&mut application);
    settle(&mut application, follower, done(pending()));
    assert!(
        application.take_relay_retries().is_empty(),
        "the follower answering after carries nothing on again"
    );
    redeemed(&mut application, second);
    assert!(
        prose(&application).contains("Paired Remotes"),
        "{}",
        prose(&application)
    );
}

#[test]
fn an_addition_the_server_refuses_stops_the_redemption_there() {
    let mut application = Application::default();
    push(&mut application, 1, Vec::new());
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let ApplicationTransition::AddRelay { request, .. } =
        refused_for_login(&mut application, first)
    else {
        panic!("a Relay the Server holds no entry for is added first");
    };
    application
        .handle_event(ApplicationEvent::RelayAdditionFailed {
            request,
            error: "the Server could not store its Relays".to_owned(),
        })
        .expect("take the refusal");
    let shown = prose(&application);
    assert!(
        shown.contains(&format!(
            "Could not add the Relay at {COMPANY}: the Server could not store its Relays"
        )),
        "{shown}"
    );
    assert!(shown.contains("Enter try again · Esc back"), "{shown}");
    // Trying again asks for the redemption afresh, which says what it needs.
    redemption(press(&mut application, KeyCode::Enter));
}

#[test]
fn a_login_that_ends_without_a_login_stops_the_redemption_on_that_step_saying_why() {
    for (outcome, why) in [
        (
            RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::NotAdmitted,
                message: "the Relay does not admit this Account".to_owned(),
            },
            format!("You are not admitted to {COMPANY}; ask the Relay's operator to admit you"),
        ),
        (
            RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::LoginsCapReached { limit: 3 },
                message: "cap".to_owned(),
            },
            format!(
                "Your Account already has 3 Servers logged in at {COMPANY}, as many as the \
                 Relay's operator allows"
            ),
        ),
        (
            RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::Expired,
                message: "nobody finished the login before it expired".to_owned(),
            },
            format!("The login at {COMPANY} ended: nobody finished the login before it expired"),
        ),
    ] {
        let mut application = Application::default();
        push(&mut application, 1, vec![relay(COMPANY)]);
        let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
        let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
        let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);
        settle(
            &mut application,
            follower,
            RelayLogin {
                outcome,
                ..pending()
            },
        );

        let shown = prose_at(&application, 48);
        assert!(shown.contains(&why), "{why}: {shown}");
        assert!(shown.contains("Enter try again · Esc back"), "{shown}");
        assert!(application.take_relay_retries().is_empty(), "{shown}");

        // Tried again, the redemption is asked afresh, and its refusal
        // begins a login anew.
        let (again, _) = redemption(press(&mut application, KeyCode::Enter));
        beginning_at(refused_for_login(&mut application, again), COMPANY);
    }
}

#[test]
fn a_login_followed_no_further_stops_the_redemption_and_trying_again_follows_it_afresh() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
    let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);
    push(
        &mut application,
        2,
        vec![with_login(relay(COMPANY), pending())],
    );
    application
        .handle_event(ApplicationEvent::RelayLoginLost {
            request: follower,
            error: "read Relay login progress".to_owned(),
        })
        .expect("take the lost follower");
    let shown = prose(&application);
    assert!(
        shown.contains(&format!(
            "Stopped following the login at {COMPANY}: read Relay login progress"
        )),
        "{shown}"
    );

    let (again, _) = redemption(press(&mut application, KeyCode::Enter));
    // The login goes on at the Server, so it is shown again, not begun anew.
    followed_at(refused_for_login(&mut application, again), COMPANY);
    assert!(
        prose(&application).contains(CODE),
        "{}",
        prose(&application)
    );
}

#[test]
fn esc_steps_back_from_the_login_and_pairing_again_shows_the_same_login_rather_than_begin_another()
{
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
    let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);

    press(&mut application, KeyCode::Esc);
    let shown = prose(&application);
    assert!(shown.contains("Configure Remote"), "{shown}");
    assert!(
        shown.contains(NO_LOGIN),
        "the step it stepped back to says what is needed: {shown}"
    );
    press(&mut application, KeyCode::Esc);
    assert!(
        !prose(&application).contains("Configure Remote"),
        "{}",
        prose(&application)
    );

    // The overlay is opened afresh while the login goes on at the Server.
    let (again, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    assert_eq!(
        refused_for_login(&mut application, again),
        ApplicationTransition::Continue,
        "the login under way is followed already, and no other begins"
    );
    assert!(
        prose(&application).contains(CODE),
        "{}",
        prose(&application)
    );

    settle(&mut application, follower, done(pending()));
    let (resumed_request, _) = resumed(&mut application);
    redeemed(&mut application, resumed_request);
    assert!(
        prose(&application).contains("Paired Remotes"),
        "{}",
        prose(&application)
    );
}

#[test]
fn a_login_under_way_that_this_client_did_not_begin_is_followed_rather_than_begun_again() {
    let mut application = Application::default();
    push(
        &mut application,
        1,
        vec![with_login(relay(COMPANY), pending())],
    );
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let follower = followed_at(refused_for_login(&mut application, first), COMPANY);
    assert!(
        prose(&application).contains(CODE),
        "{}",
        prose(&application)
    );

    // A later login begun elsewhere at the same Relay supersedes it; the one
    // on display ended, so it stops the redemption.
    settle(
        &mut application,
        follower,
        RelayLogin {
            outcome: RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::Interrupted,
                message: "the login was given up for a later one at the same Relay".to_owned(),
            },
            ..pending()
        },
    );
    assert!(
        prose(&application).contains("Enter try again · Esc back"),
        "{}",
        prose(&application)
    );
    push(
        &mut application,
        2,
        vec![with_login(
            relay(COMPANY),
            RelayLogin {
                user_code: LATER_CODE.to_owned(),
                ..pending()
            },
        )],
    );
    let (again, _) = redemption(press(&mut application, KeyCode::Enter));
    followed_at(refused_for_login(&mut application, again), COMPANY);
    assert!(
        prose(&application).contains(LATER_CODE),
        "{}",
        prose(&application)
    );
}

#[test]
fn a_login_restored_from_another_server_of_the_account_carries_the_redemption_on() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
    followed_at(begun(&mut application, beginning, pending()), COMPANY);

    push(&mut application, 2, vec![logged_in(COMPANY)]);
    resumed(&mut application);
}

#[test]
fn a_direct_way_is_tried_first_and_no_login_begins_where_one_carried_the_redemption() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (request, redemption) = pair(
        &mut application,
        vec![direct(), Way::Relay(COMPANY.to_owned())],
    );
    assert_eq!(
        redemption.ways,
        vec![direct(), Way::Relay(COMPANY.to_owned())],
        "the redemption is asked first, whatever the Relay needs"
    );
    redeemed(&mut application, request);
    assert!(
        prose(&application).contains("Paired Remotes"),
        "{}",
        prose(&application)
    );

    // Refused for any other reason, nothing is logged in at.
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (request, _) = pair(
        &mut application,
        vec![direct(), Way::Relay(COMPANY.to_owned())],
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::InviteRedemptionFailed {
                request,
                error: "could not reach an offered address with the Invite's pinned key".to_owned(),
                login_needed_at: None,
            })
            .expect("take the refusal"),
        ApplicationTransition::Continue
    );
    let shown = prose(&application);
    assert!(shown.contains("Configure Remote"), "{shown}");
    assert!(
        shown.contains("could not reach an offered address with the Invite's pinned key"),
        "{shown}"
    );
}

#[test]
fn a_refusal_under_different_accounts_is_said_on_the_step_that_failed_in_words_to_act_on() {
    for width in [80, 48] {
        let mut application = Application::default();
        push(&mut application, 1, vec![relay(COMPANY)]);
        let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
        let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
        let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);
        settle(&mut application, follower, done(pending()));
        let (second, _) = resumed(&mut application);

        application
            .handle_event(ApplicationEvent::InviteRedemptionFailed {
                request: second,
                error: DIFFERENT_ACCOUNTS.to_owned(),
                login_needed_at: None,
            })
            .expect("take the refusal");
        let shown = prose_at(&application, width);
        assert!(shown.contains("Configure Remote"), "{shown}");
        assert!(
            shown.contains(DIFFERENT_ACCOUNTS),
            "the refusal is wrapped whole at {width} columns: {shown}"
        );
        assert!(
            application.take_relay_retries().is_empty(),
            "nothing is logged in at for it"
        );
        // And the redemption can be asked again from there.
        redemption(press(&mut application, KeyCode::Enter));
    }
}

#[test]
fn late_answers_to_a_redemption_or_a_login_left_behind_move_nothing() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
    press(&mut application, KeyCode::Esc);
    assert_eq!(
        begun(&mut application, beginning, pending()),
        ApplicationTransition::Continue,
        "a login begun for a step the reader left is not followed from it"
    );
    let shown = prose(&application);
    assert!(shown.contains("Configure Remote"), "{shown}");
    assert!(!shown.contains(CODE), "{shown}");

    // A redemption the reader moved past lands nowhere.
    let (second, _) = redemption(press(&mut application, KeyCode::Enter));
    press(&mut application, KeyCode::Esc);
    let (third, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    redeemed(&mut application, second);
    assert!(
        prose(&application).contains("Pairing Remote…"),
        "{}",
        prose(&application)
    );
    refused_for_login(&mut application, second);
    assert!(
        prose(&application).contains("Pairing Remote…"),
        "{}",
        prose(&application)
    );
    redeemed(&mut application, third);
    assert!(
        prose(&application).contains("Paired Remotes"),
        "{}",
        prose(&application)
    );
}

#[test]
fn the_login_a_redemption_waits_on_is_copied_by_the_relay_logins_own_commands() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
    begun(&mut application, beginning, pending());
    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayCopyAddress),
        ApplicationTransition::CopyToClipboard(VISIT.into())
    );
    assert_eq!(
        invoke(&mut application, SemanticCommandId::RelayCopyCode),
        ApplicationTransition::CopyToClipboard(CODE.into())
    );

    // However narrow, the code and the keys stay in the box.
    let narrow = prose_at(&application, 40);
    assert!(narrow.contains(CODE), "{narrow}");
    assert!(
        narrow.contains("a copy address · c copy code · Esc back"),
        "{narrow}"
    );
}

// Seeing every way an Invite offers, whole, before trusting it.

/// A Relay's address long enough to wrap however wide the box, whose
/// registrable domain — the part that says who controls it — comes last.
const LONG: &str =
    "https://relay.company.example.a-rather-long-label-before-the-real-one.evil.example";

#[test]
fn the_invite_preview_shows_each_relays_whole_address_however_narrow() {
    for width in [80, 48, 40] {
        let mut application = Application::default();
        push(&mut application, 1, Vec::new());
        begin_pairing(&mut application);
        preview(
            &mut application,
            vec![direct(), Way::Relay(LONG.to_owned())],
        );

        let shown = unbroken_at(&application, width);
        assert!(
            shown.contains(LONG),
            "the whole address is shown at {width} columns, never shortened: {shown}"
        );
        assert!(
            prose_at(&application, width).contains("login needed"),
            "{}",
            prose_at(&application, width)
        );
        assert!(
            !prose_at(&application, width).contains('…'),
            "{}",
            prose_at(&application, width)
        );
        assert!(
            prose_at(&application, width).contains("Enter trust · Esc cancel"),
            "{}",
            prose_at(&application, width)
        );

        // And as the Remote's ways are ordered, once it is trusted.
        press(&mut application, KeyCode::Enter);
        let shown = unbroken_at(&application, width);
        assert!(shown.contains("Configure Remote"), "{shown}");
        assert!(
            shown.contains(LONG),
            "the whole address is shown at {width} columns: {shown}"
        );
        assert!(!shown.contains('…'), "{shown}");
    }
}

#[test]
fn every_way_an_invite_offers_can_be_read_before_it_is_trusted() {
    let mut ways = (1..=20)
        .map(|last| Way::Direct(SocketAddr::from((Ipv4Addr::new(10, 0, 0, last), 7777))))
        .collect::<Vec<_>>();
    ways.push(Way::Relay(COMPANY.to_owned()));
    let mut application = Application::default();
    push(&mut application, 1, Vec::new());
    begin_pairing(&mut application);
    preview(&mut application, ways);

    let shown = prose(&application);
    assert!(
        !shown.contains(COMPANY),
        "more ways than fit, so the Relay offered last is out of view: {shown}"
    );
    assert!(!shown.contains("… and"), "{shown}");
    assert!(
        shown.contains("↓ more below") && shown.contains("↑↓ scroll · Enter trust · Esc cancel"),
        "and the reader is told so, and how to see it: {shown}"
    );
    for _ in 0..30 {
        press(&mut application, KeyCode::Down);
    }
    let shown = prose(&application);
    assert!(
        shown.contains(&format!("Relay {COMPANY} · login needed")),
        "every way can be scrolled to before it is trusted: {shown}"
    );
    assert!(shown.contains("↑ more above"), "{shown}");
    assert!(
        shown.contains("Enter trust · Esc cancel"),
        "the keys stay in view: {shown}"
    );
    for _ in 0..30 {
        press(&mut application, KeyCode::Up);
    }
    assert!(
        prose(&application).contains("Fingerprint"),
        "{}",
        prose(&application)
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(
        prose(&application).contains("Configure Remote"),
        "{}",
        prose(&application)
    );
}

#[test]
fn the_relay_being_added_and_logged_in_at_is_shown_by_its_whole_address() {
    let mut application = Application::default();
    push(&mut application, 1, Vec::new());
    begin_pairing(&mut application);
    preview(&mut application, vec![Way::Relay(LONG.to_owned())]);
    press(&mut application, KeyCode::Enter);
    let (request, _) = redemption(press(&mut application, KeyCode::Enter));
    let ApplicationTransition::AddRelay { request, .. } =
        refused_for_login_at(&mut application, request, LONG)
    else {
        panic!("a Relay the Server holds no entry for is added first");
    };
    let adding = unbroken_at(&application, 40);
    assert!(adding.contains(LONG), "{adding}");
    assert!(adding.contains("Adding Relay…"), "{adding}");

    let beginning = beginning(
        application
            .handle_event(ApplicationEvent::RelayAdded {
                request,
                relay: relay(LONG),
            })
            .expect("take the added Relay"),
        LONG,
    );
    assert!(
        unbroken_at(&application, 40).contains(LONG),
        "{}",
        unbroken_at(&application, 40)
    );
    begun(&mut application, beginning, pending());
    let shown = unbroken_at(&application, 40);
    assert!(shown.contains(LONG), "{shown}");
    assert!(shown.contains(CODE), "{shown}");
}

#[test]
fn the_relay_list_shows_each_relays_whole_address_however_narrow() {
    let mut application = serving_application(true);
    push(&mut application, 1, vec![logged_in(LONG)]);
    let listing = open_relays(&mut application);
    list(&mut application, listing, 1, vec![logged_in(LONG)]);
    for width in [80, 40] {
        let shown = unbroken_at(&application, width);
        assert!(
            shown.contains(LONG),
            "the whole address is listed at {width} columns: {shown}"
        );
        assert!(
            prose_at(&application, width).contains("Logged in as octocat (github)"),
            "{}",
            prose_at(&application, width)
        );
    }
}

// A Login restored while the redemption waits on one.

#[test]
fn a_login_restored_while_its_login_is_being_begun_carries_the_redemption_on_and_shows_no_code() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let beginning = beginning(refused_for_login(&mut application, first), COMPANY);

    // Another Server of the Account restores the Login before the Server
    // answers the login begun here.
    push(&mut application, 2, vec![logged_in(COMPANY)]);
    let (second, _) = resumed(&mut application);
    assert_eq!(
        begun(&mut application, beginning, pending()),
        ApplicationTransition::Continue,
        "the login begun meanwhile is not followed from here"
    );
    let shown = prose(&application);
    assert!(!shown.contains(CODE), "{shown}");
    assert!(shown.contains("Pairing Remote…"), "{shown}");
    assert!(
        application.take_relay_retries().is_empty(),
        "carried on once"
    );
    redeemed(&mut application, second);
}

#[test]
fn a_login_restored_while_its_relay_is_being_added_carries_the_redemption_on() {
    let mut application = Application::default();
    push(&mut application, 1, Vec::new());
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let ApplicationTransition::AddRelay { request, .. } =
        refused_for_login(&mut application, first)
    else {
        panic!("a Relay the Server holds no entry for is added first");
    };
    push(&mut application, 2, vec![logged_in(COMPANY)]);
    resumed(&mut application);
    assert!(
        !matches!(
            application
                .handle_event(ApplicationEvent::RelayAdded {
                    request,
                    relay: relay(COMPANY),
                })
                .expect("take the added Relay"),
            ApplicationTransition::BeginRelayLogin { .. }
        ),
        "no login is begun for a redemption already carried on"
    );
    assert!(application.take_relay_retries().is_empty());
}

#[test]
fn a_login_the_client_already_hears_stands_carries_the_redemption_on_at_once() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, redemption_asked) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    // The Login was restored between the Server refusing and this Client
    // hearing of it.
    push(&mut application, 2, vec![logged_in(COMPANY)]);
    let (second, again) = redemption(refused_for_login(&mut application, first));
    assert_ne!(first, second);
    assert_eq!(again, redemption_asked);
}

// A follower that ends on a later login.

#[test]
fn a_follower_that_ends_on_a_later_login_never_leaves_the_redemption_waiting() {
    for (later, carried_on) in [
        (
            RelayLogin {
                user_code: LATER_CODE.to_owned(),
                outcome: RelayLoginOutcome::Refused {
                    reason: RelayLoginRefusal::Expired,
                    message: "nobody finished the login before it expired".to_owned(),
                },
                ..pending()
            },
            false,
        ),
        (
            done(RelayLogin {
                user_code: LATER_CODE.to_owned(),
                ..pending()
            }),
            true,
        ),
    ] {
        let mut application = Application::default();
        push(&mut application, 1, vec![relay(COMPANY)]);
        let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
        let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
        let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);

        // Another Client began a later login there before this follower
        // reached the Server, which it followed in its place.
        settle(&mut application, follower, later);
        let shown = prose(&application);
        assert!(
            !shown.contains("Waiting for the login…"),
            "nothing is left waiting on a login no follower follows: {shown}"
        );
        if carried_on {
            resumed(&mut application);
        } else {
            assert!(
                shown.contains(&format!(
                    "Another login begun at {COMPANY} replaced this one. The login at {COMPANY} \
                     ended: nobody finished the login before it expired"
                )),
                "{shown}"
            );
            assert!(shown.contains("Enter try again · Esc back"), "{shown}");
            redemption(press(&mut application, KeyCode::Enter));
        }
    }
}

// `/serve` keeps the keys on the way they were on.

#[test]
fn a_relay_a_push_removes_above_the_keys_leaves_them_on_the_way_they_were_on() {
    let mut application = serving_application(true);
    let a = "https://a.example.com";
    let b = "https://b.example.com";
    let c = "https://c.example.com";
    let offered = |address: &str| serving_through(logged_in(address));
    push(
        &mut application,
        1,
        vec![offered(a), offered(b), offered(c)],
    );
    open_serve(&mut application, vec![direct()]);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);

    // Another Client removes the Relay above the one the keys are on.
    push(&mut application, 2, vec![offered(b), offered(c)]);
    press(&mut application, KeyCode::Char(' '));
    let (_, asked) = issuance(press(&mut application, KeyCode::Enter));
    assert_eq!(
        asked.ways,
        vec![direct(), Way::Relay(c.to_owned())],
        "Space left out the way the keys were on, not the one that moved under them"
    );

    // Where the way the keys are on goes itself, they move to its neighbour.
    let mut application = serving_application(true);
    push(
        &mut application,
        1,
        vec![offered(a), offered(b), offered(c)],
    );
    open_serve(&mut application, vec![direct()]);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    push(&mut application, 2, vec![offered(a), offered(c)]);
    press(&mut application, KeyCode::Char(' '));
    let (_, asked) = issuance(press(&mut application, KeyCode::Enter));
    assert_eq!(asked.ways, vec![direct(), Way::Relay(a.to_owned())]);
}

// Answers the reader has moved past.

#[test]
fn a_late_preview_lands_nowhere() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    begin_pairing_with(&mut application, "suru-v1-first");
    let first = inspecting(press(&mut application, KeyCode::Enter), "suru-v1-first");
    press(&mut application, KeyCode::Esc);
    let (second, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let beginning = beginning(refused_for_login(&mut application, second), COMPANY);
    begun(&mut application, beginning, pending());

    // The preview of the Invite pasted first answers at last.
    previewed(
        &mut application,
        first,
        "suru-v1-first",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        vec![direct()],
    );
    application
        .handle_event(ApplicationEvent::InvitePreviewFailed {
            request: first,
            invite: "suru-v1-first".to_owned(),
            error: "Invite is malformed".to_owned(),
        })
        .expect("take the late refusal");
    let shown = prose(&application);
    assert!(
        shown.contains(CODE) && shown.contains("Waiting for the login…"),
        "the login the redemption waits on stands: {shown}"
    );
    assert!(!shown.contains("ffffffff"), "{shown}");
    assert!(!shown.contains("Invite is malformed"), "{shown}");
}

#[test]
fn a_late_serving_preparation_lands_nowhere() {
    let mut application = serving_application(true);
    let first = begin_serving(&mut application);
    press(&mut application, KeyCode::Esc);
    prepared(&mut application, first, vec![direct()]);
    assert!(
        !prose(&application).contains("Choose Invite addresses"),
        "a closed picker stays closed: {}",
        prose(&application)
    );

    let second = begin_serving(&mut application);
    let other = Way::Direct(SocketAddr::from((Ipv4Addr::new(10, 0, 0, 9), 7777)));
    prepared(&mut application, second, vec![direct(), other.clone()]);
    press(&mut application, KeyCode::Char(' '));
    prepared(&mut application, first, vec![direct(), other.clone()]);
    application
        .handle_event(ApplicationEvent::ServingPreparationFailed {
            request: first,
            error: "could not list the machine's addresses".to_owned(),
        })
        .expect("take the late failure");
    let shown = prose(&application);
    assert!(
        shown.contains("[ ] 10.0.0.8:7777") && shown.contains("[x] 10.0.0.9:7777"),
        "what the reader left out stays left out: {shown}"
    );
    assert!(!shown.contains("could not list"), "{shown}");
}

// A long refusal on the Remote's details.

#[test]
fn a_long_refusal_on_the_remotes_details_reads_whole_with_the_keys_in_view() {
    let account = "a-thirty-nine-character-github-username";
    assert_eq!(account.len(), 39);
    let refusal = format!(
        "this Server is logged in at the Relay at {LONG} as {account} (github), and the Server \
         it would reach there under another Account; a Relay joins only Servers logged in \
         under the same one, so log this Server in there as the user that Server is logged in \
         as, or pair the two directly"
    );
    let mut application = Application::default();
    push(&mut application, 1, vec![logged_in(LONG)]);
    begin_pairing(&mut application);
    preview(&mut application, vec![Way::Relay(LONG.to_owned())]);
    press(&mut application, KeyCode::Enter);
    let (request, _) = redemption(press(&mut application, KeyCode::Enter));
    application
        .handle_event(ApplicationEvent::InviteRedemptionFailed {
            request,
            error: refusal.clone(),
            login_needed_at: None,
        })
        .expect("take the refusal");

    let keys = "↑↓ move · Shift+↑↓ reorder · Tab field · Enter pair · Esc cancel";
    let shown = prose_at(&application, 40);
    assert!(shown.contains("this Server is logged in at"), "{shown}");
    assert!(shown.contains(keys), "the keys stay in view: {shown}");
    assert!(shown.contains("PgUp/PgDn"), "and how to read on: {shown}");
    assert!(!shown.contains("or pair the two directly"), "{shown}");

    let mut read = vec![shown];
    for _ in 0..20 {
        press(&mut application, KeyCode::PageDown);
        read.push(prose_at(&application, 40));
    }
    let end = read.last().expect("read at least once");
    assert!(
        end.contains("or pair the two directly"),
        "the refusal's last instruction is reached: {end}"
    );
    assert!(end.contains(keys), "the keys stay in view: {end}");
    press(&mut application, KeyCode::PageUp);
    for _ in 0..20 {
        press(&mut application, KeyCode::PageUp);
    }
    assert!(
        prose_at(&application, 40).contains("this Server is logged in at"),
        "{}",
        prose_at(&application, 40)
    );
}

// No login is begun again for a redemption already carried on.

#[test]
fn a_redemption_carried_on_and_refused_for_a_login_again_stops_rather_than_log_in_again() {
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let beginning = beginning(refused_for_login(&mut application, first), COMPANY);
    let follower = followed_at(begun(&mut application, beginning, pending()), COMPANY);
    settle(&mut application, follower, done(pending()));
    let (second, _) = resumed(&mut application);

    assert_eq!(
        refused_for_login(&mut application, second),
        ApplicationTransition::Continue,
        "no login begins a second time on its own"
    );
    let shown = prose(&application);
    assert!(
        shown.contains(&format!("Pairing was refused again: {NO_LOGIN}")),
        "{shown}"
    );
    assert!(shown.contains("Enter try again · Esc back"), "{shown}");

    // Tried again by the reader, the redemption logs in anew.
    let (third, _) = redemption(press(&mut application, KeyCode::Enter));
    beginning_at(refused_for_login(&mut application, third), COMPANY);
}

#[test]
fn a_login_done_once_the_reader_left_it_says_the_invite_now_pairs() {
    // The reader stepped back to the Remote's details.
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let begin = beginning(refused_for_login(&mut application, first), COMPANY);
    let follower = followed_at(begun(&mut application, begin, pending()), COMPANY);
    press(&mut application, KeyCode::Esc);
    settle(&mut application, follower, done(pending()));
    let shown = prose(&application);
    assert!(
        shown.contains(&format!(
            "Logged in at {COMPANY}; Enter pairs through it now"
        )),
        "{shown}"
    );
    assert!(!shown.contains(NO_LOGIN), "{shown}");
    assert!(
        application.take_relay_retries().is_empty(),
        "the reader stepped back, so nothing pairs without them"
    );

    // The reader closed the overlay altogether.
    let mut application = Application::default();
    push(&mut application, 1, vec![relay(COMPANY)]);
    let (first, _) = pair(&mut application, vec![Way::Relay(COMPANY.to_owned())]);
    let begin = beginning(refused_for_login(&mut application, first), COMPANY);
    let follower = followed_at(begun(&mut application, begin, pending()), COMPANY);
    press(&mut application, KeyCode::Esc);
    press(&mut application, KeyCode::Esc);
    settle(&mut application, follower, done(pending()));
    type_terminal_text(&mut application, "/pair");
    press(&mut application, KeyCode::Enter);
    let shown = prose(&application);
    assert!(shown.contains("Paste Invite"), "{shown}");
    assert!(
        shown.contains(&format!(
            "Logged in at {COMPANY}; paste the Invite again to pair through it"
        )),
        "{shown}"
    );
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

/// A Client whose Server is Serving, or not, as `serving` says, with the
/// Sidebar off the frame so the overlays have it to themselves.
fn serving_application(serving: bool) -> Application {
    let mut application = Application::default();
    let mut settings = EffectiveSettings::default();
    settings.serving.enabled = serving;
    settings.serving.port = 7777;
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    application
}

/// What a Client whose Server is Serving with its listener on or off, as
/// `listener` says, holds of the Settings in force.
fn listening(listener: bool) -> EffectiveSettings {
    let mut settings = EffectiveSettings::default();
    settings.serving.enabled = true;
    settings.serving.listener = listener;
    settings.serving.port = 7777;
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    settings
}

/// A Client whose Server is Serving with its listener on or off, as
/// `listener` says.
fn listening_application(listener: bool) -> Application {
    let mut application = Application::default();
    deliver_settings(&mut application, listening(listener));
    application
}

/// The Server pushes its Relays as they stood at `revision`.
fn push(application: &mut Application, revision: u64, relays: Vec<Relay>) {
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

/// Types `/relay`, answering the listing it asks for.
fn open_relays(application: &mut Application) -> RelayRequest {
    type_terminal_text(application, "/relay");
    let ApplicationTransition::ListRelays(request) = press(application, KeyCode::Enter) else {
        panic!("/relay asks the Client's own Server for its Relays");
    };
    request
}

fn list(application: &mut Application, request: RelayRequest, revision: u64, relays: Vec<Relay>) {
    application
        .handle_event(ApplicationEvent::RelaysListed {
            request,
            listing: RelayListing {
                instance: SERVER,
                revision,
                relays,
            },
        })
        .expect("list the Relays");
}

/// The Serve-through choice `transition` asks for, at the Relay at
/// `address`.
fn serve_through(transition: ApplicationTransition, address: &str, on: bool) -> RelayRequest {
    match transition {
        ApplicationTransition::SetRelayServeThrough {
            request,
            address: asked,
            serve_through,
        } if asked == address && serve_through == on => request,
        other => panic!("expected Serve through {address} set to {on}, got {other:?}"),
    }
}

/// Has the Server answer the Serve-through choice `request` asked for with
/// the Relay as it then stands.
fn chosen(
    application: &mut Application,
    request: RelayRequest,
    relay: Relay,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::RelayServeThroughSet { request, relay })
        .expect("take the Serve-through choice")
}

/// Types `/serve`, and finds the machine's addresses to be `addresses`.
fn open_serve(application: &mut Application, addresses: Vec<Way>) {
    let request = begin_serving(application);
    prepared(application, request, addresses);
}

/// Types `/serve`, answering the preparation it asks for.
fn begin_serving(application: &mut Application) -> ServeRequest {
    type_terminal_text(application, "/serve");
    let ApplicationTransition::BeginServing { request, .. } = press(application, KeyCode::Enter)
    else {
        panic!("/serve prepares Serving");
    };
    request
}

/// Has the preparation `request` asked for find the machine's addresses to
/// be `addresses`.
fn prepared(application: &mut Application, request: ServeRequest, addresses: Vec<Way>) {
    application
        .handle_event(ApplicationEvent::ServingPrepared {
            request,
            settings: None,
            candidates: addresses,
        })
        .expect("load the machine's addresses");
}

/// The Invite `transition` asks the Server to issue.
fn issuance(transition: ApplicationTransition) -> (ServeRequest, IssueInviteRequest) {
    match transition {
        ApplicationTransition::IssueInvite { request, issuance } => (request, issuance),
        other => panic!("expected an Invite issued, got {other:?}"),
    }
}

/// Opens `/pair`, with the Remote listing it asks for answered, and pastes
/// the Invite there.
fn begin_pairing(application: &mut Application) {
    begin_pairing_with(application, INVITE);
}

fn begin_pairing_with(application: &mut Application, invite: &str) {
    type_terminal_text(application, "/pair");
    press(application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(Vec::new()))
        .expect("list the Remotes");
    application
        .take_terminal_event(InputEvent::Paste(invite.to_owned()))
        .expect("paste the Invite");
}

/// Inspects the Invite pasted, which offers `ways`.
fn preview(application: &mut Application, ways: Vec<Way>) {
    let request = inspecting(press(application, KeyCode::Enter), INVITE);
    previewed(application, request, INVITE, FINGERPRINT, ways);
}

/// The preview `transition` asks for, of `invite`.
fn inspecting(transition: ApplicationTransition, invite: &str) -> ConnectRequest {
    match transition {
        ApplicationTransition::PreviewInvite {
            request,
            invite: asked,
        } if asked == invite => request,
        other => panic!("expected {invite} previewed, got {other:?}"),
    }
}

/// Has the preview `request` asked for show `invite` as offering `ways`,
/// under `fingerprint`.
fn previewed(
    application: &mut Application,
    request: ConnectRequest,
    invite: &str,
    fingerprint: &str,
    ways: Vec<Way>,
) {
    application
        .handle_event(ApplicationEvent::InvitePreviewed {
            request,
            invite: invite.to_owned(),
            preview: InvitePreview {
                hostname: "studio".to_owned(),
                fingerprint: fingerprint.to_owned(),
                ways,
            },
        })
        .expect("show the preview");
}

/// Pastes the Invite into `/pair`, previews it offering `ways`, trusts it,
/// and pairs, answering the redemption asked for.
fn pair(application: &mut Application, ways: Vec<Way>) -> (ConnectRequest, RedeemInviteRequest) {
    begin_pairing(application);
    preview(application, ways);
    press(application, KeyCode::Enter);
    redemption(press(application, KeyCode::Enter))
}

fn redemption(transition: ApplicationTransition) -> (ConnectRequest, RedeemInviteRequest) {
    match transition {
        ApplicationTransition::RedeemInvite {
            request,
            redemption,
        } => (request, redemption),
        other => panic!("expected the Invite redeemed, got {other:?}"),
    }
}

/// The redemption carried on once the login it waited on is done.
fn resumed(application: &mut Application) -> (ConnectRequest, RedeemInviteRequest) {
    let mut carried = application.take_relay_retries();
    assert_eq!(carried.len(), 1, "one redemption carried on: {carried:?}");
    redemption(carried.remove(0))
}

/// Has the Server refuse the redemption `request` asked for, for want of a
/// Login at the company Relay.
fn refused_for_login(
    application: &mut Application,
    request: ConnectRequest,
) -> ApplicationTransition {
    refused_for_login_at(application, request, COMPANY)
}

/// Has the Server refuse the redemption `request` asked for, for want of a
/// Login at the Relay at `relay`.
fn refused_for_login_at(
    application: &mut Application,
    request: ConnectRequest,
    relay: &str,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::InviteRedemptionFailed {
            request,
            error: format!(
                "this Server holds no Login at the Relay at {relay}; log in there, then try again"
            ),
            login_needed_at: Some(relay.to_owned()),
        })
        .expect("take the refusal")
}

fn redeemed(application: &mut Application, request: ConnectRequest) {
    application
        .handle_event(ApplicationEvent::RemoteRedeemed {
            request,
            remote: Remote {
                name: "studio".to_owned(),
                fingerprint: FINGERPRINT.to_owned(),
                ways: vec![Way::Relay(COMPANY.to_owned())],
                status: RemoteStatus::Available,
            },
        })
        .expect("take the Remote redeemed");
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

fn beginning_at(transition: ApplicationTransition, address: &str) {
    beginning(transition, address);
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

fn settle(application: &mut Application, follower: RelayRequest, login: RelayLogin) {
    application
        .handle_event(ApplicationEvent::RelayLoginSettled {
            request: follower,
            login,
        })
        .expect("take how the login ended");
}

/// What the screen says, its wrapped Rows read on as one line of prose.
fn prose(application: &Application) -> String {
    prose_at(application, 80)
}

/// What the screen `width` columns wide says, read as [`prose_at`] reads it.
fn prose_at(application: &Application, width: u16) -> String {
    boxed_rows_at(application, width).join(" ")
}

/// What the screen `width` columns wide says, its Rows run together with
/// nothing between them, so a word split across Rows reads whole.
fn unbroken_at(application: &Application, width: u16) -> String {
    boxed_rows_at(application, width).concat()
}

/// The Rows of the screen `width` columns wide that say anything — what the
/// overlay open says, where one is, read from inside its box so nothing
/// drawn beside it is read with it — each trimmed.
fn boxed_rows_at(application: &Application, width: u16) -> Vec<String> {
    let rows = rendered_application_rows_at(application, width, 24)
        .into_iter()
        .map(|row| row.chars().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let boxed = rows.iter().enumerate().find_map(|(top, row)| {
        let text = row.iter().collect::<String>();
        ["┌ Connect ", "┌ Serve ", "┌ Relay "]
            .iter()
            .any(|title| text.contains(title))
            .then(|| {
                let left = row.iter().position(|cell| *cell == '┌')?;
                let right = row.iter().position(|cell| *cell == '┐')?;
                Some((top, left, right))
            })
            .flatten()
    });
    let inside = |row: &[char]| -> String {
        match boxed {
            Some((_, left, right)) => row
                .get(left + 1..right.min(row.len()))
                .unwrap_or_default()
                .iter()
                .collect(),
            None => row.iter().collect(),
        }
    };
    rows.iter()
        .skip(boxed.map_or(0, |(top, _, _)| top + 1))
        .take_while(|row| boxed.is_none_or(|(_, left, _)| row.get(left) != Some(&'└')))
        .map(|row| inside(row).trim().to_owned())
        .filter(|row| !row.is_empty())
        .collect()
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

fn direct() -> Way {
    Way::Direct(SocketAddr::from((Ipv4Addr::new(10, 0, 0, 8), 7777)))
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

fn serving_through(relay: Relay) -> Relay {
    Relay {
        serve_through: true,
        ..relay
    }
}

fn with_login(relay: Relay, login: RelayLogin) -> Relay {
    Relay {
        login: Some(login),
        ..relay
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

fn done(login: RelayLogin) -> RelayLogin {
    RelayLogin {
        outcome: RelayLoginOutcome::Done { account: octocat() },
        ..login
    }
}
