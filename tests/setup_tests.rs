use std::path::Path;

use clap::Parser;
use wakezilla::service::{self, Mode};
use wakezilla::setup::SetupArgs;

#[test]
fn mode_parses_from_str() {
    assert_eq!(Mode::from_str_opt("proxy"), Some(Mode::Proxy));
    assert_eq!(Mode::from_str_opt("client"), Some(Mode::Client));
    assert_eq!(Mode::from_str_opt("nonsense"), None);
}

#[test]
fn mode_exposes_subcommand_and_service_name() {
    assert_eq!(Mode::Proxy.subcommand(), "proxy-server");
    assert_eq!(Mode::Client.subcommand(), "client-server");
    assert_eq!(Mode::Proxy.service_name(), "wakezilla-proxy");
    assert_eq!(Mode::Client.service_name(), "wakezilla-client");
}

#[test]
fn managed_modes_include_proxy_and_client() {
    assert_eq!(service::managed_modes(), [Mode::Proxy, Mode::Client]);
}

#[test]
fn service_program_args_disable_update_checks() {
    assert_eq!(
        service::service_program_args(Mode::Proxy),
        ["--no-update-check", "proxy-server"]
    );
    assert_eq!(
        service::service_program_args(Mode::Client),
        ["--no-update-check", "client-server"]
    );
}

#[test]
fn windows_service_program_args_use_hidden_entrypoint() {
    assert_eq!(
        service::windows_service_program_args(Mode::Proxy),
        ["--no-update-check", "windows-service", "proxy"]
    );
    assert_eq!(
        service::windows_service_program_args(Mode::Client),
        ["--no-update-check", "windows-service", "client"]
    );
}

#[test]
fn firewall_rule_names_are_stable_per_mode() {
    assert_eq!(
        service::firewall_rule_name(Mode::Proxy),
        "Wakezilla Proxy Server"
    );
    assert_eq!(
        service::firewall_rule_name(Mode::Client),
        "Wakezilla Client Server"
    );
}

#[test]
fn service_log_file_names_are_stable_per_mode() {
    assert_eq!(
        service::service_log_file_name(Mode::Proxy),
        "wakezilla-proxy.log"
    );
    assert_eq!(
        service::service_log_file_name(Mode::Client),
        "wakezilla-client.log"
    );
}

#[test]
fn protected_service_binary_paths_are_fixed_per_platform_and_mode() {
    assert_eq!(
        service::linux_service_binary_path(Mode::Proxy),
        Path::new("/usr/local/libexec/wakezilla/wakezilla-proxy")
    );
    assert_eq!(
        service::linux_service_binary_path(Mode::Client),
        Path::new("/usr/local/libexec/wakezilla/wakezilla-client")
    );
    assert_eq!(
        service::macos_service_binary_path(Mode::Proxy),
        Path::new("/Library/PrivilegedHelperTools/dev.wakezilla.proxy")
    );
    assert_eq!(
        service::macos_service_binary_path(Mode::Client),
        Path::new("/Library/PrivilegedHelperTools/dev.wakezilla.client")
    );

    let program_files = Path::new("C:/Program Files");
    assert_eq!(
        service::windows_service_binary_path_in(program_files, Mode::Proxy),
        Path::new("C:/Program Files/Wakezilla/Service/wakezilla-proxy.exe")
    );
    assert_eq!(
        service::windows_service_binary_path_in(program_files, Mode::Client),
        Path::new("C:/Program Files/Wakezilla/Service/wakezilla-client.exe")
    );
}

#[test]
#[cfg(not(target_os = "windows"))]
fn systemd_unit_uses_only_the_fixed_protected_binary() {
    let unit = service::generate_systemd_unit(Mode::Proxy);
    assert!(unit.contains(
        "ExecStart=/usr/local/libexec/wakezilla/wakezilla-proxy --no-update-check proxy-server"
    ));
    assert!(unit.contains("[Service]"));
    assert!(unit.contains("WantedBy=multi-user.target"));
    assert!(service::systemd_unit_uses_protected_binary(
        Mode::Proxy,
        &unit
    ));

    let legacy = unit.replace(
        "/usr/local/libexec/wakezilla/wakezilla-proxy",
        "/home/alice/.local/bin/wakezilla",
    );
    assert!(!service::systemd_unit_uses_protected_binary(
        Mode::Proxy,
        &legacy
    ));
}

