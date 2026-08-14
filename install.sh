#!/bin/sh
#
# manymux installer. Installs the `mm` binary from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/rayfish/manymux/master/install.sh | sh
#
# Options (env vars):
#   INSTALL_DIR      target dir (default: /usr/local/bin, ~/.local/bin on macOS)
#   MM_VERSION       pin a release tag, e.g. v0.1.0 (default: latest)
#   MM_SKIP_VERIFY   set to 1 to install without checksum verification
#   MM_SKIP_LINGER   set to 1 to leave systemd lingering alone
#   MM_SKIP_PATH     set to 1 to leave ~/.zshenv alone on macOS
#
# The same line works on the machines you want to manage: manymux needs to be on
# PATH there, and nothing else.
#
# POSIX sh: this is piped to `sh`, which is dash on most Linux distros and does
# not support bash-only options like `set -o pipefail`. `local` is not in POSIX
# either, but every shell that can be /bin/sh here (dash, ash/busybox, bash)
# implements it, so shellcheck is pointed at the dash dialect.
# shellcheck shell=dash
set -eu

REPO="rayfish/manymux"
BIN="mm"
VERSION="${MM_VERSION:-latest}"
SKIP_VERIFY="${MM_SKIP_VERIFY:-0}"

# Where to install, when not told. Set per-OS in main, once the OS is known.
#
# On Linux, /usr/local/bin, because mm has to be runnable as `ssh host mm
# agent`, and that runs a *non-interactive* shell which reads neither .zshrc
# nor .bashrc. A per-user directory like ~/.local/bin is invisible to it, so a
# machine installed that way looks like it has no mm at all. /usr/local/bin is
# on the PATH sshd hands out there (login.defs ENV_PATH), which is what makes
# a system install the reachable one, and worth a sudo prompt.
#
# On macOS that trade does not exist to make: sshd hands out a PATH without
# /usr/local/bin, which arrives in an interactive shell only via path_helper in
# /etc/zprofile, and the `zsh -c` behind `ssh host mm agent` reads no profile.
# So a system install there costs a password and still leaves the machine
# unreachable. A user install plus one line in ~/.zshenv, which every zsh reads
# including `zsh -c`, is what actually makes a Mac reachable. See ensure_path.
#
# On Android there is one place and no choice: Termux has its own prefix, which
# is the only writable directory on PATH, and root does not come into it.
SYSTEM_DIR="/usr/local/bin"
USER_DIR="${HOME}/.local/bin"
TERMUX_DIR="${PREFIX:-/data/data/com.termux/files/usr}/bin"
INSTALL_DIR="${INSTALL_DIR:-}"

if [ -t 1 ]; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; BLUE='\033[0;34m'; NC='\033[0m'
else
  RED=''; GREEN=''; BLUE=''; NC=''
fi
info()  { printf "${BLUE}%s${NC}\n" "$*"; }
ok()    { printf "${GREEN}%s${NC}\n" "$*"; }
err()   { printf "${RED}%s${NC}\n" "$*" >&2; }
die()   { err "$*"; exit 1; }

# Termux ships a very small base: curl is not in it, and neither is anything
# else assumed below, so say the one command that fixes it rather than leaving
# a bare "not found".
need() {
  local pkg
  command -v "$1" >/dev/null 2>&1 && return 0
  if [ -n "${TERMUX_VERSION:-}" ]; then
    case "$1" in
      curl) pkg="curl" ;;
      *) pkg="coreutils" ;;
    esac
    die "required command not found: $1
Install it with:
    pkg install $pkg"
  fi
  die "required command not found: $1"
}
need curl
need mktemp
need install

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Lowest glibc the gnu Linux binaries are built against (CI runs on
# ubuntu-22.04 = glibc 2.35). A host below this cannot run the gnu build.
GLIBC_MIN="2.35"

# Sets the globals OS and ASSET (base asset name, no libc suffix).
#
# Call this directly, never as `$(detect_asset)`: a command substitution runs in
# a subshell, so the OS it sets there would be lost in the caller and the `set
# -u` read below would abort the script.
detect_asset() {
  local arch
  OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "$OS" in
    linux)  OS="linux" ;;
    darwin) OS="macos" ;;
    *) die "unsupported OS: $OS" ;;
  esac
  # Android says Linux and means something else: the binary is linked against
  # bionic, so both Linux assets are wrong here. Termux sets TERMUX_VERSION in
  # the environment every shell it starts inherits, and `uname -o` covers a
  # shell that lost it.
  if [ "$OS" = "linux" ] \
    && { [ -n "${TERMUX_VERSION:-}" ] || [ "$(uname -o 2>/dev/null)" = "Android" ]; }; then
    OS="android"
  fi
  case "$arch" in
    x86_64|amd64)  arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) die "unsupported architecture: $arch" ;;
  esac
  # Only 64-bit ARM is published for Android, which is every phone Termux still
  # supports. An emulator on an x86_64 image would otherwise get a 404 and no
  # idea why.
  if [ "$OS" = "android" ] && [ "$arch" != "aarch64" ]; then
    die "manymux publishes an Android build for aarch64 only, and this is $arch.
