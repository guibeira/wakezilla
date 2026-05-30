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

assert_not_contains() {
  haystack="$1"
  needle="$2"
  label="$3"
  case "$haystack" in
    *"$needle"*) fail "$label: expected output not to contain '$needle'" ;;
    *) ;;
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

write_stub_command() {
  command_path="$1"
  printf '#!/usr/bin/env sh\nexit 0\n' > "$command_path"
  chmod +x "$command_path"
}

write_install_dependency_stubs() {
  bin_dir="$1"
  mkdir -p "$bin_dir"
  write_stub_command "$bin_dir/curl"
  write_stub_command "$bin_dir/tar"
  cat > "$bin_dir/sha256sum" <<'SH'
#!/usr/bin/env sh
printf '2bc181013bb970686145cc02319c9bb8f3f8bcce1ad18384dc49286c784bed7d  %s\n' "$1"
SH
  chmod +x "$bin_dir/sha256sum"
}

write_fixture_curl() {
  command_path="$1"
  cat > "$command_path" <<'SH'
#!/usr/bin/env sh
output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      shift
      output="$1"
      ;;
    -*)
      ;;
    *)
      url="$1"
      ;;
  esac
  shift
done

if [ -n "$output" ]; then
  case "$url" in
    */SHA256SUMS)
      printf '2bc181013bb970686145cc02319c9bb8f3f8bcce1ad18384dc49286c784bed7d  wakezilla-0.1.49-x86_64-unknown-linux-gnu.tar.gz\n' > "$output"
      ;;
    *)
      printf 'fake archive\n' > "$output"
      ;;
  esac
  exit 0
fi

cat "$WAKEZILLA_FAKE_CURL_FIXTURE"
SH
  chmod +x "$command_path"
}

