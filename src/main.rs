use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tosca_controller::controller::Controller;
use tosca_controller::device::{Device, Devices};
use tosca_controller::discovery::{Discovery, TransportProtocol};
use tosca_controller::error::Error as ToscaError;
use tosca_controller::events::EventPayload;

use serde::Deserialize;
use serde_json::{Map, Value};

use tokio::sync::RwLock;
// use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use tracing::{error, info, debug, warn};

// ============================================================================
// Constants
// ============================================================================

const DHT_ADDRESS: &str = "http://localhost:3000";
const SERVICE_DOMAIN: &str = "ascot";
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
const EVENT_BUFFER_SIZE: usize = 100;
const MAX_RETRIES: u32 = 3;

// ============================================================================
// DHT Data Structures
// ============================================================================

#[derive(Debug, Deserialize)]
struct Topic {
    topic_uuid: String,
    #[allow(dead_code)]
    topic_name: String,
    value: Value,
}

#[derive(Debug, Deserialize)]
struct SmartLightValue {
    name: String,
    mode: String,
    status: bool,
    temperature: f32,
    mac_address: String,
    ip_address: String,
    area_name: String,
    privacy: Option<bool>,
    privacy_until: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SmartLight {
    topic_uuid: String,
    #[allow(dead_code)]
    topic_name: String,
    value: SmartLightValue,
}

// ============================================================================
// Shared State
// ============================================================================

struct SharedState {
    id_mac_map: RwLock<HashMap<usize, String>>,
    mac_uuid_map: RwLock<HashMap<String, String>>,
    event_receiver: RwLock<Option<Receiver<EventPayload>>>,
    client: reqwest::Client,
}

impl SharedState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            id_mac_map: RwLock::new(HashMap::new()),
            mac_uuid_map: RwLock::new(HashMap::new()),
            event_receiver: RwLock::new(None),
            client: reqwest::Client::builder()
                .pool_idle_timeout(Duration::from_secs(30)) // riutilizza le connessioni
                .build()
                .unwrap(),
        })
    }

    async fn update_mappings(&self, id: usize, mac: String, uuid: String) {
        let mut id_mac = self.id_mac_map.write().await;
        id_mac.insert(id, mac.clone());
        drop(id_mac);

        let mut mac_uuid = self.mac_uuid_map.write().await;
        mac_uuid.insert(mac, uuid);
    }

    async fn get_topic_uuid_for_device(&self, device_id: usize) -> Option<String> {
        let id_mac = self.id_mac_map.read().await;
        let mac = id_mac.get(&device_id)?.clone();
        drop(id_mac);

        let mac_uuid = self.mac_uuid_map.read().await;
        mac_uuid.get(&mac).cloned()
    }

    async fn existing_macs(&self) -> HashSet<String> {
        let map = self.id_mac_map.read().await;
        map.values().cloned().collect()
    }

    async fn set_event_receiver(&self, receiver: Option<Receiver<EventPayload>>) {
        let mut rx = self.event_receiver.write().await;
        *rx = receiver;
    }

    async fn recv_event(&self) -> Option<EventPayload> {
        let mut guard = self.event_receiver.write().await;
        let receiver = guard.as_mut()?;
        receiver.recv().await
    }
}

// ============================================================================
// Utility Functions
// ============================================================================

fn mac_to_string(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(":")
        .to_lowercase()
}

type DhtResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn with_retry<F, Fut, T>(f: F, operation: &str) -> DhtResult<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = DhtResult<T>>,
{
    let mut attempts = 0;
    loop {
        match f().await {
            Ok(result) => return Ok(result),
            Err(e) if attempts < MAX_RETRIES => {
                attempts += 1;
                warn!(
                    "{} failed (attempt {}/{}): {}",
                    operation, attempts, MAX_RETRIES, e
                );
                tokio::time::sleep(Duration::from_millis(500 * u64::from(attempts))).await;
            }
            Err(e) => return Err(e),
        }
    }
}

// ============================================================================
// DHT Operations
// ============================================================================

