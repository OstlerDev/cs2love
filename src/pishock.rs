use std::{
    sync::{Arc, OnceLock, RwLock as StdRwLock},
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    sync::{mpsc, oneshot, Mutex, RwLock},
    time::{timeout as tokio_timeout, MissedTickBehavior},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::config::Config;

const AUTH_USER_ENDPOINT: &str = "https://auth.pishock.com/Auth/GetUserIfAPIKeyValid";
const USER_DEVICES_ENDPOINT: &str = "https://ps.pishock.com/PiShock/GetUserDevices";
const BROKER_ENDPOINT: &str = "wss://broker.pishock.com/v2";
const BROKER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(4);
const BROKER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const BROKER_RECONNECT_TICK_INTERVAL: Duration = Duration::from_secs(1);
const BROKER_RECONNECT_BASE_DELAY: Duration = Duration::from_secs(2);
const BROKER_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_millis(5000);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_millis(10000);
const INVALID_BROKER_AUTH_MESSAGE: &str =
    "PiShock rejected the broker credentials. Confirm your username and API key.";

static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static BROKER_HANDLE_STATE: OnceLock<Mutex<Option<BrokerHandle>>> = OnceLock::new();
static BROKER_HEARTBEAT_STATE: OnceLock<StdRwLock<Option<Instant>>> = OnceLock::new();

type BrokerSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn shock(config: Arc<RwLock<Config>>, intensity: i32, duration_ms: u64) {
    info!(
        target: "PiShock API",
        "Delivered shock with intensity: {} for {}ms",
        intensity,
        duration_ms
    );
    if let Err(e) = post(
        config,
        PiShockOp::Shock {
            intensity,
            duration_ms,
        },
    )
    .await
    {
        error!(target: "PiShock API", "Failed to send shock: {}", e);
    }
}

pub async fn vibrate(config: Arc<RwLock<Config>>, intensity: i32, duration: i32) {
    info!(
        target: "PiShock API",
        "Delivered vibrate with intensity: {} for {}ms",
        intensity,
        duration
    );
    if let Err(e) = post(
        config,
        PiShockOp::Vibrate {
            intensity,
            duration,
        },
    )
    .await
    {
        error!(target: "PiShock API", "Failed to send vibrate: {}", e);
    }
}

pub async fn beep(config: Arc<RwLock<Config>>, duration: i32) {
    info!(target: "PiShock API", "Delivered beep for {}ms", duration);
    if let Err(e) = post(config, PiShockOp::Beep { duration }).await {
        error!(target: "PiShock API", "Failed to send beep: {}", e);
    }
}

pub async fn warmup(config: Arc<RwLock<Config>>) -> Result<(), String> {
    let config = config.read().await.clone();
    send_warmup_request(config).await
}

pub async fn reset_session() {
    clear_broker_handle().await;
}

pub async fn post(config: Arc<RwLock<Config>>, operation: PiShockOp) -> Result<(), String> {
    let config = config.read().await.clone();
    send_publish_request(config, operation).await
}

pub fn last_heartbeat_elapsed() -> Option<Duration> {
    last_successful_heartbeat().map(|heartbeat| heartbeat.elapsed())
}

pub async fn discover_targets(
    config: Arc<RwLock<Config>>,
) -> Result<Vec<DiscoveredTarget>, String> {
    let config = config.read().await.clone();
    discover_targets_with_config(&config).await
}

async fn send_publish_request(config: Config, operation: PiShockOp) -> Result<(), String> {
    for attempt in 0..2 {
        let handle = ensure_broker_handle().await;
        let (response_tx, response_rx) = oneshot::channel();
        let request = BrokerRequest::Publish {
            config: config.clone(),
            operation: operation.clone(),
            response: response_tx,
        };

        if handle.sender.send(request).await.is_err() {
            clear_broker_handle().await;
            if attempt == 0 {
                continue;
            }
            return Err("PiShock broker owner task closed unexpectedly.".into());
        }

        match response_rx.await {
            Ok(result) => return result,
            Err(_) => {
                clear_broker_handle().await;
                if attempt == 0 {
                    continue;
                }
                return Err("PiShock broker response channel closed unexpectedly.".into());
            }
        }
    }

    Err("PiShock broker request failed after retry.".into())
}

async fn send_warmup_request(config: Config) -> Result<(), String> {
    for attempt in 0..2 {
        let handle = ensure_broker_handle().await;
        let (response_tx, response_rx) = oneshot::channel();
        let request = BrokerRequest::Warmup {
            config: config.clone(),
            response: response_tx,
        };

        if handle.sender.send(request).await.is_err() {
            clear_broker_handle().await;
            if attempt == 0 {
                continue;
            }
            return Err("PiShock broker owner task closed unexpectedly.".into());
        }

        match response_rx.await {
            Ok(result) => return result,
            Err(_) => {
                clear_broker_handle().await;
                if attempt == 0 {
                    continue;
                }
                return Err("PiShock broker response channel closed unexpectedly.".into());
            }
        }
    }

    Err("PiShock broker warmup failed after retry.".into())
}

async fn ensure_broker_handle() -> BrokerHandle {
    let mut state = broker_handle_state().lock().await;
    if let Some(handle) = state.as_ref() {
        if !handle.sender.is_closed() {
            return handle.clone();
        }
    }

    let (sender, receiver) = mpsc::channel(32);
    tokio::spawn(run_broker_owner(receiver));
    let handle = BrokerHandle { sender };
    *state = Some(handle.clone());
    handle
}

async fn clear_broker_handle() {
    let mut state = broker_handle_state().lock().await;
    *state = None;
    clear_last_successful_heartbeat();
}

async fn run_broker_owner(mut receiver: mpsc::Receiver<BrokerRequest>) {
    let mut state = BrokerOwnerState::default();
    let mut heartbeat = tokio::time::interval(BROKER_HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut reconnect = tokio::time::interval(BROKER_RECONNECT_TICK_INTERVAL);
    reconnect.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_request = receiver.recv() => {
                let Some(request) = maybe_request else {
                    break;
                };

                match request {
                    BrokerRequest::Warmup { config, response } => {
                        let result = handle_warmup_request(&mut state, &config).await;
                        let _ = response.send(result);
                    }
                    BrokerRequest::Publish { config, operation, response } => {
                        let result = handle_publish_request(&mut state, &config, operation).await;
                        let _ = response.send(result);
                    }
                }
            }
            _ = heartbeat.tick(), if state.socket.is_some() => {
                if let Err(e) = heartbeat_broker(&mut state).await {
                    warn!(target: "PiShock API", "PiShock broker heartbeat failed: {}", e);
                }
            }
            _ = reconnect.tick(), if should_reconnect_broker(&state) => {
                if let Err(e) = reconnect_broker(&mut state).await {
                    warn!(target: "PiShock API", "PiShock broker reconnect failed: {}", e);
                }
            }
        }
    }
}