Build from source instead: https://github.com/${REPO}"
  fi
  ASSET="${BIN}-${OS}-${arch}"
}

# Whether this Linux host needs the static musl binary instead of the glibc
# one: true on musl distros (Alpine) and on glibc older than the build floor.
# Conservative: if the libc cannot be identified, trust the gnu build.
linux_needs_musl() {
  local have lowest
  # Alpine and friends: ldd reports musl on stderr.
  ldd --version 2>&1 | grep -qi musl && return 0
  have="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
  [ -n "$have" ] || have="$(ldd --version 2>/dev/null | head -1 | awk '{print $NF}')"
  [ -n "$have" ] || return 1
  lowest="$(printf '%s\n%s\n' "$have" "$GLIBC_MIN" | sort -V 2>/dev/null | head -1)"
  [ "$have" != "$GLIBC_MIN" ] && [ "$lowest" = "$have" ] && return 0
  return 1
}

# True if a URL resolves to an actual asset (HEAD, following redirects).
asset_exists() { curl -fsIL "$1" >/dev/null 2>&1; }

# Base URL for the chosen release (latest = the redirecting "latest" path).
release_base() {
  if [ "$VERSION" = "latest" ]; then
    echo "https://github.com/${REPO}/releases/latest/download"
  else
    echo "https://github.com/${REPO}/releases/download/${VERSION}"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "no sha256sum or shasum on this host, cannot verify the download.
Install one, or set MM_SKIP_VERIFY=1 to install unverified."
  fi
}

# The checksum sidecar is served from the same origin as the binary, so this
# catches corruption and truncation, not a compromised release. It is still the
# integrity floor: never skip it silently, since every release publishes one.
#
# Returns non-zero on a mismatch rather than dying, so the caller can retry: see
# fetch_and_verify.
verify_sha256() {
  local file="$1" sha_file="$2" expected actual
  expected="$(head -n 1 "$sha_file" | cut -d' ' -f1)"
  case "$expected" in
    "" | *[!0-9a-fA-F]*) die "malformed checksum sidecar for $(basename "$file")" ;;
  esac
  actual="$(sha256_of "$file")"
  if [ "$actual" != "$expected" ]; then
    err "checksum mismatch
  expected: $expected
  got:      $actual"
    return 1
  fi
  ok "checksum verified"
}

# Download the binary and its sidecar, and check one against the other.
#
# Retried once, because the nightly release is rebuilt in place: for a few
# seconds during each build its assets are replaced one by one, and an install
# landing in that window gets the old binary with the new sidecar. Failing
# closed is right, but so is trying again before giving up on it.
fetch_and_verify() {
  local url="$1" attempt=1
  while :; do
    curl -fsSL "$url" -o "$TMP/$BIN" \
      || die "download failed: no release asset at $url
(does a published release exist yet for this platform?)"

    if ! curl -fsSL "${url}.sha256" -o "$TMP/$BIN.sha256" 2>/dev/null; then
      [ "$SKIP_VERIFY" = "1" ] || die "no checksum published at ${url}.sha256
Every manymux release ships a .sha256 sidecar, so this should not happen.
Refusing to install an unverified binary. Set MM_SKIP_VERIFY=1 to override."
      info "no .sha256 sidecar found; MM_SKIP_VERIFY=1, installing unverified"
      return 0
    fi

    verify_sha256 "$TMP/$BIN" "$TMP/$BIN.sha256" && return 0

    [ "$attempt" = "1" ] || die "the download does not match its checksum twice over.
Refusing to install. Set MM_SKIP_VERIFY=1 to override, or try again later."
    info "a release may be mid-upload; retrying once in 5s ..."
    sleep 5
    attempt=2
  done
}

# The nearest existing directory at or above $1. A target that does not exist
# yet is not writable, so testing $INSTALL_DIR itself would demand sudo for a
# path the user can perfectly well create (~/.local/bin, typically) and leave a
# root-owned directory in their home.
existing_ancestor() {
  local d="$1" parent
  while [ ! -d "$d" ]; do
    parent="$(dirname "$d")"
    [ "$parent" != "$d" ] || break
    d="$parent"
  done
  echo "$d"
}

