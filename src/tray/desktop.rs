use crate::{config, service, update};
use anyhow::{anyhow, Context, Result};
#[cfg(target_os = "windows")]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::io::Cursor;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Write as _;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    Icon, TrayIcon, TrayIconBuilder,
};
#[cfg(target_os = "macos")]
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::WindowId,
};

const TRAY_INSTANCE_NAME: &str = "dev.wakezilla.tray";
const OPEN_DASHBOARD_ID: &str = "open_dashboard";
const COPY_DASHBOARD_URL_ID: &str = "copy_dashboard_url";
const SETUP_ID: &str = "setup_services";
const CHECK_UPDATES_ID: &str = "check_updates";
const QUIT_ID: &str = "quit_tray";
const PROXY_START_ID: &str = "proxy_start";
const PROXY_STOP_ID: &str = "proxy_stop";
const PROXY_RESTART_ID: &str = "proxy_restart";
const PROXY_LOGS_ID: &str = "proxy_logs";
const CLIENT_START_ID: &str = "client_start";
const CLIENT_STOP_ID: &str = "client_stop";
const CLIENT_RESTART_ID: &str = "client_restart";
const CLIENT_LOGS_ID: &str = "client_logs";

#[derive(Debug)]
enum UserEvent {
    Menu(String),
    Refresh,
    Status(ServiceStatuses),
    Message(String),
}

#[derive(Debug, Clone, Copy)]
enum ServiceControl {
    Start,
    Stop,
    Restart,
}

struct ModeMenu {
    status: MenuItem,
    start: MenuItem,
    stop: MenuItem,
    restart: MenuItem,
    logs: MenuItem,
}

struct TrayMenu {
    message: MenuItem,
    proxy: ModeMenu,
    client: ModeMenu,
}

struct TrayApp {
    dashboard_url: String,
    proxy: EventLoopProxy<UserEvent>,
    menu: Option<TrayMenu>,
    tray_icon: Option<TrayIcon>,
    startup_error: Option<String>,
    status_refresh_in_flight: bool,
}

struct TrayInstanceGuard {
    #[cfg(target_os = "linux")]
    _socket: OwnedFd,
    #[cfg(target_os = "windows")]
    _instance: single_instance::SingleInstance,
    #[cfg(target_os = "macos")]
    _lock_file: std::fs::File,
}

impl TrayInstanceGuard {
    fn acquire_named(name: &str) -> Result<Option<Self>> {
        #[cfg(target_os = "linux")]
        {
            Self::acquire_linux(name)
        }

        #[cfg(target_os = "windows")]
        {
            let backend_name = name.to_owned();
            let instance =
                single_instance::SingleInstance::new(&backend_name).with_context(|| {
                    format!("failed to acquire tray instance lock `{name}` as `{backend_name}`")
                })?;
            if !instance.is_single() {
                return Ok(None);
            }

            Ok(Some(Self {
                _instance: instance,
            }))
        }

        #[cfg(target_os = "macos")]
        {
            Self::acquire_macos(name)
        }
    }

    #[cfg(target_os = "linux")]
    fn acquire_linux(name: &str) -> Result<Option<Self>> {
        let backend_name = linux_backend_name(name, effective_uid());
        let socket_type = combine_linux_socket_type(libc::SOCK_STREAM, libc::SOCK_CLOEXEC);

        // SAFETY: socket is called with valid Linux domain/type/protocol constants and no
        // pointers. A nonnegative result is a newly owned descriptor.
        let raw_socket = unsafe { libc::socket(libc::AF_UNIX, socket_type, 0) };
        if raw_socket < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("failed to create tray instance socket `{backend_name}`")
            });
        }
        // SAFETY: raw_socket was just returned as an owned descriptor and has not been wrapped
        // or closed. OwnedFd now closes it on every return path.
        let socket = unsafe { OwnedFd::from_raw_fd(raw_socket) };
        let (address, address_len) = linux_abstract_socket_address(&backend_name)?;

        // SAFETY: socket is a valid AF_UNIX descriptor; address points to an initialized
        // sockaddr_un and address_len covers only its family and populated abstract name.
        let rc = unsafe {
            libc::bind(
                socket.as_raw_fd(),
                std::ptr::addr_of!(address).cast::<libc::sockaddr>(),
                address_len,
            )
        };
        if rc == 0 {
            return Ok(Some(Self { _socket: socket }));
        }

        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EADDRINUSE) {
            Ok(None)
        } else {
            Err(error)
                .with_context(|| format!("failed to bind tray instance socket `{backend_name}`"))
        }
    }

    #[cfg(target_os = "macos")]
    fn acquire_macos(name: &str) -> Result<Option<Self>> {
        let lock_path = macos_lock_path(name)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        options.mode(0o600);
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let lock_file = options
            .open(&lock_path)
            .with_context(|| format!("failed to open tray lock {}", lock_path.display()))?;
        lock_file
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure tray lock {}", lock_path.display()))?;

        // SAFETY: lock_file owns a valid descriptor for this call, and flock does not retain it
        // or access Rust-managed memory.
        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        let errno = if rc == 0 {
            0
        } else {
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        };

        match classify_macos_flock_result(rc, errno)
            .with_context(|| format!("failed to lock tray instance file {}", lock_path.display()))?
        {
            MacosFlockOutcome::Acquired => Ok(Some(Self {
                _lock_file: lock_file,
            })),
            MacosFlockOutcome::Contended => Ok(None),
        }
    }
}

#[cfg(target_os = "linux")]
fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid has no preconditions and does not dereference any pointers.
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
fn linux_backend_name(name: &str, euid: libc::uid_t) -> String {
    format!("{name}.uid-{euid}")
}

#[cfg(any(target_os = "linux", test))]
fn combine_linux_socket_type(stream: i32, cloexec: i32) -> i32 {
    stream | cloexec
}

#[cfg(target_os = "linux")]
fn linux_abstract_socket_address(name: &str) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: sockaddr_un contains only integer fields and a c_char array, for which all-zero is
    // a valid bit pattern. Zeroing also establishes the leading NUL for an abstract address.
    let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let name_bytes = name.as_bytes();
    let maximum_name_len = address.sun_path.len().saturating_sub(1);
    if name_bytes.len() > maximum_name_len {
        anyhow::bail!(
            "tray instance socket name is {} bytes; maximum is {maximum_name_len}",
            name_bytes.len()
        );
    }
    address.sun_path[0] = 0;
    for (index, byte) in name_bytes.iter().enumerate() {
        address.sun_path[index + 1] = *byte as libc::c_char;
    }

    let address_len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name_bytes.len();
    let address_len = libc::socklen_t::try_from(address_len)
        .context("tray instance socket address length overflowed socklen_t")?;
    Ok((address, address_len))
}

#[cfg(target_os = "macos")]
#[derive(Debug, Eq, PartialEq)]
enum MacosFlockOutcome {
    Acquired,
    Contended,
}

#[cfg(target_os = "macos")]
fn classify_macos_flock_result(
    rc: libc::c_int,
    errno: libc::c_int,
) -> std::io::Result<MacosFlockOutcome> {
    if rc == 0 {
        Ok(MacosFlockOutcome::Acquired)
    } else if errno == libc::EWOULDBLOCK {
        Ok(MacosFlockOutcome::Contended)
    } else {
        Err(std::io::Error::from_raw_os_error(errno))
    }
}

#[cfg(target_os = "macos")]
fn macos_lock_path(name: &str) -> Result<PathBuf> {
    let temp_dir = std::env::temp_dir()
        .canonicalize()
        .context("failed to resolve the per-user temporary directory")?;
    let lock_dir = temp_dir.join("Wakezilla");
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&lock_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to create {}", lock_dir.display()));
        }
    }

    let metadata = std::fs::symlink_metadata(&lock_dir)
        .with_context(|| format!("failed to inspect {}", lock_dir.display()))?;
    if !metadata.file_type().is_dir() {
        anyhow::bail!(
            "tray lock directory is not a directory: {}",
            lock_dir.display()
        );
    }
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", lock_dir.display()))?;
    }

    let file_name = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    Ok(lock_dir.join(format!("instance-{file_name}.lock")))
}