async fn handle_warmup_request(
    state: &mut BrokerOwnerState,
    config: &Config,
) -> Result<(), String> {
    validate_broker_auth(config)?;
    sync_session_config(state, config);
    ensure_socket_connected(state).await
}

async fn handle_publish_request(
    state: &mut BrokerOwnerState,
    config: &Config,
    operation: PiShockOp,
) -> Result<(), String> {
    validate_control_config(config)?;
    validate_operation(&operation)?;
    sync_session_config(state, config);

    let target = resolve_cached_target(state, config).await?;
    validate_target_capabilities(&target, &operation)?;
    ensure_socket_connected(state).await?;

    let channel = publish_target(&target);
    let body = build_broker_body(&target, operation);
    let result = {
        let socket = state
            .socket
            .as_mut()
            .ok_or_else(|| "PiShock broker socket was unavailable.".to_string())?;
        publish_over_socket(socket, &channel, &body).await
    };
    if result.is_err() {
        mark_broker_socket_failed(state);
    }
    result
}

async fn heartbeat_broker(state: &mut BrokerOwnerState) -> Result<(), String> {
    let result = {
        let socket = state
            .socket
            .as_mut()
            .ok_or_else(|| "PiShock broker socket was unavailable.".to_string())?;
        send_broker_ping(socket).await
    };
    if result.is_err() {
        mark_broker_socket_failed(state);
    }
    result
}

fn sync_session_config(state: &mut BrokerOwnerState, config: &Config) {
    let auth_key = BrokerAuthKey::from_config(config);
    if state.auth_key.as_ref() != Some(&auth_key) {
        state.auth_key = Some(auth_key);
        clear_broker_socket(state);
        clear_broker_reconnect_backoff(state);
        state.cached_target = None;
        return;
    }

    let target_key = BrokerTargetKey::from_config(config);
    if state.cached_target.as_ref().map(|cached| &cached.key) != Some(&target_key) {
        state.cached_target = None;
    }
}

async fn resolve_cached_target(
    state: &mut BrokerOwnerState,
    config: &Config,
) -> Result<ResolvedTarget, String> {
    let key = BrokerTargetKey::from_config(config);
    if let Some(cached) = state.cached_target.as_ref() {
        if cached.key == key {
            return Ok(cached.target.clone());
        }
    }

    let client = http_client();
    let user_id = resolve_user_id(client, config).await?;
    let target = resolve_selected_target(client, config, user_id).await?;
    state.cached_target = Some(CachedTarget {
        key,
        target: target.clone(),
    });
    Ok(target)
}

async fn ensure_socket_connected(state: &mut BrokerOwnerState) -> Result<(), String> {
    if state.socket.is_some() {
        return Ok(());
    }

    let auth_key = state
        .auth_key
        .clone()
        .ok_or_else(|| "PiShock broker auth was unavailable.".to_string())?;
    let result = async {
        let mut socket = connect_broker_socket(&auth_key).await?;
        send_broker_ping(&mut socket).await?;
        Ok::<BrokerSocket, String>(socket)
    }
    .await;

    match result {
        Ok(socket) => {
            state.socket = Some(socket);
            clear_broker_reconnect_backoff(state);
            Ok(())
        }
        Err(error) => {
            schedule_broker_reconnect(state);
            Err(error)
        }
    }
}

