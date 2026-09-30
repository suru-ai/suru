#!/usr/bin/env bash
# Installs Suru from its GitHub releases on Linux and macOS; re-running it upgrades the install in place.
#
#   curl -fsSL https://raw.githubusercontent.com/suru-ai/suru/main/scripts/install.sh | bash
#
# Environment:
#   SURU_VERSION      Release to install, e.g. v0.1.1. Defaults to the latest release.
#   SURU_INSTALL_DIR  Directory the binary goes in. Defaults to ~/.local/bin.
#   SURU_YES          Set to 1 to answer yes to every question, for unattended installs.
#   GITHUB_TOKEN      Sent to GitHub when set, which lifts the anonymous API rate limit.
#
# Kept to what macOS's bash 3.2 runs.

set -euo pipefail

REPO=suru-ai/suru
API=https://api.github.com/repos/$REPO

say() {
  printf '%s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" > /dev/null 2>&1 || fail "$1 is required to install Suru."
}

# Piped into bash, stdin is this script, so questions are asked of the terminal itself.
has_terminal() {
  (: < /dev/tty) 2> /dev/null
}

confirm() {
  local reply
  if [ "${SURU_YES:-}" = 1 ]; then
    return 0
  fi
  has_terminal || return 1
  printf '%s [y/N] ' "$1" > /dev/tty
  read -r reply < /dev/tty || return 1
  case "$reply" in
    [yY] | [yY][eE][sS]) return 0 ;;
    *) return 1 ;;
  esac
}

detect_target() {
  local os arch supported
  supported='Suru is built for x86_64 and aarch64 Linux (glibc) and for Apple silicon macOS.'
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Linux)
      if ldd --version 2>&1 | grep -qi musl; then
        fail "musl Linux is not supported. $supported"
      fi
      case "$arch" in
        x86_64 | amd64) say x86_64-unknown-linux-gnu ;;
        aarch64 | arm64) say aarch64-unknown-linux-gnu ;;
        *) fail "$arch Linux is not supported. $supported" ;;
      esac
      ;;
    Darwin)
      # A shell running under Rosetta reports x86_64 on Apple silicon.
      if [ "$arch" = arm64 ] || [ "$(sysctl -n sysctl.proc_translated 2> /dev/null || true)" = 1 ]; then
        say aarch64-apple-darwin
      else
        fail "Intel macOS is not supported. $supported"
      fi
      ;;
    *) fail "$os is not supported. $supported" ;;
  esac
}

github() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" "$@"
  else
    curl -fsSL "$@"
  fi
}

sha256() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

# Asks before adding the install directory to PATH, and only when it is not already there.
ensure_on_path() {
  local dir=$1 rc line
  case ":$PATH:" in
    *":$dir:"*) return 0 ;;
  esac

  line="export PATH=\"$dir:\$PATH\""
  case "$(basename "${SHELL:-}")" in
    bash)
      if [ "$(uname -s)" = Darwin ]; then rc=$HOME/.bash_profile; else rc=$HOME/.bashrc; fi
      ;;
    zsh) rc=${ZDOTDIR:-$HOME}/.zshrc ;;
    fish)
      rc=${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/suru.fish
      line="contains -- \"$dir\" \$PATH; or set -gx PATH \"$dir\" \$PATH"
      ;;
    *)
      say "$dir is not on your PATH. Add it to run suru from anywhere."
      return 0
      ;;
  esac

  if [ -f "$rc" ] && grep -qF "$line" "$rc"; then
    say "$dir is already added to your PATH in $rc; open a new shell to pick it up."
  elif confirm "$dir is not on your PATH. Add it in $rc?"; then
    mkdir -p "$(dirname "$rc")"
    printf '\n%s\n' "$line" >> "$rc"
    say "Added $dir to your PATH in $rc; open a new shell to pick it up."
  else
    say "$dir is not on your PATH. To add it, put this line in $rc:"
    say "  $line"
  fi
}

