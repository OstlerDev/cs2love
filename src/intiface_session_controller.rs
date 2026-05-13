use std::{sync::mpsc::Sender, time::Duration};

use log::warn;

use crate::{
    config::Config,
    intiface::{self, DiscoveredToy},
};

const SCAN_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum SessionAsyncResult {
    Connect {
        request_id: u64,
        result: Result<(), String>,
    },
    Discovery {
        request_id: u64,
        result: Result<Vec<DiscoveredToy>, String>,
    },
}

#[derive(Debug, Default)]
pub struct IntifaceSessionController {
    discovered_toys: Vec<DiscoveredToy>,
    committed_url: Option<String>,
    next_async_request_id: u64,
    latest_connect_request_id: Option<u64>,
    latest_discovery_request_id: Option<u64>,
    connect_in_progress: bool,
    discovery_in_progress: bool,
    connect_status_error: Option<String>,
    toy_status: Option<String>,
}

impl IntifaceSessionController {
    pub fn new(config: &Config) -> Self {
        Self {
            committed_url: Some(normalized_url(config)),
            ..Self::default()
        }
    }

    pub fn sync_startup(&mut self, sender: &Sender<SessionAsyncResult>, config: &Config) {
        if !is_valid_url(&config.intiface_websocket_url) {
            return;
        }
        self.start_connect(sender, config);
    }

    pub fn refresh_after_url_commit(
        &mut self,
        sender: &Sender<SessionAsyncResult>,
        config: &mut Config,
    ) {
        normalize_url_field(config);
        let current_url = normalized_url(config);
        if self.committed_url.as_deref() == Some(&current_url) {
            return;
        }

        self.committed_url = Some(current_url);
        self.discovered_toys.clear();
        self.toy_status = None;
        self.connect_status_error = None;

        if !is_valid_url(&config.intiface_websocket_url) {
            self.connect_status_error =
                Some("Intiface URL must be a ws:// or wss:// address".into());
            tokio::spawn(async {
                intiface::disconnect().await;
            });
            return;
        }

        self.start_connect(sender, config);
    }

    pub fn refresh_manually(&mut self, sender: &Sender<SessionAsyncResult>, config: &Config) {
        if !is_valid_url(&config.intiface_websocket_url) {
            self.connect_status_error =
                Some("Enter a valid ws:// or wss:// URL before scanning".into());
            return;
        }

        self.start_discovery(sender);
        if !self.is_connected_optimistically() {
            self.start_connect(sender, config);
        }
    }

    pub fn handle_async_result(&mut self, result: SessionAsyncResult, _config: &mut Config) {
        match result {
            SessionAsyncResult::Connect { request_id, result } => {
                if self.latest_connect_request_id != Some(request_id) {
                    return;
                }
                self.connect_in_progress = false;
                match result {
                    Ok(()) => {
                        self.connect_status_error = None;
                    }
                    Err(error) => {
                        self.connect_status_error = Some(error);
                    }
                }
            }
            SessionAsyncResult::Discovery { request_id, result } => {
                if self.latest_discovery_request_id != Some(request_id) {
                    return;
                }
                self.discovery_in_progress = false;
                match result {
                    Ok(toys) => {
                        self.toy_status = Some(if toys.is_empty() {
                            "No toys were discovered. Make sure your toy is on and in range.".into()
                        } else {
                            format!("Discovered {} toy(s).", toys.len())
                        });
                        self.discovered_toys = toys;
                    }
                    Err(error) => {
                        self.toy_status = Some(error);
                        self.discovered_toys.clear();
                    }
                }
            }
        }
    }

    pub fn discovered_toys(&self) -> &[DiscoveredToy] {
        &self.discovered_toys
    }

    pub fn discovery_in_progress(&self) -> bool {
        self.discovery_in_progress
    }

    pub fn connect_in_progress(&self) -> bool {
        self.connect_in_progress
    }

    pub fn toy_status(&self) -> Option<&str> {
        self.toy_status.as_deref()
    }