#[derive(Debug, Clone, Copy)]
struct ServiceStatuses {
    proxy: ModeStatus,
    client: ModeStatus,
}

#[derive(Debug, Clone, Copy)]
struct ModeStatus {
    installed: bool,
    running: bool,
}

pub fn run() -> Result<()> {
    let Some(_instance_guard) = TrayInstanceGuard::acquire_named(TRAY_INSTANCE_NAME)? else {
        return Ok(());
    };

    #[cfg(target_os = "linux")]
    gtk::init().context("failed to initialize GTK")?;

    let config = config::Config::load();
    let dashboard_url = dashboard_url(&config);

    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    builder.with_activation_policy(ActivationPolicy::Accessory);
    let event_loop = builder
        .build()
        .context("failed to create tray event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    install_menu_event_handler(proxy.clone());
    start_refresh_timer(proxy.clone());

    let mut app = TrayApp {
        dashboard_url,
        proxy,
        menu: None,
        tray_icon: None,
        startup_error: None,
        status_refresh_in_flight: false,
    };

    event_loop
        .run_app(&mut app)
        .context("tray event loop failed")?;

    if let Some(error) = app.startup_error {
        anyhow::bail!(error);
    }
    Ok(())
}

fn install_menu_event_handler(proxy: EventLoopProxy<UserEvent>) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(event.id().as_ref().to_string()));
    }));
}

fn start_refresh_timer(proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(10));
        if proxy.send_event(UserEvent::Refresh).is_err() {
            break;
        }
    });
}

impl ApplicationHandler<UserEvent> for TrayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if !matches!(cause, StartCause::Init) || self.tray_icon.is_some() {
            return;
        }

        if let Err(error) = self.create_tray_icon() {
            self.startup_error = Some(error.to_string());
            tracing::error!("Tray startup failed: {error:#}");
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(target_os = "linux")]
        {
            gtk::main_iteration_do(false);

            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + Duration::from_millis(50),
            ));
        }

        #[cfg(not(target_os = "linux"))]
        let _ = event_loop;
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(id) => self.handle_menu_event(event_loop, &id),
            UserEvent::Refresh => self.refresh_status(),
            UserEvent::Status(statuses) => self.apply_status(statuses),
            UserEvent::Message(message) => self.set_message(message),
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }
}

impl TrayApp {
    fn create_tray_icon(&mut self) -> Result<()> {
        let (root_menu, tray_menu) = build_menu()?;
        let icon = load_tray_icon()?;

        let tray_icon_builder = TrayIconBuilder::new()
            .with_tooltip("Wakezilla")
            .with_icon(icon)
            .with_menu(Box::new(root_menu))
            .with_menu_on_left_click(true);

        #[cfg(target_os = "macos")]
        let tray_icon_builder = tray_icon_builder.with_icon_as_template(false);

        let tray_icon = tray_icon_builder
            .build()
            .context("failed to build tray icon")?;

        self.menu = Some(tray_menu);
        self.tray_icon = Some(tray_icon);
        self.refresh_status();
        Ok(())
    }

    fn handle_menu_event(&mut self, event_loop: &ActiveEventLoop, id: &str) {
        match id {
            OPEN_DASHBOARD_ID => self.open_dashboard(),
            COPY_DASHBOARD_URL_ID => self.copy_dashboard_url(),
            SETUP_ID => self.configure_startup(),
            CHECK_UPDATES_ID => self.check_for_updates(),
            QUIT_ID => event_loop.exit(),
            PROXY_START_ID => self.run_service_control(service::Mode::Proxy, ServiceControl::Start),
            PROXY_STOP_ID => self.run_service_control(service::Mode::Proxy, ServiceControl::Stop),
            PROXY_RESTART_ID => {
                self.run_service_control(service::Mode::Proxy, ServiceControl::Restart)
            }
            PROXY_LOGS_ID => self.open_logs(service::Mode::Proxy),
            CLIENT_START_ID => {
                self.run_service_control(service::Mode::Client, ServiceControl::Start)
            }
            CLIENT_STOP_ID => self.run_service_control(service::Mode::Client, ServiceControl::Stop),
            CLIENT_RESTART_ID => {
                self.run_service_control(service::Mode::Client, ServiceControl::Restart)
            }
            CLIENT_LOGS_ID => self.open_logs(service::Mode::Client),
            _ => {}
        }
    }

    fn open_dashboard(&mut self) {
        match open::that(&self.dashboard_url) {
            Ok(()) => self.set_message(format!("Opened {}", self.dashboard_url)),
            Err(error) => self.set_message(format!("Failed to open dashboard: {error}")),
        }
    }

    fn copy_dashboard_url(&mut self) {
        let result = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(self.dashboard_url.clone()));

        match result {
            Ok(()) => self.set_message("Dashboard URL copied.".to_string()),
            Err(error) => self.set_message(format!("Failed to copy dashboard URL: {error}")),
        }
    }

    fn configure_startup(&mut self) {
        let autostart = install_tray_autostart();
        let setup = open_wakezilla_command(true, &["setup"], true);

        match (autostart, setup) {
            (Ok(path), Ok(())) => self.set_message(format!(
                "Tray autostart installed at {}; opened service setup.",
                path.display()
            )),
            (Ok(path), Err(error)) => self.set_message(format!(
                "Tray autostart installed at {}; failed to open service setup: {error}",
                path.display()
            )),
            (Err(error), Ok(())) => self.set_message(format!(
                "Failed to install tray autostart: {error}; opened service setup."
            )),
            (Err(autostart_error), Err(setup_error)) => self.set_message(format!(
                "Startup setup failed: {autostart_error}; service setup failed: {setup_error}"
            )),
        }
    }

    fn open_logs(&mut self, mode: service::Mode) {
        let result = open_wakezilla_command(
            true,
            &[
                "--no-update-check",
                "service",
                "logs",
                "--mode",
                mode.service_arg(),
                "--lines",
                "100",
            ],
            true,
        );

        match result {
            Ok(()) => self.set_message(format!("Opened {} logs.", mode_label(mode))),
            Err(error) => {
                self.set_message(format!("Failed to open {} logs: {error}", mode_label(mode)))
            }
        }
    }

    fn check_for_updates(&mut self) {
        self.set_message("Checking for updates...".to_string());
        let proxy = self.proxy.clone();

        std::thread::spawn(move || {
            let message = match check_latest_version() {
                Ok(message) => message,
                Err(error) => format!("Update check failed: {error}"),
            };
            let _ = proxy.send_event(UserEvent::Message(message));
        });
    }

    fn run_service_control(&mut self, mode: service::Mode, control: ServiceControl) {
        self.set_message(format!(
            "{} {} requested...",
            mode_label(mode),
            control.verb()
        ));

        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let message = match run_service_control(mode, control) {
                Ok(message) => message,
                Err(error) => format!("{} {} failed: {error}", mode_label(mode), control.verb()),
            };
            let _ = proxy.send_event(UserEvent::Message(message));
            let _ = proxy.send_event(UserEvent::Refresh);
        });
    }

    fn refresh_status(&mut self) {
        if self.menu.is_none() || self.status_refresh_in_flight {
            return;
        }

        self.status_refresh_in_flight = true;
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let statuses = ServiceStatuses {
                proxy: query_mode_status(service::Mode::Proxy),
                client: query_mode_status(service::Mode::Client),
            };
            let _ = proxy.send_event(UserEvent::Status(statuses));
        });
    }

    fn apply_status(&mut self, statuses: ServiceStatuses) {
        self.status_refresh_in_flight = false;
        if let Some(menu) = &self.menu {
            update_mode_menu(service::Mode::Proxy, &menu.proxy, statuses.proxy);
            update_mode_menu(service::Mode::Client, &menu.client, statuses.client);
        }
    }

    fn set_message(&mut self, message: String) {
        if let Some(menu) = &self.menu {
            menu.message.set_text(message);
        }
    }
}

