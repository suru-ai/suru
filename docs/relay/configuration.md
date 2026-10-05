# Relay configuration reference

How a Relay is configured: its configuration file, the flags of `suru-relay run`, and what the Relay
refuses. [The setup guide](README.md) walks through putting a Relay up; this page is the reference for
every setting. [`suru-relay.toml`](suru-relay.toml) is a complete, commented example, and the Relay's
tests check that it is a configuration the Relay runs on.

## Where settings come from

A Relay reads its settings from two places:

- **Its configuration file**, in [TOML](https://toml.io), named by `--config <path>` or, where that is
  not given, by the `SURU_RELAY_CONFIG` environment variable. A Relay needs no file: everything can be
  given as flags instead.
- **The flags of `suru-relay run`**. Every key of the file is a flag as well, named for the key with
  `-` for `_`: `public_address` is `--public-address`. A list is given one item to a flag, which is
  named in the singular: `trusted_proxies` is `--trusted-proxy`, `admit_users` is `--admit-user`, and
  `admit_organizations` is `--admit-organization`.

A flag given overrides its key in the file. A list given on the command line replaces the file's list
whole rather than adding to it. A setting given neither way takes its default. `--config` and
`--database` may be given before or after the command.

A relative path in the file is read from the file's own directory; a relative path on the command line
is read from the working directory. On Windows, write paths in the file between single quotes, which
TOML takes literally: `database = 'C:\ProgramData\suru-relay\suru-relay.db'`.

The operator's commands — `suru-relay accounts …` and `suru-relay logins …` — read the same file to find
the database, so give them the same `--config` (or `SURU_RELAY_CONFIG`) as `run`. They use nothing in it
but `database`, though they refuse a file that cannot be read, is not TOML, or holds a key or value the
Relay does not know.

## Settings

| Key | Flag | Value | Default |
| --- | --- | --- | --- |
| `database` | `--database` | path | `suru-relay.db` in the working directory |
| `public_address` | `--public-address` | address | none: required |
| `listen_http` | `--listen-http` | socket address | none: this or `listen_https` is required |
| `listen_https` | `--listen-https` | socket address | none: this or `listen_http` is required |
| `tls_certificate_chain_file` | `--tls-certificate-chain-file` | path | none: required with `listen_https` |
| `tls_private_key_file` | `--tls-private-key-file` | path | none: required with `listen_https` |
| `trusted_proxies` | `--trusted-proxy` | list of addresses or networks | none |
| `github_client_id` | `--github-client-id` | string | none: the Relay logs nobody in |
| `github_private_key_file` | `--github-private-key-file` | path | none |
| `admit_users` | `--admit-user` | list of GitHub usernames | none |
| `admit_organizations` | `--admit-organization` | list of GitHub organization names | none |
| `recheck_minutes` | `--recheck-minutes` | whole number above 0 | `15` |
| `fresh_login_days` | `--fresh-login-days` | whole number above 0 | off |
| `logins_per_account` | `--logins-per-account` | whole number above 0 | `64` |
| `joined_connections_per_account` | `--joined-connections-per-account` | whole number above 0 | `256` |
| `connections_at_once` | `--connections-at-once` | whole number above 0 | `8192` |
| `logins_at_once` | `--logins-at-once` | whole number above 0 | `128` |
| `idle_connections_per_server` | `--idle-connections-per-server` | whole number above 0 | `32` |
| `idle_connections_per_server_without_login` | `--idle-connections-per-server-without-login` | whole number above 0 | `4` |
| `keepalive_seconds` | `--keepalive-seconds` | whole number above 0 | `20` |

### `database`

The SQLite database the Relay keeps its Accounts and Logins in, and nothing else: it keeps no history of
connections. `suru-relay run` makes it if it is not there, and carries it forward to its own version as
it starts. The Relay also keeps `-wal` and `-shm` files beside it while it runs, and holds a lock on a
`.lock` file beside it, so the directory must be writable by the Relay's user. One Relay runs on a
database at a time; a second is refused. The operator's commands refuse a database that is not there
rather than make one.

### `public_address`

The address Servers reach this Relay at, exactly as its users add it in Suru — such as
`https://relay.example.com`. It is an `https://` or `http://` address naming a host, with a port and a
path where they are needed, and no user, query or fragment; an address with no scheme is taken to be
`https://`. Every Server proves its identity key for the address it knows the Relay by, so this must be
that very address: the scheme, host, port and path users are given. Behind a reverse proxy it is the
proxy's address, not the Relay's own. A Relay is known by its address, so moving it to another means
each user adding it again and logging in again.

### `listen_http`, `listen_https`, `tls_certificate_chain_file`, `tls_private_key_file`

How the Relay listens, given exactly one way:

- **`listen_http`** — listen for plain HTTP at this address, such as `127.0.0.1:8080`, behind a reverse
  proxy that serves HTTPS for the Relay. The public address is then the proxy's, normally `https://`.
  No certificate file may be given.
- **`listen_https`** — serve HTTPS at this address, such as `0.0.0.0:443`, with the certificate in
  `tls_certificate_chain_file` and its private key in `tls_private_key_file`, both PEM. The chain file
  holds the Relay's own certificate first and then the certificates that issued it, as certificate
  authorities and tools such as certbot write them (`fullchain.pem`). The key may be PKCS #8, PKCS #1
  (RSA) or SEC1 (EC). TLS versions and cipher suites are [rustls](https://github.com/rustls/rustls)'s
  defaults: TLS 1.3 and TLS 1.2.

The Relay never obtains a certificate itself. It reads its certificate files as it starts, refusing to
start where they cannot be read, hold no certificate or key, or the key is not the certificate's. It
reads them again every minute, and where they have changed, serves what they hold to every connection
made from then on — where both can be read, hold a certificate and a key, and the key is the first
certificate's own. Anything else it passes over, going on serving the certificate it had and saying why
on standard error. It cannot tell a file still being written from a finished one, so replace each file
whole — write the new one beside it and rename it into place — rather than writing over it: a chain
caught after its first certificate but before the rest would be served as it stands. So renewing the
certificate is replacing the files, with no restart, on every platform. Connections already made keep the
certificate they were made with.

The private key files — this one and `github_private_key_file` — should be readable by the Relay's user
alone: owned by it, with mode `0600`. On Unix, the Relay warns as it starts where its group or anyone
else may read or change either.

A Relay serving HTTPS whose public address is `http://` starts, warning that Servers will reach it by
plain HTTP, since that works only through something that carries plain HTTP on to the Relay's HTTPS.

### `trusted_proxies`

The reverse proxies whose `X-Forwarded-For` header the Relay believes about the address a Server
connects from, which [the connection log](README.md#the-connection-log) names. Each is an address, such
as `10.0.0.5` or `::1`, or a network in CIDR notation, such as `10.0.0.0/8` or `fd00::/8`. The header is
believed only from a connection a named proxy made, read from the nearest hop outward: each named proxy
is believed about the address it was reached from, and the first address that is no named proxy's is
the Server's. With none named — the default — the header is ignored and the log names the address that
connected to the Relay, which behind a reverse proxy is the proxy's own.

### `github_client_id`, `github_private_key_file`

The GitHub App the Relay logs its users in through, which its operator registers
([the setup guide](README.md#registering-a-github-app) says how). `github_client_id` is the app's
**client ID**, such as `Iv23li…` — not its numeric app ID. Device login needs no client secret, and the
Relay takes none. Without a client ID the Relay logs nobody in.

`github_private_key_file` is a PEM file holding a private key generated for the app, as GitHub issues
it. The Relay signs as the app with it to check the members of organizations, so it is required with
`admit_organizations`. It is read once, as the Relay starts.

### `admit_users`, `admit_organizations`

Who the Relay admits. A Relay admits nobody until it is told who, and warns as it starts that it admits
nobody.

- `admit_users` names GitHub users by username. Each is looked up at GitHub once, as the Relay first
  starts naming it, and whoever went by the name then is admitted by it ever after, whatever they or
  anyone else go by later. The binding is kept for good, so a name removed and added again names the
  same person. The Relay refuses to start naming a user it cannot look up — GitHub knowing nobody by the
  name, or not answering.
- `admit_organizations` names GitHub organizations whose members — private members as well as public
  ones — the Relay admits. Each needs the GitHub App installed on it by an owner of the organization,
  able to read its members. The Relay checks each organization as it starts and refuses to start naming
  one it cannot check, saying which and why, GitHub not answering included; once running, it keeps its
  Accounts while GitHub does not answer.

A name removed from either list lapses, as the Relay starts, the Accounts it alone admitted: their Logins
are refused. Putting the name back does not restore them by itself: once the rules admit the user again,
one fresh login from any of the Account's Servers restores every Login under it. Both lists need
`github_client_id`.

### `recheck_minutes`

How often, in minutes, the Relay checks every Account against the admission rules again, counted from
the end of one pass to the beginning of the next. A member removed from an organization is cut off — and
the connections joined for them closed — at the next pass. Checking one Account against one organization
takes about one request of GitHub through the app's installation on it, which GitHub allows at least
5,000 of an hour, more for a large organization. A Relay with more Accounts than the interval allows for
checks some of them a pass later, each pass beginning where the last left off; for thousands of
Accounts, raise the interval.

### `fresh_login_days`

Requires each Account to have been logged in as afresh, from any one of its Servers, within this many
days. An Account not logged in as for longer lapses until one of its Servers logs in again, which
restores every Login under it. Off by default: a Login stands until it is removed.

### `logins_per_account`, `joined_connections_per_account`

Caps on each Account, so no one Account can exhaust the Relay. `logins_per_account` caps how many
Servers may be logged in under one Account, a lapsed Account's included; a login past it is refused
until a Server forgets its Login or the operator removes one. `joined_connections_per_account` caps how
many connections the Relay joins for one Account at once: a Server keeping a Remote in view through the
Relay holds one, so an Account whose Servers each keep the others in view holds one for each ordered
pair. A join past it is refused until one ends. Suru tells its user which cap was reached.

### `connections_at_once`, `logins_at_once`, `idle_connections_per_server`, `idle_connections_per_server_without_login`

Caps on connections, which anyone who can reach the Relay may open, logged in or not, so nobody — and no
number of identity keys made up on the spot — can exhaust it. Together with the caps on each Account and
those the Relay holds every Server to — four connections waiting to be reached, the oldest giving way to
a fifth, and sixteen joins asked of it and not yet taken up — whatever a connection is doing, it is held
to a cap.

- `connections_at_once` caps how many connections the Relay holds at once, from everyone together, each
  from the moment it is taken until it ends, however far it got: sending its request, making its TLS
  handshake, proving its Server's key, waiting to be reached, or carrying a join. A connection past it is
  let go as it comes — where the Relay serves HTTPS, before any TLS handshake — and nothing the Relay
  holds is disturbed; a Server let go connects again with backoff, and Suru reads the Relay as
  Unreachable meanwhile. The Relay says once on standard error that it has reached the cap, and once that
  it takes connections again. Each connection costs the Relay a socket and its buffers — a little over a
  hundred kibibytes idle, up to twice that carrying a join — so the default bounds them to a gibibyte or
  two. Keep it below the number of files the Relay's user may have open (`ulimit -n`; `LimitNOFILE=` for
  a systemd service), or connections past that are refused by the operating system instead, each costing
  the Relay a second's pause.
- `logins_at_once` caps how many logins may be under way at once, from every Server together, each for
  as long as its user takes to finish it at GitHub — up to a quarter of an hour. A login past it is
  refused, saying the Relay has as many logins under way as it takes, and the Server may begin one again
  later on the same connection. Each Server logs in on one connection at a time: a login it begins takes
  the place of the one it already has under way, which is given up, as Suru gives it up as it begins
  another.
- `idle_connections_per_server` caps how many idle connections each Server whose Login stands may hold at
  once: connections on which it is doing nothing — not waiting to be reached, not joined nor asking to
  be, not logging in. A Server holds one, to hear at once that its Login stops standing, unless it waits
  on that one to be reached, and one more for each join it asks for or takes up, for the moment between
  proving its key and asking. A connection past it is refused as it proves the Server's key, and one
  that becomes idle past it — its join refused, say — is let go.
- `idle_connections_per_server_without_login` caps the same for a Server holding no Login that stands:
  one that has yet to log in, or whose Login needs renewing. Such a Server asks what it came for — to
  log in, to be forgotten, or only to learn its Login needs renewing — the moment it has proven its key,
  so a connection of one that asks nothing for 30 seconds is let go as well. A login under way is not
  idle, however long it takes.

### `keepalive_seconds`

How many seconds the Relay lets a connection go with nothing sent on it before it sends a WebSocket
ping: a Server's waiting connection, a connection on which a login is under way — throughout it, however
long GitHub takes to begin it, to end it, or to say whether its user is admitted — and either side of a
joined connection carrying nothing. Reverse proxies, load balancers and firewalls commonly close a
connection idle for a minute; keep this well below the shortest such timeout on the way. A ping carries
nothing of what a join carries and is not counted in the connection log. A Server that does not take a
ping in within the Relay's send timeout is let go, as for anything else the Relay sends it.

## What the Relay refuses

`suru-relay run` refuses a configuration it cannot use, exiting with status 1 and saying on standard
error what is wrong and where: naming a setting as `--flag` where the command line gave it, and as `key`
in the file that holds it where the file did — and for a file a setting names, the file's path as well.
All but the last are refused before the Relay opens its database:

- a file that cannot be read, or is not TOML;
- a key the file does not know, saying which and listing those it does;
- a value of the wrong kind — a number in quotes, `0` where a number above 0 is wanted, an address or
  network that does not parse — saying which key, and where in the file;
- no `public_address`, or one that is not an `https://` or `http://` address naming a host;
- no way to listen, or both `listen_http` and `listen_https`;
- a certificate file given with `listen_http`, or one missing with `listen_https`;
- `admit_users`, `admit_organizations` or `github_private_key_file` without `github_client_id`, and
  `admit_organizations` without `github_private_key_file`;
- a certificate or GitHub App key file that cannot be read or does not hold what it should, or a
  GitHub App client ID that cannot be one;
- an address to listen at that cannot be listened at;
- a user or organization the admission rules name that cannot be looked up or checked at GitHub, named
  by its name. The rules' names are bound in the database, so this is refused only once the Relay has
  opened it, and carried it forward to its own version where it was older.

A flag that cannot be read — an unknown flag, or `--logins-per-account lots` — is refused by the command
line itself, with status 2.

## Exit status

| Status | Meaning |
| --- | --- |
| 0 | It did as asked. `run` exits 0 once stopped by Ctrl-C or, on Unix, `SIGTERM`. |
| 1 | It could not, saying why on standard error — a configuration it cannot use among them. |
| 2 | Its command line cannot be read. |
| 3 | A removal was made, and is refused from then on, but the running Relay did not confirm in time that it had cut what stood on it. |

## Environment

- `SURU_RELAY_CONFIG` names the configuration file where `--config` does not.
- `RUST_LOG` sets how much the Relay says on standard error, as
  [tracing's filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
  reads it; `info` unless set. Libraries that would log what they carry are held to warnings whatever it
  asks.
- The Relay reaches GitHub through the system's HTTP proxy — `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY`,
  and on Windows and macOS the system's proxy settings — and trusts what the operating system's trust
  store trusts, so a TLS-inspecting proxy's certificate authority is trusted for it once it is in that
  store.
