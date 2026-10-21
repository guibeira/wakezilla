//! Interactive `setup` wizard: configure and install a Wakezilla system service.

use anyhow::{Context, Result};
use clap::Parser;

use crate::config::{self, Config};
use crate::service::{self, Mode};

/// CLI arguments for the `setup` subcommand.
#[derive(Parser, Default)]
#[command()]
pub struct SetupArgs {
    /// Pre-select the mode ("proxy" or "client"); skips the TUI prompt if combined with --port.
    #[arg(long, help_heading = "Setup Options")]
    pub mode: Option<String>,

    /// Pre-select the port; skips the TUI prompt if combined with --mode.
    #[arg(long, help_heading = "Setup Options")]
    pub port: Option<u16>,

    /// HMAC key used to authenticate shutdown requests (client mode only).
    #[arg(long, value_name = "KEY", help_heading = "Setup Options")]
    pub key: Option<String>,

    /// Skip the overwrite confirmation prompt (for non-interactive use).
    #[arg(long, short = 'y', help_heading = "Setup Options")]
    pub yes: bool,
}

impl std::fmt::Debug for SetupArgs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SetupArgs")
            .field("mode", &self.mode)
            .field("port", &self.port)
            .field("key", &self.key.as_ref().map(|_| "[REDACTED]"))
            .field("yes", &self.yes)
            .finish()
    }
}

/// Action for the `service` subcommand.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    /// Report whether the service is running.
    Status,
    /// Show the service's logs (prints status first).
    Logs,
}

/// CLI arguments for the `service` subcommand.
#[derive(Parser, Debug)]
#[command()]
pub struct ServiceArgs {
    /// Action to perform on the installed service.
    #[arg(value_enum)]
    pub action: ServiceAction,

    /// Target server ("proxy" or "client"); skips the TUI prompt.
    #[arg(long, help_heading = "Service Options")]
    pub mode: Option<String>,

    /// For `logs`: keep streaming new output until interrupted.
    #[arg(long, short = 'f', help_heading = "Service Options")]
    pub follow: bool,

    /// For `logs`: number of trailing lines to show (default 50).
    #[arg(long, short = 'n', help_heading = "Service Options")]
    pub lines: Option<u32>,
}

/// Build a Config with the chosen port placed in the correct field for `mode`.
pub fn build_config(mode: Mode, port: u16) -> Config {
    let mut cfg = Config::default();
    match mode {
        Mode::Proxy => cfg.server.proxy_port = port,
        Mode::Client => cfg.server.client_port = port,
    }
    cfg.storage.machines_db_path = config::data_path(config::DEFAULT_MACHINES_DB_PATH)
        .to_string_lossy()
        .into_owned();
    cfg.storage.access_history_path = config::data_path(config::DEFAULT_ACCESS_HISTORY_PATH)
        .to_string_lossy()
        .into_owned();
    cfg
}

/// Write the config file, install the service, and validate it.
/// Returns the config path on success.
///
/// If a config file already exists, its other settings (e.g. the other server's
/// port) are preserved; only the target mode's port is updated.
pub fn apply_with_key(
    mode: Mode,
    port: u16,
    client_key: Option<&str>,
) -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;

    let path = config::config_path();
    let mut cfg = if path.exists() {
        Config::load_from(&path).unwrap_or_else(|_| build_config(mode, port))
    } else {
        build_config(mode, port)
    };
    match mode {
        Mode::Proxy => cfg.server.proxy_port = port,
        Mode::Client => {
            cfg.server.client_port = port;
            if let Some(key) = client_key {
                crate::shutdown_auth::validate_key(key).context("invalid client shutdown key")?;
                cfg.security.client_shutdown_key = Some(key.to_string());
            }
        }
    }
    if cfg.storage.machines_db_path == config::DEFAULT_MACHINES_DB_PATH {
        cfg.storage.machines_db_path = config::data_path(config::DEFAULT_MACHINES_DB_PATH)
            .to_string_lossy()
            .into_owned();
    }
    if cfg.storage.access_history_path == config::DEFAULT_ACCESS_HISTORY_PATH {
        cfg.storage.access_history_path = config::data_path(config::DEFAULT_ACCESS_HISTORY_PATH)
            .to_string_lossy()
            .into_owned();
    }
    cfg.save_to(&path)
        .with_context(|| format!("failed to write config to {}", path.display()))?;

    service::install(mode, &exe, port).context("failed to install system service")?;

    Ok(path)
}

