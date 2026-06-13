use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PortForward {
    pub name: Option<String>,
    pub local_port: u16,
    pub target_port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    pub mac: String,
    pub ip: String,
    pub description: Option<String>,
    pub turn_off_port: Option<u16>,
    pub can_be_turned_off: bool,
    pub inactivity_period: u32,
    pub port_forwards: Vec<PortForward>,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            name: "".to_string(),
            mac: "".to_string(),
            ip: "".to_string(),
            description: None,
            turn_off_port: None,
            can_be_turned_off: false,
            inactivity_period: 60,
            port_forwards: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AddMachinePayload {
    pub mac: String,
    pub ip: String,
    pub name: String,
    pub description: Option<String>,
    pub turn_off_port: Option<u16>,
    pub can_be_turned_off: bool,
    pub inactivity_period: Option<u32>,
    pub port_forwards: Option<Vec<PortForward>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UpdateMachinePayload {
    pub mac: String,
    pub ip: String,
    pub name: String,
    pub description: Option<String>,
    pub turn_off_port: Option<u16>,
    pub can_be_turned_off: bool,
    pub inactivity_period: Option<u32>,
    pub port_forwards: Option<Vec<PortForward>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DeleteMachinePayload {
    pub mac: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NetworkInterface {
    pub name: String,
    pub ip: String,
    pub mac: String,
    pub is_up: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiscoveredDevice {
    pub ip: String,
    pub mac: String,
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServiceAccessHistory {
    pub name: Option<String>,
    pub local_port: u16,
    pub target_port: u16,
    pub timestamps: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccessHistory {
    pub services: Vec<ServiceAccessHistory>,
}
