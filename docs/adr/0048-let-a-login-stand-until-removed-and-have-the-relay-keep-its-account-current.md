# Let a Login stand until removed, and have the Relay keep its Account current

A Server reached through a Relay is usually one nobody is sitting at (ADR-0046), so nothing about staying logged in may need someone at that machine. A Login is formed once: the Server's user logs in through an identity provider, and the Relay ties the resulting Account to that Server's identity key, the one ADR-0017 already gave it. From then on the Server proves itself by that key each time it connects and holds no other credential for the Relay, and the Login stands until the Server's user removes the Relay or the Relay's operator removes the Login. Whether the Account is still admitted is the Relay's to find out, without the Server: only the Relay ever speaks to an identity provider, and it checks each Account against its admission rules on its own. When an Account no longer satisfies them the Relay refuses every Login under it and cuts at once the connections it has joined for it, but forgets nothing, and one fresh login from any Server of that Account restores them all. An operator may also require a fresh login every so many days — off by default, and met the same way.

For GitHub, the first identity provider, this is done through a GitHub App the Relay's operator registers themselves. Login is GitHub's device flow, which needs no secret; the Relay reads the user's numeric id and keeps no token of theirs. Admission rules name users or organizations. A named user is resolved once to the numeric id, because a username given up can be claimed by someone else; an organization's members, private ones included, are checked through the app's installation on that organization, which also tells the Relay when a member is removed. For the OpenID Connect providers to come, a refresh token that stops working is the only sign of a deactivated user they all give, so there the Relay will hold one refresh token for each Account.

An Account is the Relay's own record, apart from the identity that logs in as it. Each answers to one identity for now and identities are never linked on their own, which leaves room for a user to link them by hand later.

## Considered Options

- **A bearer token the Relay issues and the Server stores.** Rejected: a new secret to store, rotate, and leak, and a copy of it is the Login. A Login proven by the identity key is useless off the machine that holds the key, and removing it at the Relay is the whole of revoking it.
- **A Login that expires, renewed by logging in again at its Server.** Rejected: it is the unattended Server that would expire, and it could not be reached to be renewed.
- **A GitHub OAuth App.** Rejected: organizations restrict third-party OAuth Apps by default, so private membership stays out of sight until an owner approves the app, and checking again later would mean keeping each user's token.
- **One GitHub App the Suru project publishes for every Relay.** Rejected: checking an organization needs the app's private key, which cannot be shared, and every Relay's logins would hang on the project's app continuing to exist — against the set-up-and-forget of ADR-0047.

## Consequences

- An organization rule needs an owner of that organization to install the operator's GitHub App, and a Relay refuses to start with a rule it cannot check.
- A named user gives the Relay nothing to check again at GitHub; such an Account lapses when the operator removes the name.
- Device login can be phished: a stranger who starts a login for their own Server and has a user approve it puts that Server under the user's Account. Under ADR-0046 it holds nothing there but a place against the Account's cap.
- Device login is the only way to log in for now. Microsoft's guidance has Entra tenants block it, so supporting Entra will take a second way to log in.