fn validate_broker_auth(config: &Config) -> Result<(), String> {
    if config.username.trim().is_empty() {
        return Err("PiShock username is required.".into());
    }
    if config.apikey.trim().is_empty() {
        return Err("PiShock API key is required.".into());
    }
    Ok(())
}

fn validate_control_config(config: &Config) -> Result<(), String> {
    validate_broker_auth(config)?;
    if config.selected_client_id.is_none() || config.selected_shocker_id.is_none() {
        return Err("Select a PiShock shocker before sending commands.".into());
    }
    Ok(())
}

fn validate_operation(operation: &PiShockOp) -> Result<(), String> {
    match operation {
        PiShockOp::Beep { duration } => validate_duration(*duration),
        PiShockOp::Vibrate {
            intensity,
            duration,
        } => {
            validate_intensity(*intensity)?;
            validate_duration(*duration)
        }
        PiShockOp::Shock {
            intensity,
            duration_ms,
        } => {
            validate_intensity(*intensity)?;
            validate_shock_duration_ms(*duration_ms)
        }
    }
}

fn validate_intensity(intensity: i32) -> Result<(), String> {
    if !(1..=100).contains(&intensity) {
        return Err(format!(
            "Intensity must be between 1 and 100, got {}",
            intensity
        ));
    }
    Ok(())
}

fn validate_duration(duration: i32) -> Result<(), String> {
    if !(1..=15).contains(&duration) {
        return Err(format!(
            "Duration must be between 1 and 15, got {}",
            duration
        ));
    }
    Ok(())
}

fn validate_shock_duration_ms(duration_ms: u64) -> Result<(), String> {
    if !(100..=5000).contains(&duration_ms) {
        return Err(format!(
            "Shock duration must be between 100 and 5000 milliseconds, got {}",
            duration_ms
        ));
    }
    Ok(())
}

async fn connect_broker_socket(auth_key: &BrokerAuthKey) -> Result<BrokerSocket, String> {
    let url = build_broker_connect_url(auth_key)?;
    let (socket, _) = connect_async(url.as_str())
        .await
        .map_err(|e| format!("Failed to connect to PiShock broker: {e}"))?;
    Ok(socket)
}

fn build_broker_connect_url(auth_key: &BrokerAuthKey) -> Result<Url, String> {
    Url::parse_with_params(
        BROKER_ENDPOINT,
        [
            ("Username", auth_key.username.as_str()),
            ("ApiKey", auth_key.apikey.as_str()),
        ],
    )
    .map_err(|e| format!("Failed to construct PiShock broker URL: {e}"))
}

async fn send_broker_ping(socket: &mut BrokerSocket) -> Result<(), String> {
    send_json(
        socket,
        &json!({ "Operation": "PING" }),
        "send PiShock broker ping",
    )
    .await?;
    wait_for_matching_response(
        socket,
        BROKER_RESPONSE_TIMEOUT,
        is_pong_response,
        "PiShock broker pong",
    )
    .await?;
    record_successful_heartbeat();
    Ok(())
}

async fn publish_over_socket(
    socket: &mut BrokerSocket,
    target: &str,
    body: &BrokerBody,
) -> Result<(), String> {
    send_json(
        socket,
        &BrokerPublishEnvelope {
            operation: "PUBLISH",
            publish_commands: vec![BrokerPublishCommand {
                target: target.to_owned(),
                body: body.clone(),
            }],
        },
        "publish PiShock command",
    )
    .await?;

    wait_for_matching_response(
        socket,
        BROKER_RESPONSE_TIMEOUT,
        is_publish_success,
        "PiShock publish confirmation",
    )
    .await?;

    Ok(())
}

async fn send_json<T: Serialize>(
    socket: &mut BrokerSocket,
    payload: &T,
    action: &str,
) -> Result<(), String> {
    let payload = serde_json::to_string(payload)
        .map_err(|e| format!("Failed to serialize payload for {action}: {e}"))?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .map_err(|e| format!("Failed to {action}: {e}"))
}

async fn wait_for_matching_response<F>(
    socket: &mut BrokerSocket,
    response_timeout: Duration,
    mut is_match: F,
    expected: &str,
) -> Result<BrokerResponse, String>
where
    F: FnMut(&BrokerResponse) -> bool,
{
    let started_at = tokio::time::Instant::now();

    loop {
        let Some(remaining) = response_timeout.checked_sub(started_at.elapsed()) else {
            return Err(format!("Timed out waiting for {expected}."));
        };

        let response = read_broker_response(socket, remaining)
            .await?
            .ok_or_else(|| format!("Timed out waiting for {expected}."))?;

        if let Some(error) = broker_error_message(&response) {
            return Err(error);
        }
        if is_match(&response) {
            return Ok(response);
        }
    }
}