async fn topic_exists(state: &SharedState, topic_uuid: &str) -> bool {
    let url = format!("{DHT_ADDRESS}/topic_name/smart_light_assistants/topic_uuid/{topic_uuid}");

    match state.client.get(&url).send().await {
        Ok(resp) => {
            match resp.json::<Value>().await {
                Ok(json) => {
                    let exists = json
                        .get("topic_uuid")
                        .and_then(|v| v.as_str())
                        .is_some();

                    exists
                }
                Err(e) => {
                    warn!("Failed to decode JSON for {}: {}", url, e);
                    false
                }
            }
        }
        Err(e) => {
            warn!("HTTP error calling {}: {}", url, e);
            false
        }
    }
}

async fn fetch_first_room_uuid() -> DhtResult<String> {
    let fetch = || async {
        let url = format!("{}/topic_name/domo_room", DHT_ADDRESS);
        let resp = reqwest::get(&url).await?;
        let topics: Vec<Topic> = resp.json().await?;

        topics
            .into_iter()
            .find(|t| t.value.get("order_id").and_then(|v| v.as_u64()) == Some(0))
            .map(|t| t.topic_uuid)
            .ok_or_else(|| "No topic with order_id == 0 found".into())
    };

    with_retry(fetch, "fetch_first_room_uuid").await
}

async fn fetch_smart_lights() -> DhtResult<Vec<SmartLight>> {
    let fetch = || async {
        let url = format!("{}/topic_name/smart_light_assistants", DHT_ADDRESS);
        let resp = reqwest::get(&url).await?;
        let lights: Vec<SmartLight> = resp.json().await?;
        Ok(lights)
    };

    with_retry(fetch, "fetch_smart_lights_assistants").await
}

async fn create_smart_light_topic(
    state: &SharedState,
    index: usize,
    topic_uuid: &str,
    mac: &str,
    ip: String,
) -> DhtResult<()> {
    let area_name = fetch_first_room_uuid().await?;
    let url = format!(
        "{}/topic_name/smart_light_assistants/topic_uuid/{}",
        DHT_ADDRESS, topic_uuid
    );

    let payload = serde_json::json!({
        "name": format!("Luce Intelligente {}", index + 1),
        "mode": "manual",
        "status": false,
        "temperature": 18,
        "mac_address": mac,
        "ip_address": ip,
        "area_name": area_name
    });

    info!("Creating Smartotum DHT topic for Luce Intelligente {}", index + 1);

    let create = || async {
        state.client
            .post(&url)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    };

    with_retry(create, &format!("create_topic_{}", topic_uuid)).await
}

async fn update_smart_light_topic(
    state: &SharedState,
    topic_uuid: &str,
    mac: &str,
    event: &EventPayload,
) -> DhtResult<()> {
    let url = format!(
        "{}/topic_name/smart_light_assistants/topic_uuid/{}",
        DHT_ADDRESS, topic_uuid
    );

    // Fetch current state.
    let topic = {
        let fetch = || async {
            let resp = reqwest::get(&url).await?;
            let topic: SmartLight = resp.json().await?;
            Ok(topic)
        };
        with_retry(fetch, &format!("fetch_topic_{}", topic_uuid)).await?
    };

    // Extract updated values from events.
    let (status, temperature, mode) = extract_device_state(&topic.value, event);

    let mut payload = Map::from_iter([
        ("name".into(), Value::String(topic.value.name.clone())),
        ("mode".into(), Value::String(mode)),
        ("status".into(), Value::Bool(status)),
        ("temperature".into(), Value::from(temperature)),
        ("mac_address".into(), Value::String(mac.to_string())),
        ("ip_address".into(), Value::String(topic.value.ip_address.clone())),
        ("area_name".into(), Value::String(topic.value.area_name.clone())),
    ]);

    if let Some(privacy) = topic.value.privacy {
        payload.insert("privacy".into(), Value::Bool(privacy));
    }

    if let Some(until) = &topic.value.privacy_until {
        payload.insert("privacy_until".into(), Value::String(until.clone()));
    }

    let payload = Value::Object(payload);

    let update = || async {
        state.client
            .post(&url)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    };

    with_retry(update, &format!("update_topic_{}", topic_uuid)).await
}

