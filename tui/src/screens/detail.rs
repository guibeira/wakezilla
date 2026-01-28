use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};
use wakezilla_common::Machine;

use crate::theme;

pub struct MachineDetailState {
    pub mac: String,
    pub name: String,
    pub ip: String,
    pub description: String,
    pub turn_off_port: String,
    pub can_be_turned_off: bool,
    pub inactivity_period: String,
    pub port_forwards: Vec<PortForwardRow>,
    pub focused_field: usize,
    pub mode: DetailMode,
    pub loading: bool,
    pub error: Option<String>,
    pub show_raw_json: bool,
    pub raw_json: String,
    pub focus_area: FocusArea,
    pub pf_selected: usize,
    pub pf_column: usize, // 0=name, 1=local_port, 2=target_port
    pub pf_table_state: TableState,
}

pub struct PortForwardRow {
    pub name: String,
    pub local_port: String,
    pub target_port: String,
}

#[derive(PartialEq)]
pub enum FocusArea {
    Fields,
    PortForwards,
}

#[derive(PartialEq)]
pub enum DetailMode {
    Normal,
    Insert,
}

const FIELD_COUNT: usize = 7; // name, mac, ip, desc, turn_off_port, can_be_turned_off, inactivity

impl MachineDetailState {
    pub fn new(mac: String) -> Self {
        Self {
            mac,
            name: String::new(),
            ip: String::new(),
            description: String::new(),
            turn_off_port: String::new(),
            can_be_turned_off: false,
            inactivity_period: "30".to_string(),
            port_forwards: Vec::new(),
            focused_field: 0,
            mode: DetailMode::Normal,
            loading: true,
            error: None,
            show_raw_json: false,
            raw_json: String::new(),
            focus_area: FocusArea::Fields,
            pf_selected: 0,
            pf_column: 0,
            pf_table_state: TableState::default(),
        }
    }

    pub fn populate_from_machine(&mut self, m: &Machine) {
        self.name = m.name.clone();
        self.mac = m.mac.clone();
        self.ip = m.ip.to_string();
        self.description = m.description.clone().unwrap_or_default();
        self.turn_off_port = m.turn_off_port.map_or(String::new(), |p| p.to_string());
        self.can_be_turned_off = m.can_be_turned_off;
        self.inactivity_period = m.inactivity_period.to_string();
        self.port_forwards = m
            .port_forwards
            .iter()
            .map(|pf| PortForwardRow {
                name: pf.name.clone(),
                local_port: pf.local_port.to_string(),
                target_port: pf.target_port.to_string(),
            })
            .collect();
        self.raw_json = serde_json::to_string_pretty(m).unwrap_or_default();
        self.loading = false;
    }

    pub fn next_field(&mut self) {
        self.focused_field = (self.focused_field + 1) % FIELD_COUNT;
    }

    pub fn prev_field(&mut self) {
        self.focused_field = if self.focused_field == 0 {
            FIELD_COUNT - 1
        } else {
            self.focused_field - 1
        };
    }

    pub fn current_field_mut(&mut self) -> Option<&mut String> {
        match self.focused_field {
            0 => Some(&mut self.name),
            1 => Some(&mut self.mac),
            2 => Some(&mut self.ip),
            3 => Some(&mut self.description),
            4 => Some(&mut self.turn_off_port),
            5 => None, // boolean toggle
            6 => Some(&mut self.inactivity_period),
            _ => None,
        }
    }

    pub fn toggle_boolean(&mut self) {
        if self.focused_field == 5 {
            self.can_be_turned_off = !self.can_be_turned_off;
        }
    }

    pub fn pf_next(&mut self) {
        if !self.port_forwards.is_empty() {
            self.pf_selected = (self.pf_selected + 1) % self.port_forwards.len();
        }
    }

    pub fn pf_previous(&mut self) {
        if !self.port_forwards.is_empty() {
            self.pf_selected = if self.pf_selected == 0 {
                self.port_forwards.len() - 1
            } else {
                self.pf_selected - 1
            };
        }
    }

    pub fn pf_next_column(&mut self) {
        self.pf_column = (self.pf_column + 1) % 3;
    }

    pub fn pf_current_field_mut(&mut self) -> Option<&mut String> {
        let row = self.port_forwards.get_mut(self.pf_selected)?;
        match self.pf_column {
            0 => Some(&mut row.name),
            1 => Some(&mut row.local_port),
            2 => Some(&mut row.target_port),
            _ => None,
        }
    }

    pub fn pf_add_row(&mut self) {
        self.port_forwards.push(PortForwardRow {
            name: String::new(),
            local_port: String::new(),
            target_port: String::new(),
        });
        self.pf_selected = self.port_forwards.len() - 1;
        self.pf_column = 0;
    }

    pub fn pf_delete_row(&mut self) {
        if !self.port_forwards.is_empty() {
            self.port_forwards.remove(self.pf_selected);
            if self.pf_selected >= self.port_forwards.len() && self.pf_selected > 0 {
                self.pf_selected -= 1;
            }
        }
    }
}

