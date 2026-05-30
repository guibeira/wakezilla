#!/usr/bin/env sh
set -eu

REPO="${REPO:-guibeira/wakezilla}"
BIN_NAME="${BIN_NAME:-wakezilla}"

info() {
  printf '%s\n' "$*"
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

err() {
  stage="$1"
  shift
  printf 'error[%s]: %s\n' "$stage" "$*" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
Usage: install.sh [OPTIONS] [VERSION]

Install Wakezilla from GitHub Releases.

Options:
  -h, --help      Show this help message

Environment variables:
  VERSION         Version to install, without leading v (example: 0.1.49)
  BIN_DIR         Binary installation directory
  PREFIX          Installation prefix used when BIN_DIR is unset (default: $HOME/.local)
  TARGET          Override target triple (example: x86_64-unknown-linux-gnu)
  REPO            GitHub repository (default: guibeira/wakezilla)
  GITHUB_TOKEN    Token for authenticated GitHub API requests

Examples:
  curl -fsSL https://raw.githubusercontent.com/guibeira/wakezilla/main/install.sh | sh
  curl -fsSL https://raw.githubusercontent.com/guibeira/wakezilla/main/install.sh | sh -s -- 0.1.49
  VERSION=0.1.49 BIN_DIR=/usr/local/bin sh install.sh
USAGE
}

detect_target() {
  if [ -n "${TARGET:-}" ]; then
    printf '%s\n' "$TARGET"
    return 0
  fi

  uname_s="${WAKEZILLA_UNAME_S:-$(uname -s 2>/dev/null || echo unknown)}"
  uname_m="${WAKEZILLA_UNAME_M:-$(uname -m 2>/dev/null || echo unknown)}"

  case "$uname_m" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) err "platform" "unsupported architecture: $uname_m" ;;
  esac

  case "$uname_s:$arch" in
    Linux:x86_64) printf 'x86_64-unknown-linux-gnu\n' ;;
    Darwin:x86_64) printf 'x86_64-apple-darwin\n' ;;
    Darwin:aarch64) printf 'aarch64-apple-darwin\n' ;;
    *)
      err "platform" "unsupported platform: $uname_s/$uname_m. Supported release targets are x86_64-unknown-linux-gnu, x86_64-apple-darwin, aarch64-apple-darwin"
      ;;
  esac
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -h|--help)
        usage
        exit 0
        ;;
      -*)
        err "args" "unknown option: $1 (use --help for usage)"
        ;;
      *)
        if [ -n "${VERSION:-}" ]; then
          err "args" "unexpected argument: $1 (VERSION is already set to $VERSION)"
        fi
        VERSION="$1"
        ;;
    esac
    shift
  done
}

resolve_bin_dir() {
  if [ -n "${BIN_DIR:-}" ]; then
    printf '%s\n' "$BIN_DIR"
  elif [ -n "${PREFIX:-}" ]; then
    printf '%s/bin\n' "$PREFIX"
  else
    printf '%s/.local/bin\n' "$HOME"
  fi
}

if [ -n "${WAKEZILLA_INSTALL_SH_TEST_MODE:-}" ]; then
  return 0 2>/dev/null || exit 0
fi

parse_args "$@"
usage
