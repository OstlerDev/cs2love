use std::{
    sync::mpsc::Sender,
    time::{Duration, Instant},
};

use log::warn;

use crate::{
    config::Config,
    intiface::{self, DiscoveredToy},
};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum SessionAsyncResult {
    Connect {
        request_id: u64,
        result: Result<(), String>,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum ReconnectAction {
    Idle,
    Schedule(Instant),
    Fire,
}

#[derive(Debug, Default)]
pub struct IntifaceSessionController {
    committed_url: Option<String>,
    next_async_request_id: u64,
    latest_connect_request_id: Option<u64>,
    connect_in_progress: bool,
    connect_status_error: Option<String>,
    next_reconnect_attempt_at: Option<Instant>,
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
        self.connect_status_error = None;
        self.next_reconnect_attempt_at = None;

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
        }
    }

    pub fn available_toys(&self) -> Vec<DiscoveredToy> {
        intiface::list_devices()
    }

    pub fn connection_status_label(&self) -> String {
        self.connection_status_label_at(Instant::now())
    }

    fn connection_status_label_at(&self, now: Instant) -> String {
        if self.connect_in_progress {
            return "Intiface: connecting...".into();
        }

        if intiface::is_connected() {
            return match intiface::last_event_elapsed() {
                Some(elapsed) => {
                    format!("Intiface: connected, last event {}s ago", elapsed.as_secs())
                }
                None => "Intiface: connected.".into(),
            };
        }

        let retry_suffix = self
            .next_reconnect_attempt_at
            .map(|at| {
                let remaining = at.saturating_duration_since(now).as_secs() + 1;
                format!(" (retrying in {remaining}s)")
            })
            .unwrap_or_default();

        if let Some(error) = self.connect_status_error.as_deref() {
            return format!("Intiface: {error}{retry_suffix}");
        }

        if retry_suffix.is_empty() {
            "Intiface: not connected.".into()
        } else {
            format!("Intiface: not connected{retry_suffix}")
        }
    }

    fn next_request_id(&mut self) -> u64 {
        self.next_async_request_id += 1;
        self.next_async_request_id
    }

    fn start_connect(&mut self, sender: &Sender<SessionAsyncResult>, config: &Config) {
        let request_id = self.next_request_id();
        self.latest_connect_request_id = Some(request_id);
        self.connect_in_progress = true;
        self.connect_status_error = None;
        self.next_reconnect_attempt_at = None;
        let tx = sender.clone();
        let url = config.intiface_websocket_url.trim().to_string();
        tokio::spawn(async move {
            let result = intiface::connect(&url).await;
            if let Err(err) = tx.send(SessionAsyncResult::Connect { request_id, result }) {
                warn!(target: "Intiface", "Could not deliver connect result: {err}");
            }
        });
    }

    pub fn pump(&mut self, sender: &Sender<SessionAsyncResult>, config: &Config) {
        self.pump_at(Instant::now(), sender, config);
    }

    fn pump_at(
        &mut self,
        now: Instant,
        sender: &Sender<SessionAsyncResult>,
        config: &Config,
    ) {
        let url_valid = is_valid_url(&config.intiface_websocket_url);
        let connected = intiface::is_connected();
        match self.decide_reconnect_at(now, url_valid, connected) {
            ReconnectAction::Idle => {}
            ReconnectAction::Schedule(at) => {
                self.next_reconnect_attempt_at = Some(at);
            }
            ReconnectAction::Fire => {
                self.next_reconnect_attempt_at = None;
                self.start_connect(sender, config);
            }
        }
    }

    fn decide_reconnect_at(
        &self,
        now: Instant,
        url_valid: bool,
        connected: bool,
    ) -> ReconnectAction {
        if let Some(at) = self.next_reconnect_attempt_at {
            return if now >= at {
                ReconnectAction::Fire
            } else {
                ReconnectAction::Idle
            };
        }
        if !self.connect_in_progress && !connected && url_valid {
            return ReconnectAction::Schedule(now + RECONNECT_DELAY);
        }
        ReconnectAction::Idle
    }
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
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::{
        is_valid_url, normalize_url_field, normalized_url, IntifaceSessionController,
        ReconnectAction, SessionAsyncResult, RECONNECT_DELAY,
    };
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

    #[test]
    fn connection_status_label_reports_default_state() {
        let controller = IntifaceSessionController::default();
        assert_eq!(controller.connection_status_label(), "Intiface: not connected.");
    }

    #[test]
    fn connection_status_label_reports_in_flight_connect() {
        let mut controller = IntifaceSessionController::default();
        controller.connect_in_progress = true;
        assert_eq!(controller.connection_status_label(), "Intiface: connecting...");
    }

    #[test]
    fn connection_status_label_surfaces_connect_error() {
        let mut controller = IntifaceSessionController::default();
        controller.connect_status_error = Some("boom".into());
        assert_eq!(controller.connection_status_label(), "Intiface: boom");
    }

    #[test]
    fn handle_async_result_ignores_stale_connect_response() {
        let mut controller = IntifaceSessionController::default();
        controller.latest_connect_request_id = Some(7);
        controller.connect_in_progress = true;

        let mut config = Config::default();
        controller.handle_async_result(
            SessionAsyncResult::Connect {
                request_id: 3,
                result: Ok(()),
            },
            &mut config,
        );

        assert!(controller.connect_in_progress);
        assert!(controller.connect_status_error.is_none());
    }

    #[test]
    fn handle_async_result_records_latest_connect_outcome() {
        let mut controller = IntifaceSessionController::default();
        controller.latest_connect_request_id = Some(2);
        controller.connect_in_progress = true;
        controller.connect_status_error = Some("stale".into());

        let mut config = Config::default();
        controller.handle_async_result(
            SessionAsyncResult::Connect {
                request_id: 2,
                result: Ok(()),
            },
            &mut config,
        );

        assert!(!controller.connect_in_progress);
        assert!(controller.connect_status_error.is_none());
    }

    fn fixed_now() -> Instant {
        Instant::now()
    }

    #[test]
    fn decide_reconnect_schedules_when_disconnected_with_valid_url() {
        let controller = IntifaceSessionController::default();
        let now = fixed_now();
        assert_eq!(
            controller.decide_reconnect_at(now, true, false),
            ReconnectAction::Schedule(now + RECONNECT_DELAY)
        );
    }

    #[test]
    fn decide_reconnect_idle_with_invalid_url() {
        let controller = IntifaceSessionController::default();
        assert_eq!(
            controller.decide_reconnect_at(fixed_now(), false, false),
            ReconnectAction::Idle
        );
    }

    #[test]
    fn decide_reconnect_idle_while_connect_in_progress() {
        let mut controller = IntifaceSessionController::default();
        controller.connect_in_progress = true;
        assert_eq!(
            controller.decide_reconnect_at(fixed_now(), true, false),
            ReconnectAction::Idle
        );
    }

    #[test]
    fn decide_reconnect_idle_when_already_connected() {
        let controller = IntifaceSessionController::default();
        assert_eq!(
            controller.decide_reconnect_at(fixed_now(), true, true),
            ReconnectAction::Idle
        );
    }

    #[test]
    fn decide_reconnect_idle_while_pending_delay_has_not_elapsed() {
        let mut controller = IntifaceSessionController::default();
        let now = fixed_now();
        controller.next_reconnect_attempt_at = Some(now + Duration::from_secs(1));
        assert_eq!(
            controller.decide_reconnect_at(now, true, false),
            ReconnectAction::Idle
        );
    }

    #[test]
    fn decide_reconnect_fires_when_pending_delay_has_elapsed() {
        let mut controller = IntifaceSessionController::default();
        let now = fixed_now();
        controller.next_reconnect_attempt_at = Some(now);
        assert_eq!(
            controller.decide_reconnect_at(now, true, false),
            ReconnectAction::Fire
        );
    }

    #[test]
    fn connection_status_label_appends_retry_countdown_when_disconnected() {
        let mut controller = IntifaceSessionController::default();
        let now = Instant::now();
        controller.next_reconnect_attempt_at = Some(now + Duration::from_secs(2));
        assert_eq!(
            controller.connection_status_label_at(now),
            "Intiface: not connected (retrying in 3s)"
        );
    }

    #[test]
    fn connection_status_label_combines_error_and_retry_countdown() {
        let mut controller = IntifaceSessionController::default();
        let now = Instant::now();
        controller.connect_status_error = Some("boom".into());
        controller.next_reconnect_attempt_at = Some(now + Duration::from_millis(500));
        assert_eq!(
            controller.connection_status_label_at(now),
            "Intiface: boom (retrying in 1s)"
        );
    }

    #[tokio::test]
    async fn refresh_after_url_commit_cancels_pending_retry_for_valid_url() {
        let (tx, _rx) = mpsc::channel();
        let mut controller = IntifaceSessionController::default();
        controller.next_reconnect_attempt_at = Some(Instant::now() + Duration::from_secs(2));
        let mut config = Config::default();
        config.intiface_websocket_url = "ws://example/".into();

        controller.refresh_after_url_commit(&tx, &mut config);

        assert!(controller.next_reconnect_attempt_at.is_none());
    }

    #[tokio::test]
    async fn refresh_after_url_commit_cancels_pending_retry_for_invalid_url() {
        let (tx, _rx) = mpsc::channel();
        let mut controller = IntifaceSessionController::default();
        controller.next_reconnect_attempt_at = Some(Instant::now() + Duration::from_secs(2));
        let mut config = Config::default();
        config.intiface_websocket_url = "not-a-url".into();

        controller.refresh_after_url_commit(&tx, &mut config);

        assert!(controller.next_reconnect_attempt_at.is_none());
        assert!(controller.connect_status_error.is_some());
    }
}