async fn read_broker_response(
    socket: &mut BrokerSocket,
    wait_timeout: Duration,
) -> Result<Option<BrokerResponse>, String> {
    let started_at = tokio::time::Instant::now();

    loop {
        let Some(remaining) = wait_timeout.checked_sub(started_at.elapsed()) else {
            return Ok(None);
        };

        match tokio_timeout(remaining, socket.next()).await {
            Err(_) => return Ok(None),
            Ok(None) => return Err("PiShock broker closed the connection unexpectedly.".into()),
            Ok(Some(Err(e))) => return Err(format!("PiShock broker stream error: {e}")),
            Ok(Some(Ok(message))) => {
                if let Some(parsed) = parse_broker_response(message)? {
                    return Ok(Some(parsed));
                }
            }
        }
    }
}

fn parse_broker_response(message: Message) -> Result<Option<BrokerResponse>, String> {
    match message {
        Message::Text(text) => serde_json::from_str::<BrokerResponse>(&text)
            .map(Some)
            .map_err(|e| format!("Failed to parse PiShock broker message: {e}")),
        Message::Close(frame) => {
            if let Some(frame) = frame {
                if !frame.reason.is_empty() {
                    return Err(format!(
                        "PiShock broker closed the connection: {}",
                        frame.reason
                    ));
                }
            }
            Err("PiShock broker closed the connection.".into())
        }
        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
    }
}

fn broker_error_message(response: &BrokerResponse) -> Option<String> {
    if response.is_error.unwrap_or(false) || response.error_code.is_some() {
        return Some(match response.error_code.as_deref() {
            Some("AUTH_TOKEN_ERROR") => INVALID_BROKER_AUTH_MESSAGE.to_owned(),
            Some(code) => match response.message.as_deref() {
                Some(message) if !message.trim().is_empty() => {
                    format!("PiShock broker error ({code}): {message}")
                }
                _ => format!("PiShock broker error ({code})."),
            },
            None => response
                .message
                .clone()
                .unwrap_or_else(|| "PiShock broker returned an unknown error.".into()),
        });
    }

    None
}

fn is_pong_response(response: &BrokerResponse) -> bool {
    response
        .message
        .as_deref()
        .map(|message| message.eq_ignore_ascii_case("PONG"))
        .unwrap_or(false)
}

fn is_publish_success(response: &BrokerResponse) -> bool {
    response
        .message
        .as_deref()
        .map(|message| message.eq_ignore_ascii_case("Publish successful."))
        .unwrap_or(false)
}

fn publish_target(target: &ResolvedTarget) -> String {
    format!("c{}-ops", target.client_id)
}

fn build_broker_body(target: &ResolvedTarget, operation: PiShockOp) -> BrokerBody {
    let (method, intensity, duration_ms) = match operation {
        PiShockOp::Beep { duration } => ("b", 0, duration as u64 * 1000),
        PiShockOp::Vibrate {
            intensity,
            duration,
        } => ("v", intensity, duration as u64 * 1000),
        PiShockOp::Shock {
            intensity,
            duration_ms,
        } => ("s", intensity, duration_ms),
    };

    BrokerBody {
        id: target.shocker_id,
        method: method.to_owned(),
        intensity,
        duration_ms,
        repeating: true,
        metadata: BrokerMetadata {
            user_id: target.user_id,
            target_type: "api".into(),
            warning: false,
            hold: false,
            origin: "cs2shock".into(),
        },
    }
}

async fn resolve_user_id(client: &Client, config: &Config) -> Result<u64, String> {
    let response = client
        .get(AUTH_USER_ENDPOINT)
        .query(&[
            ("apikey", config.apikey.as_str()),
            ("username", config.username.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Failed to validate PiShock API key: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to validate PiShock API key: HTTP {}",
            response.status().as_u16()
        ));
    }

    let auth_user: AuthUser = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse PiShock user lookup response: {e}"))?;
    Ok(auth_user.user_id)
}

async fn discover_targets_with_config(config: &Config) -> Result<Vec<DiscoveredTarget>, String> {
    if config.username.trim().is_empty() {
        return Err("PiShock username is required.".into());
    }
    if config.apikey.trim().is_empty() {
        return Err("PiShock API key is required.".into());
    }

    let client = http_client();
    let user_id = resolve_user_id(client, config).await?;
    let devices = load_owned_devices(client, config, user_id).await?;
    Ok(discovered_targets_from_devices(&devices))
}

async fn load_owned_devices(
    client: &Client,
    config: &Config,
    user_id: u64,
) -> Result<Vec<OwnedDevice>, String> {
    let response = client
        .get(USER_DEVICES_ENDPOINT)
        .query(&[
            ("UserId", user_id.to_string()),
            ("Token", config.apikey.clone()),
            ("api", "true".to_string()),
        ])
        .send()
        .await
        .map_err(|e| format!("Failed to load PiShock devices: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to load PiShock devices: HTTP {}",
            response.status().as_u16()
        ));
    }

    response
        .json()
        .await
        .map_err(|e| format!("Failed to parse PiShock devices response: {e}"))
}