impl ServiceControl {
    fn verb(self) -> &'static str {
        match self {
            ServiceControl::Start => "start",
            ServiceControl::Stop => "stop",
            ServiceControl::Restart => "restart",
        }
    }
}

fn build_menu() -> Result<(Menu, TrayMenu)> {
    let open_dashboard = MenuItem::with_id(OPEN_DASHBOARD_ID, "Open dashboard", true, None);
    let copy_dashboard_url =
        MenuItem::with_id(COPY_DASHBOARD_URL_ID, "Copy dashboard URL", true, None);
    let setup = MenuItem::with_id(SETUP_ID, "Configure startup", true, None);
    let check_updates = MenuItem::with_id(CHECK_UPDATES_ID, "Check for updates", true, None);
    let quit = MenuItem::with_id(QUIT_ID, "Quit tray", true, None);
    let message = MenuItem::with_id("tray_message", "Ready", false, None);

    let (proxy_submenu, proxy) = build_mode_submenu(service::Mode::Proxy)?;
    let (client_submenu, client) = build_mode_submenu(service::Mode::Client)?;

    let separator1 = PredefinedMenuItem::separator();
    let separator2 = PredefinedMenuItem::separator();
    let separator3 = PredefinedMenuItem::separator();
    let separator4 = PredefinedMenuItem::separator();

    let root = Menu::new();
    root.append_items(&[
        &message,
        &separator1,
        &open_dashboard,
        &copy_dashboard_url,
        &separator2,
        &proxy_submenu,
        &client_submenu,
        &separator3,
        &setup,
        &check_updates,
        &separator4,
        &quit,
    ])
    .context("failed to build tray menu")?;

    Ok((
        root,
        TrayMenu {
            message,
            proxy,
            client,
        },
    ))
}

fn build_mode_submenu(mode: service::Mode) -> Result<(Submenu, ModeMenu)> {
    let (status_id, start_id, stop_id, restart_id, logs_id) = match mode {
        service::Mode::Proxy => (
            "proxy_status",
            PROXY_START_ID,
            PROXY_STOP_ID,
            PROXY_RESTART_ID,
            PROXY_LOGS_ID,
        ),
        service::Mode::Client => (
            "client_status",
            CLIENT_START_ID,
            CLIENT_STOP_ID,
            CLIENT_RESTART_ID,
            CLIENT_LOGS_ID,
        ),
    };

    let status = MenuItem::with_id(
        status_id,
        format!("{}: unknown", mode_label(mode)),
        false,
        None,
    );
    let start = MenuItem::with_id(start_id, "Start", true, None);
    let stop = MenuItem::with_id(stop_id, "Stop", true, None);
    let restart = MenuItem::with_id(restart_id, "Restart", true, None);
    let logs = MenuItem::with_id(logs_id, "Logs", true, None);
    let separator1 = PredefinedMenuItem::separator();
    let separator2 = PredefinedMenuItem::separator();

    let submenu = Submenu::with_id(mode.service_arg(), mode_label(mode), true);
    submenu
        .append_items(&[
            &status,
            &separator1,
            &start,
            &stop,
            &restart,
            &separator2,
            &logs,
        ])
        .with_context(|| format!("failed to build {} tray menu", mode_label(mode)))?;

    Ok((
        submenu,
        ModeMenu {
            status,
            start,
            stop,
            restart,
            logs,
        },
    ))
}

fn update_mode_menu(mode: service::Mode, menu: &ModeMenu, status: ModeStatus) {
    let installed = status.installed;
    let running = status.running;
    let label = service_status_label(status);

    menu.status
        .set_text(format!("{}: {label}", mode_label(mode)));
    menu.start.set_enabled(installed && !running);
    menu.stop.set_enabled(installed && running);
    menu.restart.set_enabled(installed);
    menu.logs.set_enabled(installed);
}

fn query_mode_status(mode: service::Mode) -> ModeStatus {
    let installed = service::is_installed(mode);
    let running = installed && service::is_running(mode);

    ModeStatus { installed, running }
}

fn service_status_label(status: ModeStatus) -> &'static str {
    if !status.installed {
        "not installed"
    } else if status.running {
        "running"
    } else {
        "stopped"
    }
}

fn run_service_control(mode: service::Mode, control: ServiceControl) -> Result<String> {
    if service::is_elevated() {
        match control {
            ServiceControl::Start => service::start(mode),
            ServiceControl::Stop => service::stop(mode),
            ServiceControl::Restart => service::restart(mode),
        }
        .with_context(|| format!("failed to {} {} service", control.verb(), mode_label(mode)))?;

        return Ok(format!(
            "{} {} completed.",
            mode_label(mode),
            control.verb()
        ));
    }

    open_wakezilla_command(
        true,
        &[
            "--no-update-check",
            "service",
            control.verb(),
            "--mode",
            mode.service_arg(),
        ],
        true,
    )?;
    Ok(format!(
        "Opened elevated {} {} command.",
        mode_label(mode),
        control.verb()
    ))
}

fn check_latest_version() -> Result<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create update check runtime")?;

    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .context("failed to create update check HTTP client")?;

        match update::check_latest(&client, env!("CARGO_PKG_VERSION")).await? {
            update::UpdateStatus::Current { current } => {
                Ok(format!("Wakezilla is up to date ({current})."))
            }
            update::UpdateStatus::Available { current, latest } => Ok(format!(
                "Wakezilla {latest} is available (current {current})."
            )),
        }
    })
}

fn dashboard_url(config: &config::Config) -> String {
    format!("http://127.0.0.1:{}", config.server.proxy_port)
}

fn mode_label(mode: service::Mode) -> &'static str {
    match mode {
        service::Mode::Proxy => "Proxy",
        service::Mode::Client => "Client",
    }
}

fn load_tray_icon() -> Result<Icon> {
    let bytes = include_bytes!("../../frontend/public/images/wakezilla.png");
    let mut decoder = png::Decoder::new(Cursor::new(&bytes[..]));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("failed to decode tray icon")?;
    let output_size = reader
        .output_buffer_size()
        .context("tray icon output buffer is too large")?;
    let mut buffer = vec![0; output_size];
    let frame = reader
        .next_frame(&mut buffer)
        .context("failed to read tray icon frame")?;
    let bytes = &buffer[..frame.buffer_size()];
    let rgba = rgba_from_png_frame(bytes, frame.color_type)?;

    #[cfg(target_os = "macos")]
    let rgba = {
        let mut rgba = rgba;
        macos_white_mascot_rgba(&mut rgba, frame.width, frame.height);
        rgba
    };

    Icon::from_rgba(rgba, frame.width, frame.height).context("failed to create tray icon")
}

#[cfg(target_os = "macos")]
fn macos_white_mascot_rgba(rgba: &mut [u8], width: u32, height: u32) {
    const DARK_LINE_LUMA_MAX: u16 = 100;
    const INNER_LINE_MARGIN: usize = 2;

    let width = usize::try_from(width).expect("tray icon width must fit usize");
    let height = usize::try_from(height).expect("tray icon height must fit usize");
    let pixel_count = width
        .checked_mul(height)
        .expect("tray icon pixel count must fit usize");
    assert_eq!(rgba.len(), pixel_count * 4, "tray icon must be RGBA");

    let alpha = rgba.chunks(4).map(|pixel| pixel[3]).collect::<Vec<_>>();
    let dark = rgba
        .chunks(4)
        .map(|pixel| {
            let luma =
                (54 * u16::from(pixel[0]) + 183 * u16::from(pixel[1]) + 19 * u16::from(pixel[2]))
                    / 256;
            luma <= DARK_LINE_LUMA_MAX
        })
        .collect::<Vec<_>>();

    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            let pixel = &mut rgba[index * 4..index * 4 + 4];
            if alpha[index] == 0 {
                continue;
            }

            let has_opaque_margin = x >= INNER_LINE_MARGIN
                && y >= INNER_LINE_MARGIN
                && x + INNER_LINE_MARGIN < width
                && y + INNER_LINE_MARGIN < height
                && (y - INNER_LINE_MARGIN..=y + INNER_LINE_MARGIN).all(|neighbor_y| {
                    (x - INNER_LINE_MARGIN..=x + INNER_LINE_MARGIN)
                        .all(|neighbor_x| alpha[neighbor_y * width + neighbor_x] != 0)
                });
            pixel[..3].fill(if dark[index] && has_opaque_margin {
                0
            } else {
                255
            });
        }
    }
}

