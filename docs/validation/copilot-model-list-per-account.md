# Copilot lists its catalog once per account

Recorded 2026-09-30 against GitHub Copilot CLI 1.0.88 and 1.0.89 on Linux, each asked
`models.list` and `account.getAllUsers` directly over its stdio server wire. The report it
explains came from a Windows machine on CLI 1.0.89, right after the user ran `copilot login` on a
machine whose gh CLI was already signed in.

## The symptom

The Providers tab reported Copilot as failed:

```
Model catalog contains an empty or duplicate Model ID `auto`
```

Suru's catalog admits a discovery only after `validate_models` (`src/provider.rs`) has found
every Model ID exactly once, and `auto` is the first entry Copilot lists.

## What the CLI answers

`account.getAllUsers` on a machine with both a `copilot login` and gh CLI credentials lists two
accounts with the same GitHub login, one of type `user` and one of type `gh-cli`. The flat
`models.list` then answers as follows:

| CLI | Accounts on file | Entries | Duplicate IDs |
| --- | --- | --- | --- |
| 1.0.88 | user login and gh CLI | 17 | none |
| 1.0.89 | user login and gh CLI | 34 | every one of the 17, `auto` first |
| 1.0.89 | user login only | 17 | none |

The two copies are identical entry for entry, and no entry says which account it came from. The
SDK snapshot regenerated for 1.0.89 (`references/copilot-sdk`) adds a per-Model `provider`
attribution, but documents it as present only on the per-Session `session.model.list` and absent
on the flat list Suru asks.

## What Suru does about it

`model_descriptors` in `src/provider/copilot/catalog.rs` keeps each Model ID where it first
appeared and drops the later copies, which preserves the CLI's own picker order and leaves the
routed `auto` Model the default. The scripted CLI answers `models.list` with its catalog listed
twice over in `a_catalog_the_cli_lists_once_per_account_serves_each_model_once`, which fails
without the collapse. Validation stays strict for every other Provider and for a catalog that is
malformed in any other way.

## Left open

A flat list repeated once per account is a 1.0.89 regression in the CLI, worth reporting upstream
to copilot-cli. Should a later CLI ever list copies that differ — say, a Model an administrator's
policy disables on one account and not the other — the first account's entry is the one Suru
shows, since the flat list gives it nothing better to choose by.
