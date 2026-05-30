#!/usr/bin/env sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SCRIPT="$ROOT_DIR/install.sh"

failures=0

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  failures=$((failures + 1))
}

assert_contains() {
  haystack="$1"
  needle="$2"
  label="$3"
  case "$haystack" in
    *"$needle"*) ;;
    *) fail "$label: expected output to contain '$needle'" ;;
  esac
}

assert_eq() {
  expected="$1"
  actual="$2"
  label="$3"
  if [ "$expected" != "$actual" ]; then
    fail "$label: expected '$expected', got '$actual'"
  fi
}

test_help_includes_usage() {
  output=$("$SCRIPT" --help 2>&1 || true)
  assert_contains "$output" "Usage: install.sh" "help usage"
  assert_contains "$output" "VERSION" "help VERSION"
  assert_contains "$output" "BIN_DIR" "help BIN_DIR"
}

test_help_includes_usage

if [ "$failures" -ne 0 ]; then
  printf '%s test(s) failed\n' "$failures" >&2
  exit 1
fi

printf 'install.sh tests passed\n'
