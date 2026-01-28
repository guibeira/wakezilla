use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use wakezilla_common::{AddMachineForm, MachinePayload, PortForward};

use crate::client::ApiClient;
use crate::screens::{
    add_machine::{AddFocusArea, AddMachineState},
    detail::{DetailMode, FocusArea, MachineDetailState},
    machines::MachinesState,
    scanner::{Panel, ScannerState},
};

#[derive(Clone, PartialEq)]
pub enum Tab {
    Scanner,
    Machines,
    Detail(String),
    AddMachine,
}

pub struct Notification {
    pub message: String,
    pub level: NotificationLevel,
    pub created_at: Instant,
}

pub enum NotificationLevel {
    Success,
    Error,
    Info,
}

enum ApiResponse {
    Machines(anyhow::Result<Vec<wakezilla_common::Machine>>),
    MachineStatus(String, anyhow::Result<bool>),
    MachineDetail(anyhow::Result<wakezilla_common::Machine>),
    Interfaces(anyhow::Result<Vec<wakezilla_common::NetworkInterface>>),
    ScanResult(anyhow::Result<Vec<wakezilla_common::DiscoveredDevice>>),
    ActionResult(anyhow::Result<String>),
    AddResult(anyhow::Result<()>),
    UpdateResult(anyhow::Result<()>),
    DeleteResult(anyhow::Result<()>),
}

pub struct App {
    pub active_tab_index: usize,
    tabs: Vec<Tab>,
    pub machines_state: MachinesState,
    pub scanner_state: ScannerState,
    pub detail_state: Option<MachineDetailState>,
    pub add_machine_state: AddMachineState,
    pub confirm_delete: bool,
    notifications: Vec<Notification>,
    client: ApiClient,
    api_tx: mpsc::Sender<ApiResponse>,
    api_rx: mpsc::Receiver<ApiResponse>,
    colon_pressed: bool,
    quit: bool,
    last_refresh: Instant,
}

impl App {
    pub fn new(client: ApiClient) -> Self {
        let (api_tx, api_rx) = mpsc::channel(64);
        Self {
            active_tab_index: 1,
            tabs: vec![Tab::Scanner, Tab::Machines],
            machines_state: MachinesState::new(),
            scanner_state: ScannerState::new(),
            detail_state: None,
            add_machine_state: AddMachineState::new(),
            confirm_delete: false,
            notifications: Vec::new(),
            client,
            api_tx,
            api_rx,
            colon_pressed: false,
            quit: false,
            last_refresh: Instant::now(),
        }
    }

