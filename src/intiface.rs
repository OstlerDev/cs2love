use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock},
    time::{Duration, Instant},
};

use buttplug::{
    connector::ButtplugRemoteClientConnector,
    device::{ClientDeviceCommandValue, ClientDeviceOutputCommand},
    serializer::ButtplugClientJSONSerializer,
    ButtplugClient, ButtplugClientEvent, ButtplugWebsocketClientTransport,
};
use futures::StreamExt;
use log::{debug, error, info, warn};
use tokio::sync::Mutex as TokioMutex;

const CLIENT_NAME: &str = "cs2love";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

static CLIENT: OnceLock<StdRwLock<Option<Arc<ButtplugClient>>>> = OnceLock::new();
static LAST_EVENT_AT: OnceLock<StdRwLock<Option<Instant>>> = OnceLock::new();
static DEVICE_LOCKS: OnceLock<StdMutex<HashMap<String, Arc<TokioMutex<()>>>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredToy {
    pub index: u32,
    pub name: String,
}

fn client_cell() -> &'static StdRwLock<Option<Arc<ButtplugClient>>> {
    CLIENT.get_or_init(|| StdRwLock::new(None))
}

fn last_event_cell() -> &'static StdRwLock<Option<Instant>> {
    LAST_EVENT_AT.get_or_init(|| StdRwLock::new(None))
}

fn device_locks() -> &'static StdMutex<HashMap<String, Arc<TokioMutex<()>>>> {
    DEVICE_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn current_client() -> Option<Arc<ButtplugClient>> {
    client_cell().read().ok()?.clone()
}

fn store_client(client: Option<Arc<ButtplugClient>>) {
    if let Ok(mut guard) = client_cell().write() {
        *guard = client;
    }
}

fn touch_last_event() {
    if let Ok(mut guard) = last_event_cell().write() {
        *guard = Some(Instant::now());
    }
}

fn clear_last_event() {
    if let Ok(mut guard) = last_event_cell().write() {
        *guard = None;
    }
}

fn lock_for_device(name: &str) -> Arc<TokioMutex<()>> {
    let mut map = device_locks().lock().expect("device locks poisoned");
    map.entry(name.to_string())
        .or_insert_with(|| Arc::new(TokioMutex::new(())))
        .clone()
}

pub fn last_event_elapsed() -> Option<Duration> {
    last_event_cell().read().ok()?.map(|t| t.elapsed())
}

pub fn is_connected() -> bool {
    current_client().map(|client| client.connected()).unwrap_or(false)
}

pub async fn connect(url: &str) -> Result<(), String> {
    disconnect().await;

    let client = ButtplugClient::new(CLIENT_NAME);
    let mut events = client.event_stream();

    let connector = ButtplugRemoteClientConnector::<
        ButtplugWebsocketClientTransport,
        ButtplugClientJSONSerializer,
    >::new(ButtplugWebsocketClientTransport::new_insecure_connector(
        url,
    ));

    let connect_result = tokio::time::timeout(CONNECT_TIMEOUT, client.connect(connector))
        .await
        .map_err(|_| format!("Timed out connecting to Intiface at {url}"))?;

    connect_result.map_err(|e| format!("Failed to connect to Intiface at {url}: {e}"))?;

    info!(target: "Intiface", "Connected to Intiface server at {url}");

    store_client(Some(Arc::new(client)));
    touch_last_event();

    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            touch_last_event();
            match event {
                ButtplugClientEvent::DeviceAdded(device) => {
                    info!(target: "Intiface", "Device connected: {}", device.name());
                }
                ButtplugClientEvent::DeviceRemoved(device) => {
                    info!(target: "Intiface", "Device disconnected: {}", device.name());
                }
                ButtplugClientEvent::ScanningFinished => {
                    debug!(target: "Intiface", "Device scanning finished");
                }
                ButtplugClientEvent::ServerDisconnect => {
                    warn!(target: "Intiface", "Intiface server disconnected");
                    store_client(None);
                    clear_last_event();
                }
                ButtplugClientEvent::Error(err) => {
                    error!(target: "Intiface", "Server error: {err}");
                }
                _ => {}
            }
        }
        debug!(target: "Intiface", "Event stream closed");
    });

    Ok(())
}

pub async fn disconnect() {
    let Some(client) = current_client() else {
        return;
    };
    store_client(None);
    clear_last_event();
    if let Err(err) = client.disconnect().await {
        warn!(target: "Intiface", "Disconnect returned an error: {err}");
    }
    info!(target: "Intiface", "Disconnected from Intiface server");
}

pub fn list_devices() -> Vec<DiscoveredToy> {
    let Some(client) = current_client() else {
        return Vec::new();
    };
    client
        .devices()
        .into_iter()
        .map(|(index, device)| DiscoveredToy {
            index,
            name: device.name().clone(),
        })
        .collect()
}

pub async fn vibrate_for(toy_names: Vec<String>, strength_percent: u32, duration_ms: u64) {
    if toy_names.is_empty() || duration_ms == 0 {
        return;
    }

    let Some(client) = current_client() else {
        warn!(target: "Intiface", "Skipping vibration; not connected to Intiface");
        return;
    };

    let strength = (strength_percent.min(100) as f64) / 100.0;
    let duration = Duration::from_millis(duration_ms);

    let matching_devices: Vec<_> = client
        .devices()
        .into_values()
        .filter(|device| toy_names.iter().any(|name| name == device.name()))
        .collect();

    if matching_devices.is_empty() {
        debug!(
            target: "Intiface",
            "Vibration request matched no connected toys (requested {} toy(s))",
            toy_names.len()
        );
        return;
    }

    for device in matching_devices {
        let device_mutex = lock_for_device(device.name());
        tokio::spawn(async move {
            let _guard = device_mutex.lock().await;
            let device_name = device.name().clone();

            if let Err(err) = device
                .run_output(&ClientDeviceOutputCommand::Vibrate(
                    ClientDeviceCommandValue::Percent(strength),
                ))
                .await
            {
                error!(target: "Intiface", "Failed to vibrate `{device_name}`: {err}");
                return;
            }
            touch_last_event();

            tokio::time::sleep(duration).await;

            if let Err(err) = device.stop().await {
                error!(target: "Intiface", "Failed to stop `{device_name}`: {err}");
                return;
            }
            touch_last_event();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{clear_last_event, last_event_elapsed, touch_last_event};
    use std::sync::Mutex;

    // The LAST_EVENT_AT helpers all read/write a single process-global OnceLock,
    // so tests that touch it must run sequentially to stay deterministic.
    static SERIALIZE: Mutex<()> = Mutex::new(());

    #[test]
    fn last_event_helpers_round_trip_through_global_state() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());

        clear_last_event();
        assert!(last_event_elapsed().is_none());

        touch_last_event();
        let elapsed = last_event_elapsed().expect("touch should populate last event");
        assert!(elapsed.as_secs() < 5);

        clear_last_event();
        assert!(last_event_elapsed().is_none());
    }
}
