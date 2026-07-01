use crate::{config, service, update};
use anyhow::{anyhow, Context, Result};
#[cfg(target_os = "windows")]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::io::Cursor;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    Icon, TrayIcon, TrayIconBuilder,
};
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::WindowId,
};

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
    let config = config::Config::load();
    let dashboard_url = dashboard_url(&config);

    let event_loop = EventLoop::<UserEvent>::with_user_event()
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

        let tray_icon = TrayIconBuilder::new()
            .with_tooltip("Wakezilla")
            .with_icon(icon)
            .with_menu(Box::new(root_menu))
            .with_menu_on_left_click(true)
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

    Icon::from_rgba(rgba, frame.width, frame.height).context("failed to create tray icon")
}

fn rgba_from_png_frame(bytes: &[u8], color_type: png::ColorType) -> Result<Vec<u8>> {
    match color_type {
        png::ColorType::Rgba => Ok(bytes.to_vec()),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(bytes.len() / 3 * 4);
            for chunk in bytes.chunks_exact(3) {
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
            for chunk in bytes.chunks_exact(2) {
                rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            Ok(rgba)
        }
        png::ColorType::Indexed => Err(anyhow!("indexed tray icon was not expanded to RGBA")),
    }
}

fn open_wakezilla_command(elevated: bool, args: &[&str], keep_open: bool) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve wakezilla executable")?;
    open_command(elevated, &exe, args, keep_open)
}

#[cfg(target_os = "linux")]
fn install_tray_autostart() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve wakezilla executable")?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
        .context("HOME or XDG_CONFIG_HOME is required to install tray autostart")?;
    let autostart_dir = config_home.join("autostart");
    std::fs::create_dir_all(&autostart_dir)
        .with_context(|| format!("failed to create {}", autostart_dir.display()))?;
    let path = autostart_dir.join("wakezilla-tray.desktop");
    let content = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Wakezilla Tray\n\
         Exec={} tray\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        desktop_entry_quote(&exe.to_string_lossy()),
    );
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn install_tray_autostart() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve wakezilla executable")?;
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .context("HOME is required to install tray autostart")?;
    let launch_agents = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&launch_agents)
        .with_context(|| format!("failed to create {}", launch_agents.display()))?;
    let path = launch_agents.join("dev.wakezilla.tray.plist");
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>dev.wakezilla.tray</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{}</string>\n\
         \t\t<string>tray</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n",
        xml_escape(&exe.to_string_lossy()),
    );
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

#[cfg(target_os = "windows")]
fn install_tray_autostart() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve wakezilla executable")?;
    let command = format!("\"{}\" tray", exe.display());
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

#[cfg(target_os = "linux")]
fn desktop_entry_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_entry_quote_escapes_quotes() {
        assert_eq!(desktop_entry_quote("/tmp/a\"b%20"), "\"/tmp/a\\\"b%%20\"");
    }
}
