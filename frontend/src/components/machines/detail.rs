use leptos::prelude::*;

use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;
use web_sys::window;

use leptos_router::hooks::use_params_map;

use web_sys::SubmitEvent;

use crate::api::{get_details_machine, turn_off_machine, wake_machine};
use crate::models::{Machine, PortForward, UpdateMachinePayload};

use crate::api::get_access_history;
use crate::models::AccessHistory;
use chrono::{DateTime, TimeZone, Utc};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
export function render_usage_chart(canvas_id, labels_json, datasets_json) {
    if (typeof window.Chart === 'undefined') { return; }
    const el = document.getElementById(canvas_id);
    if (!el) { return; }
    const labels = JSON.parse(labels_json);
    const datasets = JSON.parse(datasets_json);
    const palette = ['#2563eb','#16a34a','#dc2626','#d97706','#7c3aed','#0891b2','#db2777'];
    datasets.forEach((d, i) => {
        d.backgroundColor = palette[i % palette.length];
        d.borderColor = palette[i % palette.length];
    });
    if (el._chart) { el._chart.destroy(); }
    el._chart = new window.Chart(el, {
        type: 'bar',
        data: { labels: labels, datasets: datasets },
        options: {
            responsive: true,
            scales: { y: { beginAtZero: true, ticks: { precision: 0 } } },
            plugins: { legend: { position: 'bottom' } }
        }
    });
}
"#)]
extern "C" {
    fn render_usage_chart(canvas_id: &str, labels_json: &str, datasets_json: &str);
}

// Returns (sorted day labels "YYYY-MM-DD", per-service datasets as JSON values with counts aligned to labels)
fn bucket_by_day(history: &AccessHistory) -> (Vec<String>, Vec<serde_json::Value>) {
    use std::collections::{BTreeSet, HashMap};

    let day_of = |ts: i64| -> String {
        let dt: DateTime<Utc> = Utc
            .timestamp_millis_opt(ts)
            .single()
            .unwrap_or_else(Utc::now);
        dt.format("%Y-%m-%d").to_string()
    };

    let mut all_days: BTreeSet<String> = BTreeSet::new();
    let mut per_service: Vec<(String, HashMap<String, u32>)> = Vec::new();

    for svc in &history.services {
        let label = svc
            .name
            .clone()
            .unwrap_or_else(|| format!("port {}", svc.local_port));
        let mut counts: HashMap<String, u32> = HashMap::new();
        for &ts in &svc.timestamps {
            let day = day_of(ts);
            all_days.insert(day.clone());
            *counts.entry(day).or_insert(0) += 1;
        }
        per_service.push((label, counts));
    }

    let labels: Vec<String> = all_days.into_iter().collect();
    let datasets: Vec<serde_json::Value> = per_service
        .into_iter()
        .map(|(label, counts)| {
            let data: Vec<u32> = labels
                .iter()
                .map(|d| *counts.get(d).unwrap_or(&0))
                .collect();
            serde_json::json!({ "label": label, "data": data })
        })
        .collect();

    (labels, datasets)
}