fn discovered_targets_from_devices(devices: &[OwnedDevice]) -> Vec<DiscoveredTarget> {
    let mut targets: Vec<DiscoveredTarget> = devices
        .iter()
        .flat_map(|device| {
            device.shockers.iter().map(|shocker| DiscoveredTarget {
                client_id: device.client_id,
                shocker_id: shocker.shocker_id,
                device_name: device.name.clone(),
                shocker_name: shocker.name.clone(),
                is_paused: shocker.is_paused,
            })
        })
        .collect();

    targets.sort_by(|left, right| {
        (
            left.device_name.to_ascii_lowercase(),
            left.shocker_name.to_ascii_lowercase(),
            left.client_id,
            left.shocker_id,
        )
            .cmp(&(
                right.device_name.to_ascii_lowercase(),
                right.shocker_name.to_ascii_lowercase(),
                right.client_id,
                right.shocker_id,
            ))
    });

    targets
}

async fn resolve_selected_target(
    client: &Client,
    config: &Config,
    user_id: u64,
) -> Result<ResolvedTarget, String> {
    let selected_client_id = config
        .selected_client_id
        .ok_or_else(|| "Select a PiShock device before sending commands.".to_string())?;
    let selected_shocker_id = config
        .selected_shocker_id
        .ok_or_else(|| "Select a PiShock shocker before sending commands.".to_string())?;
    let devices = load_owned_devices(client, config, user_id).await?;
    let target = devices
        .into_iter()
        .find_map(|device| {
            if device.client_id != selected_client_id {
                return None;
            }

            let client_id = device.client_id;
            let device_name = device.name;

            device.shockers.into_iter().find_map(|shocker| {
                (shocker.shocker_id == selected_shocker_id).then_some(ResolvedTarget {
                    user_id,
                    client_id,
                    shocker_id: shocker.shocker_id,
                    device_name: device_name.clone(),
                    shocker_name: shocker.name,
                    max_intensity: shocker.max_intensity,
                    max_duration: shocker.max_duration,
                    is_paused: shocker.is_paused,
                    can_shock: shocker.can_shock,
                    can_vibrate: shocker.can_vibrate,
                    can_beep: shocker.can_beep,
                })
            })
        })
        .ok_or_else(|| {
            format!(
                "PiShock device `{selected_client_id}` / shocker `{selected_shocker_id}` was not found in the current API response."
            )
        })?;

    Ok(target)
}

fn validate_target_capabilities(
    target: &ResolvedTarget,
    operation: &PiShockOp,
) -> Result<(), String> {
    if target.is_paused {
        return Err(format!(
            "PiShock shocker `{}` on `{}` is paused. Unpause it before sending commands.",
            target.shocker_name, target.device_name
        ));
    }

    match operation {
        PiShockOp::Beep { duration } => {
            if !target.can_beep {
                return Err("Beep is not allowed for the selected PiShock shocker.".into());
            }
            if *duration > target.max_duration {
                return Err(format!(
                    "Duration must be between 1 and {}, got {}",
                    target.max_duration, duration
                ));
            }
        }
        PiShockOp::Vibrate {
            intensity,
            duration,
        } => {
            if !target.can_vibrate {
                return Err("Vibrate is not allowed for the selected PiShock shocker.".into());
            }
            if *intensity > target.max_intensity {
                return Err(format!(
                    "Intensity must be between 1 and {}, got {}",
                    target.max_intensity, intensity
                ));
            }
            if *duration > target.max_duration {
                return Err(format!(
                    "Duration must be between 1 and {}, got {}",
                    target.max_duration, duration
                ));
            }
        }
        PiShockOp::Shock {
            intensity,
            duration_ms,
        } => {
            if !target.can_shock {
                return Err("Shock is not allowed for the selected PiShock shocker.".into());
            }
            if *intensity > target.max_intensity {
                return Err(format!(
                    "Intensity must be between 1 and {}, got {}",
                    target.max_intensity, intensity
                ));
            }
            let max_duration_ms = target.max_duration as u64 * 1000;
            if *duration_ms > max_duration_ms {
                return Err(format!(
                    "Shock duration must be between 100 and {} milliseconds, got {}",
                    max_duration_ms, duration_ms
                ));
            }
        }
    }

    Ok(())
}

fn http_client() -> &'static Client {
    HTTP_CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .expect("Failed to build PiShock HTTP client")
    })
}