    pub fn connection_status_label(&self) -> String {
        if self.connect_in_progress {
            return "Intiface: connecting...".into();
        }

        if let Some(error) = self.connect_status_error.as_deref() {
            return format!("Intiface: {error}");
        }

        if intiface::is_connected() {
            match intiface::last_event_elapsed() {
                Some(elapsed) => {
                    format!("Intiface: connected, last event {}s ago", elapsed.as_secs())
                }
                None => "Intiface: connected.".into(),
            }
        } else {
            "Intiface: not connected.".into()
        }
    }

    fn next_request_id(&mut self) -> u64 {
        self.next_async_request_id += 1;
        self.next_async_request_id
    }

    fn is_connected_optimistically(&self) -> bool {
        self.connect_in_progress
            || (self.latest_connect_request_id.is_some() && self.connect_status_error.is_none())
    }

    fn start_connect(&mut self, sender: &Sender<SessionAsyncResult>, config: &Config) {
        let request_id = self.next_request_id();
        self.latest_connect_request_id = Some(request_id);
        self.connect_in_progress = true;
        self.connect_status_error = None;
        let tx = sender.clone();
        let url = config.intiface_websocket_url.trim().to_string();
        tokio::spawn(async move {
            let result = intiface::connect(&url).await;
            if let Err(err) = tx.send(SessionAsyncResult::Connect { request_id, result }) {
                warn!(target: "Intiface", "Could not deliver connect result: {err}");
            }
        });
    }

    fn start_discovery(&mut self, sender: &Sender<SessionAsyncResult>) {
        let request_id = self.next_request_id();
        self.latest_discovery_request_id = Some(request_id);
        self.discovery_in_progress = true;
        self.toy_status = Some("Scanning for toys...".into());
        let tx = sender.clone();
        tokio::spawn(async move {
            let result = run_discovery().await;
            if let Err(err) = tx.send(SessionAsyncResult::Discovery { request_id, result }) {
                warn!(target: "Intiface", "Could not deliver discovery result: {err}");
            }
        });
    }
}

async fn run_discovery() -> Result<Vec<DiscoveredToy>, String> {
    if !intiface::is_connected() {
        return Err("Connect to Intiface before scanning".into());
    }

    intiface::start_scanning().await?;
    tokio::time::sleep(SCAN_DURATION).await;
    let _ = intiface::stop_scanning().await;
    Ok(intiface::list_devices())
}

fn normalize_url_field(config: &mut Config) {
    config.intiface_websocket_url = config.intiface_websocket_url.trim().to_owned();
}

fn normalized_url(config: &Config) -> String {
    config.intiface_websocket_url.trim().to_owned()
}

pub fn is_valid_url(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    match url::Url::parse(trimmed) {
        Ok(parsed) => matches!(parsed.scheme(), "ws" | "wss"),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_valid_url, normalize_url_field, normalized_url};
    use crate::config::Config;

    #[test]
    fn is_valid_url_accepts_ws_and_wss_schemes() {
        assert!(is_valid_url("ws://127.0.0.1:12345"));
        assert!(is_valid_url("wss://intiface.example.com:443"));
    }

    #[test]
    fn is_valid_url_rejects_other_schemes_and_empty_strings() {
        assert!(!is_valid_url(""));
        assert!(!is_valid_url("   "));
        assert!(!is_valid_url("http://127.0.0.1:12345"));
        assert!(!is_valid_url("not-a-url"));
    }

    #[test]
    fn normalize_url_field_trims_in_place() {
        let mut config = Config::default();
        config.intiface_websocket_url = "  ws://example/  ".into();

        normalize_url_field(&mut config);

        assert_eq!(config.intiface_websocket_url, "ws://example/");
    }

    #[test]
    fn normalized_url_returns_trimmed_value() {
        let mut config = Config::default();
        config.intiface_websocket_url = "  ws://example/  ".into();

        assert_eq!(normalized_url(&config), "ws://example/");
    }
}
