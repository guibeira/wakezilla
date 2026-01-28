use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
    Frame,
};

use crate::app::{App, Notification, NotificationLevel, Tab};
use crate::screens;
use crate::theme;

pub fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab bar
            Constraint::Min(1),   // content
            Constraint::Length(2), // footer
        ])
        .split(f.area());

    render_tab_bar(f, chunks[0], app);
    render_content(f, chunks[1], app);
    render_footer(f, chunks[2], app);

    // Confirmation overlay
    if app.confirm_delete {
        screens::detail::render_confirm_delete(f, f.area());
    }
}

fn render_tab_bar(f: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = app
        .tabs()
        .iter()
        .map(|t| {
            let label = match t {
                Tab::Scanner => "Scanner",
                Tab::Machines => "Machines",
                Tab::Detail(mac) => mac.as_str(),
                Tab::AddMachine => "Add Machine",
            };
            Line::from(label)
        })
        .collect();

    let tabs = Tabs::new(titles)
        .select(app.active_tab_index)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2))
                .title(" Wakezilla TUI ")
                .title_style(Style::default().fg(theme::MAUVE).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().fg(theme::SUBTEXT0))
        .highlight_style(
            Style::default()
                .fg(theme::BLUE)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::styled(" │ ", Style::default().fg(theme::SURFACE2)));

    f.render_widget(tabs, area);
}

fn render_content(f: &mut Frame, area: Rect, app: &mut App) {
    match app.active_tab() {
        Some(Tab::Scanner) => {
            screens::scanner::render(f, area, &mut app.scanner_state);
        }
        Some(Tab::Machines) => {
            screens::machines::render(f, area, &mut app.machines_state);
        }
        Some(Tab::Detail(_)) => {
            if let Some(ref mut state) = app.detail_state {
                screens::detail::render(f, area, state);
            }
        }
        Some(Tab::AddMachine) => {
            screens::add_machine::render(f, area, &mut app.add_machine_state);
        }
        None => {}
    }
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let mut spans: Vec<Span> = Vec::new();

    // Show notification if any
    if let Some(notif) = app.current_notification() {
        let color = match notif.level {
            NotificationLevel::Success => theme::GREEN,
            NotificationLevel::Error => theme::RED,
            NotificationLevel::Info => theme::BLUE,
        };
        spans.push(Span::styled(&notif.message, Style::default().fg(color)));
    } else {
        // Show keybinding hints
        let hints = match app.active_tab() {
            Some(Tab::Machines) => {
                if app.machines_state.filtering {
                    "ESC close filter │ type to filter"
                } else {
                    "j/k navigate │ / filter │ w wake │ t turn off │ d delete │ a add │ Enter detail │ Tab switch tab │ :q quit"
                }
            }
            Some(Tab::Scanner) => {
                "j/k navigate │ h/l switch panel │ Enter scan/add │ Tab switch tab │ :q quit"
            }
            Some(Tab::Detail(_)) => {
                "j/k fields │ i insert │ Esc normal │ Space toggle │ s save │ w wake │ t turn off │ d delete │ Tab switch tab │ :q quit"
            }
            Some(Tab::AddMachine) => {
                "j/k fields │ i insert │ Esc normal │ Space toggle │ s/Enter submit │ Tab switch tab │ :q quit"
            }
            None => ":q quit",
        };
        spans.push(Span::styled(hints, Style::default().fg(theme::SUBTEXT0)));
    }

    let footer = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::NONE),
    );

    f.render_widget(footer, area);
}