# Keep a systemd user instance running for this account whether or not anyone is
# logged in.
#
# Two things go wrong without it, and one command fixes both. A user service is
# killed with the rest of the user slice at logout, which is the opposite of what
# a persistent session is for. And an ssh login on a host whose PAM stack has no
# pam_systemd never gets a user instance at all, so there is no $XDG_RUNTIME_DIR,
# no session bus, and `mm service install` has nothing to talk to.
#
# Only ever an addition: lingering is left alone when it is already on, and
# MM_SKIP_LINGER=1 skips the whole thing.
enable_linger() {
  local user priv=""
  [ "$OS" = "linux" ] || return 0
  [ "${MM_SKIP_LINGER:-0}" = "1" ] && return 0
  # systemd as init, and a logind to ask. Neither holds on Alpine, WSL without
  # systemd, or a container, where there is nothing to enable.
  [ -d /run/systemd/system ] || return 0
  command -v loginctl >/dev/null 2>&1 || return 0

  # Under sudo, the account being installed for is the one that invoked it, not
  # root. root's services run without lingering anyway.
  user="${SUDO_USER:-$(id -un)}"
  [ "$user" != "root" ] || return 0

  # `--value` is too new to rely on, and show-user exits non-zero for a user with
  # neither a session nor lingering, which is exactly the case being fixed here.
  if loginctl show-user "$user" --property=Linger 2>/dev/null | grep -q '^Linger=yes'; then
    return 0
  fi

  if [ "$(id -u)" = "0" ]; then
    priv=""
  elif command -v sudo >/dev/null 2>&1; then
    priv="sudo"
  else
    err "sessions here will stop at logout, and sudo is unavailable to fix it.
Run this as root, then \`mm service install\`:
    loginctl enable-linger $user"
    return 0
  fi

  info "Enabling systemd lingering for ${user}, so sessions outlive your login ..."
  if $priv loginctl enable-linger "$user"; then
    ok "lingering enabled"
  else
    err "could not enable lingering; sessions will stop at logout. Run:
    sudo loginctl enable-linger $user"
  fi
}

# Put INSTALL_DIR on the PATH, on a macOS install that landed in the user's
# home. This is what that install has instead of /usr/local/bin, so it is the
# step that makes the machine reachable, not a convenience.
#
# ~/.zshenv, not .zprofile or .zshrc: zsh reads .zshenv on every invocation,
# including the non-login non-interactive `zsh -c` that `ssh host mm agent`
# becomes. The other two are read by login or interactive shells, which that is
# neither. zsh has been the macOS default shell since Catalina; on anything
# else, say what is needed rather than guess at a file.
#
# Only ever an addition, and never a second one: a line naming INSTALL_DIR
# already there is left alone. MM_SKIP_PATH=1 skips it.
#
# Sets PATH_ADVICE, so the closing hint knows whether PATH has been dealt with
# ("wrote"), already spelled out ("told"), or not raised at all ("").
PATH_ADVICE=""
ensure_path() {
  local file="${ZDOTDIR:-$HOME}/.zshenv"
  [ "$OS" = "macos" ] || return 0
  [ "${MM_SKIP_PATH:-0}" = "1" ] && return 0

  # An asked-for system install on a Mac: their call, but not one to make
  # quietly, since the symptom is a machine that lists as if mm were missing.
  if [ "$INSTALL_DIR" = "$SYSTEM_DIR" ]; then
    PATH_ADVICE="told"
    err "note: macOS leaves ${SYSTEM_DIR} off the PATH a non-interactive ssh gets, so
\`ssh <thishost> mm agent\` will not find mm there and this machine cannot be
managed from another one. Install to ${USER_DIR} instead, or arrange the PATH
yourself in ~/.zshenv."
    return 0
  fi

  # Not .zshenv for a shell that never reads it. What that shell does read on a
  # non-login non-interactive run varies (bash: only $BASH_ENV, unless it was
  # built to source .bashrc under sshd), so name the requirement, not a file.
  case "${SHELL##*/}" in
    zsh) ;;
    *)
      PATH_ADVICE="told"
      err "your login shell is ${SHELL:-unknown}, so putting ${INSTALL_DIR} on its PATH is
yours to do. It has to be somewhere that shell reads even when it is neither a
login nor an interactive shell, or \`ssh <thishost> mm agent\` will not find mm:
    export PATH=\"${INSTALL_DIR}:\$PATH\""
      return 0 ;;
  esac

  if [ -f "$file" ] && grep -qF "$INSTALL_DIR" "$file"; then
    PATH_ADVICE="wrote"
    return 0
  fi
  # shellcheck disable=SC2016  # $PATH is for the file to expand, not this script
  if ! printf '\n# manymux: also read by the non-interactive `zsh -c` behind `ssh host mm agent`\nexport PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$file"; then
    PATH_ADVICE="told"
    err "could not write ${file}; add this to it yourself:
    export PATH=\"${INSTALL_DIR}:\$PATH\""
    return 0
  fi
  ok "put ${INSTALL_DIR} on your PATH in ${file}"
  PATH_ADVICE="wrote"
}

main() {
  local asset base url sudo=""
  detect_asset
  asset="$ASSET"
  base="$(release_base)"

  if [ -z "$INSTALL_DIR" ]; then
    case "$OS" in
      macos)   INSTALL_DIR="$USER_DIR" ;;
      android) INSTALL_DIR="$TERMUX_DIR" ;;
      *)       INSTALL_DIR="$SYSTEM_DIR" ;;
    esac
  fi

  # Every push to master publishes a rolling `nightly` pre-release, and GitHub
  # excludes pre-releases from /releases/latest. So until a stable release
  # exists, "latest" resolves to nothing and the nightly is what to install.
  if [ "$VERSION" = "latest" ] && ! asset_exists "${base}/${asset}"; then
    if asset_exists "https://github.com/${REPO}/releases/download/nightly/${asset}"; then
      info "no stable release yet; installing the rolling nightly build"
      base="https://github.com/${REPO}/releases/download/nightly"
      # What is being installed, not what was asked for: saying "latest" while
      # fetching the nightly is how you end up unsure which one you have.
      VERSION="nightly"
    fi
  fi

  # On Linux, switch to the static musl asset when the glibc binary will not run
  # here (musl distro, or glibc older than the build floor) but only if a musl
  # asset was actually published for this version.
  if [ "$OS" = "linux" ] && linux_needs_musl; then
    if asset_exists "${base}/${asset}-musl"; then
      info "glibc is unsuitable here; using the static musl build"
      asset="${asset}-musl"
    else
      info "glibc looks unsuitable but no musl build is published for ${VERSION}; trying glibc anyway"
    fi
  fi

  url="${base}/${asset}"

  info "Downloading ${asset} (${VERSION}) ..."
  fetch_and_verify "$url"

  chmod +x "$TMP/$BIN"

  # Install, taking sudo if that is what it costs to land on the PATH ssh uses.
  # With no way to do that, fall back to a per-user directory rather than
  # failing outright, and say what it means.
  if [ -w "$(existing_ancestor "$INSTALL_DIR")" ] || [ "$(id -u)" = "0" ]; then
    info "Installing to ${INSTALL_DIR} ..."
  elif command -v sudo >/dev/null 2>&1; then
    info "Installing to ${INSTALL_DIR} (needs sudo) ..."
    sudo=sudo
  elif [ "$INSTALL_DIR" = "$SYSTEM_DIR" ]; then
    INSTALL_DIR="$USER_DIR"
    if [ "$OS" = "macos" ]; then
      # Where a Mac was headed anyway; ensure_path makes it reachable.
      info "cannot write ${SYSTEM_DIR} and sudo is unavailable; installing to ${INSTALL_DIR}."
    else
      err "cannot write ${SYSTEM_DIR} and sudo is unavailable; installing to ${INSTALL_DIR}.
manymux will work here, but this machine cannot be managed from another one:
a non-interactive ssh does not see ${INSTALL_DIR}. Move the binary into
${SYSTEM_DIR} when you can."
    fi
  else
    die "$INSTALL_DIR is not writable and sudo is unavailable. Set INSTALL_DIR to a writable path."
  fi
  $sudo mkdir -p "$INSTALL_DIR"
  $sudo install -m 0755 "$TMP/$BIN" "$INSTALL_DIR/$BIN"

  ok "Installed $("$INSTALL_DIR/$BIN" --version 2>/dev/null || echo "$BIN") to $INSTALL_DIR/$BIN"

  enable_linger
  ensure_path

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      case "$PATH_ADVICE" in
        wrote) info "Open a new shell, or for this one:
    export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
        told) ;;
        *) info "Add ${INSTALL_DIR} to your PATH:
    export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
      esac ;;
  esac

  echo
  ok "Next: start a session somewhere you can already ssh into"
  echo "    mm new <host>"
  echo
  echo "  That host needs mm too, and will offer to install it when you name it."
  echo "  Then \`mm ls\` shows every session on every machine you use."

  # The one thing that behaves differently here, and the one command that fixes
  # it. A node on a phone is a node inside an app Android is free to freeze.
  if [ "$OS" = "android" ]; then
    echo
    info "On Android, sessions started here only last as long as Termux does.
  \`termux-wake-lock\` keeps it running in the background. Sessions on the
  machines you ssh into are unaffected: they live on those machines."
  fi
}

main "$@"