#[component]
pub fn MachineDetailPage() -> impl IntoView {
    let params = use_params_map();
    let mac = move || params.read().get("mac").unwrap_or_default();
    let (loading, set_loading) = signal(false);
    let (machine_details, set_machine_details) = signal::<Machine>(Machine::default());

    // Load initial machine details
    Effect::new(move || {
        leptos::task::spawn_local(async move {
            if let Ok(cats) = get_details_machine(&mac()).await {
                set_machine_details.set(cats);
            }
        });
    });

    let (access_history, set_access_history) = signal::<Option<AccessHistory>>(None);

    Effect::new(move || {
        let mac_val = mac();
        leptos::task::spawn_local(async move {
            if let Ok(h) = get_access_history(&mac_val).await {
                set_access_history.set(Some(h));
            }
        });
    });

    Effect::new(move || {
        if let Some(history) = access_history.get() {
            let (labels, datasets) = bucket_by_day(&history);
            let labels_json = serde_json::to_string(&labels).unwrap_or_else(|_| "[]".into());
            let datasets_json = serde_json::to_string(&datasets).unwrap_or_else(|_| "[]".into());
            render_usage_chart("usage-chart", &labels_json, &datasets_json);
        }
    });

    // Form state
    let (name, set_name) = signal(String::new());
    let (ip, set_ip) = signal(String::new());
    let (description, set_description) = signal(String::new());
    let (turn_off_port, set_turn_off_port) = signal::<Option<u16>>(None);
    let (can_be_turned_off, set_can_be_turned_off) = signal(false);
    let (port_forwards, set_port_forwards) = signal::<Vec<PortForward>>(vec![]);
    let (inactivity_period, set_inactivity_period) = signal(60u32);
    let (turn_off_loading, set_turn_off_loading) = signal(false);
    let (turn_off_feedback, set_turn_off_feedback) = signal::<Option<(bool, String)>>(None);
    let (wake_loading, set_wake_loading) = signal(false);
    let (wake_feedback, set_wake_feedback) = signal::<Option<(bool, String)>>(None);

    let can_turn_off_machine = Memo::new(move |_| {
        let machine = machine_details.get();
        machine.can_be_turned_off && machine.turn_off_port.is_some()
    });

    // Update form fields when machine details load
    Effect::new(move || {
        let machine = machine_details.get();
        set_name.set(machine.name.clone());
        set_ip.set(machine.ip.clone());
        set_description.set(machine.description.clone().unwrap_or_default());
        set_turn_off_port.set(machine.turn_off_port); // This should now match the type
        set_can_be_turned_off.set(machine.can_be_turned_off);
        set_port_forwards.set(machine.port_forwards.clone());
        set_inactivity_period.set(machine.inactivity_period);
    });

    let update_machine = move |ev: SubmitEvent| {
        ev.prevent_default();
        set_loading.set(true);

        let updated_mac = mac();
        let updated_name = name.get();
        let updated_ip = ip.get();
        let updated_description = if description.get().trim().is_empty() {
            None
        } else {
            Some(description.get())
        };
        let updated_turn_off_port = if can_be_turned_off.get() {
            turn_off_port.get()
        } else {
            None
        };
        let updated_can_be_turned_off = can_be_turned_off.get();
        let updated_port_forwards = port_forwards.get();

        // Create updated machine object for local state refresh
        let updated_machine = Machine {
            name: updated_name,
            mac: updated_mac.clone(),
            ip: updated_ip,
            description: updated_description,
            turn_off_port: updated_turn_off_port,
            can_be_turned_off: updated_can_be_turned_off,
            inactivity_period: inactivity_period.get(),
            port_forwards: updated_port_forwards.clone(),
        };

        let payload = UpdateMachinePayload {
            mac: updated_machine.mac.clone(),
            ip: updated_machine.ip.clone(),
            name: updated_machine.name.clone(),
            description: updated_machine.description.clone(),
            turn_off_port: updated_machine.turn_off_port,
            can_be_turned_off: updated_machine.can_be_turned_off,
            inactivity_period: Some(updated_machine.inactivity_period),
            port_forwards: Some(updated_machine.port_forwards),
        };

        leptos::task::spawn_local(async move {
            match crate::api::update_machine(&updated_mac, &payload).await {
                Ok(_) => {
                    web_sys::console::log_1(&"Machine updated successfully".into());
                    // Reload the machine details to reflect changes
                    if let Ok(updated_details) = get_details_machine(&updated_mac).await {
                        set_machine_details.set(updated_details);
                    }
                    window()
                        .unwrap()
                        .alert_with_message("Machine updated successfully!")
                        .unwrap();
                }
                Err(e) => {
                    web_sys::console::log_1(&format!("Error updating machine: {}", e).into());
                    window()
                        .unwrap()
                        .alert_with_message(&format!("Error updating machine: {}", e))
                        .unwrap();
                }
            }
            set_loading.set(false);
        });
    };

    let trigger_turn_off = move |_| {
        if !can_turn_off_machine.get() || turn_off_loading.get() {
            return;
        }

        let mac_address = mac();
        set_turn_off_loading.set(true);
        set_turn_off_feedback.set(None);

        let set_turn_off_loading = set_turn_off_loading;
        let set_turn_off_feedback = set_turn_off_feedback;

        leptos::task::spawn_local(async move {
            match turn_off_machine(&mac_address).await {
                Ok(message) => {
                    set_turn_off_feedback.set(Some((true, message.clone())));
                    if let Some(window) = window() {
                        let _ = window.alert_with_message(&message);
                    }
                }
                Err(message) => {
                    set_turn_off_feedback.set(Some((false, message.clone())));
                    if let Some(window) = window() {
                        let _ = window.alert_with_message(&format!(
                            "Failed to turn off machine: {}",
                            message
                        ));
                    }
                }
            }
            set_turn_off_loading.set(false);
        });
    };

    let trigger_wake = move |_| {
        if wake_loading.get() {
            return;
        }

        let mac_address = mac();
        set_wake_loading.set(true);
        set_wake_feedback.set(None);

        let set_wake_loading = set_wake_loading;
        let set_wake_feedback = set_wake_feedback;

        leptos::task::spawn_local(async move {
            match wake_machine(&mac_address).await {
                Ok(message) => {
                    set_wake_feedback.set(Some((true, message.clone())));
                    if let Some(window) = window() {
                        let _ = window.alert_with_message(&message);
                    }
                }
                Err(message) => {
                    set_wake_feedback.set(Some((false, message.clone())));
                    if let Some(window) = window() {
                        let _ = window
                            .alert_with_message(&format!("Failed to wake machine: {}", message));
                    }
                }
            }
            set_wake_loading.set(false);
        });
    };

    view! {
        <div class="page-stack">
            <a class="back-link" href="/">
                <span aria-hidden="true">"←"</span>
                <span>"Back to dashboard"</span>
            </a>

            <div class="card">
                <header class="card-header">
                    <h2 class="card-title">
                        {move || {
                            let current_name = name.get();
                            if current_name.trim().is_empty() {
                                "Machine Overview".to_string()
                            } else {
                                current_name
                            }
                        }}
                    </h2>
                    <p class="card-subtitle">
                        {move || format!("MAC {}", machine_details.get().mac)}
                    </p>
                </header>

                <form on:submit=update_machine class="form-grid">
                    <div class="form-grid two-column">
                        <div class="field">
                            <label for="name">"Name"</label>
                            <input
                                type="text"
                                id="name"
                                name="name"
                                class="input"
                                required
                                value=move || name.get()
                                on:input=move |ev| {
                                    let target = ev.target().unwrap();
                                    let input: HtmlInputElement = target.dyn_into().unwrap();
                                    set_name.set(input.value());
                                }
                            />
                        </div>
                        <div class="field">
                            <label for="ip">"IP address"</label>
                            <input
                                type="text"
                                id="ip"
                                name="ip"
                                class="input"
                                required
                                value=move || ip.get()
                                on:input=move |ev| {
                                    let target = ev.target().unwrap();
                                    let input: HtmlInputElement = target.dyn_into().unwrap();
                                    set_ip.set(input.value());
                                }
                            />
                        </div>
                    </div>

                    <div class="field">
                        <label for="description">"Description"</label>
                        <input
                            type="text"
                            id="description"
                            name="description"
                            class="input"
                            value=move || description.get()
                            on:input=move |ev| {
                                let target = ev.target().unwrap();
                                let input: HtmlInputElement = target.dyn_into().unwrap();
                                set_description.set(input.value());
                            }
                        />
                        <p class="field-help">
                            "Optional label to help the team recognise this machine."
                        </p>
                    </div>

                    <div class="field field-toggle">
                        <input
                            type="checkbox"
                            id="can_be_turned_off"
                            name="can_be_turned_off"
                            class="checkbox"
                            checked=move || can_be_turned_off.get()
                            on:change=move |ev| {
                                let target = ev.target().unwrap();
                                let input: HtmlInputElement = target.dyn_into().unwrap();
                                set_can_be_turned_off.set(input.checked());
                            }
                        />
                        <div class="field-toggle__content">
                            <label for="can_be_turned_off">"Enable remote turn off"</label>
                            <p class="field-help">
                                "Requires an accessible shutdown endpoint on the machine."
                            </p>
                        </div>
                    </div>

                    <Show when=move || can_be_turned_off.get() fallback=|| view! { <></> }>
                        <div class="field">
                            <label for="turn_off_port">"Turn off port (optional)"</label>
                            <input
                                type="number"
                                id="turn_off_port"
                                name="turn_off_port"
                                class="input"
                                min="1"
                                max="65535"
                                value=move || {
                                    turn_off_port.get().map(|p| p.to_string()).unwrap_or_default()
                                }
                                on:input=move |ev| {
                                    let target = ev.target().unwrap();
                                    let input: HtmlInputElement = target.dyn_into().unwrap();
                                    let value = input.value();
                                    set_turn_off_port.set(value.parse().ok());
                                }
                            />
                            <p class="field-help">
                                "Port exposed by the machine to receive shutdown requests."
                            </p>
                        </div>
                    </Show>

                    <div class="field">
                        <div class="field-header">
                            <label>"Port forwards"</label>
                            <button
                                type="button"
                                class="btn btn-soft btn-sm"
                                on:click=move |_| {
                                    set_port_forwards
                                        .update(|pfs| {
                                            pfs.push(PortForward {
                                                name: None,
                                                local_port: 0,
                                                target_port: 0,
                                            });
                                        });
                                }
                            >
                                "+ Add port"
                            </button>
                        </div>
                        <p class="field-help">
                            "Start lightweight TCP tunnels when this machine is online."
                        </p>
                        <Show
                            when=move || !port_forwards.get().is_empty()
                            fallback=|| {
                                view! {
                                    <p class="field-empty">"No port forwards configured yet."</p>
                                }
                            }
                        >
                            <div class="port-forward-list">
                                <For
                                    each=move || {
                                        port_forwards
                                            .get()
                                            .into_iter()
                                            .enumerate()
                                            .collect::<Vec<(usize, PortForward)>>()
                                    }
                                    key=|(idx, _)| *idx
                                    children=move |(idx, _port_forward)| {
                                        let row_number = idx + 1;
                                        let name_id = format!("pf-name-{}", row_number);
                                        let local_id = format!("pf-local-{}", row_number);
                                        let target_id = format!("pf-target-{}", row_number);
                                        let name_label = format!("Service name {}", row_number);
                                        let local_label = format!("Local port {}", row_number);
                                        let target_label = format!(
                                            "Forward to port {}",
                                            row_number,
                                        );
                                        let forward_label = format!("Forward {}", row_number);

                                        view! {
                                            <div class="port-forward-item">
                                                <div class="port-forward-item__header">
                                                    <span class="port-forward-item__title">
                                                        {forward_label}
                                                    </span>
                                                    <button
                                                        type="button"
                                                        class="btn btn-ghost btn-sm port-forward-item__remove"
                                                        on:click=move |_| {
                                                            set_port_forwards
                                                                .update(|pfs| {
                                                                    if idx < pfs.len() {
                                                                        pfs.remove(idx);
                                                                    }
                                                                });
                                                        }
                                                    >
                                                        "Remove"
                                                    </button>
                                                </div>
                                                <div class="port-forward-item__grid">
                                                    <div class="field">
                                                        <label for=name_id.clone()>{name_label.clone()}</label>
                                                        <input
                                                            class="input"
                                                            id=name_id
                                                            placeholder="Service name"
                                                            value=move || {
                                                                port_forwards
                                                                    .get()
                                                                    .get(idx)
                                                                    .and_then(|pf| pf.name.clone())
                                                                    .unwrap_or_default()
                                                            }
                                                            on:input=move |ev| {
                                                                let target = ev.target().unwrap();
                                                                let input: HtmlInputElement = target.dyn_into().unwrap();
                                                                let value = input.value();
                                                                let trimmed = value.trim().is_empty();
                                                                set_port_forwards
                                                                    .update(|pfs| {
                                                                        if let Some(pf) = pfs.get_mut(idx) {
                                                                            pf.name = if trimmed { None } else { Some(value.clone()) };
                                                                        }
                                                                    });
                                                            }
                                                        />
                                                    </div>
                                                    <div class="field">
                                                        <label for=local_id.clone()>{local_label.clone()}</label>
                                                        <input
                                                            class="input"
                                                            id=local_id
                                                            placeholder="Local port"
                                                            type="number"
                                                            min="0"
                                                            max="65535"
                                                            value=move || {
                                                                port_forwards
                                                                    .get()
                                                                    .get(idx)
                                                                    .map(|pf| pf.local_port.to_string())
                                                                    .unwrap_or_default()
                                                            }
                                                            on:input=move |ev| {
                                                                let target = ev.target().unwrap();
                                                                let input: HtmlInputElement = target.dyn_into().unwrap();
                                                                let value = input.value();
                                                                let parsed = value.parse::<u16>().unwrap_or(0);
                                                                set_port_forwards
                                                                    .update(|pfs| {
                                                                        if let Some(pf) = pfs.get_mut(idx) {
                                                                            pf.local_port = parsed;
                                                                        }
                                                                    });
                                                            }
                                                        />
                                                    </div>
                                                    <div class="field">
                                                        <label for=target_id.clone()>{target_label.clone()}</label>
                                                        <input
                                                            class="input"
                                                            id=target_id
                                                            placeholder="Target port"
                                                            type="number"
                                                            min="0"
                                                            max="65535"
                                                            value=move || {
                                                                port_forwards
                                                                    .get()
                                                                    .get(idx)
                                                                    .map(|pf| pf.target_port.to_string())
                                                                    .unwrap_or_default()
                                                            }
                                                            on:input=move |ev| {
                                                                let target = ev.target().unwrap();
                                                                let input: HtmlInputElement = target.dyn_into().unwrap();
                                                                let value = input.value();
                                                                let parsed = value.parse::<u16>().unwrap_or(0);
                                                                set_port_forwards
                                                                    .update(|pfs| {
                                                                        if let Some(pf) = pfs.get_mut(idx) {
                                                                            pf.target_port = parsed;
                                                                        }
                                                                    });
                                                            }
                                                        />
                                                    </div>
                                                </div>
                                            </div>
                                        }
                                    }
                                />
                            </div>
                        </Show>
                    </div>

                    <div class="field">
                        <label for="inactivity_period">"Inactivity Period (minutes)"</label>
                        <input
                            type="number"
                            id="inactivity_period"
                            name="inactivity_period"
                            class="input"
                            min="1"
                            value=move || inactivity_period.get().to_string()
                            on:input=move |ev| {
                                let target = ev.target().unwrap();
                                let input: HtmlInputElement = target.dyn_into().unwrap();
                                if let Ok(value) = input.value().parse() {
                                    set_inactivity_period.set(value);
                                }
                            }
                        />
                    </div>

                    <div class="form-footer">
                        <button
                            type="submit"
                            class="btn btn-primary"
                            disabled=move || loading.get()
                        >
                            {move || if loading.get() { "Saving..." } else { "Save changes" }}
                        </button>
                    </div>
                </form>
            </div>

            <div class="card card-actions">
                <header class="card-header">
                    <h3 class="card-title">"Remote controls"</h3>
                    <p class="card-subtitle">"Send wake and shutdown signals instantly."</p>
                </header>
                <div class="actions-row">
                    <button
                        type="button"
                        class="btn btn-success"
                        on:click=trigger_wake
                        disabled=move || wake_loading.get()
                    >
                        {move || if wake_loading.get() { "Waking..." } else { "Wake machine" }}
                    </button>
                    <button
                        type="button"
                        class="btn btn-danger"
                        on:click=trigger_turn_off
                        disabled=move || turn_off_loading.get() || !can_turn_off_machine.get()
                    >
                        {move || {
                            if turn_off_loading.get() {
                                "Turning off..."
                            } else {
                                "Turn off machine"
                            }
                        }}
                    </button>
                </div>
                {move || {
                    if let Some((success, message)) = wake_feedback.get() {
                        let class = if success {
                            "feedback feedback--success"
                        } else {
                            "feedback feedback--danger"
                        }
                            .to_string();
                        view! { <p class=class>{message}</p> }
                    } else {
                        let class = "feedback feedback--hidden".to_string();
                        let empty = String::new();
                        view! { <p class=class>{empty}</p> }
                    }
                }}
                {move || {
                    if let Some((success, message)) = turn_off_feedback.get() {
                        let class = if success {
                            "feedback feedback--success"
                        } else {
                            "feedback feedback--danger"
                        }
                            .to_string();
                        view! { <p class=class>{message}</p> }
                    } else {
                        let class = "feedback feedback--hidden".to_string();
                        let empty = String::new();
                        view! { <p class=class>{empty}</p> }
                    }
                }}
                <Show when=move || !can_turn_off_machine.get() fallback=|| view! { <></> }>
                    <p class="field-help">
                        "Configure a remote shutdown port on the machine to activate this action."
                    </p>
                </Show>
            </div>

            <div class="card">
                <header class="card-header">
                    <h3 class="card-title">"Access history"</h3>
                    <p class="card-subtitle">"Connections per service, by day."</p>
                </header>
                <canvas id="usage-chart"></canvas>
            </div>

            <div class="card">
                <header class="card-header">
                    <h3 class="card-title">"Raw machine data"</h3>
                    <p class="card-subtitle">"Debug snapshot of the API payload."</p>
                </header>
                <pre class="code-block">
                    {move || {
                        serde_json::to_string_pretty(&machine_details.get()).unwrap_or_default()
                    }}
                </pre>
            </div>
        </div>
    }
}