/// Summarize any existing Wakezilla configuration/services on this host.
/// Returns `None` if nothing is installed and no config file exists.
fn existing_summary() -> Option<String> {
    let installed = service::installed_modes();
    let path = config::config_path();
    let cfg_exists = path.exists();
    if installed.is_empty() && !cfg_exists {
        return None;
    }

    let mut lines = Vec::new();
    if cfg_exists {
        if let Ok(cfg) = Config::load_from(&path) {
            lines.push(format!(
                "  config: {} (proxy_port={}, client_port={})",
                path.display(),
                cfg.server.proxy_port,
                cfg.server.client_port
            ));
        } else {
            lines.push(format!("  config: {}", path.display()));
        }
    }
    if !installed.is_empty() {
        let names: Vec<&str> = installed.iter().map(|m| m.subcommand()).collect();
        lines.push(format!("  installed services: {}", names.join(", ")));
    }
    Some(lines.join("\n"))
}

/// Prompt the operator to confirm overwriting an existing configuration.
/// Returns `Ok(true)` when there is nothing to overwrite or the user confirms.
fn confirm_overwrite() -> Result<bool> {
    let Some(summary) = existing_summary() else {
        return Ok(true);
    };

    println!("An existing Wakezilla configuration was detected:");
    println!("{summary}");
    print!("Overwrite / reconfigure? [y/N]: ");
    std::io::stdout().flush().ok();

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("failed to read confirmation")?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

use std::io;
use std::io::Write;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

/// Wizard entry point. Requires elevation. Falls back to the TUI when the mode/port
/// are not both provided on the command line.
pub fn run(args: SetupArgs) -> Result<()> {
    if !service::is_elevated() {
        eprintln!(
            "wakezilla setup must run with administrator privileges.\n\
             Re-run with: sudo wakezilla setup   (Linux/macOS)  or  an elevated shell (Windows)."
        );
        std::process::exit(1);
    }

    let skip_confirm = args.yes;
    let client_key = args.key.clone();

    // Headless path: both flags provided.
    if let (Some(mode_str), Some(port)) = (&args.mode, args.port) {
        let mode = Mode::from_str_opt(mode_str)
            .with_context(|| format!("invalid --mode '{mode_str}' (use 'proxy' or 'client')"))?;
        if !skip_confirm && !confirm_overwrite()? {
            println!("Aborted; no changes made.");
            return Ok(());
        }
        if args.key.is_some() && mode != Mode::Client {
            anyhow::bail!("--key is only valid with --mode client");
        }
        let path = apply_with_key(mode, port, args.key.as_deref())?;
        println!(
            "Configured {} on port {port}. Config written to {}.",
            mode.subcommand(),
            path.display()
        );
        return Ok(());
    }

    let (mode, port) = run_wizard(args)?;
    if !skip_confirm && !confirm_overwrite()? {
        println!("Aborted; no changes made.");
        return Ok(());
    }
    if client_key.is_some() && mode != Mode::Client {
        anyhow::bail!("--key is only valid with --mode client");
    }
    let path = apply_with_key(mode, port, client_key.as_deref())?;
    println!(
        "Configured {} on port {port}. Config written to {}.",
        mode.subcommand(),
        path.display()
    );
    Ok(())
}

/// Entry point for the `uninstall` subcommand. Removes Wakezilla services and
/// autostart hooks created by `setup`, but preserves config and data files.
pub fn run_uninstall() -> Result<()> {
    if !service::is_elevated() {
        eprintln!(
            "wakezilla uninstall must run with administrator privileges.\n\
             Re-run with: sudo wakezilla uninstall   (Linux/macOS)  or  an elevated shell (Windows)."
        );
        std::process::exit(1);
    }

    let removed = service::uninstall_all().context("failed to uninstall Wakezilla services")?;
    if removed.is_empty() {
        println!("No Wakezilla service was installed.");
    } else {
        for mode in removed {
            println!("Removed {} service.", mode.subcommand());
        }
    }
    println!("Wakezilla configuration, data, and logs were left in place.");
    Ok(())
}

#[derive(PartialEq)]
enum Step {
    ModeSelect,
    PortInput,
    Confirm,
}

struct Wizard {
    step: Step,
    mode: Mode,
    port_input: String,
    error: Option<String>,
}

impl Wizard {
    fn new(args: &SetupArgs) -> Self {
        let mode = args
            .mode
            .as_deref()
            .and_then(Mode::from_str_opt)
            .unwrap_or(Mode::Proxy);
        Wizard {
            step: Step::ModeSelect,
            mode,
            port_input: args
                .port
                .map(|p| p.to_string())
                .unwrap_or_else(|| mode.default_port().to_string()),
            error: None,
        }
    }
}

/// Run the interactive wizard, returning the chosen (mode, port).
fn run_wizard(args: SetupArgs) -> Result<(Mode, u16)> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let result = wizard_loop(&mut terminal, args);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn wizard_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    args: SetupArgs,
) -> Result<(Mode, u16)>
where
    B::Error: Send + Sync + 'static,
{
    let mut w = Wizard::new(&args);

    loop {
        terminal.draw(|f| draw_wizard(f, &w))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if !is_action_key_event(&key) {
                continue;
            }

            // Ctrl-C / Esc aborts.
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return Err(anyhow::anyhow!("setup cancelled"));
            }

            match w.step {
                Step::ModeSelect => match key.code {
                    KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                        let mode = match w.mode {
                            Mode::Proxy => Mode::Client,
                            Mode::Client => Mode::Proxy,
                        };
                        set_wizard_mode(&mut w, mode);
                    }
                    KeyCode::Char('1') | KeyCode::Char('p') | KeyCode::Char('P') => {
                        set_wizard_mode(&mut w, Mode::Proxy);
                    }
                    KeyCode::Char('2') | KeyCode::Char('c') | KeyCode::Char('C') => {
                        set_wizard_mode(&mut w, Mode::Client);
                    }
                    KeyCode::Enter => w.step = Step::PortInput,
                    _ => {}
                },
                Step::PortInput => match key.code {
                    KeyCode::Char(c) if c.is_ascii_digit() && w.port_input.len() < 5 => {
                        w.port_input.push(c);
                    }
                    KeyCode::Backspace => {
                        w.port_input.pop();
                    }
                    KeyCode::Enter => match w.port_input.parse::<u16>() {
                        Ok(p) if p > 0 => {
                            w.error = None;
                            w.step = Step::Confirm;
                        }
                        _ => w.error = Some("Enter a valid port (1-65535)".to_string()),
                    },
                    _ => {}
                },
                Step::Confirm => match key.code {
                    KeyCode::Enter => {
                        let port: u16 = w.port_input.parse().unwrap_or(w.mode.default_port());
                        return Ok((w.mode, port));
                    }
                    KeyCode::Char('b') => w.step = Step::PortInput,
                    _ => {}
                },
            }
        }
    }
}

