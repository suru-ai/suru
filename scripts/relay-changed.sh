#!/usr/bin/env bash
# Says whether the Relay has changed between two commits, so that it is released only when it does (ADR-0047):
# whether anything `suru-relay` is built and released from differs, and nothing else the repository holds. It
# compares
#   - the Relay's two crates, their tests aside, its container image among them;
#   - the packages and features a build of the Relay resolves to, as `cargo tree` reads each commit's Cargo.lock,
#     so a release of Suru, or a dependency only Suru uses, changes nothing;
#   - the workspace's build profiles, its toolchain and its Cargo configuration;
#   - the Relay's build and release workflows.
#
#   scripts/relay-changed.sh <previous> [<current>]
#
# Prints what changed and exits 0 where anything did, exits 1 where nothing did, and 2 where it cannot tell.
# <current> is HEAD unless given. Needs git, and cargo to resolve each commit's lock file.

set -euo pipefail
trap 'exit 2' ERR

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  echo "usage: $0 <previous> [<current>]" >&2
  exit 2
fi
previous=$1
current=${2:-HEAD}
changed=()

if ! git diff --quiet "$previous" "$current" -- crates/suru-relay crates/suru-relay-protocol \
  ':(exclude)crates/suru-relay/tests' ':(exclude)crates/suru-relay-protocol/tests'; then
  changed+=("the Relay's crates")
fi
if ! git diff --quiet "$previous" "$current" -- rust-toolchain.toml .cargo/config.toml; then
  changed+=("the toolchain or Cargo's configuration")
fi
if ! git diff --quiet "$previous" "$current" -- .github/workflows/relay-build.yml .github/workflows/relay-release.yml; then
  changed+=("the Relay's workflows")
fi

# The [profile.*] tables of the workspace's manifest, which every build of the Relay takes.
profiles() {
  git show "$1:Cargo.toml" | awk '/^\[/ { keep = ($0 ~ /^\[profile[].]/) } keep'
}
if [ "$(profiles "$previous")" != "$(profiles "$current")" ]; then
  changed+=("the workspace's build profiles")
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Every package a build of the Relay is made of, on any platform, with the features it is built with: each
# commit's tree, as its own Cargo.lock resolves it, without the paths it was read from.
dependencies() {
  mkdir "$work/$1"
  git archive "$2" | tar -x -C "$work/$1"
  cargo tree --manifest-path "$work/$1/Cargo.toml" --locked --package suru-relay --edges normal,build \
    --target all --prefix none --no-dedupe --format '{p} {f}' \
    | sed 's| (/[^)]*)||' | sort -u
}
before=$(dependencies previous "$previous")
after=$(dependencies current "$current")
if [ "$before" != "$after" ]; then
  changed+=("its dependencies")
fi

if [ ${#changed[@]} -eq 0 ]; then
  echo "The Relay has not changed between $previous and $current."
  exit 1
fi
echo "The Relay has changed between $previous and $current:"
printf '  - %s\n' "${changed[@]}"
