use std::collections::HashMap;

pub use wakezilla_common::{
    AccessHistory, DiscoveredDevice, Machine, NetworkInterface, PortForward, ServiceAccessHistory,
    UpdateMachinePayload,
};

pub fn validate_machine_form(machine: &Machine) -> HashMap<String, Vec<String>> {
    let mut errors = HashMap::new();

    if machine.name.trim().is_empty() {
        errors.insert("name".to_string(), vec!["Name is required".to_string()]);
    }

    if machine.ip.parse::<std::net::IpAddr>().is_err() {
        errors.insert("ip".to_string(), vec!["Invalid IP address".to_string()]);
    }

    if let Some(port) = machine.turn_off_port
        && port == 0
    {
        errors.insert(
            "turn_off_port".to_string(),
            vec!["Port must be between 1 and 65535".to_string()],
        );
    }

    if machine.mac.trim().is_empty() {
        errors.insert(
            "mac".to_string(),
            vec!["MAC address is required".to_string()],
        );
    }

    if !machine.mac.trim().is_empty() {
        let is_valid_mac = machine
            .mac
            .chars()
            .filter(|c| c.is_ascii_hexdigit() || *c == ':' || *c == '-')
            .count()
            == machine.mac.len()
            && (machine.mac.len() == 17 || machine.mac.len() == 12);

        if !is_valid_mac {
            errors.insert("mac".to_string(), vec!["Invalid MAC address".to_string()]);
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    #[test]
    fn validate_machine_form_rejects_invalid_ip() {
        let machine = wakezilla_common::Machine {
            name: "x".into(),
            mac: "AA:BB:CC:DD:EE:FF".into(),
            ip: "bad-ip".into(),
            description: None,
            turn_off_port: None,
            can_be_turned_off: false,
            inactivity_period: 30,
            port_forwards: vec![],
        };

        let errors = super::validate_machine_form(&machine);
        assert!(errors.contains_key("ip"));
    }
}