fn is_action_key_event(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn set_wizard_mode(w: &mut Wizard, mode: Mode) {
    if w.mode != mode {
        w.mode = mode;
        w.port_input = mode.default_port().to_string();
    }
}

fn draw_wizard(f: &mut Frame, w: &Wizard) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(f.area());

    let title = Paragraph::new("Wakezilla Setup")
        .style(Style::default().add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    let body = match w.step {
        Step::ModeSelect => {
            let proxy = mode_span("1 Proxy server", w.mode == Mode::Proxy);
            let client = mode_span("2 Client server", w.mode == Mode::Client);
            vec![
                Line::from("What do you want to configure? (1/2 or Left/Right, Enter to confirm)"),
                Line::from(""),
                Line::from(vec![proxy, Span::raw("   "), client]),
            ]
        }
        Step::PortInput => vec![
            Line::from(format!(
                "Port for {} (Enter to confirm):",
                w.mode.subcommand()
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("> {}", w.port_input),
                Style::default().add_modifier(Modifier::BOLD),
            )),
        ],
        Step::Confirm => vec![
            Line::from("Confirm configuration (Enter to apply, 'b' to go back):"),
            Line::from(""),
            Line::from(format!("  Mode: {}", w.mode.subcommand())),
            Line::from(format!("  Port: {}", w.port_input)),
            Line::from(format!("  Config: {}", config::config_path().display())),
        ],
    };

    let mut lines = body;
    if let Some(err) = &w.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
    }

    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL));
    f.render_widget(para, chunks[1]);

    let footer =
        Paragraph::new("Esc / Ctrl-C: cancel").block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

