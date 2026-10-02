# Hold the Relay to compatibility the rest of Suru is not held to

Suru breaks freely: nothing in it accounts for an earlier version of itself, and a Pairing between Servers of different protocol versions is refused (ADR-0017). A Relay is the exception. Whoever runs one sets it up once and should be able to forget it; it is upgraded on a schedule of its own, apart from the Suru installs that use it, and it has to go on working while they change. So from the Relay's first release:

- a newer Suru works with an older Relay, and an upgraded Relay with an older Suru: Server and Relay agree a version of the protocol between them as they connect, and that protocol changes by addition and is kept small;
- upgrading a Relay in place keeps its Accounts, its Logins, and its configuration;
- a Suru upgrade carries forward the Server's identity key and its Logins — the one part of a Server's own data held to this — because a Server that lost them would drop off its Relay with nobody at the machine to log it in again.

The Relay carries a version number of its own and is released only when it changes. Until its first release the protocol is marked unstable, must match exactly, and changes as freely as anything else; the first release freezes it as version 1.

ADR-0045 is what makes the promise affordable. A Relay never parses what Servers say to each other, so everything Suru breaks in the Pairing protocol passes through it untouched, and the promise covers only how a Server logs in, proves itself, waits to be reached, and asks to be joined.

## Considered Options

- **Hold the Relay to the project's rule: versions match exactly, and a mismatch is refused saying which side is behind.** Rejected: every breaking Suru release would become an upgrade each operator has to make, with their users cut off until they do.
- **Give the Relay Suru's version number.** Rejected: every Suru release would then look like a Relay release an operator ought to install.

## Consequences

- The promise is to keep compatibility wherever possible, not absolutely. A change that cannot be made by addition is raised and decided rather than simply made.
- A Server goes on speaking older versions of the Relay protocol for as long as Relays that old are in use, so code that looks dead by the project's usual standard is not.
- What a Server and a Relay say to each other tolerates what it does not recognize, where the Pairing's own structures reject an unknown field.
- A Server's identity key and Logins, and a Relay's own records, are migrated forward rather than replaced. Everything else a Server stores stays under the project's rule.
