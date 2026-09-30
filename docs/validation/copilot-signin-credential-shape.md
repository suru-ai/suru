# Copilot's sign-in answer carries no credentials

Recorded 2026-09-30 against GitHub Copilot CLI 1.0.88 on Linux, asked `account.getCurrentAuth`
directly over its stdio server wire. The report it explains came from a Windows machine on CLI
1.0.89.

## The symptom

The Providers tab reported Copilot as failed, on a machine whose Copilot CLI was signed in:

```
Copilot sign-in check failed: data did not match any variant of untagged enum AuthInfo
```

Nothing reached the server Log. The same build served Copilot's Models on another machine, whose
CLI had been signed in with `copilot login`.

## What the CLI answers

Both answers below are as the CLI sent them, with the `copilotUser` object shortened.

A CLI signed in with `copilot login`:

```json
{"authInfo":{"type":"user","host":"https://github.com","login":"…","copilotUser":{"login":"…"}}}
```

The same CLI with no login of its own and the gh CLI's credentials on file — what a user who has
run `gh auth login` but never `copilot login` has:

```json
{"authInfo":{"type":"gh-cli","host":"https://github.com","login":"…","copilotUser":{"login":"…"}}}
```

Neither carries a token. The CLI keeps the credentials to itself and answers with who is signed in
and how the credentials were resolved.

## Why the SDK cannot read it

`github-copilot-sdk` 1.0.15-preview.3 types the answer as `AuthInfo`, an untagged serde union of
eight shapes. Its `hmac`, `env`, `token`, `api-key`, and `gh-cli` shapes each require a secret
field (`token`, `hmac`, or `apiKey`), and only its `user` shape requires none. A `user` answer
therefore parses and a `gh-cli` answer matches nothing, which is the error above. The SDK's own
schema comment on `AuthInfo` says it is accepted only at protocol ingress and that runtime outputs
use the credential-free `AuthIdentity`, yet the `account.getCurrentAuth` result is still typed as
`AuthInfo`. The SDK snapshot regenerated for CLI 1.0.89 (`references/copilot-sdk`) carries the
same types, so an SDK upgrade does not change this.

## What Suru does about it

Suru asks `account.getCurrentAuth` through the SDK client's raw `call` and reads the answer only
as far as it needs it: whether `authInfo` is present, and what `authErrors` says when it is not
(`CurrentAuth` in `src/provider/copilot/transport.rs`). The credentials stay the CLI's own, which
is where ADR-0008 wants them. The scripted CLI answers the sign-in check the way 1.0.88 does, and
`a_cli_signed_in_through_the_gh_cli_serves_its_models_without_showing_suru_a_token` fails against
the SDK's typed query.

A Model Catalog discovery that fails is now also written to the Log at `warn`, naming the
Provider and the failure (`ProviderCatalog::discover` in `src/model_catalog.rs`), so a condition
the Providers tab shows is one the Log carries too.

## Left open

The SDK's typing of the `account.getCurrentAuth` result is the upstream defect: it should be
`AuthIdentity`. Until it is, every SDK release keeps the mismatch, and Suru keeps reading the wire
itself.
