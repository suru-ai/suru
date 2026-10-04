# Running a Relay

A **Relay** lets Suru Servers that cannot reach each other directly — behind a company firewall, across NAT, on a
network that lets only ordinary web traffic out — carry a Pairing between them. Every Server connects outward to
the Relay over HTTPS, and the Relay joins two such connections and passes bytes between them. It cannot read or
alter what the Servers say to each other: the Pairing's own mutual-key TLS runs end to end through it. It admits
users by GitHub login, and joins only Servers logged in under the same Account.

This guide is for whoever runs one: yourself, for your own machines, or a company, for its people. You run your
own Relay; the Suru project runs none, there is no default Relay address in Suru, and a running Relay depends on
nothing the Suru project operates — only on its own machine and GitHub. [The configuration
reference](configuration.md) describes every setting, and [`suru-relay.toml`](suru-relay.toml) is a complete,
commented example.

- [What you need](#what-you-need)
- [Installing the Relay](#installing-the-relay)
- [Registering a GitHub App](#registering-a-github-app)
- [Installing the app on an organization](#installing-the-app-on-an-organization)
- [Writing the admission rules](#writing-the-admission-rules)
- [Configuring the Relay](#configuring-the-relay)
- [Behind a reverse proxy](#behind-a-reverse-proxy)
- [Serving HTTPS itself](#serving-https-itself)
- [The network](#the-network)
- [Running it as a service](#running-it-as-a-service)
- [Running it in a container](#running-it-in-a-container)
- [The operator's commands](#the-operators-commands)
- [The connection log](#the-connection-log)
- [Caps](#caps)
- [Requiring a fresh login](#requiring-a-fresh-login)
- [Keeping the GitHub App working](#keeping-the-github-app-working)
- [Backups](#backups)
- [Versions and upgrades](#versions-and-upgrades)
- [Using the Relay from Suru](#using-the-relay-from-suru)

## What you need

- A machine to run the Relay on — Linux, macOS or Windows — or somewhere to run a Linux container. The Relay is one
  process with its records in a SQLite file; it needs no database server or other service.
- An address for it that every Server can reach over HTTPS, such as `https://relay.example.com`.
- A certificate for that address: served either by a reverse proxy in front of the Relay, or by the Relay itself
  from certificate files you give it. The Relay never obtains a certificate itself.
- A GitHub account that can register a GitHub App, and, to admit an organization's members, an owner of that
  organization to install the app on it.
- An accurate clock on the Relay's machine. The Relay signs short-lived tokens as its GitHub App, and GitHub
  refuses them from a clock that is wrong by more than a minute or so. Keep NTP running.

## Installing the Relay

The Relay is released apart from Suru, with a version of its own (see [Versions and upgrades](#versions-and-upgrades)).
Its releases are the repository's GitHub releases tagged `suru-relay-vX.Y.Z`, each with:

- an archive for each platform — `suru-relay-vX.Y.Z-<target>.tar.gz`, or `.zip` on Windows — holding the
  `suru-relay` binary, this guide, the configuration reference and the example configuration, for
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` and
  `aarch64-pc-windows-msvc`;
- a Linux container image for `amd64` and `arm64`, `ghcr.io/suru-ai/suru-relay:X.Y.Z`.

Put the binary somewhere on the machine's `PATH`, such as `/usr/local/bin/suru-relay`. To build it from source
instead, from a checkout of the repository at the release's tag:

```sh
cargo build --locked --profile dist --package suru-relay
# target/dist/suru-relay
```

`suru-relay --version` says the Relay's version and the versions of the Relay protocol it speaks:

```console
$ suru-relay --version
suru-relay 0.1.0 (Relay protocol unstable-1)
```

## Registering a GitHub App

Users log in to the Relay with GitHub, through a GitHub App that you register for your Relay. The app is how GitHub
knows your Relay: it is what users see when they log in, and what an organization installs to let the Relay check
who its members are. It needs no server of its own and receives nothing from GitHub. Every Relay has its own app:
the Suru project publishes none.

Logging in uses GitHub's *device flow*: Suru shows its user an address and a short code, and they enter the code at
that address on any device. The Relay never sees anyone's password, keeps no GitHub token of anyone's, and needs no
client secret.

1. Choose who owns the app. An app owned by an **organization** is managed by its owners; one owned by **your own
   account** is managed by you. If the Relay is for one company, register the app under the company's organization.
   - Under an organization: *Organization settings → Developer settings → GitHub Apps → New GitHub App*, or
     `https://github.com/organizations/<organization>/settings/apps/new`.
   - Under your account: *Settings → Developer settings → GitHub Apps → New GitHub App*, or
     `https://github.com/settings/apps/new`.
2. Fill in the form:
   - **GitHub App name**: whatever users should see as they log in, such as `Example Corp Suru Relay`. GitHub
     requires it to be unique across GitHub.
   - **Homepage URL**: required by GitHub but unused by the Relay; your company's site, or the Relay's address.
   - **Callback URL**: leave empty. The device flow uses none.
   - **Expire user authorization tokens**: leave **ticked**. The Relay reads who logged in with the token GitHub
     gives it and then discards it, but cannot revoke it without a client secret, so an expiring token is one
     that soon stops being anyone's.
   - **Request user authorization (OAuth) during installation**: leave **unticked**.
   - **Enable Device Flow**: **tick it**. Without it, every login is refused, and Suru says device login is not
     enabled for the Relay's app.
   - **Setup URL**: leave empty.
   - **Webhook**: untick **Active**. The Relay receives nothing from GitHub; it asks GitHub on its own schedule.
3. **Permissions**: open *Organization permissions* and set **Members** to **Read-only**. Give it nothing else: no
   repository permissions, no account permissions. The Relay uses this one permission to check whether a user is a
   member of an organization the admission rules name, private members included. A Relay that admits named users
   alone uses no permission at all, but granting Members now saves every organization owner accepting a changed
   permission later.
4. **Where can this GitHub App be installed?**
   - **Only on this account** if the app's owner is the one organization whose members the Relay admits.
   - **Any account** if it must be installed on any other organization — an app registered under your own account
     to admit a company's members, or one Relay admitting several organizations.
5. *Create GitHub App*.
6. On the app's page, note its **Client ID**, which starts `Iv`. That — not the numeric *App ID* — is what the
   Relay is given as `github_client_id`.
7. Under *Private keys*, *Generate a private key*. GitHub downloads a `.pem` file. Put it on the Relay's machine
   where only the Relay's user can read it — owned by that user, mode `0600` — such as
   `/etc/suru-relay/github-app.pem`, and give its path as
   `github_private_key_file`. The Relay signs as the app with it to check organizations' members; it is not needed
   to admit named users alone. Anyone holding it can act as the app, so keep it as you would a password, and delete
   it from GitHub if it leaks, generating another. Do not generate a client secret: the Relay takes none.

## Installing the app on an organization

To admit an organization's members, the app must be installed on that organization, by an **owner of the
organization**. Installing it on a personal account does not let the Relay check an organization's members.

- Where the app is owned by the organization itself, an owner opens the app's settings and chooses *Install App*,
  then the organization.
- Otherwise, an owner of the organization opens the app's public page, `https://github.com/apps/<app-name>`, and
  chooses *Install*, then the organization. A member who is not an owner can only request the installation, which
  an owner must approve.

GitHub shows what the app asks for — read access to the organization's members — and the owner approves it. The
app asks for no access to any repository. Install it on each organization the admission rules name.

## Writing the admission rules

A Relay admits nobody until it is told who, and warns as it starts that it admits nobody. It admits anyone either
rule names:

```toml
# By GitHub username: for yourself, or a few people.
admit_users = ["octocat", "mona"]

# By GitHub organization: every member, private members included. Needs the app installed on each,
# and github_private_key_file.
admit_organizations = ["example-corp"]
```

- **Named users** are looked up at GitHub once, as the Relay first starts naming them, and kept by GitHub's numeric
  ID for them. Whoever went by the name then is admitted by it ever after, so a username given up and claimed by
  someone else admits nobody new. The binding is kept even after the name is taken out of the rules, so putting it
  back names the same person. The Relay refuses to start naming a user GitHub knows nobody by, or while GitHub
  cannot be asked about a name it has not looked up yet.
- **Organizations** are checked as each user logs in, and again for every Account every `recheck_minutes` (15
  unless set), through the app's installation on the organization. Someone removed from the organization is cut off
  at the next check — every Server of theirs refused, and every connection joined for them closed. Each check is
  about one request of GitHub for each Account and organization, and GitHub allows an installation at least 5,000 an
  hour, more for a large organization; a Relay with thousands of Accounts should raise `recheck_minutes`. The Relay
  checks every organization as it starts and refuses to start with one it cannot check, saying which and why —
  GitHub not answering included — so a mistake fails loudly rather than admitting the wrong people, or nobody.
  Once running, it keeps its Accounts while GitHub does not answer, and refuses new logins it cannot check.
- **Taking a name out of the rules** lapses, as the Relay next starts, the Accounts it alone admitted: their Logins
  are refused. Putting the name back does not restore them by itself: once the rules admit the user again, one
  fresh login from any of the Account's Servers restores every Login under it. The rules are the whole truth of who
  may use the Relay.

## Configuring the Relay

Start from [`suru-relay.toml`](suru-relay.toml) and save it as `/etc/suru-relay/suru-relay.toml` (or wherever you
like). At the least it says:

```toml
database = "/var/lib/suru-relay/suru-relay.db"
public_address = "https://relay.example.com"
listen_http = "127.0.0.1:8080"              # or listen_https and the certificate files
trusted_proxies = ["127.0.0.1", "::1"]
github_client_id = "Iv23li..."
github_private_key_file = "/etc/suru-relay/github-app.pem"
admit_organizations = ["example-corp"]
```

**`public_address` must be exactly the address your users add to Suru** — scheme, host, port and path. Each Server
proves itself to the Relay for the address it knows the Relay by, and the Relay refuses a proof made for any other.
Behind a reverse proxy, it is the proxy's address. A Relay is known by its address: moving it to another means
every user adding it again and logging in again.

Run it:

```sh
suru-relay --config /etc/suru-relay/suru-relay.toml run
```

Every setting can be given as a flag as well, which overrides the file — `--public-address`, `--listen-http`, and so
on — and the file can be named by `SURU_RELAY_CONFIG` instead of `--config`. The Relay refuses a configuration it
cannot use, saying what is wrong and where — a misspelled key among them, so a typo cannot quietly admit nobody, or
everybody — and exits with status 1. It refuses settings, and files they name, before it opens its database; a name
in the admission rules that GitHub cannot look up only once it has. [The configuration reference](configuration.md)
has every setting, its default, and everything the Relay refuses.

The Relay says on standard error when it is ready, and where it listens:

```text
2026-10-05T09:00:00.000000Z  INFO suru_relay: Relay ready address=127.0.0.1:8080
```

It stops on Ctrl-C, and on Unix on `SIGTERM`, ending every connection it carries and writing the connection log's
last lines before it exits. Servers reconnect on their own once it is back.

## Behind a reverse proxy

With `listen_http`, the Relay listens for plain HTTP, and a reverse proxy in front of it serves HTTPS at the public
address. Servers speak WebSocket to the Relay at `/connect` beneath its address, so the proxy must pass on WebSocket
upgrades and leave the connection open for as long as it lasts:

- **Upgrade**: pass on the `Upgrade` and `Connection` headers, over HTTP/1.1 to the Relay.
- **Timeouts**: a Server keeps one connection open at the Relay for as long as it Serves through it, and one for
  each Remote it keeps in view through it, which can stay quiet for hours, and a login waits on its user. The Relay
  sends a WebSocket ping on any connection it has sent nothing on for 20 seconds (`keepalive_seconds`) — a Server
  waiting to be reached, one logging in, however long its user or GitHub takes, and either side of a joined
  connection carrying nothing — and Servers keep their own connections alive too, so an idle timeout of a minute or
  more works; but set the proxy's timeouts comfortably above that.
- **No buffering**: the proxy passes bytes on as they come.
- **The client's address**: the proxy names the address it forwards for in `X-Forwarded-For`, and you name the
  proxy in `trusted_proxies`, so [the connection log](#the-connection-log) names each Server's own address. The
  Relay believes the header from the proxies you name and from nothing else, so a Server cannot forge it.

**nginx**:

```nginx
server {
    listen 443 ssl;
    server_name relay.example.com;
    ssl_certificate     /etc/letsencrypt/live/relay.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/relay.example.com/privkey.pem;

    location = /connect {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_read_timeout 300s;
        proxy_send_timeout 300s;
        proxy_buffering off;
    }
}
```

**Caddy**, which passes WebSocket upgrades and sets `X-Forwarded-For` on its own, and sets no idle timeout on them:

```caddy
relay.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Reloading Caddy's configuration closes the WebSockets it carries; Servers reconnect on their own.

With either, in the Relay's configuration:

```toml
public_address = "https://relay.example.com"
listen_http = "127.0.0.1:8080"
trusted_proxies = ["127.0.0.1", "::1"]
```

A load balancer or firewall on the way that closes idle connections — many close them after a minute — is kept
from closing the Relay's by its pings; set `keepalive_seconds` well below the shortest idle timeout on the way.

A Relay at a path beneath its host, such as `https://example.com/relay`, is reached at `/relay/connect`, which the
proxy passes on to the Relay as `/connect`; in nginx, `location = /relay/connect { proxy_pass
http://127.0.0.1:8080/connect; … }`.

## Serving HTTPS itself

With `listen_https`, the Relay serves HTTPS itself from a certificate chain and its private key in PEM files —
`fullchain.pem` and `privkey.pem`, as certbot writes them, or as your certificate authority issues them:

```toml
public_address = "https://relay.example.com"
listen_https = "0.0.0.0:443"
tls_certificate_chain_file = "/etc/suru-relay/fullchain.pem"
tls_private_key_file = "/etc/suru-relay/privkey.pem"
```

TLS versions and cipher suites are rustls's defaults, TLS 1.3 and TLS 1.2. The Relay reads the files as it starts,
refusing to start with files it cannot serve from, and reads them again every minute: where they have changed, and
both can be read, hold a certificate and a key, and the key is the first certificate's own, it serves what they hold
to every new connection, with no restart. Anything else it passes over, going on serving the certificate it had and
saying why on standard error. Connections already open keep the certificate they began with.

So renewing is replacing the files — whole. The Relay cannot tell a file still being written from a finished one: a
chain caught after its first certificate but before the certificates that issued it would be served as it stands,
where the key has not changed. So write each new file beside the old one and rename it into place, which replaces it
at once. With certbot, a deploy hook such as `/etc/letsencrypt/renewal-hooks/deploy/suru-relay` does that, leaving
the key readable by the Relay's user alone:

```sh
#!/bin/sh
set -e
cd /etc/suru-relay
install -m 0644 "$RENEWED_LINEAGE/fullchain.pem" fullchain.pem.new
install -m 0600 -o suru-relay "$RENEWED_LINEAGE/privkey.pem" privkey.pem.new
mv -f fullchain.pem.new fullchain.pem
mv -f privkey.pem.new privkey.pem
```

Port 443 is privileged on Linux: the systemd unit below grants the Relay that one capability when it serves HTTPS
there. Or listen at another port, such as `0.0.0.0:8443`, and make the public address `https://relay.example.com:8443`
— remembering that some networks allow HTTPS out on port 443 alone.

## The network

What a network must allow, and nothing more:

- **From every Server to the Relay**: outbound HTTPS — TCP to the Relay's public address, on port 443 unless the
  address names another. It is one WebSocket over HTTPS, the one address and port a network rule has to name. A
  Server reaches it through the system's HTTP proxy where one is set, and verifies the Relay's certificate against
  the operating system's trust store, so a TLS-inspecting proxy whose certificate authority the machine trusts
  works, as long as it passes WebSocket upgrades on.
- **From the Relay to GitHub**: outbound HTTPS to `github.com` and `api.github.com`, on port 443, for logins and
  admission checks — through the system's HTTP proxy where one is set (`HTTPS_PROXY`).
- **To the Relay**: inbound to the port it listens at, from the reverse proxy, or from the Servers where it serves
  HTTPS itself.
- **To the Servers**: nothing inbound. A Server Serving through a Relay needs no listening port open at all.

## Running it as a service

On Linux, with systemd. Make a user for it, its configuration directory, and the unit:

```sh
sudo useradd --system --home-dir /var/lib/suru-relay --shell /usr/sbin/nologin suru-relay
sudo install -d -m 0750 -g suru-relay /etc/suru-relay
# The configuration, readable by the Relay's group:
sudo install -m 0640 -g suru-relay suru-relay.toml /etc/suru-relay/
# The GitHub App's private key — and the certificate's, where the Relay serves HTTPS itself — readable by the
# Relay's user alone:
sudo install -m 0600 -o suru-relay github-app.pem /etc/suru-relay/
```

A private key file its group or anyone else may read or change is warned of as the Relay starts.

`/etc/systemd/system/suru-relay.service`:

```ini
[Unit]
Description=Suru Relay
Wants=network-online.target
After=network-online.target

[Service]
User=suru-relay
Group=suru-relay
ExecStart=/usr/local/bin/suru-relay --config /etc/suru-relay/suru-relay.toml run
Restart=on-failure
RestartSec=5s
# /var/lib/suru-relay, where the database lives, made and kept writable for the Relay.
StateDirectory=suru-relay
StateDirectoryMode=0700
# Serving HTTPS itself on port 443:
#AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now suru-relay
journalctl -u suru-relay -f
```

Every way of running the Relay names the `run` command. The connection log, on standard output, and the Relay's
diagnostics, on standard error, both go to the journal. To keep the connection log in a file of its own instead, add
`LogsDirectory=suru-relay` and `StandardOutput=append:/var/log/suru-relay/connections.log`.

A Relay that cannot start — GitHub not answering as it checks an organization, say — exits with status 1, and
systemd starts it again after `RestartSec`.

## Running it in a container

The image runs `suru-relay run` as an unprivileged user, reads its configuration from
`/etc/suru-relay/suru-relay.toml` (`SURU_RELAY_CONFIG` names it), and keeps its records in the volume at
`/var/lib/suru-relay`. Inside the container, the Relay listens on every address, so Docker can reach it:

```toml
database = "/var/lib/suru-relay/suru-relay.db"
public_address = "https://relay.example.com"
listen_http = "0.0.0.0:8080"
# The network Docker connects the proxy on the host through.
trusted_proxies = ["172.16.0.0/12"]
github_client_id = "Iv23li..."
github_private_key_file = "/etc/suru-relay/github-app.pem"
admit_organizations = ["example-corp"]
```

```sh
docker volume create suru-relay
docker run --detach --name suru-relay --restart unless-stopped \
    --publish 127.0.0.1:8080:8080 \
    --volume suru-relay:/var/lib/suru-relay \
    --volume /etc/suru-relay:/etc/suru-relay:ro \
    ghcr.io/suru-ai/suru-relay:0.1.0
```

Or with Compose:

```yaml
services:
  relay:
    image: ghcr.io/suru-ai/suru-relay:0.1.0
    restart: unless-stopped
    ports:
      - "127.0.0.1:8080:8080"
    volumes:
      - relay-records:/var/lib/suru-relay
      - /etc/suru-relay:/etc/suru-relay:ro
volumes:
  relay-records:
```

The files in `/etc/suru-relay` must be readable by the container's user, UID 65532 — the private keys by it alone:
`sudo chown 65532 /etc/suru-relay/github-app.pem && sudo chmod 0600 /etc/suru-relay/github-app.pem`. To serve HTTPS
from the container itself, mount the certificate files there too, listen at `0.0.0.0:8443`, and publish
`443:8443`.

`docker logs suru-relay` shows both logs; `docker logs suru-relay 2>/dev/null` shows the connection log alone.
`docker stop` sends `SIGTERM`, on which the Relay stops as it does on Ctrl-C.

The operator's commands run inside the container, as its user, on the same records:

```sh
docker exec suru-relay suru-relay logins list
```

## The operator's commands

Beside its configuration file, the Relay's command line is its only administrative interface. Give the commands the
same configuration as `run` — `--config`, or `SURU_RELAY_CONFIG` — so they work on the same database, and run them
as the Relay's user, which must be able to write the database and the files beside it:

```sh
sudo -u suru-relay suru-relay --config /etc/suru-relay/suru-relay.toml accounts list
```

They work whether or not the Relay is running; they never carry the database forward to a newer version, which only
`run` does.

- `accounts list` lists every Account: its user's identity provider (`github`), their ID there — at GitHub, a
  number nobody else will ever have — their username as of their latest login, how many Logins stand under it, and
  whether it has lapsed.
- `logins list` lists every Login: its Server's key fingerprint, the hostname the Server reported, its Account,
  and when it was formed.
- `accounts remove github <id>` removes an Account and every Login under it.
- `logins remove <fingerprint>` removes one Login — a lost laptop, say — named by its fingerprint or as much of
  the beginning of it, at least 8 characters, as no other Login's shares.

`--json` prints a list as JSON for a script. A removal is refused from the moment it is made, and the command returns
once the running Relay has cut every connection that stood on what was removed. It waits for that 10 seconds unless
`--wait <seconds>` says otherwise; past it, the removal stands but the command exits with status 3. A Server whose
Login is removed reads **login needed** in Suru, and logs in again to come back. Nothing the Relay does ends a
Pairing: a Remote with a direct way goes on working.

Removing an Account does not keep anyone out: anyone the admission rules admit may log in again, as a new Account.
To keep someone out, take them out of the rules — or out of the organization.

Exit status: 0 when the command did as asked; 1 when it could not, saying why on standard error; 2 when its command
line cannot be read; 3 when a removal was made but the running Relay did not confirm the cut in time.

## The connection log

The Relay writes one JSON line to standard output for each connection it joins, once the connection ends:

```json
{"event":"joined_connection","start":"2026-10-04T09:30:00.000Z","end":"2026-10-04T09:41:12.345Z","account":{"id":1,"provider":"github","subject":"583231","username":"octocat"},"joining":{"fingerprint":"9f86…","hostname":"laptop","address":"203.0.113.7","bytes_sent":1832},"serving":{"fingerprint":"60303…","hostname":"workstation","address":"198.51.100.2","bytes_sent":90211}}
```

It names the Account, and for each Server — `joining` asked to be joined, `serving` Serves through the Relay — its
identity key's fingerprint, the hostname it reported as it logged in, its network address, and the bytes it sent
that the Relay passed on. It never holds anything a connection carried, which the Relay cannot read. The Relay keeps
no history of connections in its database: ship standard output wherever you keep logs. Its own diagnostics go to
standard error; `RUST_LOG` sets how much it says there (`info` unless set).

The address is the Server's own only where [`trusted_proxies`](configuration.md#trusted_proxies) names the reverse
proxy in front of the Relay; otherwise it is the proxy's.

Whatever reads standard output must keep up. If it stops taking lines in, the Relay goes on carrying the connections
it has but refuses new joins once the log has fallen far behind, until it catches up. If writing to standard output
fails outright — a closed pipe, a full disk — the Relay says so on standard error, writes the lines it owes there,
and joins nothing more until it is restarted.

## Caps

Each Account may have at most 64 Servers logged in (`logins_per_account`) and 256 connections joined at once
(`joined_connections_per_account`), so no one Account can exhaust the Relay. A Server keeping a Remote in view
through the Relay holds one joined connection. Past a cap, a login or a join is refused, and Suru tells its user
which cap was reached, to ask you to raise it or to remove a Login. Raise either in the configuration.

## Requiring a fresh login

A Login stands until it is removed, so an unattended machine stays reachable for months. To require each Account to
log in afresh every so many days, set `fresh_login_days`: an Account not logged in as for longer lapses until one of
its Servers logs in again, which restores every Login under it.

## Keeping the GitHub App working

- **Keep the installation active.** An organization owner who suspends or uninstalls the app, or declines a change
  to its permissions, leaves the Relay unable to check that organization's members. While running, the Relay keeps
  its Accounts and refuses new logins it cannot check, saying why on standard error; as it next starts, it refuses
  to start, saying which organization and why — such as *the app's installation on it is suspended; an owner of the
  organization must unsuspend it*, or *this Relay's GitHub App is not installed on it*.
- **Adding the Members permission to an app already installed** makes GitHub ask each organization owner to accept
  it; until they do, the Relay refuses to start naming that organization. Give the app Members: Read-only from the
  start.
- **Device Flow** must stay enabled, and the client ID and private key the Relay is given must be the same app's.
  GitHub refusing the app's credentials is said on standard error, naming those and the machine's clock as the
  likely causes.
- **Rotating the private key**: generate a new one, give the Relay its path, restart it, then delete the old one at
  GitHub.

## Backups

The database holds every Account, every Login, and the GitHub IDs the admission rules' names were bound to; nothing
else needs keeping but the configuration file and the app's private key. Losing it means every user logging in
again. SQLite keeps it in write-ahead mode, in the database file and a `-wal` file beside it, so copy it whole:

- stop the Relay and copy the database file, with the `-wal` file beside it if there is one; or
- while it runs, with SQLite's own tool, as the Relay's user:
  `sudo -u suru-relay sqlite3 /var/lib/suru-relay/suru-relay.db ".backup '/var/lib/suru-relay/backup.db'"`, and
  then move the copy wherever backups go.

To restore, stop the Relay, put the copy in the database's place — removing any `-wal` and `-shm` files there — and
start it. The `.lock` file needs no backing up.

## Versions and upgrades

The Relay carries a version of its own, apart from Suru's, and is released only when it changes: a Suru release is
not a Relay release you need to install. Once released, a Relay is held to a compatibility promise: a newer Suru
works with an older Relay and an upgraded Relay with an older Suru, so upgrading the Relay needs no coordinating
with its users' Suru installs, nor theirs with you. A build whose `--version` says it speaks an `unstable-` Relay
protocol is from before that promise, and works only with Suru built from the same source.

Upgrade in place: stop the Relay, replace the binary or the image, and start the new one's `run` on the same
configuration and database. As it starts, it carries the database forward to its own version, keeping every Account,
Login and name binding, so nobody logs in again. Servers reconnect on their own. Start the new `run` before using
the new version's operator commands, which refuse records an older Relay left until a newer one has carried them
forward; never point a newer command line at the records of an older Relay still running. A database a newer Relay
has carried forward is refused by an older one, so take a backup before upgrading if you might go back. The new
Relay carries the database forward before it looks up the names in its admission rules, so a start refused there —
GitHub not answering as it checks an organization, say — has carried it forward already, and the older Relay will
not start on it again.

## Using the Relay from Suru

Give your users the Relay's public address. In Suru, each of them:

1. runs `/relay` to open the list of their Server's Relays;
2. presses `a`, types the address — exactly the public address — and presses Enter;
3. chooses the Relay, which reads **login needed**, and presses Enter to log in. Suru shows an address and a code,
   each of which `a` and `c` copy; they visit the address on any device, enter the code, and authorize the app.

The Relay then reads as logged in. Each Server logs in on its own, and stays logged in until its user removes the
Relay (`x`) or you remove its Login. A user the rules do not admit is told so, and that only the Relay's operator
can change it.

Should a Login come to be refused — its Account lapsed, or you removed it — Suru raises a Notice once, *Login needed
at* the Relay's address, and the Relay reads **login needed** in `/relay`, where Enter logs in again. A Remote that
nothing but that Relay reaches reads Unreachable, offering to log in to try again, which leads to the same login.
Where the Account lapsed, one fresh login from any of its Servers restores every Login under it. A Relay that has
merely stopped answering reads Unreachable instead, and Suru keeps trying it on its own.