fn rgba_from_png_frame(bytes: &[u8], color_type: png::ColorType) -> Result<Vec<u8>> {
    match color_type {
        png::ColorType::Rgba => Ok(bytes.to_vec()),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(bytes.len() / 3 * 4);
            for chunk in bytes.as_chunks::<3>().0 {
                rgba.extend_from_slice(chunk);
                rgba.push(255);
            }
            Ok(rgba)
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(bytes.len() * 4);
            for gray in bytes {
                rgba.extend_from_slice(&[*gray, *gray, *gray, 255]);
            }
            Ok(rgba)
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(bytes.len() / 2 * 4);
            for chunk in bytes.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            Ok(rgba)
        }
        png::ColorType::Indexed => Err(anyhow!("indexed tray icon was not expanded to RGBA")),
    }
}

fn open_wakezilla_command(elevated: bool, args: &[&str], keep_open: bool) -> Result<()> {
    let exe = wakezilla_cli_exe()?;
    open_command(elevated, &exe, args, keep_open)
}

fn wakezilla_cli_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve wakezilla executable")?;
    if !is_wakezilla_tray_exe(&exe) {
        return Ok(exe);
    }

    sibling_exe(&exe, "wakezilla").with_context(|| {
        format!(
            "failed to find wakezilla CLI executable next to {}",
            exe.display()
        )
    })
}

#[cfg(target_os = "windows")]
fn wakezilla_tray_command() -> Result<(PathBuf, Vec<&'static str>)> {
    let exe = std::env::current_exe().context("failed to resolve wakezilla executable")?;
    if is_wakezilla_tray_exe(&exe) {
        return Ok((exe, Vec::new()));
    }

    if let Some(tray_exe) = sibling_exe(&exe, "wakezilla-tray") {
        return Ok((tray_exe, Vec::new()));
    }

    Err(anyhow!(
        "wakezilla-tray helper is required for graphical startup; refusing to launch the console CLI"
    ))
}

fn is_wakezilla_tray_exe(exe: &Path) -> bool {
    exe.file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("wakezilla-tray"))
}

fn sibling_exe(exe: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let file_name = format!("{name}.exe");
    #[cfg(not(target_os = "windows"))]
    let file_name = name;

    let candidate = exe.parent()?.join(file_name);
    candidate.is_file().then_some(candidate)
}

#[cfg(target_os = "linux")]
fn install_tray_autostart() -> Result<std::path::PathBuf> {
    let current_exe = std::env::current_exe().context("failed to resolve wakezilla executable")?;
    let helper = if is_wakezilla_tray_exe(&current_exe) {
        current_exe
    } else {
        sibling_exe(&current_exe, "wakezilla-tray").with_context(|| {
            format!(
                "failed to find wakezilla-tray next to {}",
                current_exe.display()
            )
        })?
    };
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    let config_home = resolve_linux_config_home(xdg_config_home.as_deref(), home.as_deref())?;
    install_linux_tray_autostart_at(&config_home, &helper)
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn resolve_linux_config_home(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(path) = xdg_config_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
    {
        return Ok(path.to_path_buf());
    }
    let home = home
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .context("absolute HOME or XDG_CONFIG_HOME is required to install tray autostart")?;
    Ok(home.join(".config"))
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn install_linux_tray_autostart_at(config_home: &Path, helper: &Path) -> Result<PathBuf> {
    let helper = helper
        .to_str()
        .context("wakezilla-tray path must be valid UTF-8 for a desktop entry")?;
    let autostart_dir = config_home.join("autostart");
    let mut directory_builder = std::fs::DirBuilder::new();
    directory_builder.recursive(true).mode(0o700);
    directory_builder
        .create(&autostart_dir)
        .with_context(|| format!("failed to create {}", autostart_dir.display()))?;
    let canonical = autostart_dir.join("dev.wakezilla.tray.desktop");
    let content = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Wakezilla\n\
         Comment=Wakezilla network wake-on-LAN tray application\n\
         TryExec={}\n\
         Exec={}\n\
         Icon=dev.wakezilla.Wakezilla\n\
         Terminal=false\n\
         StartupNotify=false\n",
        desktop_string_escape(helper)?,
        desktop_entry_quote(helper)?,
    );
    atomic_write_linux_autostart(&canonical, content.as_bytes())?;

    let legacy = autostart_dir.join("wakezilla-tray.desktop");
    if let Ok(metadata) = std::fs::symlink_metadata(&legacy) {
        if metadata.file_type().is_file()
            && std::fs::read(&legacy).is_ok_and(|legacy_content| {
                std::str::from_utf8(&legacy_content).is_ok_and(linux_legacy_autostart_is_owned)
            })
        {
            std::fs::remove_file(&legacy)
                .with_context(|| format!("failed to remove {}", legacy.display()))?;
        }
    }
    Ok(canonical)
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn atomic_write_linux_autostart(path: &Path, content: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .context("Linux autostart path has no parent directory")?;
    let file_name = path
        .file_name()
        .context("Linux autostart path has no file name")?
        .to_string_lossy();
    let (temp_path, mut temp_file) = loop {
        let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            suffix
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", candidate.display()));
            }
        }
    };

    let publish_result = (|| -> Result<()> {
        temp_file
            .write_all(content)
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        temp_file
            .sync_all()
            .with_context(|| format!("failed to sync {}", temp_path.display()))?;
        std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o644))
            .with_context(|| format!("failed to chmod {}", temp_path.display()))?;
        std::fs::rename(&temp_path, path).with_context(|| {
            format!(
                "failed to publish Linux autostart {} -> {}",
                temp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    })();
    if publish_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    publish_result
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn linux_legacy_autostart_is_owned(content: &str) -> bool {
    let mut in_desktop_entry = false;
    let mut entry_type = false;
    let mut name = false;
    let mut exec = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            if in_desktop_entry && entry_type && name && exec {
                return true;
            }
            in_desktop_entry = line == "[Desktop Entry]";
            entry_type = false;
            name = false;
            exec = false;
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Type" => entry_type = value == "Application",
            "Name" => name = matches!(value, "Wakezilla" | "Wakezilla Tray"),
            "Exec" => exec = linux_legacy_exec_is_owned(value),
            _ => {}
        }
    }
    entry_type && name && exec
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn linux_legacy_exec_is_owned(value: &str) -> bool {
    let tokens = linux_desktop_exec_tokens(value, 2);
    let Some(executable) = tokens.first() else {
        return false;
    };
    let Some(basename) = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    basename == "wakezilla-tray"
        || (basename == "wakezilla" && tokens.get(1).is_some_and(|argument| argument == "tray"))
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn linux_desktop_exec_tokens(value: &str, limit: usize) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut characters = value.chars().peekable();
    while tokens.len() < limit {
        while characters
            .peek()
            .is_some_and(|character| character.is_ascii_whitespace())
        {
            characters.next();
        }
        if characters.peek().is_none() {
            break;
        }
        let quoted = characters.peek() == Some(&'"');
        if quoted {
            characters.next();
        }
        let mut token = String::new();
        let mut terminated = !quoted;
        while let Some(character) = characters.next() {
            if quoted && character == '"' {
                terminated = true;
                break;
            }
            if !quoted && character.is_ascii_whitespace() {
                break;
            }
            if character == '\\' {
                let Some(escaped) = characters.next() else {
                    return Vec::new();
                };
                token.push(escaped);
            } else {
                token.push(character);
            }
        }
        if !terminated || token.is_empty() {
            return Vec::new();
        }
        tokens.push(token);
    }
    tokens
}

#[cfg(target_os = "macos")]
fn install_tray_autostart() -> Result<std::path::PathBuf> {
    let executable = std::env::current_exe().context("failed to resolve wakezilla executable")?;
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .context("HOME is required to install tray autostart")?;
    install_macos_tray_autostart_at(&home, &executable)
}

#[cfg(target_os = "macos")]
fn install_macos_tray_autostart_at(home: &Path, executable: &Path) -> Result<PathBuf> {
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let effective_uid = unsafe { libc::geteuid() };
    install_macos_tray_autostart_for_uid(home, executable, effective_uid)
}

#[cfg(target_os = "macos")]
fn install_macos_tray_autostart_for_uid(
    home: &Path,
    executable: &Path,
    effective_uid: libc::uid_t,
) -> Result<PathBuf> {
    if effective_uid == 0 {
        anyhow::bail!("macOS tray autostart must be installed without sudo");
    }
    if !home.is_absolute() {
        anyhow::bail!("HOME must be absolute to install macOS tray autostart");
    }
    let home_metadata = std::fs::symlink_metadata(home)
        .with_context(|| format!("failed to inspect HOME {}", home.display()))?;
    if !home_metadata.file_type().is_dir() || home_metadata.file_type().is_symlink() {
        anyhow::bail!("HOME must be a real, non-symlink directory");
    }
    if home_metadata.uid() != effective_uid {
        anyhow::bail!("HOME is not owned by the effective user");
    }
    let home = home
        .canonicalize()
        .with_context(|| format!("failed to resolve HOME {}", home.display()))?;
    let bundle = macos_bundle_from_executable(executable)?;
    let content = macos_launch_agent_content(&bundle)?;

    let library = home.join("Library");
    ensure_macos_profile_directory(&home, &library, effective_uid)?;
    let launch_agents = library.join("LaunchAgents");
    ensure_macos_profile_directory(&home, &launch_agents, effective_uid)?;
    let path = launch_agents.join("dev.wakezilla.tray.plist");
    atomic_write_macos_launch_agent(&path, content.as_bytes())?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn macos_bundle_from_executable(executable: &Path) -> Result<PathBuf> {
    let executable = executable
        .canonicalize()
        .with_context(|| format!("failed to resolve executable {}", executable.display()))?;
    if !executable.is_file() || !is_wakezilla_tray_exe(&executable) {
        anyhow::bail!(
            "macOS tray autostart requires a wakezilla-tray executable inside Wakezilla.app"
        );
    }
    let macos = executable
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "MacOS"))
        .context("wakezilla-tray is not inside a bundle Contents/MacOS directory")?;
    let contents = macos
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "Contents"))
        .context("wakezilla-tray is not inside a bundle Contents/MacOS directory")?;
    let bundle = contents
        .parent()
        .filter(|path| path.extension().is_some_and(|extension| extension == "app"))
        .context("wakezilla-tray is not inside a macOS application bundle")?;
    if !bundle.is_dir() {
        anyhow::bail!("macOS application bundle is not a directory");
    }
    Ok(bundle.to_path_buf())
}

