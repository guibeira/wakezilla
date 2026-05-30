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

if [ -n "${WAKEZILLA_INSTALL_SH_TEST_MODE:-}" ]; then
  return 0 2>/dev/null || exit 0
fi

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    *)
      err "args" "unknown option or argument before parser is implemented: $1"
      ;;
  esac
done

usage
