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

run_script() {
  output_file=$(mktemp)
  set +e
  "$SCRIPT" "$@" >"$output_file" 2>&1
  status=$?
  set -e
  output=$(cat "$output_file")
  rm -f "$output_file"
}

test_help_includes_required_docs() {
  run_script --help
  assert_eq "0" "$status" "help exit status"
  assert_contains "$output" "Usage: install.sh" "help usage"
  assert_contains "$output" "VERSION" "help VERSION"
  assert_contains "$output" "BIN_DIR" "help BIN_DIR"
  assert_contains "$output" "PREFIX" "help PREFIX"
  assert_contains "$output" "TARGET" "help TARGET"
  assert_contains "$output" "REPO" "help REPO"
  assert_contains "$output" "GITHUB_TOKEN" "help GITHUB_TOKEN"
  assert_contains "$output" "curl -fsSL https://raw.githubusercontent.com/guibeira/wakezilla/main/install.sh | sh" "help curl example"
  assert_contains "$output" "curl -fsSL https://raw.githubusercontent.com/guibeira/wakezilla/main/install.sh | sh -s -- 0.1.49" "help version curl example"
  assert_contains "$output" "VERSION=0.1.49 BIN_DIR=/usr/local/bin sh install.sh" "help local install example"
}

test_no_args_prints_usage() {
  run_script
  assert_eq "0" "$status" "no args exit status"
  assert_contains "$output" "Usage: install.sh" "no args usage"
}

test_unknown_args_fail_with_placeholder() {
  run_script --unknown
  if [ "$status" -eq 0 ]; then
    fail "unknown args exit status: expected nonzero, got 0"
  fi
  assert_contains "$output" "error[args]: unknown option or argument before parser is implemented: --unknown" "unknown args placeholder"
}

test_mode_executes_cleanly() {
  output_file=$(mktemp)
  set +e
  WAKEZILLA_INSTALL_SH_TEST_MODE=1 "$SCRIPT" >"$output_file" 2>&1
  status=$?
  set -e
  output=$(cat "$output_file")
  rm -f "$output_file"

  assert_eq "0" "$status" "test mode execute exit status"
  assert_eq "" "$output" "test mode execute output"
}

test_mode_sources_cleanly() {
  output_file=$(mktemp)
  set +e
  WAKEZILLA_INSTALL_SH_TEST_MODE=1 sh -c '. "$1"; printf sourced' sh "$SCRIPT" >"$output_file" 2>&1
  status=$?
  set -e
  output=$(cat "$output_file")
  rm -f "$output_file"

  assert_eq "0" "$status" "test mode source exit status"
  assert_eq "sourced" "$output" "test mode source output"
}

test_help_includes_required_docs
test_no_args_prints_usage
test_unknown_args_fail_with_placeholder
test_mode_executes_cleanly
test_mode_sources_cleanly

if [ "$failures" -ne 0 ]; then
  printf '%s test(s) failed\n' "$failures" >&2
  exit 1
fi

printf 'install.sh tests passed\n'