#[cfg(target_os = "macos")]
fn macos_launch_agent_content(bundle: &Path) -> Result<String> {
    let bundle = bundle
        .to_str()
        .context("Wakezilla.app path must be valid UTF-8 for a LaunchAgent")?;
    let bundle = xml_escape(bundle)?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
           <key>Label</key>\n\
           <string>dev.wakezilla.tray</string>\n\
           <key>ProgramArguments</key>\n\
           <array>\n\
             <string>/usr/bin/open</string>\n\
             <string>-g</string>\n\
             <string>{bundle}</string>\n\
           </array>\n\
           <key>RunAtLoad</key>\n\
           <true/>\n\
           <key>LimitLoadToSessionType</key>\n\
           <string>Aqua</string>\n\
           <key>ProcessType</key>\n\
           <string>Interactive</string>\n\
           <key>AssociatedBundleIdentifiers</key>\n\
           <array>\n\
             <string>dev.wakezilla.Wakezilla</string>\n\
           </array>\n\
         </dict>\n\
         </plist>\n"
    ))
}

#[cfg(target_os = "macos")]
fn ensure_macos_profile_directory(
    home: &Path,
    directory: &Path,
    effective_uid: libc::uid_t,
) -> Result<()> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                anyhow::bail!("unsafe macOS profile directory: {}", directory.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(directory)
                .with_context(|| format!("failed to create {}", directory.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", directory.display()));
        }
    }
    let canonical = directory
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", directory.display()))?;
    if !canonical.starts_with(home) || canonical == home {
        anyhow::bail!(
            "macOS profile directory escaped HOME: {}",
            directory.display()
        );
    }
    let owner = std::fs::metadata(&canonical)
        .with_context(|| format!("failed to inspect {}", canonical.display()))?
        .uid();
    if owner != effective_uid {
        anyhow::bail!(
            "macOS profile directory is not owned by the effective user: {}",
            directory.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
struct MacosAtomicTemp(PathBuf);

#[cfg(target_os = "macos")]
impl Drop for MacosAtomicTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(target_os = "macos")]
fn atomic_write_macos_launch_agent(path: &Path, content: &[u8]) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                anyhow::bail!(
                    "refusing unsafe LaunchAgent destination: {}",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    let directory = path.parent().context("LaunchAgent path has no parent")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("LaunchAgent file name must be valid UTF-8")?;
    let mut temporary = None;
    let mut file = None;
    for attempt in 0..100_u32 {
        let candidate =
            directory.join(format!(".{file_name}.tmp.{}.{attempt}", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&candidate) {
            Ok(opened) => {
                temporary = Some(MacosAtomicTemp(candidate));
                file = Some(opened);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to stage {}", path.display()));
            }
        }
    }
    let mut file = file.context("failed to allocate a unique LaunchAgent staging file")?;
    let mut temporary = temporary.context("LaunchAgent staging path was not recorded")?;
    file.write_all(content)
        .with_context(|| format!("failed to stage {}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o644))
        .with_context(|| format!("failed to set mode on staged {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync staged {}", path.display()))?;
    drop(file);
    std::fs::rename(&temporary.0, path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    temporary.0 = PathBuf::new();
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_tray_autostart() -> Result<std::path::PathBuf> {
    let (exe, args) = wakezilla_tray_command()?;
    let command = windows_command(&exe, &args);
    let status = Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "WakezillaTray",
            "/t",
            "REG_SZ",
            "/d",
        ])
        .arg(&command)
        .arg("/f")
        .status()
        .context("failed to invoke reg.exe")?;
    if !status.success() {
        anyhow::bail!("reg.exe failed to install tray autostart with status {status}");
    }
    Ok(exe)
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn desktop_entry_quote(value: &str) -> Result<String> {
    reject_desktop_controls(value)?;
    let exec_layer = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    Ok(format!("\"{}\"", exec_layer.replace('\\', "\\\\")))
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn desktop_string_escape(value: &str) -> Result<String> {
    reject_desktop_controls(value)?;
    Ok(value.replace('\\', "\\\\"))
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn reject_desktop_controls(value: &str) -> Result<()> {
    if value.chars().any(|character| character.is_ascii_control()) {
        anyhow::bail!("desktop entry value contains an ASCII control character");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> Result<String> {
    if value.chars().any(|character| character.is_ascii_control()) {
        anyhow::bail!("LaunchAgent value contains an ASCII control character");
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

#[cfg(target_os = "windows")]
fn windows_command(exe: &Path, args: &[&str]) -> String {
    std::iter::once(format!("\"{}\"", exe.display()))
        .chain(args.iter().map(|arg| format!("\"{arg}\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_command(elevated: bool, exe: &Path, args: &[&str], keep_open: bool) -> Result<()> {
    let mut parts = Vec::with_capacity(args.len() + 2);
    if elevated {
        parts.push("sudo".to_string());
    }
    parts.push(exe.to_string_lossy().into_owned());
    parts.extend(args.iter().map(|arg| (*arg).to_string()));

    #[cfg(target_os = "linux")]
    {
        open_linux_terminal(&parts, keep_open)
    }
    #[cfg(target_os = "macos")]
    {
        open_macos_terminal(&parts, keep_open)
    }
}

#[cfg(target_os = "linux")]
fn open_linux_terminal(parts: &[String], keep_open: bool) -> Result<()> {
    let script = shell_script(parts, keep_open);
    let candidates: [(&str, Vec<&str>); 5] = [
        ("x-terminal-emulator", vec!["-e", "sh", "-lc", &script]),
        ("gnome-terminal", vec!["--", "sh", "-lc", &script]),
        ("konsole", vec!["-e", "sh", "-lc", &script]),
        ("xfce4-terminal", vec!["-e", &script]),
        ("xterm", vec!["-e", "sh", "-lc", &script]),
    ];

    for (program, args) in candidates {
        if Command::new(program).args(args).spawn().is_ok() {
            return Ok(());
        }
    }

    Err(anyhow!(
        "no supported terminal emulator found (tried x-terminal-emulator, gnome-terminal, konsole, xfce4-terminal, xterm)"
    ))
}

#[cfg(target_os = "macos")]
fn open_macos_terminal(parts: &[String], keep_open: bool) -> Result<()> {
    let script = shell_script(parts, keep_open);
    let script = script.replace('\\', "\\\\").replace('"', "\\\"");
    let apple_script = format!("tell application \"Terminal\" to do script \"{script}\"");

    Command::new("osascript")
        .args(["-e", &apple_script])
        .spawn()
        .context("failed to open macOS Terminal")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_command(elevated: bool, exe: &Path, args: &[&str], keep_open: bool) -> Result<()> {
    let ps_command = powershell_invocation(exe, args);
    let encoded_command = powershell_encoded_command(&ps_command);
    let mut powershell_args = vec!["-NoProfile", "-ExecutionPolicy", "Bypass"];
    if keep_open {
        powershell_args.push("-NoExit");
    }
    powershell_args.push("-EncodedCommand");
    powershell_args.push(&encoded_command);
    let argument_list = powershell_array_literal(&powershell_args);

    if elevated {
        let script = format!(
            "Start-Process -FilePath powershell -Verb RunAs -ArgumentList @({argument_list})"
        );
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]);
        command.arg(&script);
        command
            .spawn()
            .context("failed to open elevated PowerShell")?;
    } else {
        let script = format!("Start-Process -FilePath powershell -ArgumentList @({argument_list})");
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]);
        command
            .arg(&script)
            .spawn()
            .context("failed to open PowerShell")?;
    }

    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn open_command(_elevated: bool, _exe: &Path, _args: &[&str], _keep_open: bool) -> Result<()> {
    Err(anyhow!("tray commands are not supported on this OS"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn shell_script(parts: &[String], keep_open: bool) -> String {
    let command = parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");

    if keep_open {
        format!("{command}; echo; printf 'Press Enter to close...'; read _")
    } else {
        command
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "windows")]
fn powershell_invocation(exe: &Path, args: &[&str]) -> String {
    let invocation = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .map(|part| powershell_quote(&part))
        .collect::<Vec<_>>()
        .join(" ");
    format!("& {invocation}")
}

#[cfg(target_os = "windows")]
fn powershell_encoded_command(command: &str) -> String {
    let bytes: Vec<u8> = command
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    BASE64_STANDARD.encode(bytes)
}

#[cfg(target_os = "windows")]
fn powershell_array_literal(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| powershell_quote(value))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(target_os = "windows")]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_white_mascot_keeps_only_interior_dark_lines() {
        let mut rgba = [140, 90, 60, 255].repeat(25);
        rgba[12 * 4..13 * 4].copy_from_slice(&[60, 60, 60, 255]);

        macos_white_mascot_rgba(&mut rgba, 5, 5);

        let mut expected = vec![255; 25 * 4];
        expected[12 * 4..13 * 4].copy_from_slice(&[0, 0, 0, 255]);
        assert_eq!(rgba, expected);
    }

    #[test]
    fn tray_instance_rejects_a_second_guard() {
        const CHILD_ENV: &str = "WAKEZILLA_TEST_TRAY_INSTANCE_CHILD";

        if let Some(name) = std::env::var_os(CHILD_ENV) {
            let name = name.to_string_lossy();
            assert!(TrayInstanceGuard::acquire_named(&name)
                .expect("child acquire")
                .is_none());
            return;
        }

        let name = format!("dev.wakezilla.tray.test.{}", std::process::id());
        let first = TrayInstanceGuard::acquire_named(&name)
            .expect("first acquire")
            .expect("first instance");

        assert!(TrayInstanceGuard::acquire_named(&name)
            .expect("second acquire")
            .is_none());

        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tray::desktop::tests::tray_instance_rejects_a_second_guard",
            ])
            .env(CHILD_ENV, &name)
            .status()
            .expect("run child test process");
        assert!(child.success(), "child should observe the held guard");

        drop(first);

        let reacquired = TrayInstanceGuard::acquire_named(&name)
            .expect("acquire after drop")
            .expect("instance after drop");
        drop(reacquired);

        #[cfg(target_os = "macos")]
        std::fs::remove_file(macos_lock_path(&name).expect("test lock path"))
            .expect("remove test lock file");
    }

    #[test]
    fn tray_instance_linux_socket_type_combines_cloexec_atomically() {
        assert_eq!(combine_linux_socket_type(0b0001, 0b1000), 0b1001);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tray_instance_linux_backend_name_is_scoped_by_euid() {
        let first_user = linux_backend_name(TRAY_INSTANCE_NAME, 1000);
        let second_user = linux_backend_name(TRAY_INSTANCE_NAME, 1001);

        assert_eq!(first_user, "dev.wakezilla.tray.uid-1000");
        assert_ne!(first_user, second_user);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tray_instance_linux_socket_does_not_survive_exec() {
        use std::io::{BufRead as _, Read as _, Write as _};
        use std::process::{Child, Stdio};

        const CHILD_ENV: &str = "WAKEZILLA_TEST_TRAY_INSTANCE_CLOEXEC_CHILD";
        const READY_MARKER: &str = "WAKEZILLA_TRAY_INSTANCE_CHILD_READY";

        if std::env::var_os(CHILD_ENV).is_some() {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{READY_MARKER}").expect("write child ready marker");
            stdout.flush().expect("flush child ready marker");
            drop(stdout);

            let mut release = [0_u8; 1];
            std::io::stdin()
                .read_exact(&mut release)
                .expect("wait for parent release");
            return;
        }

        struct ChildGuard(Option<Child>);

        impl ChildGuard {
            fn child_mut(&mut self) -> &mut Child {
                self.0.as_mut().expect("child process")
            }

            fn wait(mut self) -> std::io::Result<std::process::ExitStatus> {
                let result = self.0.as_mut().expect("child process").wait();
                if result.is_ok() {
                    self.0.take();
                }
                result
            }
        }

        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if let Some(mut child) = self.0.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }

        let name = format!("dev.wakezilla.tray.cloexec.test.{}", std::process::id());
        let first = TrayInstanceGuard::acquire_named(&name)
            .expect("first acquire")
            .expect("first instance");
        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tray::desktop::tests::tray_instance_linux_socket_does_not_survive_exec",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn child test process");
        let mut child = ChildGuard(Some(child));
        let stdout = child.child_mut().stdout.take().expect("child stdout pipe");
        let mut stdout = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(stdout.read_line(&mut line).expect("read child output"), 0);
            if line.contains(READY_MARKER) {
                break;
            }
        }

        drop(first);
        let reacquired = TrayInstanceGuard::acquire_named(&name)
            .expect("reacquire while child lives")
            .expect("socket fd must close during exec");
        drop(reacquired);

        child
            .child_mut()
            .stdin
            .as_mut()
            .expect("child stdin pipe")
            .write_all(b"x")
            .expect("release child");
        let status = child.wait().expect("wait for child test process");
        assert!(status.success(), "child test process should exit cleanly");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tray_instance_flock_success_is_acquired() {
        assert_eq!(
            classify_macos_flock_result(0, 0).expect("successful flock"),
            MacosFlockOutcome::Acquired
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tray_instance_flock_would_block_is_duplicate() {
        assert_eq!(
            classify_macos_flock_result(-1, libc::EWOULDBLOCK).expect("contended flock"),
            MacosFlockOutcome::Contended
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tray_instance_flock_other_errno_is_error() {
        let error = classify_macos_flock_result(-1, libc::EINVAL)
            .expect_err("unexpected flock errno must fail closed");

        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_autostart_uses_canonical_bundle_and_native_open_contract() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temporary macOS home parent");
        let home = temp.path().join("Home & <Primary>");
        let executable = home.join("Applications/Wakezilla.app/Contents/MacOS/wakezilla-tray");
        std::fs::create_dir_all(executable.parent().expect("bundle executable parent"))
            .expect("create bundle fixture");
        std::fs::write(&executable, b"tray executable fixture")
            .expect("write bundle executable fixture");

        let installed = install_macos_tray_autostart_at(&home, &executable)
            .expect("install native macOS LaunchAgent");
        let canonical_home = home.canonicalize().expect("canonical fixture HOME");
        assert_eq!(
            installed,
            canonical_home.join("Library/LaunchAgents/dev.wakezilla.tray.plist")
        );
        let bundle = executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("bundle fixture path")
            .canonicalize()
            .expect("canonical bundle fixture");
        let content = std::fs::read_to_string(&installed).expect("read LaunchAgent");
        for required in [
            "<key>Label</key>",
            "<string>dev.wakezilla.tray</string>",
            "<key>ProgramArguments</key>",
            "<string>/usr/bin/open</string>",
            "<string>-g</string>",
            "<key>RunAtLoad</key>\n<true/>",
            "<key>LimitLoadToSessionType</key>",
            "<string>Aqua</string>",
            "<key>ProcessType</key>",
            "<string>Interactive</string>",
            "<key>AssociatedBundleIdentifiers</key>\n<array>\n<string>dev.wakezilla.Wakezilla</string>\n</array>",
        ] {
            assert!(content.contains(required), "missing contract: {required}");
        }
        assert!(content.contains(
            &xml_escape(bundle.to_str().expect("UTF-8 bundle fixture"))
                .expect("escape bundle path")
        ));
        assert!(!content.contains("KeepAlive"));
        assert!(!content.contains("Terminal"));
        assert!(!content.contains("wakezilla-tray</string>"));
        assert!(!content.contains("/bin/sh"));
        assert_eq!(
            std::fs::metadata(&installed)
                .expect("LaunchAgent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        let lint = std::process::Command::new("/usr/bin/plutil")
            .args(["-lint"])
            .arg(&installed)
            .status()
            .expect("run real plutil against runtime LaunchAgent");
        assert!(
            lint.success(),
            "real plutil must accept runtime LaunchAgent"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_autostart_rejects_nonbundle_and_control_paths_before_publish() {
        let temp = tempfile::tempdir().expect("temporary macOS fixture");
        let home = temp.path().join("home");
        std::fs::create_dir(&home).expect("create fixture HOME");
        let loose_helper = temp.path().join("wakezilla-tray");
        std::fs::write(&loose_helper, b"loose helper").expect("write loose helper");

        assert!(install_macos_tray_autostart_at(&home, &loose_helper).is_err());
        assert!(!home.join("Library").exists());
        assert!(
            install_macos_tray_autostart_at(Path::new("relative-home"), &loose_helper).is_err()
        );
        assert!(xml_escape("bad\npath").is_err());
        assert!(xml_escape("bad\u{1b}path").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_autostart_rejects_root_and_wrong_owner_before_publish() {
        let temp = tempfile::tempdir().expect("temporary macOS fixture");
        let home = temp.path().join("home");
        let executable = home.join("Applications/Wakezilla.app/Contents/MacOS/wakezilla-tray");
        std::fs::create_dir_all(executable.parent().expect("bundle executable parent"))
            .expect("create bundle fixture");
        std::fs::write(&executable, b"tray executable fixture")
            .expect("write bundle executable fixture");
        let owner = std::fs::metadata(&home).expect("HOME metadata").uid();

        assert!(install_macos_tray_autostart_for_uid(&home, &executable, 0).is_err());
        assert!(!home.join("Library").exists());
        assert!(install_macos_tray_autostart_for_uid(&home, &executable, owner + 1).is_err());
        assert!(!home.join("Library").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_autostart_replaces_atomically_and_rejects_symlink_destination() {
        let temp = tempfile::tempdir().expect("temporary macOS fixture");
        let home = temp.path().join("home");
        let executable = home.join("Applications/Wakezilla.app/Contents/MacOS/wakezilla-tray");
        std::fs::create_dir_all(executable.parent().expect("bundle executable parent"))
            .expect("create bundle fixture");
        std::fs::write(&executable, b"tray executable fixture")
            .expect("write bundle executable fixture");
        let launch_agents = home.join("Library/LaunchAgents");
        std::fs::create_dir_all(&launch_agents).expect("create LaunchAgents fixture");
        let destination = launch_agents.join("dev.wakezilla.tray.plist");
        std::fs::write(&destination, b"old contents").expect("write prior LaunchAgent");

        install_macos_tray_autostart_at(&home, &executable)
            .expect("atomically replace LaunchAgent");
        let temporary_count = std::fs::read_dir(&launch_agents)
            .expect("read LaunchAgents")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(temporary_count, 0);

        std::fs::remove_file(&destination).expect("remove installed LaunchAgent");
        let foreign = temp.path().join("foreign-agent");
        std::fs::write(&foreign, b"foreign contents").expect("write foreign agent");
        std::os::unix::fs::symlink(&foreign, &destination)
            .expect("create LaunchAgent symlink fixture");
        assert!(install_macos_tray_autostart_at(&home, &executable).is_err());
        assert_eq!(
            std::fs::read(&foreign).expect("read preserved foreign agent"),
            b"foreign contents"
        );
    }

    #[test]
    fn dashboard_url_uses_proxy_port_from_config() {
        let mut config = config::Config::default();
        config.server.proxy_port = 4567;

        assert_eq!(dashboard_url(&config), "http://127.0.0.1:4567");
    }

    #[test]
    fn mode_labels_match_menu_text() {
        assert_eq!(mode_label(service::Mode::Proxy), "Proxy");
        assert_eq!(mode_label(service::Mode::Client), "Client");
    }

    #[test]
    fn service_status_labels_match_state() {
        assert_eq!(
            service_status_label(ModeStatus {
                installed: false,
                running: false
            }),
            "not installed"
        );
        assert_eq!(
            service_status_label(ModeStatus {
                installed: true,
                running: false
            }),
            "stopped"
        );
        assert_eq!(
            service_status_label(ModeStatus {
                installed: true,
                running: true
            }),
            "running"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn shell_quote_wraps_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn desktop_entry_quote_escapes_quotes() {
        assert_eq!(
            desktop_entry_quote("/tmp/a\"b%20").expect("quote desktop entry"),
            "\"/tmp/a\\\\\"b%%20\""
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn desktop_entry_quote_rejects_ascii_controls() {
        assert!(desktop_entry_quote("/tmp/bad\npath").is_err());
        assert!(desktop_entry_quote("/tmp/bad\u{1b}path").is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_autostart_is_canonical_atomic_and_removes_owned_legacy() {
        let temp = tempfile::tempdir().expect("temporary config home");
        let helper = temp.path().join("bin with spaces/wakezilla-tray");
        std::fs::create_dir_all(helper.parent().expect("helper parent"))
            .expect("create helper parent");
        std::fs::write(&helper, b"helper").expect("write helper fixture");
        let autostart = temp.path().join("autostart");
        std::fs::create_dir_all(&autostart).expect("create autostart fixture");
        let legacy = autostart.join("wakezilla-tray.desktop");
        std::fs::write(
            &legacy,
            b"[Desktop Entry]\nType=Application\nName=Wakezilla Tray\nExec=/old/wakezilla-tray\n",
        )
        .expect("write owned legacy entry");
        let canonical = autostart.join("dev.wakezilla.tray.desktop");
        std::fs::write(&canonical, b"old canonical contents").expect("write old canonical entry");

        let installed = install_linux_tray_autostart_at(temp.path(), &helper)
            .expect("install canonical Linux autostart");

        assert_eq!(installed, canonical);
        assert!(!legacy.exists(), "owned legacy entry must be removed");
        let content = std::fs::read_to_string(&installed).expect("read canonical entry");
        assert!(content.contains("Name=Wakezilla"));
        assert!(content.contains(&format!("Exec=\"{}\"", helper.display())));
        assert!(!content.contains(" wakezilla tray"));
        assert!(!content.contains("Version=0.1"));
        let temporary_entries = std::fs::read_dir(&autostart)
            .expect("read autostart directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(temporary_entries, 0);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_autostart_preserves_foreign_legacy_named_entry() {
        let temp = tempfile::tempdir().expect("temporary config home");
        let helper = temp.path().join("wakezilla-tray");
        std::fs::write(&helper, b"helper").expect("write helper fixture");
        let autostart = temp.path().join("autostart");
        std::fs::create_dir_all(&autostart).expect("create autostart fixture");
        let legacy = autostart.join("wakezilla-tray.desktop");
        let foreign = "[Other Group]\nName=Wakezilla Tray\nExec=/old/wakezilla-tray\n\
                       [Desktop Entry]\nType=Application\nName=Another App\nExec=/other/app\n";
        std::fs::write(&legacy, foreign).expect("write foreign legacy entry");

        install_linux_tray_autostart_at(temp.path(), &helper)
            .expect("install canonical Linux autostart");

        assert_eq!(
            std::fs::read_to_string(&legacy).expect("read preserved legacy entry"),
            foreign
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_legacy_autostart_matcher_requires_one_exact_owned_group() {
        assert!(linux_legacy_autostart_is_owned(
            "[Desktop Entry]\nType=Application\nName=Wakezilla Tray\nExec=/old/bin/wakezilla-tray\n"
        ));
        assert!(linux_legacy_autostart_is_owned(
            "[Desktop Entry]\nType=Application\nName=Wakezilla\nExec=\"/old/bin/wakezilla\" tray\n"
        ));
        assert!(!linux_legacy_autostart_is_owned(
            "[Desktop Entry]\nType=Application\nName=Wakezilla Tray\nExec=/other/not-wakezilla-tray-helper\n"
        ));
        assert!(!linux_legacy_autostart_is_owned(
            "[Desktop Entry]\nType=Application\nName=Wakezilla Tray\n[Desktop Entry]\nExec=/old/bin/wakezilla-tray\n"
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_autostart_directory_is_private_without_chmodding_existing_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temporary config home");
        let new_config = temp.path().join("new-config");
        let helper = temp.path().join("wakezilla-tray");
        std::fs::write(&helper, b"helper").expect("write helper fixture");
        install_linux_tray_autostart_at(&new_config, &helper)
            .expect("install into new config home");
        let new_mode = std::fs::metadata(new_config.join("autostart"))
            .expect("new autostart metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(new_mode, 0o700);

        let existing_config = temp.path().join("existing-config");
        let existing_autostart = existing_config.join("autostart");
        std::fs::create_dir_all(&existing_autostart).expect("create existing autostart");
        std::fs::set_permissions(&existing_autostart, std::fs::Permissions::from_mode(0o755))
            .expect("set existing directory mode");
        install_linux_tray_autostart_at(&existing_config, &helper)
            .expect("install into existing config home");
        let existing_mode = std::fs::metadata(&existing_autostart)
            .expect("existing autostart metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(existing_mode, 0o755);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_config_home_requires_an_absolute_path_with_home_fallback() {
        use std::ffi::OsStr;

        assert_eq!(
            resolve_linux_config_home(Some(OsStr::new("/xdg/config")), Some(OsStr::new("/home/u")))
                .expect("absolute XDG config home"),
            PathBuf::from("/xdg/config")
        );
        for invalid_xdg in ["", "relative/config"] {
            assert_eq!(
                resolve_linux_config_home(
                    Some(OsStr::new(invalid_xdg)),
                    Some(OsStr::new("/home/u"))
                )
                .expect("absolute HOME fallback"),
                PathBuf::from("/home/u/.config")
            );
        }
        assert!(resolve_linux_config_home(None, Some(OsStr::new("relative-home"))).is_err());
        assert!(resolve_linux_config_home(None, None).is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_autostart_rejects_non_utf8_helper_before_publish() {
        use std::os::unix::ffi::OsStringExt as _;

        let temp = tempfile::tempdir().expect("temporary config home");
        let helper = temp.path().join(std::ffi::OsString::from_vec(vec![
            b'w', b'a', b'k', b'e', b'z', b'i', b'l', b'l', b'a', b'-', 0xff,
        ]));

        assert!(install_linux_tray_autostart_at(temp.path(), &helper).is_err());
        assert!(!temp
            .path()
            .join("autostart/dev.wakezilla.tray.desktop")
            .exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_autostart_preserves_non_utf8_legacy_as_foreign() {
        let temp = tempfile::tempdir().expect("temporary config home");
        let helper = temp.path().join("wakezilla-tray");
        std::fs::write(&helper, b"helper").expect("write helper fixture");
        let autostart = temp.path().join("autostart");
        std::fs::create_dir_all(&autostart).expect("create autostart fixture");
        let legacy = autostart.join("wakezilla-tray.desktop");
        let foreign = b"[Desktop Entry]\nType=Application\nName=Wakezilla Tray\nExec=/old/wakezilla-tray\n\xff";
        std::fs::write(&legacy, foreign).expect("write non-UTF-8 legacy entry");

        let canonical = install_linux_tray_autostart_at(temp.path(), &helper)
            .expect("install with foreign non-UTF-8 legacy");

        assert!(canonical.is_file());
        assert_eq!(
            std::fs::read(&legacy).expect("read preserved legacy"),
            foreign
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_autostart_preserves_unreadable_legacy_as_foreign() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temporary config home");
        let helper = temp.path().join("wakezilla-tray");
        std::fs::write(&helper, b"helper").expect("write helper fixture");
        let autostart = temp.path().join("autostart");
        std::fs::create_dir_all(&autostart).expect("create autostart fixture");
        let legacy = autostart.join("wakezilla-tray.desktop");
        std::fs::write(
            &legacy,
            b"[Desktop Entry]\nType=Application\nName=Wakezilla Tray\nExec=/old/wakezilla-tray\n",
        )
        .expect("write legacy entry");
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o000))
            .expect("make legacy unreadable");
        if std::fs::read(&legacy).is_ok() {
            return;
        }

        let canonical = install_linux_tray_autostart_at(temp.path(), &helper)
            .expect("install with unreadable foreign legacy");

        assert!(canonical.is_file());
        assert!(legacy.exists());
    }
}