main() {
  local target install_dir dest release tag file digest asset_url installed running tmp actual

  need curl
  need tar
  need awk
  command -v sha256sum > /dev/null 2>&1 || need shasum

  target=$(detect_target)
  install_dir=${SURU_INSTALL_DIR:-$HOME/.local/bin}
  dest=$install_dir/suru

  if [ -n "${SURU_VERSION:-}" ]; then
    release=$(github "$API/releases/tags/$SURU_VERSION") \
      || fail "Could not find the Suru release $SURU_VERSION."
  else
    release=$(github "$API/releases/latest") \
      || fail "Could not read the latest Suru release from GitHub. If GitHub is rate limiting you, set GITHUB_TOKEN and try again."
  fi
  tag=$(printf '%s\n' "$release" | sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$tag" ] || fail "Could not read the release's tag from GitHub's answer."

  if [ -x "$dest" ]; then
    installed=$("$dest" --version 2> /dev/null | awk '{ print $2 }' || true)
    if [ "v$installed" = "$tag" ]; then
      say "Suru $tag is already installed at $dest."
      ensure_on_path "$install_dir"
      return 0
    fi
  fi

  # An asset's API URL comes before its name in the release, and its digest after.
  file=suru-$tag-$target.tar.gz
  asset_url=$(printf '%s\n' "$release" | awk -v file="$file" -F'"' '
    $2 == "url" && $4 ~ /\/releases\/assets\/[0-9]+$/ { url = $4 }
    $2 == "name" && $4 == file { print url; exit }
  ')
  digest=$(printf '%s\n' "$release" | awk -v file="$file" -F'"' '
    $2 == "name" && $4 == file { found = 1 }
    found && $2 == "digest" { sub(/^sha256:/, "", $4); print $4; exit }
  ')
  [ -n "$asset_url" ] || fail "Suru $tag has no build for $target."
  [ -n "$digest" ] || fail "Suru $tag publishes no checksum for $file, so it cannot be verified."

  # Asked before anything is downloaded, and acted on only once the new binary is verified.
  running=false
  if [ -x "$dest" ] && "$dest" server status > /dev/null 2>&1; then
    running=true
    if ! confirm "Suru is running. Stopping it will interrupt any work in progress. Stop it and continue?"; then
      if [ "${SURU_YES:-}" != 1 ] && ! has_terminal; then
        fail "Suru is running and there is no terminal to ask on. Stop it with 'suru server stop', or set SURU_YES=1, and try again."
      fi
      fail "Suru is still running, so ${installed:+v}${installed:-the installed version} stays installed."
    fi
  fi

  tmp=$(mktemp -d)
  # shellcheck disable=SC2064 # Expanded now: tmp is local, and gone by the time the trap runs.
  trap "rm -rf '$tmp'" EXIT

  say "Downloading Suru $tag for $target"
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    # The only download a private repository allows.
    github -H 'Accept: application/octet-stream' -o "$tmp/$file" "$asset_url"
  else
    curl -fsSL -o "$tmp/$file" "https://github.com/$REPO/releases/download/$tag/$file"
  fi || fail "Could not download $file."

  actual=$(sha256 "$tmp/$file")
  if [ "$actual" != "$digest" ]; then
    fail "$file does not match its published checksum (expected $digest, got $actual)."
  fi
  tar -xzf "$tmp/$file" -C "$tmp"

  if [ "$running" = true ]; then
    say "Stopping Suru"
    "$dest" server stop > /dev/null || fail "Could not stop Suru; ${installed:+v}${installed:-the installed version} stays installed."
  fi

  # Staged beside the destination so the rename that replaces the old binary is atomic.
  mkdir -p "$install_dir"
  cp "$tmp/suru-$tag-$target/suru" "$install_dir/.suru.$$"
  chmod 755 "$install_dir/.suru.$$"
  mv -f "$install_dir/.suru.$$" "$dest"

  say "Installed Suru $tag to $dest"
  ensure_on_path "$install_dir"
}

# Run only once the whole script has arrived, so a cut-off download does nothing.
main "$@"