    pub fn init(&mut self) {
        self.fetch_machines();
        self.fetch_interfaces();
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab_index)
    }

    pub fn current_notification(&self) -> Option<&Notification> {
        self.notifications
            .last()
            .filter(|n| n.created_at.elapsed() < Duration::from_secs(3))
    }

    fn notify(&mut self, message: String, level: NotificationLevel) {
        self.notifications.push(Notification {
            message,
            level,
            created_at: Instant::now(),
        });
    }

    fn open_tab(&mut self, tab: Tab) {
        if let Some(idx) = self.tabs.iter().position(|t| t == &tab) {
            self.active_tab_index = idx;
            return;
        }
        self.tabs.push(tab);
        self.active_tab_index = self.tabs.len() - 1;
    }

    fn close_current_tab(&mut self) {
        if self.tabs.len() <= 2 {
            return;
        }
        let idx = self.active_tab_index;
        match &self.tabs[idx] {
            Tab::Scanner | Tab::Machines => return,
            _ => {}
        }
        self.tabs.remove(idx);
        if self.active_tab_index >= self.tabs.len() {
            self.active_tab_index = self.tabs.len() - 1;
        }
    }

    fn fetch_machines(&self) {
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        tokio::spawn(async move {
            let result = client.list_machines().await;
            let _ = tx.send(ApiResponse::Machines(result)).await;
        });
    }

    fn fetch_machine_status(&self, mac: &str) {
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        let mac = mac.to_string();
        tokio::spawn(async move {
            let result = client.is_machine_on(&mac).await;
            let _ = tx.send(ApiResponse::MachineStatus(mac, result)).await;
        });
    }

    fn fetch_machine_detail(&self, mac: &str) {
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        let mac = mac.to_string();
        tokio::spawn(async move {
            let result = client.get_machine(&mac).await;
            let _ = tx.send(ApiResponse::MachineDetail(result)).await;
        });
    }

    fn fetch_interfaces(&self) {
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        tokio::spawn(async move {
            let result = client.list_interfaces().await;
            let _ = tx.send(ApiResponse::Interfaces(result)).await;
        });
    }

    fn scan_network(&self, interface: Option<String>) {
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        tokio::spawn(async move {
            let result = client.scan_network(interface.as_deref()).await;
            let _ = tx.send(ApiResponse::ScanResult(result)).await;
        });
    }

    fn wake_machine(&self, mac: &str) {
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        let mac = mac.to_string();
        tokio::spawn(async move {
            let result = client.wake_machine(&mac).await;
            let _ = tx.send(ApiResponse::ActionResult(result)).await;
        });
    }

    fn turn_off_machine(&self, mac: &str) {
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        let mac = mac.to_string();
        tokio::spawn(async move {
            let result = client.turn_off_machine(&mac).await;
            let _ = tx.send(ApiResponse::ActionResult(result)).await;
        });
    }

    fn submit_add_machine(&mut self) {
        let state = &self.add_machine_state;
        let form = AddMachineForm {
            mac: state.mac.clone(),
            ip: state.ip.clone(),
            name: state.name.clone(),
            description: if state.description.is_empty() {
                None
            } else {
                Some(state.description.clone())
            },
            turn_off_port: state.turn_off_port.parse().ok(),
            can_be_turned_off: state.can_be_turned_off,
            inactivity_period: state.inactivity_period.parse().ok(),
            port_forwards: Some(
                state
                    .port_forwards
                    .iter()
                    .filter_map(|pf| {
                        Some(PortForward {
                            name: pf.name.clone(),
                            local_port: pf.local_port.parse().ok()?,
                            target_port: pf.target_port.parse().ok()?,
                        })
                    })
                    .collect(),
            ),
        };
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        tokio::spawn(async move {
            let result = client.add_machine(&form).await;
            let _ = tx.send(ApiResponse::AddResult(result)).await;
        });
    }

    fn submit_update_machine(&mut self) {
        if let Some(ref state) = self.detail_state {
            let payload = MachinePayload {
                mac: state.mac.clone(),
                ip: state.ip.clone(),
                name: state.name.clone(),
                description: if state.description.is_empty() {
                    None
                } else {
                    Some(state.description.clone())
                },
                turn_off_port: state.turn_off_port.parse().ok(),
                can_be_turned_off: state.can_be_turned_off,
                inactivity_period: state.inactivity_period.parse().ok(),
                port_forwards: Some(
                    state
                        .port_forwards
                        .iter()
                        .filter_map(|pf| {
                            Some(PortForward {
                                name: pf.name.clone(),
                                local_port: pf.local_port.parse().ok()?,
                                target_port: pf.target_port.parse().ok()?,
                            })
                        })
                        .collect(),
                ),
            };
            let mac = state.mac.clone();
            let client = self.client.clone();
            let tx = self.api_tx.clone();
            tokio::spawn(async move {
                let result = client.update_machine(&mac, &payload).await;
                let _ = tx.send(ApiResponse::UpdateResult(result)).await;
            });
        }
    }

    fn delete_current_machine(&mut self) {
        let mac = if let Some(ref state) = self.detail_state {
            state.mac.clone()
        } else if let Some(m) = self.machines_state.selected_machine() {
            m.mac.clone()
        } else {
            return;
        };
        let client = self.client.clone();
        let tx = self.api_tx.clone();
        tokio::spawn(async move {
            let result = client.delete_machine(&mac).await;
            let _ = tx.send(ApiResponse::DeleteResult(result)).await;
        });
    }

    pub fn process_api_responses(&mut self) {
        while let Ok(response) = self.api_rx.try_recv() {
            self.handle_api_response(response);
        }
        // Clean expired notifications
        self.notifications
            .retain(|n| n.created_at.elapsed() < Duration::from_secs(3));
    }

    pub fn check_refresh(&mut self) {
        if self.last_refresh.elapsed() >= Duration::from_secs(30) {
            self.fetch_machines();
            let macs: Vec<String> = self
                .machines_state
                .machines
                .iter()
                .map(|m| m.mac.clone())
                .collect();
            for mac in &macs {
                self.fetch_machine_status(mac);
            }
            self.last_refresh = Instant::now();
        }
    }

    fn handle_api_response(&mut self, response: ApiResponse) {
        match response {
            ApiResponse::Machines(result) => match result {
                Ok(machines) => {
                    for m in &machines {
                        self.fetch_machine_status(&m.mac);
                    }
                    self.machines_state.machines = machines;
                    self.machines_state.loading = false;
                    if self.machines_state.table_state.selected().is_none()
                        && !self.machines_state.machines.is_empty()
                    {
                        self.machines_state.table_state.select(Some(0));
                    }
                }
                Err(e) => {
                    self.machines_state.loading = false;
                    self.notify(
                        format!("Failed to load machines: {}", e),
                        NotificationLevel::Error,
                    );
                }
            },
            ApiResponse::MachineStatus(mac, result) => {
                let is_on = result.unwrap_or(false);
                self.machines_state.statuses.insert(mac, Some(is_on));
            }
            ApiResponse::MachineDetail(result) => match result {
                Ok(machine) => {
                    if let Some(ref mut state) = self.detail_state {
                        state.populate_from_machine(&machine);
                    }
                }
                Err(e) => {
                    if let Some(ref mut state) = self.detail_state {
                        state.loading = false;
                        state.error = Some(format!("Failed to load: {}", e));
                    }
                }
            },
            ApiResponse::Interfaces(result) => match result {
                Ok(interfaces) => {
                    self.scanner_state.interfaces = interfaces;
                    self.scanner_state.loading_interfaces = false;
                    if !self.scanner_state.interfaces.is_empty() {
                        self.scanner_state.interface_state.select(Some(0));
                    }
                }
                Err(e) => {
                    self.scanner_state.loading_interfaces = false;
                    self.notify(
                        format!("Failed to load interfaces: {}", e),
                        NotificationLevel::Error,
                    );
                }
            },
            ApiResponse::ScanResult(result) => {
                self.scanner_state.scanning = false;
                match result {
                    Ok(devices) => {
                        self.scanner_state.devices = devices;
                        if !self.scanner_state.devices.is_empty() {
                            self.scanner_state.device_state.select(Some(0));
                        }
                        self.notify("Scan complete".to_string(), NotificationLevel::Success);
                    }
                    Err(e) => {
                        self.notify(format!("Scan failed: {}", e), NotificationLevel::Error);
                    }
                }
            }
            ApiResponse::ActionResult(result) => match result {
                Ok(msg) => self.notify(msg, NotificationLevel::Success),
                Err(e) => {
                    self.notify(format!("Action failed: {}", e), NotificationLevel::Error)
                }
            },
            ApiResponse::AddResult(result) => match result {
                Ok(()) => {
                    self.notify("Machine added".to_string(), NotificationLevel::Success);
                    self.close_current_tab();
                    self.fetch_machines();
                }
                Err(e) => {
                    self.add_machine_state.error = Some(format!("Failed: {}", e));
                }
            },
            ApiResponse::UpdateResult(result) => match result {
                Ok(()) => {
                    self.notify("Machine updated".to_string(), NotificationLevel::Success);
                    self.fetch_machines();
                }
                Err(e) => {
                    if let Some(ref mut state) = self.detail_state {
                        state.error = Some(format!("Save failed: {}", e));
                    }
                }
            },
            ApiResponse::DeleteResult(result) => match result {
                Ok(()) => {
                    self.notify("Machine deleted".to_string(), NotificationLevel::Success);
                    self.close_current_tab();
                    self.fetch_machines();
                }
                Err(e) => {
                    self.notify(format!("Delete failed: {}", e), NotificationLevel::Error);
                }
            },
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        if self.confirm_delete {
            match key.code {
                KeyCode::Char('y') => {
                    self.confirm_delete = false;
                    self.delete_current_machine();
                }
                _ => {
                    self.confirm_delete = false;
                }
            }
            return;
        }

        if self.colon_pressed {
            self.colon_pressed = false;
            if key.code == KeyCode::Char('q') {
                self.quit = true;
            }
            return;
        }

        match self.active_tab().cloned() {
            Some(Tab::Machines) => self.handle_machines_key(key),
            Some(Tab::Scanner) => self.handle_scanner_key(key),
            Some(Tab::Detail(_)) => self.handle_detail_key(key),
            Some(Tab::AddMachine) => self.handle_add_machine_key(key),
            None => {
                if key.code == KeyCode::Char(':') {
                    self.colon_pressed = true;
                }
            }
        }
    }

    fn handle_machines_key(&mut self, key: KeyEvent) {
        if self.machines_state.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.machines_state.filtering = false;
                    self.machines_state.filter.clear();
                }
                KeyCode::Backspace => {
                    self.machines_state.filter.pop();
                }
                KeyCode::Char(c) => {
                    self.machines_state.filter.push(c);
                }
                KeyCode::Enter => {
                    self.machines_state.filtering = false;
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.machines_state.next(),
            KeyCode::Char('k') | KeyCode::Up => self.machines_state.previous(),
            KeyCode::Char('/') => {
                self.machines_state.filtering = true;
                self.machines_state.filter.clear();
            }
            KeyCode::Char('w') => {
                if let Some(m) = self.machines_state.selected_machine() {
                    let mac = m.mac.clone();
                    self.wake_machine(&mac);
                }
            }
            KeyCode::Char('t') => {
                if let Some(m) = self.machines_state.selected_machine() {
                    let mac = m.mac.clone();
                    self.turn_off_machine(&mac);
                }
            }
            KeyCode::Char('d') => {
                if self.machines_state.selected_machine().is_some() {
                    self.confirm_delete = true;
                }
            }
            KeyCode::Char('a') => {
                self.add_machine_state = AddMachineState::new();
                self.open_tab(Tab::AddMachine);
            }
            KeyCode::Enter => {
                if let Some(m) = self.machines_state.selected_machine() {
                    let mac = m.mac.clone();
                    self.detail_state = Some(MachineDetailState::new(mac.clone()));
                    self.open_tab(Tab::Detail(mac.clone()));
                    self.fetch_machine_detail(&mac);
                }
            }
            KeyCode::Tab => {
                self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
            }
            KeyCode::BackTab => {
                self.active_tab_index = if self.active_tab_index == 0 {
                    self.tabs.len() - 1
                } else {
                    self.active_tab_index - 1
                };
            }
            KeyCode::Char(':') => {
                self.colon_pressed = true;
            }
            _ => {}
        }
    }

    fn handle_scanner_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.scanner_state.next(),
            KeyCode::Char('k') | KeyCode::Up => self.scanner_state.previous(),
            KeyCode::Char('h') | KeyCode::Left => {
                self.scanner_state.active_panel = Panel::Interfaces;
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.scanner_state.active_panel = Panel::Devices;
            }
            KeyCode::Enter => match self.scanner_state.active_panel {
                Panel::Interfaces => {
                    if let Some(iface) = self.scanner_state.selected_interface() {
                        let name = iface.name.clone();
                        self.scanner_state.scanning = true;
                        self.scanner_state.devices.clear();
                        self.scan_network(Some(name));
                    }
                }
                Panel::Devices => {
                    if let Some(device) = self.scanner_state.selected_device() {
                        let mut state = AddMachineState::new();
                        state.prefill(&device.mac, &device.ip, device.hostname.as_deref());
                        self.add_machine_state = state;
                        self.open_tab(Tab::AddMachine);
                    }
                }
            },
            KeyCode::Tab => {
                self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
            }
            KeyCode::BackTab => {
                self.active_tab_index = if self.active_tab_index == 0 {
                    self.tabs.len() - 1
                } else {
                    self.active_tab_index - 1
                };
            }
            KeyCode::Char(':') => {
                self.colon_pressed = true;
            }
            _ => {}
        }
    }

    fn handle_detail_key(&mut self, key: KeyEvent) {
        // Extract state info without holding mutable borrow
        let (in_insert, in_pf) = match self.detail_state {
            Some(ref s) => (s.mode == DetailMode::Insert, s.focus_area == FocusArea::PortForwards),
            None => return,
        };

        // INSERT mode in port forwards
        if in_insert && in_pf {
            if let Some(ref mut state) = self.detail_state {
                match key.code {
                    KeyCode::Esc => state.mode = DetailMode::Normal,
                    KeyCode::Tab => state.pf_next_column(),
                    KeyCode::Backspace => {
                        if let Some(field) = state.pf_current_field_mut() {
                            field.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(field) = state.pf_current_field_mut() {
                            field.push(c);
                        }
                    }
                    _ => {}
                }
            }
            return;
        }

        // INSERT mode in fields
        if in_insert {
            if let Some(ref mut state) = self.detail_state {
                match key.code {
                    KeyCode::Esc => state.mode = DetailMode::Normal,
                    KeyCode::Tab => state.next_field(),
                    KeyCode::Backspace => {
                        if let Some(field) = state.current_field_mut() {
                            field.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(field) = state.current_field_mut() {
                            field.push(c);
                        }
                    }
                    _ => {}
                }
            }
            return;
        }

        // NORMAL mode in port forwards
        if in_pf {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(ref mut s) = self.detail_state {
                        s.pf_next();
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(ref mut s) = self.detail_state {
                        s.pf_previous();
                    }
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    if let Some(ref mut s) = self.detail_state {
                        s.focus_area = FocusArea::Fields;
                    }
                }
                KeyCode::Char('i') => {
                    if let Some(ref mut s) = self.detail_state {
                        s.mode = DetailMode::Insert;
                    }
                }
                KeyCode::Char('a') => {
                    if let Some(ref mut s) = self.detail_state {
                        s.pf_add_row();
                        s.mode = DetailMode::Insert;
                    }
                }
                KeyCode::Char('x') => {
                    if let Some(ref mut s) = self.detail_state {
                        s.pf_delete_row();
                    }
                }
                KeyCode::Char('s') => {
                    self.submit_update_machine();
                }
                KeyCode::Tab => {
                    self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
                }
                KeyCode::BackTab => {
                    self.active_tab_index = if self.active_tab_index == 0 {
                        self.tabs.len() - 1
                    } else {
                        self.active_tab_index - 1
                    };
                }
                KeyCode::Char(':') => {
                    self.colon_pressed = true;
                }
                _ => {}
            }
            return;
        }

        // NORMAL mode in fields
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ref mut s) = self.detail_state {
                    s.next_field();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ref mut s) = self.detail_state {
                    s.prev_field();
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if let Some(ref mut s) = self.detail_state {
                    s.focus_area = FocusArea::PortForwards;
                }
            }
            KeyCode::Char('i') => {
                if let Some(ref mut s) = self.detail_state {
                    s.mode = DetailMode::Insert;
                }
            }
            KeyCode::Char(' ') => {
                if let Some(ref mut s) = self.detail_state {
                    s.toggle_boolean();
                }
            }
            KeyCode::Char('s') => {
                self.submit_update_machine();
            }
            KeyCode::Char('w') => {
                if let Some(ref s) = self.detail_state {
                    let mac = s.mac.clone();
                    self.wake_machine(&mac);
                }
            }
            KeyCode::Char('t') => {
                if let Some(ref s) = self.detail_state {
                    let mac = s.mac.clone();
                    self.turn_off_machine(&mac);
                }
            }
            KeyCode::Char('d') => {
                self.confirm_delete = true;
            }
            KeyCode::Tab => {
                self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
            }
            KeyCode::BackTab => {
                self.active_tab_index = if self.active_tab_index == 0 {
                    self.tabs.len() - 1
                } else {
                    self.active_tab_index - 1
                };
            }
            KeyCode::Char(':') => {
                self.colon_pressed = true;
            }
            _ => {}
        }
    }

    fn handle_add_machine_key(&mut self, key: KeyEvent) {
        let in_insert = self.add_machine_state.inserting;
        let in_pf = self.add_machine_state.focus_area == AddFocusArea::PortForwards;

        // INSERT mode in port forwards
        if in_insert && in_pf {
            match key.code {
                KeyCode::Esc => self.add_machine_state.inserting = false,
                KeyCode::Tab => self.add_machine_state.pf_next_column(),
                KeyCode::Backspace => {
                    if let Some(field) = self.add_machine_state.pf_current_field_mut() {
                        field.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(field) = self.add_machine_state.pf_current_field_mut() {
                        field.push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        // INSERT mode in fields
        if in_insert {
            match key.code {
                KeyCode::Esc => self.add_machine_state.inserting = false,
                KeyCode::Tab => self.add_machine_state.next_field(),
                KeyCode::Backspace => {
                    if let Some(field) = self.add_machine_state.current_field_mut() {
                        field.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(field) = self.add_machine_state.current_field_mut() {
                        field.push(c);
                    }
                }
                KeyCode::Enter => {
                    self.submit_add_machine();
                }
                _ => {}
            }
            return;
        }

        // NORMAL mode in port forwards
        if in_pf {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.add_machine_state.pf_next(),
                KeyCode::Char('k') | KeyCode::Up => self.add_machine_state.pf_previous(),
                KeyCode::Char('h') | KeyCode::Left => {
                    self.add_machine_state.focus_area = AddFocusArea::Fields;
                }
                KeyCode::Char('i') => self.add_machine_state.inserting = true,
                KeyCode::Char('a') => {
                    self.add_machine_state.pf_add_row();
                    self.add_machine_state.inserting = true;
                }
                KeyCode::Char('x') => self.add_machine_state.pf_delete_row(),
                KeyCode::Char('s') => self.submit_add_machine(),
                KeyCode::Tab => {
                    self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
                }
                KeyCode::BackTab => {
                    self.active_tab_index = if self.active_tab_index == 0 {
                        self.tabs.len() - 1
                    } else {
                        self.active_tab_index - 1
                    };
                }
                KeyCode::Char(':') => self.colon_pressed = true,
                _ => {}
            }
            return;
        }

        // NORMAL mode in fields
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.add_machine_state.next_field(),
            KeyCode::Char('k') | KeyCode::Up => self.add_machine_state.prev_field(),
            KeyCode::Char('l') | KeyCode::Right => {
                self.add_machine_state.focus_area = AddFocusArea::PortForwards;
            }
            KeyCode::Char('i') => self.add_machine_state.inserting = true,
            KeyCode::Char(' ') => self.add_machine_state.toggle_boolean(),
            KeyCode::Char('s') | KeyCode::Enter => self.submit_add_machine(),
            KeyCode::Tab => {
                self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
            }
            KeyCode::BackTab => {
                self.active_tab_index = if self.active_tab_index == 0 {
                    self.tabs.len() - 1
                } else {
                    self.active_tab_index - 1
                };
            }
            KeyCode::Char(':') => self.colon_pressed = true,
            _ => {}
        }
    }
}