pub fn render(f: &mut Frame, area: Rect, state: &mut MachineDetailState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(8)])
        .split(area);

    let fields: [(&str, &str); 7] = [
        ("Name", &state.name),
        ("MAC", &state.mac),
        ("IP", &state.ip),
        ("Description", &state.description),
        ("Turn Off Port", &state.turn_off_port),
        ("Can Be Turned Off", if state.can_be_turned_off { "Yes" } else { "No" }),
        ("Inactivity Period", &state.inactivity_period),
    ];

    let mut lines: Vec<Line> = Vec::new();
    for (i, (label, value)) in fields.iter().enumerate() {
        let focused = i == state.focused_field;
        let indicator = if focused { "▶ " } else { "  " };
        let label_style = if focused {
            Style::default().fg(theme::BLUE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::SUBTEXT0)
        };
        let value_style = if focused && state.mode == DetailMode::Insert {
            Style::default().fg(theme::YELLOW)
        } else {
            Style::default().fg(theme::TEXT)
        };

        lines.push(Line::from(vec![
            Span::styled(indicator, label_style),
            Span::styled(format!("{}: ", label), label_style),
            Span::styled(value.to_string(), value_style),
        ]));
    }

    if let Some(ref err) = state.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(theme::RED),
        )));
    }

    let title = if state.loading {
        " Machine Detail (loading...) "
    } else {
        " Machine Detail "
    };

    let mode_indicator = match state.mode {
        DetailMode::Normal => " [NORMAL] ",
        DetailMode::Insert => " [INSERT] ",
    };

    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2))
                .title(title)
                .title_style(Style::default().fg(theme::BLUE))
                .title_bottom(Line::from(mode_indicator).style(Style::default().fg(theme::PEACH))),
        )
        .wrap(Wrap { trim: false });

    f.render_widget(para, chunks[0]);

    // Port forwards table
    let pf_border_color = if state.focus_area == FocusArea::PortForwards {
        theme::BLUE
    } else {
        theme::SURFACE2
    };

    let pf_header = Row::new(vec![
        Cell::from("Name").style(Style::default().fg(theme::MAUVE).add_modifier(Modifier::BOLD)),
        Cell::from("Local Port")
            .style(Style::default().fg(theme::MAUVE).add_modifier(Modifier::BOLD)),
        Cell::from("Target Port")
            .style(Style::default().fg(theme::MAUVE).add_modifier(Modifier::BOLD)),
    ])
    .height(1);

    let pf_rows: Vec<Row> = state
        .port_forwards
        .iter()
        .enumerate()
        .map(|(i, pf)| {
            let selected = state.focus_area == FocusArea::PortForwards && i == state.pf_selected;
            let editing = selected && state.mode == DetailMode::Insert;

            let name_style = if editing && state.pf_column == 0 {
                Style::default().fg(theme::YELLOW)
            } else if selected {
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::TEXT)
            };
            let lp_style = if editing && state.pf_column == 1 {
                Style::default().fg(theme::YELLOW)
            } else if selected {
                Style::default().fg(theme::SUBTEXT0).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::SUBTEXT0)
            };
            let tp_style = if editing && state.pf_column == 2 {
                Style::default().fg(theme::YELLOW)
            } else if selected {
                Style::default().fg(theme::SUBTEXT0).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::SUBTEXT0)
            };

            Row::new(vec![
                Cell::from(pf.name.clone()).style(name_style),
                Cell::from(pf.local_port.clone()).style(lp_style),
                Cell::from(pf.target_port.clone()).style(tp_style),
            ])
        })
        .collect();

    let pf_title = if state.port_forwards.is_empty() {
        " Port Forwards (a to add) "
    } else {
        " Port Forwards "
    };

    let pf_table = Table::new(
        pf_rows,
        [
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ],
    )
    .header(pf_header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(pf_border_color))
            .title(pf_title)
            .title_style(Style::default().fg(theme::BLUE)),
    )
    .row_highlight_style(
        Style::default()
            .add_modifier(Modifier::BOLD),
    );

    if state.focus_area == FocusArea::PortForwards && !state.port_forwards.is_empty() {
        state.pf_table_state.select(Some(state.pf_selected));
    } else {
        state.pf_table_state.select(None);
    }
    f.render_stateful_widget(pf_table, chunks[1], &mut state.pf_table_state);
}

pub fn render_confirm_delete(f: &mut Frame, area: Rect) {
    let popup_area = centered_rect(40, 6, area);
    f.render_widget(Clear, popup_area);

    let text = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Delete this machine?",
            Style::default().fg(theme::YELLOW).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" y ", Style::default().fg(theme::GREEN)),
            Span::styled("yes  ", Style::default().fg(theme::TEXT)),
            Span::styled(" n ", Style::default().fg(theme::RED)),
            Span::styled("no", Style::default().fg(theme::TEXT)),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::YELLOW))
            .title(" Confirm ")
            .title_style(Style::default().fg(theme::YELLOW)),
    )
    .alignment(ratatui::layout::Alignment::Center);

    f.render_widget(text, popup_area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