fn extract_device_state(current: &SmartLightValue, event: &EventPayload) -> (bool, f32, String) {
    let mut status = current.status;
    let mut temperature = current.temperature;
    let mut mode = current.mode.clone();

    // Process bool events.
    for bool_event in event
        .events
        .bool_events_as_slice()
        .iter()
        .chain(event.events.periodic_bool_events_as_slice().iter().map(|pe| &pe.event))
    {
        if bool_event.name == "status" {
            let new_status = bool_event.value;

            if new_status != status {
                info!(
                    "Updating 'status' from {} to {} for {}",
                    status, new_status, current.name
                );
                status = new_status;
            }
        }
    }

    // Process u8 events.
    for u8_event in event
        .events
        .u8_events_as_slice()
        .iter()
        .chain(event.events.periodic_u8_events_as_slice().iter().map(|pe| &pe.event))
    {
        if u8_event.name == "mode" {
            let new_mode = match u8_event.value {
                0 => Some("manual"),
                1 => Some("motion-detection"),
                2 => Some("ambient-light"),
                _ => None,
            };

            if let Some(new_mode) = new_mode {
                if new_mode != mode {
                    info!(
                        "Updating 'mode' from {} to {} for {}",
                        mode, new_mode, current.name
                    );
                    mode = new_mode.to_string();
                }
            }
        }
    }

    // Process f32 events.
    for f32_event in event
        .events
        .f32_events_as_slice()
        .iter()
        .chain(event.events.periodic_f32_events_as_slice().iter().map(|pe| &pe.event))
    {
        if f32_event.name == "temperature" {
            let new_temperature = f32_event.value;

            if new_temperature != temperature {
                info!(
                    "Updating 'temperature' from {} to {} for {}",
                    temperature, new_temperature, current.name
                );
                temperature = new_temperature;
            }
        }
    }

    (status, temperature, mode)
}

// ============================================================================
// Device Management
// ============================================================================

struct NewDevice {
    index: usize,
    mac: String,
    ip: String,
}

impl NewDevice {
    fn from_device(index: usize, device: &Device) -> Option<Self> {
        let mac = device.network_info().wifi_mac.map(|m| mac_to_string(&m))?;
        let ip = device
            .network_info()
            .addresses
            .iter()
            .next()
            .map(|addr| addr.to_string())?;

        Some(Self { index, mac, ip })
    }
}

async fn find_new_devices(
    devices: &Devices,
    existing_macs: &HashSet<String>,
) -> Vec<NewDevice> {
    devices
        .iter()
        .enumerate()
        .filter_map(|(idx, device)| NewDevice::from_device(idx, device))
        .filter(|nd| !existing_macs.contains(&nd.mac))
        .collect()
}

async fn get_or_create_topic_uuid(
    state: &SharedState,
    new_device: &NewDevice,
    smart_lights: &[SmartLight],
) -> DhtResult<String> {
    // Check if topic already exists.
    if let Some(light) = smart_lights
        .iter()
        .find(|al| al.value.mac_address == new_device.mac)
    {
        info!(
            "Device {} already has a topic in Smartotum DHT",
            light.value.name
        );
        return Ok(light.topic_uuid.clone());
    }

    // Create new topic.
    let uuid = Uuid::new_v4().to_string();
    create_smart_light_topic(state, new_device.index, &uuid, &new_device.mac, new_device.ip.clone())
        .await?;
    Ok(uuid)
}

async fn reconcile_existing_devices(
    controller: &Controller,
    state: Arc<SharedState>,
) -> DhtResult<()> {
    let devices = controller.devices();

    for (device_id, device) in devices.iter().enumerate() {
        let mac = match device.network_info().wifi_mac {
            Some(mac) => mac_to_string(&mac),
            None => continue,
        };

        let ip = match device
            .network_info()
            .addresses
            .iter()
            .next()
        {
            Some(ip) => ip.to_string(),
            None => continue,
        };

        let existing_uuid = {
            let mac_uuid = state.mac_uuid_map.read().await;
            mac_uuid.get(&mac).cloned()
        };

        if let Some(uuid) = existing_uuid {
            if !topic_exists(&state, &uuid).await {
                warn!(
                    "Topic {} for MAC {} no longer exists, recreating",
                    uuid, mac
                );

                let new_uuid = Uuid::new_v4().to_string();

                create_smart_light_topic(
                    &state,
                    device_id,
                    &new_uuid,
                    &mac,
                    ip.clone(),
                )
                .await?;

                state
                    .update_mappings(device_id, mac.clone(), new_uuid)
                    .await;
            }
        }
    }

    Ok(())
}