#[derive(Debug, Clone)]
pub enum PiShockOp {
    Beep { duration: i32 },
    Vibrate { intensity: i32, duration: i32 },
    Shock { intensity: i32, duration_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTarget {
    pub client_id: u64,
    pub shocker_id: u64,
    pub device_name: String,
    pub shocker_name: String,
    pub is_paused: bool,
}

#[derive(Clone)]
struct BrokerHandle {
    sender: mpsc::Sender<BrokerRequest>,
}

enum BrokerRequest {
    Warmup {
        config: Config,
        response: oneshot::Sender<Result<(), String>>,
    },
    Publish {
        config: Config,
        operation: PiShockOp,
        response: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Default)]
struct BrokerOwnerState {
    auth_key: Option<BrokerAuthKey>,
    socket: Option<BrokerSocket>,
    cached_target: Option<CachedTarget>,
    reconnect_failures: u32,
    next_reconnect_at: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrokerAuthKey {
    username: String,
    apikey: String,
}

impl BrokerAuthKey {
    fn from_config(config: &Config) -> Self {
        Self {
            username: config.username.trim().to_owned(),
            apikey: config.apikey.trim().to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrokerTargetKey {
    username: String,
    apikey: String,
    selected_client_id: Option<u64>,
    selected_shocker_id: Option<u64>,
}

impl BrokerTargetKey {
    fn from_config(config: &Config) -> Self {
        Self {
            username: config.username.trim().to_owned(),
            apikey: config.apikey.trim().to_owned(),
            selected_client_id: config.selected_client_id,
            selected_shocker_id: config.selected_shocker_id,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedTarget {
    key: BrokerTargetKey,
    target: ResolvedTarget,
}

#[derive(Debug, Clone)]
struct ResolvedTarget {
    user_id: u64,
    client_id: u64,
    shocker_id: u64,
    device_name: String,
    shocker_name: String,
    max_intensity: i32,
    max_duration: i32,
    is_paused: bool,
    can_shock: bool,
    can_vibrate: bool,
    can_beep: bool,
}

#[derive(Debug, Deserialize)]
struct AuthUser {
    #[serde(rename = "UserId")]
    user_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnedDevice {
    client_id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    shockers: Vec<OwnedShocker>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnedShocker {
    #[serde(default)]
    name: String,
    shocker_id: u64,
    is_paused: bool,
    #[serde(default = "default_max_intensity")]
    max_intensity: i32,
    #[serde(default = "default_max_duration")]
    max_duration: i32,
    #[serde(default = "default_true")]
    can_shock: bool,
    #[serde(default = "default_true")]
    can_vibrate: bool,
    #[serde(default = "default_true")]
    can_beep: bool,
}

#[derive(Debug, Clone, Serialize)]
struct BrokerPublishEnvelope {
    #[serde(rename = "Operation")]
    operation: &'static str,
    #[serde(rename = "PublishCommands")]
    publish_commands: Vec<BrokerPublishCommand>,
}

#[derive(Debug, Clone, Serialize)]
struct BrokerPublishCommand {
    #[serde(rename = "Target")]
    target: String,
    #[serde(rename = "Body")]
    body: BrokerBody,
}

#[derive(Debug, Clone, Serialize)]
struct BrokerBody {
    id: u64,
    #[serde(rename = "m")]
    method: String,
    #[serde(rename = "i")]
    intensity: i32,
    #[serde(rename = "d")]
    duration_ms: u64,
    #[serde(rename = "r")]
    repeating: bool,
    #[serde(rename = "l")]
    metadata: BrokerMetadata,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BrokerMetadata {
    #[serde(rename = "u")]
    user_id: u64,
    #[serde(rename = "ty")]
    target_type: String,
    #[serde(rename = "w")]
    warning: bool,
    #[serde(rename = "h")]
    hold: bool,
    #[serde(rename = "o")]
    origin: String,
}

#[derive(Debug, Deserialize)]
struct BrokerResponse {
    #[serde(rename = "ErrorCode")]
    error_code: Option<String>,
    #[serde(rename = "IsError")]
    is_error: Option<bool>,
    #[serde(rename = "Message")]
    message: Option<String>,
}

fn default_max_intensity() -> i32 {
    100
}

fn default_max_duration() -> i32 {
    15
}

fn default_true() -> bool {
    true
}

fn broker_handle_state() -> &'static Mutex<Option<BrokerHandle>> {
    BROKER_HANDLE_STATE.get_or_init(|| Mutex::new(None))
}

fn broker_heartbeat_state() -> &'static StdRwLock<Option<Instant>> {
    BROKER_HEARTBEAT_STATE.get_or_init(|| StdRwLock::new(None))
}

fn last_successful_heartbeat() -> Option<Instant> {
    *broker_heartbeat_state()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn record_successful_heartbeat() {
    record_successful_heartbeat_at(Instant::now());
}

fn record_successful_heartbeat_at(heartbeat: Instant) {
    *broker_heartbeat_state()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(heartbeat);
}

fn clear_last_successful_heartbeat() {
    *broker_heartbeat_state()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

fn clear_broker_socket(state: &mut BrokerOwnerState) {
    state.socket = None;
    clear_last_successful_heartbeat();
}

fn mark_broker_socket_failed(state: &mut BrokerOwnerState) {
    clear_broker_socket(state);
    schedule_broker_reconnect(state);
}

async fn reconnect_broker(state: &mut BrokerOwnerState) -> Result<(), String> {
    if state.socket.is_some() {
        clear_broker_reconnect_backoff(state);
        return Ok(());
    }

    ensure_socket_connected(state).await
}

fn should_reconnect_broker(state: &BrokerOwnerState) -> bool {
    state.socket.is_none()
        && state.auth_key.is_some()
        && state
            .next_reconnect_at
            .is_some_and(|next_reconnect_at| Instant::now() >= next_reconnect_at)
}

fn schedule_broker_reconnect(state: &mut BrokerOwnerState) {
    state.reconnect_failures = state.reconnect_failures.saturating_add(1);
    let delay = broker_reconnect_delay(state.reconnect_failures);
    state.next_reconnect_at = Some(Instant::now() + delay);
}

fn clear_broker_reconnect_backoff(state: &mut BrokerOwnerState) {
    state.reconnect_failures = 0;
    state.next_reconnect_at = None;
}

fn broker_reconnect_delay(reconnect_failures: u32) -> Duration {
    let exponent = reconnect_failures.saturating_sub(1).min(4);
    BROKER_RECONNECT_BASE_DELAY
        .saturating_mul(1u32 << exponent)
        .min(BROKER_RECONNECT_MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use super::{
        broker_error_message, broker_reconnect_delay, build_broker_body, build_broker_connect_url,
        clear_broker_reconnect_backoff, clear_last_successful_heartbeat,
        discovered_targets_from_devices, is_pong_response, is_publish_success,
        last_heartbeat_elapsed, parse_broker_response, publish_target,
        record_successful_heartbeat_at, should_reconnect_broker, sync_session_config,
        validate_broker_auth, validate_control_config, BrokerAuthKey, BrokerMetadata,
        BrokerOwnerState, BrokerResponse, OwnedDevice, PiShockOp, ResolvedTarget,
        INVALID_BROKER_AUTH_MESSAGE,
    };
    use crate::config::Config;
    use std::{
        sync::{Mutex as StdMutex, OnceLock as StdOnceLock},
        time::{Duration, Instant},
    };
    use tokio_tungstenite::tungstenite::Message;

    fn sample_target() -> ResolvedTarget {
        ResolvedTarget {
            user_id: 12,
            client_id: 34,
            shocker_id: 56,
            device_name: "Desk".into(),
            shocker_name: "Collar".into(),
            max_intensity: 100,
            max_duration: 15,
            is_paused: false,
            can_shock: true,
            can_vibrate: true,
            can_beep: true,
        }
    }

    #[test]
    fn build_broker_body_maps_shock_operation() {
        let body = build_broker_body(
            &sample_target(),
            PiShockOp::Shock {
                intensity: 25,
                duration_ms: 300,
            },
        );

        assert_eq!(body.id, 56);
        assert_eq!(body.method, "s");
        assert_eq!(body.intensity, 25);
        assert_eq!(body.duration_ms, 300);
        assert!(body.repeating);
        assert_eq!(
            body.metadata,
            BrokerMetadata {
                user_id: 12,
                target_type: "api".into(),
                warning: false,
                hold: false,
                origin: "cs2shock".into(),
            }
        );
    }

    #[test]
    fn publish_target_uses_api_channel() {
        assert_eq!(publish_target(&sample_target()), "c34-ops");
    }

    #[test]
    fn parse_broker_response_accepts_non_error_payloads() {
        let message = Message::Text(r#"{"Message":"PONG"}"#.into());
        let parsed = parse_broker_response(message).unwrap().unwrap();
        assert_eq!(parsed.message.as_deref(), Some("PONG"));
    }

    #[test]
    fn broker_error_message_maps_auth_error() {
        let response = BrokerResponse {
            error_code: Some("AUTH_TOKEN_ERROR".into()),
            is_error: Some(true),
            message: Some("User is not logged in.".into()),
        };
        assert_eq!(
            broker_error_message(&response).as_deref(),
            Some(INVALID_BROKER_AUTH_MESSAGE)
        );
    }

    #[test]
    fn publish_success_requires_documented_acknowledgement() {
        let response = BrokerResponse {
            error_code: None,
            is_error: Some(false),
            message: Some("Publish successful.".into()),
        };
        assert!(is_publish_success(&response));

        let response = BrokerResponse {
            error_code: None,
            is_error: Some(false),
            message: Some("PONG".into()),
        };
        assert!(!is_publish_success(&response));
    }

    #[test]
    fn pong_response_requires_documented_message() {
        let response = BrokerResponse {
            error_code: None,
            is_error: Some(false),
            message: Some("PONG".into()),
        };
        assert!(is_pong_response(&response));
    }

    #[test]
    fn validate_control_config_requires_username_api_key_and_selected_shocker() {
        let mut config = Config::default();
        config.username = "user".into();
        config.apikey = "key".into();
        config.selected_client_id = Some(34);
        config.selected_shocker_id = Some(56);
        assert!(validate_control_config(&config).is_ok());

        config.apikey.clear();
        assert_eq!(
            validate_control_config(&config).unwrap_err(),
            "PiShock API key is required."
        );

        config.apikey = "key".into();
        config.selected_shocker_id = None;
        assert_eq!(
            validate_control_config(&config).unwrap_err(),
            "Select a PiShock shocker before sending commands."
        );
    }

    #[test]
    fn validate_broker_auth_requires_username_and_api_key() {
        let mut config = Config::default();
        assert_eq!(
            validate_broker_auth(&config).unwrap_err(),
            "PiShock username is required."
        );

        config.username = "user".into();
        assert_eq!(
            validate_broker_auth(&config).unwrap_err(),
            "PiShock API key is required."
        );

        config.apikey = "key".into();
        assert!(validate_broker_auth(&config).is_ok());
    }

    #[test]
    fn broker_url_uses_username_and_api_key_query_keys() {
        let url = build_broker_connect_url(&BrokerAuthKey {
            username: "my user".into(),
            apikey: "abc+123".into(),
        })
        .unwrap()
        .to_string();
        assert!(url.contains("Username=my+user"));
        assert!(url.contains("ApiKey=abc%2B123"));
    }

    #[test]
    fn last_heartbeat_elapsed_starts_empty() {
        let _guard = heartbeat_test_guard()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        clear_last_successful_heartbeat();
        assert_eq!(last_heartbeat_elapsed(), None);
    }

    #[test]
    fn broker_reconnect_delay_backs_off_and_caps() {
        assert_eq!(broker_reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(broker_reconnect_delay(2), Duration::from_secs(4));
        assert_eq!(broker_reconnect_delay(3), Duration::from_secs(8));
        assert_eq!(broker_reconnect_delay(4), Duration::from_secs(16));
        assert_eq!(broker_reconnect_delay(5), Duration::from_secs(30));
        assert_eq!(broker_reconnect_delay(6), Duration::from_secs(30));
    }

    #[test]
    fn clear_broker_reconnect_backoff_resets_retry_state() {
        let mut state = BrokerOwnerState::default();
        state.reconnect_failures = 3;
        state.next_reconnect_at = Some(Instant::now() + Duration::from_secs(8));

        clear_broker_reconnect_backoff(&mut state);

        assert_eq!(state.reconnect_failures, 0);
        assert_eq!(state.next_reconnect_at, None);
    }

    #[test]
    fn should_reconnect_broker_requires_due_retry() {
        let mut state = BrokerOwnerState::default();
        state.auth_key = Some(BrokerAuthKey {
            username: "user".into(),
            apikey: "key".into(),
        });

        assert!(!should_reconnect_broker(&state));

        state.next_reconnect_at = Some(Instant::now() + Duration::from_secs(1));
        assert!(!should_reconnect_broker(&state));

        state.next_reconnect_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(should_reconnect_broker(&state));
    }

    #[test]
    fn sync_session_config_clears_cached_target_when_selected_shocker_changes() {
        let mut state = BrokerOwnerState::default();
        let mut config = Config::default();
        config.username = "user".into();
        config.apikey = "key".into();
        config.selected_client_id = Some(34);
        config.selected_shocker_id = Some(56);

        sync_session_config(&mut state, &config);
        state.cached_target = Some(super::CachedTarget {
            key: super::BrokerTargetKey::from_config(&config),
            target: sample_target(),
        });

        config.selected_shocker_id = Some(57);
        sync_session_config(&mut state, &config);
        assert!(state.cached_target.is_none());
    }

    #[test]
    fn last_heartbeat_elapsed_reports_recorded_age() {
        let _guard = heartbeat_test_guard()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        clear_last_successful_heartbeat();
        record_successful_heartbeat_at(Instant::now() - Duration::from_secs(2));

        let elapsed = last_heartbeat_elapsed().expect("heartbeat should be recorded");
        assert!(elapsed >= Duration::from_secs(2));
        assert!(elapsed < Duration::from_secs(3));
    }

    #[test]
    fn sync_session_config_clears_heartbeat_when_auth_changes() {
        let _guard = heartbeat_test_guard()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        clear_last_successful_heartbeat();
        record_successful_heartbeat_at(Instant::now() - Duration::from_secs(2));

        let mut state = BrokerOwnerState::default();
        state.auth_key = Some(BrokerAuthKey {
            username: "old-user".into(),
            apikey: "old-key".into(),
        });
        state.reconnect_failures = 2;
        state.next_reconnect_at = Some(Instant::now() + Duration::from_secs(4));

        let mut config = Config::default();
        config.username = "new-user".into();
        config.apikey = "new-key".into();

        sync_session_config(&mut state, &config);
        assert_eq!(last_heartbeat_elapsed(), None);
        assert_eq!(state.reconnect_failures, 0);
        assert_eq!(state.next_reconnect_at, None);
    }

    #[test]
    fn discovered_targets_include_owned_device_and_shocker_names() {
        let devices: Vec<OwnedDevice> = serde_json::from_value(serde_json::json!([
            {
                "clientId": 70,
                "name": "Living Room",
                "shockers": [
                    {
                        "name": "Left",
                        "shockerId": 701,
                        "isPaused": false
                    },
                    {
                        "name": "Right",
                        "shockerId": 702,
                        "isPaused": true
                    }
                ]
            }
        ]))
        .unwrap();

        let targets = discovered_targets_from_devices(&devices);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].device_name, "Living Room");
        assert_eq!(targets[0].shocker_name, "Left");
        assert_eq!(targets[0].client_id, 70);
        assert_eq!(targets[0].shocker_id, 701);
        assert!(!targets[0].is_paused);
        assert!(targets[1].is_paused);
    }

    fn heartbeat_test_guard() -> &'static StdMutex<()> {
        static GUARD: StdOnceLock<StdMutex<()>> = StdOnceLock::new();
        GUARD.get_or_init(|| StdMutex::new(()))
    }
}