write_recording_fixture_curl() {
  command_path="$1"
  cat > "$command_path" <<'SH'
#!/usr/bin/env sh
: > "$WAKEZILLA_FAKE_CURL_ARGS"
for arg do
  printf '%s\n' "$arg" >> "$WAKEZILLA_FAKE_CURL_ARGS"
done
cat "$WAKEZILLA_FAKE_CURL_FIXTURE"
SH
  chmod +x "$command_path"
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

test_no_args_resolves_release_metadata() {
  temp_dir=$(mktemp -d)
  old_path="$PATH"
  write_install_dependency_stubs "$temp_dir/bin"
  write_fixture_curl "$temp_dir/bin/curl"
  TARGET=x86_64-unknown-linux-gnu
  BIN_DIR="$temp_dir/install-bin"
  GITHUB_TOKEN=secret-token
  WAKEZILLA_FAKE_CURL_FIXTURE="$ROOT_DIR/tests/fixtures/install/release-v0.1.49.json"
  PATH="$temp_dir/bin:$PATH"
  export TARGET BIN_DIR GITHUB_TOKEN WAKEZILLA_FAKE_CURL_FIXTURE PATH
  run_script
  unset TARGET BIN_DIR GITHUB_TOKEN WAKEZILLA_FAKE_CURL_FIXTURE
  PATH="$old_path"
  export PATH
  assert_eq "0" "$status" "release metadata exit status"
  assert_contains "$output" "installing wakezilla for x86_64-unknown-linux-gnu" "release metadata target"
  assert_contains "$output" "resolved wakezilla v0.1.49" "release metadata version"
  assert_contains "$output" "asset: https://example.test/wakezilla-0.1.49-x86_64-unknown-linux-gnu.tar.gz" "release metadata asset"
  assert_contains "$output" "install dir: $temp_dir/install-bin" "release metadata install dir"
  assert_not_contains "$output" "secret-token" "release metadata output token"
  rm -rf "$temp_dir"
}

test_missing_dependency_reports_hint() {
  temp_dir=$(mktemp -d)
  mkdir -p "$temp_dir/bin"
  write_stub_command "$temp_dir/bin/curl"
  write_stub_command "$temp_dir/bin/tar"
  write_stub_command "$temp_dir/bin/sha256sum"
  write_stub_command "$temp_dir/bin/apt-get"

  output_file=$(mktemp)
  set +e
  PATH="$temp_dir/bin" /bin/sh "$SCRIPT" >"$output_file" 2>&1
  status=$?
  set -e
  output=$(cat "$output_file")
  rm -f "$output_file"
  rm -rf "$temp_dir"

  if [ "$status" -eq 0 ]; then
    fail "missing dependency exit status: expected nonzero, got 0"
  fi
  assert_contains "$output" "error[dependency]: jq is required" "missing dependency error"
  assert_contains "$output" "apt-get install -y jq" "missing dependency hint"
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
test_no_args_resolves_release_metadata
test_missing_dependency_reports_hint
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
  assert_command_exists pkg_manager_hint "package manager hint helper" || missing=1
  assert_command_exists have_checksum_tool "checksum tool helper" || missing=1
  assert_command_exists check_dependencies "dependency check helper" || missing=1
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

test_resolve_bin_dir_requires_home_for_default() {
  if output=$(
    unset BIN_DIR || true
    unset PREFIX || true
    unset HOME || true
    resolve_bin_dir 2>&1
  ); then
    fail "missing HOME bin dir: expected failure, got '$output'"
  else
    assert_contains "$output" "HOME is not set" "missing HOME bin dir"
  fi
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
  test_resolve_bin_dir_requires_home_for_default
fi

test_pkg_manager_hint_apt() {
  temp_dir=$(mktemp -d)
  mkdir -p "$temp_dir/bin"
  printf '#!/usr/bin/env sh\nexit 0\n' > "$temp_dir/bin/apt-get"
  chmod +x "$temp_dir/bin/apt-get"
  hint=$(PATH="$temp_dir/bin" pkg_manager_hint jq)
  assert_eq "apt-get install -y jq" "$hint" "apt package hint"
  rm -rf "$temp_dir"
}

test_pkg_manager_hint_unknown() {
  temp_dir=$(mktemp -d)
  hint=$(PATH="$temp_dir" pkg_manager_hint jq)
  assert_eq "install jq via your package manager" "$hint" "unknown package hint"
  rm -rf "$temp_dir"
}

if command -v pkg_manager_hint >/dev/null 2>&1; then
  test_pkg_manager_hint_apt
  test_pkg_manager_hint_unknown
fi

test_github_api_helpers_defined() {
  missing=0
  assert_command_exists github_api "github api helper" || missing=1
  assert_command_exists fetch_release_json "fetch release json helper" || missing=1
  [ "$missing" -eq 0 ]
}

test_fetch_release_json_latest_request() {
  temp_dir=$(mktemp -d)
  mkdir -p "$temp_dir/bin"
  args_file="$temp_dir/curl-args"
  write_recording_fixture_curl "$temp_dir/bin/curl"

  json=$(
    unset GITHUB_TOKEN || true
    REPO=guibeira/wakezilla
    WAKEZILLA_FAKE_CURL_ARGS="$args_file"
    WAKEZILLA_FAKE_CURL_FIXTURE="$ROOT_DIR/tests/fixtures/install/release-v0.1.49.json"
    PATH="$temp_dir/bin:$PATH"
    export WAKEZILLA_FAKE_CURL_ARGS WAKEZILLA_FAKE_CURL_FIXTURE PATH
    fetch_release_json ""
  )
  curl_args=$(cat "$args_file")

  assert_contains "$json" '"tag_name": "v0.1.49"' "latest request fixture output"
  assert_contains "$curl_args" "https://api.github.com/repos/guibeira/wakezilla/releases/latest" "latest request endpoint"
  assert_contains "$curl_args" "-H" "latest request header flag"
  assert_contains "$curl_args" "Accept: application/vnd.github+json" "latest request accept header"
  assert_contains "$curl_args" "X-GitHub-Api-Version: 2022-11-28" "latest request api version header"
  assert_not_contains "$curl_args" "Authorization:" "latest request without token"

  rm -rf "$temp_dir"
}

test_fetch_release_json_version_request() {
  temp_dir=$(mktemp -d)
  mkdir -p "$temp_dir/bin"
  args_file="$temp_dir/curl-args"
  write_recording_fixture_curl "$temp_dir/bin/curl"

  (
    REPO=guibeira/wakezilla
    WAKEZILLA_FAKE_CURL_ARGS="$args_file"
    WAKEZILLA_FAKE_CURL_FIXTURE="$ROOT_DIR/tests/fixtures/install/release-v0.1.49.json"
    PATH="$temp_dir/bin:$PATH"
    export WAKEZILLA_FAKE_CURL_ARGS WAKEZILLA_FAKE_CURL_FIXTURE PATH
    fetch_release_json "0.1.49"
  ) >/dev/null
  curl_args=$(cat "$args_file")

  assert_contains "$curl_args" "https://api.github.com/repos/guibeira/wakezilla/releases/tags/v0.1.49" "version request endpoint"

  rm -rf "$temp_dir"
}

test_fetch_release_json_token_request() {
  temp_dir=$(mktemp -d)
  mkdir -p "$temp_dir/bin"
  args_file="$temp_dir/curl-args"
  write_recording_fixture_curl "$temp_dir/bin/curl"

  (
    REPO=guibeira/wakezilla
    GITHUB_TOKEN=secret-token
    WAKEZILLA_FAKE_CURL_ARGS="$args_file"
    WAKEZILLA_FAKE_CURL_FIXTURE="$ROOT_DIR/tests/fixtures/install/release-v0.1.49.json"
    PATH="$temp_dir/bin:$PATH"
    export WAKEZILLA_FAKE_CURL_ARGS WAKEZILLA_FAKE_CURL_FIXTURE PATH
    fetch_release_json ""
  ) >/dev/null
  curl_args=$(cat "$args_file")

  assert_contains "$curl_args" "Authorization: Bearer secret-token" "token request authorization header"

  rm -rf "$temp_dir"
}

if test_github_api_helpers_defined; then
  test_fetch_release_json_latest_request
  test_fetch_release_json_version_request
  test_fetch_release_json_token_request
fi

test_install_release_json_helpers_defined() {
  missing=0
  assert_command_exists release_version_from_json "release version json helper" || missing=1
  assert_command_exists asset_url_from_json "asset url json helper" || missing=1
  assert_command_exists available_targets_from_json "available targets json helper" || missing=1
  assert_command_exists download_file "download helper" || missing=1
  assert_command_exists checksum_url_for_release "checksum url helper" || missing=1
  assert_command_exists verify_checksum "verify checksum helper" || missing=1
  [ "$missing" -eq 0 ]
}

test_release_version_from_json() {
  version=$(release_version_from_json < "$ROOT_DIR/tests/fixtures/install/release-v0.1.49.json")
  assert_eq "0.1.49" "$version" "release version from json"
}

test_asset_url_from_json() {
  url=$(asset_url_from_json wakezilla 0.1.49 x86_64-unknown-linux-gnu < "$ROOT_DIR/tests/fixtures/install/release-v0.1.49.json")
  assert_eq "https://example.test/wakezilla-0.1.49-x86_64-unknown-linux-gnu.tar.gz" "$url" "asset url"
}

test_available_targets_from_json() {
  targets=$(available_targets_from_json wakezilla < "$ROOT_DIR/tests/fixtures/install/release-v0.1.49.json" | tr '\n' ' ')
  assert_contains "$targets" "x86_64-unknown-linux-gnu" "available linux target"
  assert_contains "$targets" "aarch64-apple-darwin" "available mac target"
}

test_verify_checksum_sha256sum() {
  if ! command -v sha256sum >/dev/null 2>&1; then
    printf 'SKIP: sha256sum checksum test\n'
    return 0
  fi

  temp_dir=$(mktemp -d)
  printf 'hello\n' > "$temp_dir/file.txt"
  sha=$(sha256sum "$temp_dir/file.txt" | awk '{print $1}')
  printf '%s  file.txt\n' "$sha" > "$temp_dir/SHA256SUMS"

  verify_checksum "$temp_dir/file.txt" "$temp_dir/SHA256SUMS" "file.txt"
  rm -rf "$temp_dir"
}

test_verify_checksum_rejects_mismatch() {
  if ! command -v sha256sum >/dev/null 2>&1; then
    printf 'SKIP: sha256sum mismatch test\n'
    return 0
  fi

  temp_dir=$(mktemp -d)
  printf 'hello\n' > "$temp_dir/file.txt"
  printf '0000000000000000000000000000000000000000000000000000000000000000  file.txt\n' > "$temp_dir/SHA256SUMS"

  if output=$(verify_checksum "$temp_dir/file.txt" "$temp_dir/SHA256SUMS" "file.txt" 2>&1); then
    fail "checksum mismatch: expected failure, got '$output'"
  else
    assert_contains "$output" "checksum verification failed" "checksum mismatch"
  fi
  rm -rf "$temp_dir"
}

if test_install_release_json_helpers_defined; then
  test_release_version_from_json
  test_asset_url_from_json
  test_available_targets_from_json
  test_verify_checksum_sha256sum
  test_verify_checksum_rejects_mismatch
fi

if [ "$failures" -ne 0 ]; then
  printf '%s test(s) failed\n' "$failures" >&2
  exit 1
fi

printf 'install.sh tests passed\n'