async fn process_new_devices(
    controller: &mut Controller,
    state: Arc<SharedState>
) -> DhtResult<()> {
    reconcile_existing_devices(controller, state.clone()).await?;

    let existing_macs = state.existing_macs().await;
    let new_devices = find_new_devices(controller.devices(), &existing_macs).await;

    if new_devices.is_empty() {
        return Ok(());
    }

    for nd in new_devices.iter() {
        info!("Found a new device with IP: {}", nd.ip);
    }

    let smart_lights = fetch_smart_lights().await?;

    for new_device in new_devices {
        let topic_uuid = match get_or_create_topic_uuid(&state, &new_device, &smart_lights).await {
            Ok(uuid) => uuid,
            Err(e) => {
                error!(
                    "Failed to get/create topic for device {}: {}",
                    new_device.mac, e
                );
                continue;
            }
        };

        state
            .update_mappings(new_device.index, new_device.mac, topic_uuid)
            .await;
    }

    // Restart event receivers with updated devices.
    match controller.start_event_receivers(EVENT_BUFFER_SIZE).await {
        Ok(rx) => state.set_event_receiver(Some(rx)).await,
        Err(e) => {
            error!("Unable to start event receivers: {}", e);
            state.set_event_receiver(None).await;
        }
    }

    Ok(())
}

// ============================================================================
// Main Event Loops
// ============================================================================

async fn discovery_loop(
    mut controller: Controller,
    state: Arc<SharedState>,
    token: CancellationToken,
) {
    info!("Initializing device discovery...");

    let mut discovery_interval = Duration::from_secs(0);

    loop {
        tokio::select! {
            () = token.cancelled() => {
                controller.shutdown().await;
                info!("Shutting down device discovery...");
                break;
            }

            () = tokio::time::sleep(discovery_interval) => {
                discovery_interval = DISCOVERY_INTERVAL;
                info!("Scanning the network for new devices...");

                if let Err(e) = controller.discover().await {
                    error!("Discovery failed: {:?}", e);
                    continue;
                }

                if let Err(e) = process_new_devices(&mut controller, state.clone()).await {
                    error!("Error processing new devices: {}", e);
                }
            }
        }
    }
}

async fn events_loop(state: Arc<SharedState>, token: CancellationToken) {
    info!("Events monitoring started");

    loop {
        tokio::select! {
            () = token.cancelled() => {
                info!("Shutting down device events monitoring...");
                break;
            }

            event = state.recv_event() => {
                let Some(event) = event else {
                    debug!("Event receiver closed, exiting loop");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                };

                debug!("Received event from device {}", event.device_id);

                let Some(topic_uuid) = state.get_topic_uuid_for_device(event.device_id).await else {
                    warn!("No topic UUID mapping for device {}", event.device_id);
                    continue;
                };

                let mac = {
                    let id_mac = state.id_mac_map.read().await;
                    id_mac.get(&event.device_id).cloned()
                };

                let Some(mac) = mac else {
                    warn!("No MAC mapping for device {}", event.device_id);
                    continue;
                };

                if let Err(e) = update_smart_light_topic(&state, &topic_uuid, &mac, &event).await {
                    warn!("Failed to update topic {}: {}", topic_uuid, e);
                }
            }
        }
    }
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), ToscaError> {
    // tracing_subscriber::fmt()
    //     .with_max_level(LevelFilter::INFO)
    //     .init();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::new("info")
                .add_directive("tosca_controller=off".parse().unwrap())
        )
        .init();

    let discovery = Discovery::new(SERVICE_DOMAIN)
        .timeout(Duration::from_secs(2))
        .transport_protocol(TransportProtocol::UDP)
        .disable_ipv6()
        .disable_network_interface("docker0")
        .domain("ascot");

    let controller = Controller::new(discovery);
    let state = SharedState::new();

    let token = CancellationToken::new();
    let discovery_handle = tokio::spawn(discovery_loop(
        controller,
        state.clone(),
        token.child_token(),
    ));
    let events_handle = tokio::spawn(events_loop(state, token.child_token()));

    info!("Controller ready, awaiting devices...");

    tokio::signal::ctrl_c().await.expect("Failed to install Ctrl+C handler");
    info!("Received shutdown signal");

    info!("Shutting down the controller...");

    token.cancel();

    let _ = tokio::join!(discovery_handle, events_handle);

    info!("Shutdown complete");
    Ok(())
}