#[test]
#[cfg(not(target_os = "windows"))]
fn launchd_plist_uses_only_the_fixed_protected_binary() {
    let plist = service::generate_launchd_plist(Mode::Client);
    assert!(plist.contains("dev.wakezilla.client"));
    assert!(plist.contains("/Library/PrivilegedHelperTools/dev.wakezilla.client"));
    assert!(plist.contains("<string>--no-update-check</string>"));
    assert!(plist.contains("<string>client-server</string>"));
    assert!(plist.contains("<key>RunAtLoad</key>"));
    assert!(plist.contains("<key>StandardErrorPath</key>"));
    assert!(plist.contains("<key>StandardOutPath</key>"));
    assert!(plist.contains("/Library/Logs/wakezilla/dev.wakezilla.client.err.log"));
    assert!(service::launchd_plist_uses_protected_binary(
        Mode::Client,
        &plist
    ));

    let legacy = plist.replace(
        "/Library/PrivilegedHelperTools/dev.wakezilla.client",
        "/Users/alice/Applications/Wakezilla.app/Contents/MacOS/wakezilla",
    );
    assert!(!service::launchd_plist_uses_protected_binary(
        Mode::Client,
        &legacy
    ));
}

#[test]
fn windows_image_path_validation_rejects_user_writable_legacy_binary() {
    let program_files = Path::new("C:/Program Files");
    let protected = service::windows_service_binary_path_in(program_files, Mode::Proxy);
    assert!(service::windows_image_path_uses_protected_binary(
        program_files,
        Mode::Proxy,
        &protected
    ));
    assert!(!service::windows_image_path_uses_protected_binary(
        program_files,
        Mode::Proxy,
        Path::new(r"C:\Users\alice\AppData\Local\Wakezilla\wakezilla.exe")
    ));
}

#[test]
fn windows_image_path_validation_accepts_only_the_canonical_service_command() {
    let program_files = Path::new("C:/Program Files");
    let canonical = Path::new(
        r#""C:\Program Files\Wakezilla\Service\wakezilla-client.exe" --no-update-check windows-service client"#,
    );
    assert!(service::windows_image_path_uses_protected_binary(
        program_files,
        Mode::Client,
        canonical
    ));

    let unexpected_args =
        Path::new(r#""C:\Program Files\Wakezilla\Service\wakezilla-client.exe" client-server"#);
    assert!(!service::windows_image_path_uses_protected_binary(
        program_files,
        Mode::Client,
        unexpected_args
    ));
}

#[test]
fn windows_protected_acl_contract_allows_only_system_and_administrators() {
    let directory = service::windows_service_directory_sddl();
    let file = service::windows_service_file_sddl();

    assert_eq!(directory, "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    assert_eq!(file, "D:P(A;;FA;;;SY)(A;;FA;;;BA)");
    for unsafe_sid in [";;;BU", ";;;AU", ";;;WD"] {
        assert!(!directory.contains(unsafe_sid));
        assert!(!file.contains(unsafe_sid));
    }
}

#[test]
fn validate_succeeds_when_port_is_listening() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    // Listener stays alive for the duration of the test.
    service::validate(port, 5).expect("validate connects to open port");
}

#[test]
fn validate_fails_when_port_is_closed() {
    // Bind then drop to obtain an almost-certainly-free port.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let result = service::validate(port, 2);
    assert!(result.is_err(), "validate should fail on closed port");
}

#[test]
fn build_config_sets_correct_port_for_mode() {
    let proxy = wakezilla::setup::build_config(Mode::Proxy, 5000);
    assert_eq!(proxy.server.proxy_port, 5000);
    assert_eq!(proxy.server.client_port, 3001); // untouched default

    let client = wakezilla::setup::build_config(Mode::Client, 6000);
    assert_eq!(client.server.client_port, 6000);
    assert_eq!(client.server.proxy_port, 3000); // untouched default
}

#[test]
fn setup_accepts_a_valid_client_key() {
    let key = wakezilla::shutdown_auth::generate_key();
    let args = SetupArgs::try_parse_from([
        "setup", "--mode", "client", "--port", "3001", "--key", &key, "--yes",
    ])
    .expect("client key should parse");

    assert_eq!(args.key.as_deref(), Some(key.as_str()));
    assert!(!format!("{args:?}").contains(&key));
}

#[test]
fn setup_accepts_client_keys_starting_with_hyphens() {
    for key in [
        "-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "--AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        wakezilla::shutdown_auth::validate_key(key).expect("valid URL-safe key fixture");
        let args = SetupArgs::try_parse_from([
            "setup", "--mode", "client", "--port", "3001", "--key", key, "--yes",
        ])
        .expect("hyphen-prefixed client key should parse");

        assert_eq!(args.key.as_deref(), Some(key));
        assert!(args.yes);
    }
}
