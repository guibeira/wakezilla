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

assert_command_exists() {
  command_name="$1"
  label="$2"
  if ! command -v "$command_name" >/dev/null 2>&1; then
    fail "$label: expected command '$command_name' to be defined"
    return 1
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

test_unknown_args_fail_with_parser_error() {
  run_script --unknown
  if [ "$status" -eq 0 ]; then
    fail "unknown args exit status: expected nonzero, got 0"
  fi
  assert_contains "$output" "error[args]: unknown option: --unknown (use --help for usage)" "unknown args parser error"
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
test_unknown_args_fail_with_parser_error
test_mode_executes_cleanly
test_mode_sources_cleanly

load_install_helpers() {
  WAKEZILLA_INSTALL_SH_TEST_MODE=1 . "$SCRIPT"
}

test_detect_target_linux_x86_64() {
  target=$(WAKEZILLA_UNAME_S=Linux WAKEZILLA_UNAME_M=x86_64 detect_target)
  assert_eq "x86_64-unknown-linux-gnu" "$target" "linux x86_64 target"
}

test_detect_target_macos_x86_64() {
  target=$(WAKEZILLA_UNAME_S=Darwin WAKEZILLA_UNAME_M=x86_64 detect_target)
  assert_eq "x86_64-apple-darwin" "$target" "macos x86_64 target"
}

test_detect_target_macos_arm64() {
  target=$(WAKEZILLA_UNAME_S=Darwin WAKEZILLA_UNAME_M=arm64 detect_target)
  assert_eq "aarch64-apple-darwin" "$target" "macos arm64 target"
}

test_detect_target_override() {
  target=$(TARGET=custom-target WAKEZILLA_UNAME_S=Other WAKEZILLA_UNAME_M=Other detect_target)
  assert_eq "custom-target" "$target" "target override"
}

test_detect_target_unsupported_linux_arm64() {
  if output=$(WAKEZILLA_UNAME_S=Linux WAKEZILLA_UNAME_M=aarch64 detect_target 2>&1); then
    fail "unsupported linux arm64 target: expected failure, got '$output'"
  else
    assert_contains "$output" "unsupported platform" "unsupported linux arm64"
  fi
}

test_install_argument_helpers_defined() {
  missing=0
  assert_command_exists parse_args "parse args helper" || missing=1
  assert_command_exists resolve_bin_dir "resolve bin dir helper" || missing=1
  [ "$missing" -eq 0 ]
}

test_parse_args_positional_version() {
  parsed_version=$(
    VERSION=
    parse_args 0.1.49
    printf '%s\n' "$VERSION"
  )
  assert_eq "0.1.49" "$parsed_version" "positional version"
}

test_parse_args_rejects_two_versions() {
  if output=$(
    VERSION=
    parse_args 0.1.49 0.1.50 2>&1
  ); then
    fail "parse args duplicate version: expected failure, got '$output'"
  else
    assert_contains "$output" "unexpected argument" "duplicate version error"
  fi
}

test_resolve_bin_dir_default() {
  bin_dir=$(
    HOME=/tmp/wakezilla-home
    unset BIN_DIR || true
    unset PREFIX || true
    resolve_bin_dir
  )
  assert_eq "/tmp/wakezilla-home/.local/bin" "$bin_dir" "default bin dir"
}

test_resolve_bin_dir_prefix() {
  bin_dir=$(
    unset BIN_DIR || true
    PREFIX=/opt/wakezilla
    resolve_bin_dir
  )
  assert_eq "/opt/wakezilla/bin" "$bin_dir" "prefix bin dir"
}

test_resolve_bin_dir_override() {
  bin_dir=$(
    BIN_DIR=/custom/bin
    PREFIX=/ignored
    resolve_bin_dir
  )
  assert_eq "/custom/bin" "$bin_dir" "BIN_DIR override"
}

load_install_helpers
test_detect_target_linux_x86_64
test_detect_target_macos_x86_64
test_detect_target_macos_arm64
test_detect_target_override
test_detect_target_unsupported_linux_arm64
if test_install_argument_helpers_defined; then
  test_parse_args_positional_version
  test_parse_args_rejects_two_versions
  test_resolve_bin_dir_default
  test_resolve_bin_dir_prefix
  test_resolve_bin_dir_override
fi

if [ "$failures" -ne 0 ]; then
  printf '%s test(s) failed\n' "$failures" >&2
  exit 1
fi

printf 'install.sh tests passed\n'