fn mode_span(label: &str, selected: bool) -> Span<'static> {
    let text = if selected {
        format!("[ {label} ]")
    } else {
        format!("  {label}  ")
    };
    let style = if selected {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default()
    };
    Span::styled(text, style)
}

/// Entry point for the `service` subcommand: start/stop/restart/status/logs of an
/// installed service. Requires elevation. Resolves the target mode from `--mode`, a
/// single install, or a TUI picker when both proxy and client are installed.
pub fn run_service(args: ServiceArgs) -> Result<()> {
    if !service::is_elevated() {
        eprintln!(
            "wakezilla service must run with administrator privileges.\n\
             Re-run with: sudo wakezilla service <action>   (Linux/macOS)  or  an elevated shell (Windows)."
        );
        std::process::exit(1);
    }

    #[cfg(target_os = "windows")]
    if args.action == ServiceAction::Logs && args.mode.is_none() {
        return run_windows_logs_for_installed_services(args.follow, args.lines.unwrap_or(50));
    }

    let mode = match &args.mode {
        Some(mode_str) => {
            let mode = Mode::from_str_opt(mode_str).with_context(|| {
                format!("invalid --mode '{mode_str}' (use 'proxy' or 'client')")
            })?;
            if !service::is_installed(mode) {
                anyhow::bail!(
                    "{} service is not installed. Run `wakezilla setup` first.",
                    mode.subcommand()
                );
            }
            mode
        }
        None => {
            let installed = service::installed_modes();
            match installed.as_slice() {
                [] => {
                    anyhow::bail!("No Wakezilla service is installed. Run `wakezilla setup` first.")
                }
                [only] => *only,
                _ => pick_mode(&installed)?,
            }
        }
    };

    match args.action {
        ServiceAction::Start => {
            service::start(mode).context("failed to start service")?;
            println!("{} service started.", mode.subcommand());
        }
        ServiceAction::Stop => {
            service::stop(mode).context("failed to stop service")?;
            println!("{} service stopped.", mode.subcommand());
        }
        ServiceAction::Restart => {
            service::restart(mode).context("failed to restart service")?;
            println!("{} service restarted.", mode.subcommand());
        }
        ServiceAction::Status => print_status(mode),
        ServiceAction::Logs => {
            print_status(mode);
            println!("--- logs ---");
            service::logs(mode, args.follow, args.lines.unwrap_or(50))
                .context("failed to show logs")?;
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_windows_logs_for_installed_services(follow: bool, lines: u32) -> Result<()> {
    let installed = service::installed_modes();
    match installed.as_slice() {
        [] => anyhow::bail!("No Wakezilla service is installed. Run `wakezilla setup` first."),
        [mode] => {
            print_status(*mode);
            println!("--- logs ---");
            service::logs(*mode, follow, lines).context("failed to show logs")?;
        }
        modes if follow => {
            let names: Vec<&str> = modes.iter().map(|m| m.subcommand()).collect();
            anyhow::bail!(
                "Multiple Wakezilla services are installed ({}). Use `--mode <proxy|client>` with `--follow`.",
                names.join(", ")
            );
        }
        modes => {
            for mode in modes {
                print_status(*mode);
                println!("--- {} logs ---", mode.subcommand());
                service::logs(*mode, false, lines).context("failed to show logs")?;
            }
        }
    }
    Ok(())
}

/// Print whether the service for `mode` is currently running.
fn print_status(mode: Mode) {
    let state = if service::is_running(mode) {
        "running"
    } else {
        "stopped"
    };
    println!("{} service: {state}", mode.subcommand());
}

/// Interactive picker to choose one mode from the installed set (Left/Right, Enter).
fn pick_mode(modes: &[Mode]) -> Result<Mode> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let result = pick_mode_loop(&mut terminal, modes);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn pick_mode_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    modes: &[Mode],
) -> Result<Mode>
where
    B::Error: Send + Sync + 'static,
{
    let mut selected = 0usize;

    loop {
        terminal.draw(|f| draw_pick_mode(f, modes, selected))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if !is_action_key_event(&key) {
                continue;
            }

            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return Err(anyhow::anyhow!("cancelled"));
            }
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                    selected = (selected + 1) % modes.len();
                }
                KeyCode::Char(c) => {
                    if let Some(index) = mode_index_for_shortcut(modes, c) {
                        selected = index;
                    }
                }
                KeyCode::Enter => return Ok(modes[selected]),
                _ => {}
            }
        }
    }
}

