# Keep the Server identity key in the platform credential store

Supersedes ADR-0017's clause that identity keys live in the data directory as owner-only files. A Server's identity key is the one lasting secret it holds: it is what a Pairing pins and, under ADR-0048, the whole of a Relay Login. As an owner-only file it travels wherever the data directory does — backups, synced home directories, disk images, a copied directory — and whoever holds a copy holds that Server's Pairings and Login. So the key now lives in the platform's own credential store — the login keychain on macOS, Credential Manager on Windows, Secret Service on Linux — as one item per Server, and the data directory keeps only a marker that names the item and records the key's public fingerprint. What this defends is the key at rest. Suru still loads the key into memory to speak TLS, so a process running as the same user that the store lets in can read it; a key that never leaves hardware is a separate, larger step (#513).

Where no store answers when a Server first needs a key — headless Linux, a container, CI — the key stays in the owner-only file, and a Log line says so. A key a Server finds in that file while a store is usable is moved into the store, read back, and the file deleted, so an upgrade and a machine that gains a keyring later are the same case. The key moves as it is: copies earlier backups already hold are a leak that has happened, and a user who fears them can remove and re-invite their Remotes. Where the marker names an item the store will not give up — a Mac reached over SSH while nobody has logged in at its screen, a keyring that is down — the Server never mints a new key, which would end every Pairing and the Login without a word. It goes on serving its own machine, and Serving, redeeming an Invite, and reaching a Relay fail where the user can see it until the store answers.

A legacy keychain item trusts an unsigned program by the hash of its exact binary, so every upgrade would ask "suru wants to use your confidential information" — and an unattended Server would wait on a prompt nobody answers. Suru's macOS releases are therefore signed with a Developer ID under the code-signing identifier `ai.suru.cli`, with hardened runtime, and notarized. The keychain trusts that identifier and team, not a binary, so the identifier never changes: changing it brings the prompt back for every user. Debug builds change hash with each build and keep the file unless told otherwise.

## Considered Options

- **Keep the key in a file, encrypted under a key held in the store.** Rejected: one more file and one more step, guarded by the store exactly as the key itself would be.
- **Refuse to Serve or reach a Relay where no store answers.** Rejected: it shuts out the unattended machines a Relay exists for (ADR-0046).
- **Mint a new identity key on moving it into the store.** Rejected: it would end every Pairing behind the user's back to defend against copies that already exist.
- **Name each store item by channel.** Rejected: two data directories on one channel, by `SURU_DATA_DIR`, would share one identity. The marker names its item by an id of its own.
- **Ship unsigned on macOS and accept a prompt after each upgrade.** Rejected as above.
- **Also move the loopback token into the store.** Rejected: it lasts one run and every Client reads it to attach, so it would only put the store in front of every attach.

## Consequences

- A Mac reached only over SSH can no longer Serve after a reboot until someone logs in at its screen or unlocks the login keychain.
- Deleting a data directory leaves its item in the store. Items are labelled with their channel and data directory so a user can find and remove them.
- The marker, not the key, is what a Server reads to know it already has an identity, so ADR-0047's carry-forward of the identity key now covers the marker and the item it names.
