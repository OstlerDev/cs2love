use std::sync::{mpsc::Sender, Arc};

use tokio::sync::RwLock;

use crate::{config::Config, pishock};

#[derive(Debug)]
pub enum SessionAsyncResult {
    TargetDiscovery {
        request_id: u64,
        result: Result<Vec<pishock::DiscoveredTarget>, String>,
    },
    BrokerWarmup {
        request_id: u64,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthSnapshot {
    username: String,
    apikey: String,
}

impl AuthSnapshot {
    fn from_config(config: &Config) -> Self {
        Self {
            username: config.username.trim().to_owned(),
            apikey: config.apikey.trim().to_owned(),
        }
    }
}

#[derive(Debug, Default)]
pub struct PishockSessionController {
    discovered_targets: Vec<pishock::DiscoveredTarget>,
    committed_auth: Option<AuthSnapshot>,
    next_async_request_id: u64,
    latest_target_discovery_request_id: Option<u64>,
    latest_broker_warmup_request_id: Option<u64>,
    discovery_in_progress: bool,
    broker_warmup_in_progress: bool,
    broker_status_error: Option<String>,
    target_status: Option<String>,
}

impl PishockSessionController {
    pub fn new(config: &Config) -> Self {
        Self {
            committed_auth: Some(AuthSnapshot::from_config(config)),
            ..Self::default()
        }
    }

    pub fn sync_startup(&mut self, sender: &Sender<SessionAsyncResult>, config: &Config) {
        self.sync_pishock_state_if_ready(sender, config, "Loading owned PiShock shockers...");
    }

    pub fn refresh_after_auth_commit(
        &mut self,
        sender: &Sender<SessionAsyncResult>,
        config: &mut Config,
    ) {
        normalize_auth_fields(config);
        let current_auth = AuthSnapshot::from_config(config);
        if self.committed_auth.as_ref() == Some(&current_auth) {
            return;
        }

        self.committed_auth = Some(current_auth);
        clear_selected_target(config);
        self.discovered_targets.clear();
        self.target_status = None;
        self.sync_pishock_state_if_ready(sender, config, "Refreshing owned PiShock shockers...");
    }

    pub fn refresh_manually(&mut self, sender: &Sender<SessionAsyncResult>, config: &Config) {
        self.start_target_discovery(sender, config, "Loading owned PiShock shockers...");
        if has_auth_credentials(config) {
            self.start_broker_warmup(sender, config);
        }
    }

    pub fn handle_async_result(&mut self, result: SessionAsyncResult, config: &mut Config) {
        match result {
            SessionAsyncResult::TargetDiscovery { request_id, result } => {
                if self.latest_target_discovery_request_id != Some(request_id) {
                    return;
                }

                self.discovery_in_progress = false;
                match result {
                    Ok(targets) => {
                        let selected_target = targets
                            .iter()
                            .find(|target| {
                                Some(target.client_id) == config.selected_client_id
                                    && Some(target.shocker_id) == config.selected_shocker_id
                            })
                            .cloned();

                        self.discovered_targets = targets;
                        if let Some(target) = selected_target {
                            apply_selected_target(config, &target);
                        } else {
                            clear_selected_target(config);
                        }

                        self.target_status = Some(if self.discovered_targets.is_empty() {
                            "No owned PiShock shockers were returned for this account.".into()
                        } else {
                            format!("Loaded {} PiShock shockers.", self.discovered_targets.len())
                        });
                    }
                    Err(error) => {
                        self.target_status = Some(error);
                        self.discovered_targets.clear();
                    }
                }
            }
            SessionAsyncResult::BrokerWarmup { request_id, result } => {
                if self.latest_broker_warmup_request_id != Some(request_id) {
                    return;
                }

                self.broker_warmup_in_progress = false;
                match result {
                    Ok(()) => {
                        self.broker_status_error = None;
                    }
                    Err(error) => {
                        self.broker_status_error = Some(error);
                    }
                }
            }
        }
    }

    pub fn set_target_status(&mut self, status: impl Into<String>) {
        self.target_status = Some(status.into());
    }

    pub fn discovered_targets(&self) -> &[pishock::DiscoveredTarget] {
        &self.discovered_targets
    }

    pub fn discovery_in_progress(&self) -> bool {
        self.discovery_in_progress
    }

    pub fn target_status(&self) -> Option<&str> {
        self.target_status.as_deref()
    }

    pub fn broker_status_label(&self) -> String {
        if self.broker_warmup_in_progress {
            return "PiShock API: connecting...".into();
        }

        if let Some(elapsed) = pishock::last_heartbeat_elapsed() {
            return format!("PiShock API: last heartbeat {}s ago", elapsed.as_secs());
        }

        if let Some(error) = self.broker_status_error.as_deref() {
            return format!("PiShock API: {error}");
        }

        "PiShock API: not connected yet.".into()
    }

    fn next_request_id(&mut self) -> u64 {
        self.next_async_request_id += 1;
        self.next_async_request_id
    }

    fn start_target_discovery(
        &mut self,
        sender: &Sender<SessionAsyncResult>,
        config: &Config,
        status: &str,
    ) {
        let request_id = self.next_request_id();
        self.latest_target_discovery_request_id = Some(request_id);
        self.discovery_in_progress = true;
        self.target_status = Some(status.into());
        let tx = sender.clone();
        let config = Arc::new(RwLock::new(config.clone()));
        tokio::spawn(async move {
            let result = pishock::discover_targets(config).await;
            let _ = tx.send(SessionAsyncResult::TargetDiscovery { request_id, result });
        });
    }

    fn start_broker_warmup(&mut self, sender: &Sender<SessionAsyncResult>, config: &Config) {
        let request_id = self.next_request_id();
        self.latest_broker_warmup_request_id = Some(request_id);
        self.broker_warmup_in_progress = true;
        self.broker_status_error = None;
        let tx = sender.clone();
        let config = Arc::new(RwLock::new(config.clone()));
        tokio::spawn(async move {
            let result = pishock::warmup(config).await;
            let _ = tx.send(SessionAsyncResult::BrokerWarmup { request_id, result });
        });
    }

    fn clear_async_pishock_state(&mut self) {
        self.latest_target_discovery_request_id = None;
        self.latest_broker_warmup_request_id = None;
        self.discovery_in_progress = false;
        self.broker_warmup_in_progress = false;
        self.broker_status_error = None;
        self.target_status = None;
        self.discovered_targets.clear();
    }

    fn sync_pishock_state_if_ready(
        &mut self,
        sender: &Sender<SessionAsyncResult>,
        config: &Config,
        discovery_status: &str,
    ) {
        if has_auth_credentials(config) {
            self.start_target_discovery(sender, config, discovery_status);
            self.start_broker_warmup(sender, config);
            return;
        }

        self.clear_async_pishock_state();
        tokio::spawn(async {
            pishock::reset_session().await;
        });
    }
}

fn has_auth_credentials(config: &Config) -> bool {
    !config.username.trim().is_empty() && !config.apikey.trim().is_empty()
}

fn has_selected_target(config: &Config) -> bool {
    config.selected_client_id.is_some() && config.selected_shocker_id.is_some()
}

fn normalize_auth_fields(config: &mut Config) {
    config.username = config.username.trim().to_owned();
    config.apikey = config.apikey.trim().to_owned();
}

fn clear_selected_target(config: &mut Config) {
    config.selected_client_id = None;
    config.selected_shocker_id = None;
    config.selected_device_name.clear();
    config.selected_shocker_name.clear();
}

fn apply_selected_target(config: &mut Config, target: &pishock::DiscoveredTarget) {
    config.selected_client_id = Some(target.client_id);
    config.selected_shocker_id = Some(target.shocker_id);
    config.selected_device_name = target.device_name.clone();
    config.selected_shocker_name = target.shocker_name.clone();
}

#[cfg(test)]
mod tests {
    use super::{clear_selected_target, has_auth_credentials, normalize_auth_fields, AuthSnapshot};
    use crate::config::Config;

    #[test]
    fn auth_snapshot_trims_username_and_api_key() {
        let mut config = Config::default();
        config.username = "  player  ".into();
        config.apikey = "  key  ".into();

        assert_eq!(
            AuthSnapshot::from_config(&config),
            AuthSnapshot {
                username: "player".into(),
                apikey: "key".into(),
            }
        );
    }

    #[test]
    fn normalize_auth_fields_trims_in_place() {
        let mut config = Config::default();
        config.username = "  player  ".into();
        config.apikey = "  key  ".into();

        normalize_auth_fields(&mut config);

        assert_eq!(config.username, "player");
        assert_eq!(config.apikey, "key");
    }

    #[test]
    fn has_auth_credentials_requires_both_fields() {
        let mut config = Config::default();
        assert!(!has_auth_credentials(&config));

        config.username = "player".into();
        assert!(!has_auth_credentials(&config));

        config.apikey = "key".into();
        assert!(has_auth_credentials(&config));
    }

    #[test]
    fn clear_selected_target_removes_ids_and_labels() {
        let mut config = Config::default();
        config.selected_client_id = Some(12);
        config.selected_shocker_id = Some(34);
        config.selected_device_name = "Desk".into();
        config.selected_shocker_name = "Collar".into();

        clear_selected_target(&mut config);

        assert_eq!(config.selected_client_id, None);
        assert_eq!(config.selected_shocker_id, None);
        assert!(config.selected_device_name.is_empty());
        assert!(config.selected_shocker_name.is_empty());
    }
}