fn mode_index_for_shortcut(modes: &[Mode], key: char) -> Option<usize> {
    let mode = match key.to_ascii_lowercase() {
        '1' | 'p' => Mode::Proxy,
        '2' | 'c' => Mode::Client,
        _ => return None,
    };
    modes.iter().position(|m| *m == mode)
}

fn draw_pick_mode(f: &mut Frame, modes: &[Mode], selected: usize) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(f.area());

    let title = Paragraph::new("Wakezilla Service")
        .style(Style::default().add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    let spans: Vec<Span> = modes
        .iter()
        .enumerate()
        .flat_map(|(i, m)| {
            let label = match m {
                Mode::Proxy => "1 Proxy server",
                Mode::Client => "2 Client server",
            };
            vec![mode_span(label, i == selected), Span::raw("   ")]
        })
        .collect();

    let body = vec![
        Line::from("Which service? (1/2 or Left/Right to switch, Enter to select)"),
        Line::from(""),
        Line::from(spans),
    ];
    let para = Paragraph::new(body).block(Block::default().borders(Borders::ALL));
    f.render_widget(para, chunks[1]);

    let footer =
        Paragraph::new("Esc / Ctrl-C: cancel").block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_key_filter_ignores_release_events() {
        let press =
            KeyEvent::new_with_kind(KeyCode::Right, KeyModifiers::NONE, KeyEventKind::Press);
        let repeat =
            KeyEvent::new_with_kind(KeyCode::Right, KeyModifiers::NONE, KeyEventKind::Repeat);
        let release =
            KeyEvent::new_with_kind(KeyCode::Right, KeyModifiers::NONE, KeyEventKind::Release);

        assert!(is_action_key_event(&press));
        assert!(is_action_key_event(&repeat));
        assert!(!is_action_key_event(&release));
    }

    #[test]
    fn wizard_mode_selection_updates_default_port() {
        let mut wizard = Wizard::new(&SetupArgs::default());
        assert_eq!(wizard.mode, Mode::Proxy);
        assert_eq!(wizard.port_input, "3000");

        set_wizard_mode(&mut wizard, Mode::Client);
        assert_eq!(wizard.mode, Mode::Client);
        assert_eq!(wizard.port_input, "3001");
    }

    #[test]
    fn mode_shortcuts_select_installed_mode_index() {
        let modes = [Mode::Proxy, Mode::Client];

        assert_eq!(mode_index_for_shortcut(&modes, '1'), Some(0));
        assert_eq!(mode_index_for_shortcut(&modes, 'p'), Some(0));
        assert_eq!(mode_index_for_shortcut(&modes, '2'), Some(1));
        assert_eq!(mode_index_for_shortcut(&modes, 'C'), Some(1));
        assert_eq!(mode_index_for_shortcut(&[Mode::Proxy], '2'), None);
        assert_eq!(mode_index_for_shortcut(&modes, 'x'), None);
    }
}
