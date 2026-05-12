use std::{
    process,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

use eframe::icon_data::from_png_bytes;
use egui::{widgets::DragValue, Button, ComboBox, Id, TextEdit, ViewportBuilder};
use log::{debug, error, info};
use tokio::sync::RwLock;

use crate::{
    config::{
        Config, RoundEndRewardGating, ShockMode, ShockTimingMode, CONFIG_FILE_PATH,
        MAX_REWARD_KILL_THRESHOLD, MAX_SHOCK_DURATION, MIN_REWARD_KILL_THRESHOLD,
        MIN_SHOCK_DURATION,
    },
    pishock,
    pishock_session_controller::{PishockSessionController, SessionAsyncResult},
    setup::{self, Cs2IntegrationStatus, SetupStep, SetupSummary},
    sounds::{self, SoundChoice, VOLUME_PERCENT_MAX},
};

const AUTO_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

pub async fn run(config: Arc<RwLock<Config>>) {
    let png_bytes = include_bytes!("../assets/icon.png");
    let viewport = ViewportBuilder::default()
        .with_inner_size([360.0, 670.0])
        .with_resizable(false)
        .with_icon(Arc::new(
            from_png_bytes(png_bytes).expect("Failed to load icon"),
        ));

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let changes = config.read().await.clone();
    let (async_result_tx, async_result_rx) = mpsc::channel();
    let _ = eframe::run_native(
        "CS2 Shock",
        options,
        Box::new(move |_cc| {
            Box::new(MyApp::new(
                config,
                changes,
                async_result_tx,
                async_result_rx,
            ))
        }),
    );
}

struct MyApp {
    config: Arc<RwLock<Config>>,
    changes: Config,
    cs2_integration_status: Cs2IntegrationStatus,
    show_setup_manual_steps: bool,
    setup_install_action_status: Option<String>,
    setup_section_revision: u64,
    last_setup_step: SetupStep,
    revealed_auth_field: Option<AuthField>,
    async_result_tx: mpsc::Sender<SessionAsyncResult>,
    async_result_rx: mpsc::Receiver<SessionAsyncResult>,
    session_controller: PishockSessionController,
    auto_save: AutoSaveState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthField {
    ApiKey,
}

#[derive(Debug, Default)]
struct AutoSaveState {
    pending_immediate_save: bool,
    last_debounced_change_at: Option<Instant>,
}

impl AutoSaveState {
    fn request_immediate_save(&mut self) {
        self.pending_immediate_save = true;
    }

    fn request_debounced_save(&mut self) {
        self.request_debounced_save_at(Instant::now());
    }

    fn request_debounced_save_at(&mut self, changed_at: Instant) {
        self.last_debounced_change_at = Some(changed_at);
    }

    fn take_save_due(&mut self) -> bool {
        self.take_save_due_at(Instant::now())
    }

    fn take_save_due_at(&mut self, now: Instant) -> bool {
        if self.pending_immediate_save {
            self.pending_immediate_save = false;
            self.last_debounced_change_at = None;
            return true;
        }

        if let Some(changed_at) = self.last_debounced_change_at {
            if now.duration_since(changed_at) >= AUTO_SAVE_DEBOUNCE {
                self.last_debounced_change_at = None;
                return true;
            }
        }

        false
    }

    fn has_pending(&self) -> bool {
        self.pending_immediate_save || self.last_debounced_change_at.is_some()
    }
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String, edit_id: Id) -> egui::Response {
    ui.horizontal(|ui| {
        let mut label_id = Id::NULL;
        ui.horizontal(|ui| {
            ui.set_width(85.0);
            label_id = ui.label(label).id;
        });

        ui.add(TextEdit::singleline(value).id_source(edit_id))
            .labelled_by(label_id)
    })
    .inner
}

fn secret_text_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    edit_id: Id,
    auth_field: AuthField,
    revealed_auth_field: &mut Option<AuthField>,
) -> egui::Response {
    ui.horizontal(|ui| {
        let mut label_id = Id::NULL;
        ui.horizontal(|ui| {
            ui.set_width(85.0);
            label_id = ui.label(label).id;
        });

        let is_revealed = *revealed_auth_field == Some(auth_field);
        let response = ui
            .add(
                TextEdit::singleline(value)
                    .id_source(edit_id)
                    .password(!is_revealed),
            )
            .labelled_by(label_id);

        if response.has_focus() && *revealed_auth_field != Some(auth_field) {
            *revealed_auth_field = Some(auth_field);
            ui.ctx().request_repaint();
        } else if response.lost_focus() && *revealed_auth_field == Some(auth_field) {
            *revealed_auth_field = None;
            ui.ctx().request_repaint();
        }
        response
    })
    .inner
}

fn has_selected_target(config: &Config) -> bool {
    config.selected_client_id.is_some() && config.selected_shocker_id.is_some()
}

fn should_require_selected_target(config: &Config) -> bool {
    !config.username.trim().is_empty() || !config.apikey.trim().is_empty()
}

fn apply_selected_target(config: &mut Config, target: &pishock::DiscoveredTarget) {
    config.selected_client_id = Some(target.client_id);
    config.selected_shocker_id = Some(target.shocker_id);
    config.selected_device_name = target.device_name.clone();
    config.selected_shocker_name = target.shocker_name.clone();
}

fn discovered_target_label(target: &pishock::DiscoveredTarget) -> String {
    let paused_suffix = if target.is_paused { " (paused)" } else { "" };
    format!(
        "{} / {}{}",
        target.device_name, target.shocker_name, paused_suffix
    )
}

fn selected_target_label(config: &Config) -> String {
    match (
        config.selected_device_name.trim(),
        config.selected_shocker_name.trim(),
        config.selected_client_id,
        config.selected_shocker_id,
    ) {
        (device_name, shocker_name, Some(_), Some(_))
            if !device_name.is_empty() && !shocker_name.is_empty() =>
        {
            format!("{device_name} / {shocker_name}")
        }
        (_, _, Some(client_id), Some(shocker_id)) => {
            format!("Device {client_id} / Shocker {shocker_id}")
        }
        _ => "Choose a PiShock shocker".into(),
    }
}

fn setup_section_title(label: &str, is_complete: bool) -> String {
    if is_complete {
        format!("{label} (done)")
    } else {
        format!("{label} (needed)")
    }
}

fn sound_picker(
    ui: &mut egui::Ui,
    id_source: &'static str,
    choice: &mut SoundChoice,
    enabled: bool,
) -> bool {
    let mut changed = false;
    ComboBox::from_id_source(id_source)
        .selected_text(choice.display_label())
        .show_ui(ui, |ui| {
            ui.set_enabled(enabled);
            for &bundled in sounds::bundled_sounds() {
                let is_selected = matches!(choice, SoundChoice::Bundled(b) if *b == bundled);
                if ui
                    .selectable_label(is_selected, bundled.tag())
                    .clicked()
                {
                    *choice = SoundChoice::Bundled(bundled);
                    changed = true;
                }
            }

            ui.separator();
            let custom_selected = matches!(choice, SoundChoice::Custom(_));
            if ui
                .selectable_label(custom_selected, "Custom file...")
                .clicked()
            {
                let starting_dir = if let SoundChoice::Custom(p) = choice {
                    p.parent().map(|p| p.to_path_buf())
                } else {
                    None
                };
                if let Some(path) = sounds::pick_custom_sound_file(starting_dir.as_deref()) {
                    *choice = SoundChoice::Custom(path);
                    changed = true;
                }
            }
        });
    changed
}

impl MyApp {
    fn new(
        config: Arc<RwLock<Config>>,
        changes: Config,
        async_result_tx: mpsc::Sender<SessionAsyncResult>,
        async_result_rx: mpsc::Receiver<SessionAsyncResult>,
    ) -> Self {
        let cs2_integration_status = setup::detect_cs2_integration();
        let initial_setup_step =
            SetupSummary::from_config(&changes, cs2_integration_status.clone()).current_step();
        let mut session_controller = PishockSessionController::new(&changes);
        session_controller.sync_startup(&async_result_tx, &changes);

        Self {
            config,
            changes,
            cs2_integration_status,
            show_setup_manual_steps: false,
            setup_install_action_status: None,
            setup_section_revision: 0,
            last_setup_step: initial_setup_step,
            revealed_auth_field: None,
            async_result_tx,
            async_result_rx,
            session_controller,
            auto_save: AutoSaveState::default(),
        }
    }

    fn persist_changes_if_needed(&mut self) {
        let Some(current_config) = self.config.try_read().ok().map(|config| config.to_owned())
        else {
            return;
        };
        if current_config == self.changes {
            return;
        }

        if let Ok(mut owned_config) = self.config.clone().try_write() {
            *owned_config = self.changes.clone();
            match owned_config.try_write_to_file(CONFIG_FILE_PATH) {
                Ok(()) => debug!(target: "GUI", "Auto-saved config"),
                Err(err) => error!(target: "GUI", "Failed to auto-save config: {}", err),
            }
        }
    }

    fn refresh_cs2_integration_status(&mut self) {
        self.cs2_integration_status = setup::detect_cs2_integration();
    }

    fn setup_summary(&self) -> SetupSummary {
        SetupSummary::from_config(&self.changes, self.cs2_integration_status.clone())
    }

    fn should_show_setup_modal(&self) -> bool {
        self.setup_summary().needs_setup() && !self.changes.setup_dismissed
    }

    fn dismiss_setup(&mut self) {
        if !self.changes.setup_dismissed {
            self.changes.setup_dismissed = true;
            self.auto_save.request_immediate_save();
        }
        self.show_setup_manual_steps = false;
        self.setup_install_action_status = None;
    }

    fn reopen_setup(&mut self) {
        self.show_setup_manual_steps = false;
        self.setup_install_action_status = None;
        if self.changes.setup_dismissed {
            self.changes.setup_dismissed = false;
            self.auto_save.request_immediate_save();
        }
        self.refresh_cs2_integration_status();
    }

    fn reset_setup_dismissal_if_complete(&mut self) {
        if self.setup_summary().is_complete() && self.changes.setup_dismissed {
            self.changes.setup_dismissed = false;
            self.auto_save.request_immediate_save();
        }
    }

    fn sync_setup_section_revision(&mut self) {
        let current_step = self.setup_summary().current_step();
        if current_step != self.last_setup_step {
            self.last_setup_step = current_step;
            self.setup_section_revision = self.setup_section_revision.wrapping_add(1);
        }
    }

    fn render_auth_fields(
        &mut self,
        ui: &mut egui::Ui,
        username_id_source: &'static str,
        api_key_id_source: &'static str,
    ) {
        let username_response = text_row(
            ui,
            "Username: ",
            &mut self.changes.username,
            ui.make_persistent_id(username_id_source),
        );
        let api_key_response = secret_text_row(
            ui,
            "API key: ",
            &mut self.changes.apikey,
            ui.make_persistent_id(api_key_id_source),
            AuthField::ApiKey,
            &mut self.revealed_auth_field,
        );

        if username_response.lost_focus() || api_key_response.lost_focus() {
            self.session_controller
                .refresh_after_auth_commit(&self.async_result_tx, &mut self.changes);
            self.auto_save.request_immediate_save();
        }
    }

    fn refresh_shockers(&mut self) {
        info!(target: "GUI", "Loading PiShock shockers");
        self.session_controller
            .refresh_manually(&self.async_result_tx, &self.changes);
    }

    fn send_test_beep(&self) {
        info!(target: "GUI", "Sending test beep");
        let config = Arc::new(RwLock::new(self.changes.clone()));
        tokio::spawn(async move {
            pishock::beep(config, 1).await;
        });
    }

    fn open_cs2_cfg_folder(&mut self, target_path: &std::path::Path) {
        self.setup_install_action_status = Some(match setup::open_cs2_cfg_folder(target_path) {
            Ok(()) => format!("Opened `{}`.", target_path.parent().unwrap().display()),
            Err(message) => message,
        });
    }

    fn save_cs2_integration_to_downloads(&mut self) {
        self.setup_install_action_status = Some(match setup::save_cs2_integration_to_downloads() {
            Ok(download_path) => format!("Saved `{}`.", download_path.display()),
            Err(message) => message,
        });
    }

    fn render_refresh_and_test_buttons(&mut self, ui: &mut egui::Ui, refresh_label: &str) {
        let discovery_button = if self.session_controller.discovery_in_progress() {
            Button::new("Loading shockers...")
        } else {
            Button::new(refresh_label)
        };

        if ui
            .add_enabled(
                !self.session_controller.discovery_in_progress(),
                discovery_button,
            )
            .clicked()
        {
            self.refresh_shockers();
        }

        if ui
            .add_enabled(has_selected_target(&self.changes), Button::new("Test beep"))
            .clicked()
        {
            self.send_test_beep();
        }
    }

    fn render_shocker_picker(&mut self, ui: &mut egui::Ui, combo_id_source: &'static str) {
        if !self.session_controller.discovered_targets().is_empty() {
            let mut chosen_target = None;

            ui.horizontal(|ui| {
                ui.horizontal(|ui| {
                    ui.set_width(85.0);
                    ui.label("Shocker: ");
                });

                ComboBox::from_id_source(combo_id_source)
                    .selected_text(selected_target_label(&self.changes))
                    .show_ui(ui, |ui| {
                        for target in self.session_controller.discovered_targets() {
                            let is_selected = Some(target.client_id)
                                == self.changes.selected_client_id
                                && Some(target.shocker_id) == self.changes.selected_shocker_id;

                            if ui
                                .selectable_label(is_selected, discovered_target_label(target))
                                .clicked()
                            {
                                chosen_target = Some(target.clone());
                            }
                        }
                    });
            });

            if let Some(target) = chosen_target {
                apply_selected_target(&mut self.changes, &target);
                self.session_controller
                    .set_target_status(format!("Selected {}.", discovered_target_label(&target)));
                self.auto_save.request_immediate_save();
            }
        } else if has_selected_target(&self.changes) {
            ui.label(format!(
                "Saved shocker: {}",
                selected_target_label(&self.changes)
            ));
        }
    }

    fn render_status_labels(&self, ui: &mut egui::Ui) {
        if should_require_selected_target(&self.changes) && !has_selected_target(&self.changes) {
            ui.label("Select the shocker you want to use.");
        }

        if let Some(status) = self.session_controller.target_status() {
            ui.label(status);
        }

        ui.label(self.session_controller.broker_status_label());
    }

    fn render_rewards_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Rewards")
            .id_source("rewards_section")
            .default_open(false)
            .show(ui, |ui| {
                self.render_kill_reward_block(ui);
                ui.separator();
                self.render_round_end_reward_block(ui);
            });
    }

    fn render_kill_reward_block(&mut self, ui: &mut egui::Ui) {
        ui.label("Instant reward on every kill");

        let enabled_changed = ui
            .add(egui::Checkbox::new(
                &mut self.changes.rewards.kill_reward_enabled,
                "Play sound on each kill",
            ))
            .changed();
        if enabled_changed {
            self.auto_save.request_immediate_save();
        }

        let enabled = self.changes.rewards.kill_reward_enabled;
        ui.horizontal(|ui| {
            ui.set_enabled(enabled);
            ui.label("Sound: ");
            if sound_picker(
                ui,
                "kill_reward_sound_picker",
                &mut self.changes.rewards.kill_reward_sound,
                enabled,
            ) {
                self.auto_save.request_immediate_save();
            }
            if ui.button("Preview").clicked() {
                sounds::play(
                    self.changes.rewards.kill_reward_sound.clone(),
                    self.changes.rewards.kill_reward_volume_percent,
                );
            }
        });

        ui.horizontal(|ui| {
            let label = ui.label("Volume: ");
            let response = ui.add_enabled(
                enabled,
                DragValue::new(&mut self.changes.rewards.kill_reward_volume_percent)
                    .speed(1)
                    .clamp_range(0..=VOLUME_PERCENT_MAX)
                    .suffix("%"),
            );
            let changed = response.changed();
            response.labelled_by(label.id);
            if changed {
                self.auto_save.request_debounced_save();
            }
        });
    }

    fn render_round_end_reward_block(&mut self, ui: &mut egui::Ui) {
        ui.label("End-of-round reward when kill threshold is met");

        let enabled_changed = ui
            .add(egui::Checkbox::new(
                &mut self.changes.rewards.round_end_reward_enabled,
                "Play sound at round end if threshold is met",
            ))
            .changed();
        if enabled_changed {
            self.auto_save.request_immediate_save();
        }

        let enabled = self.changes.rewards.round_end_reward_enabled;

        ui.horizontal(|ui| {
            let label = ui.label("Kill threshold: ");
            let response = ui.add_enabled(
                enabled,
                DragValue::new(&mut self.changes.rewards.round_end_reward_kill_threshold)
                    .speed(1)
                    .clamp_range(MIN_REWARD_KILL_THRESHOLD..=MAX_REWARD_KILL_THRESHOLD)
                    .suffix(" kills"),
            );
            let changed = response.changed();
            response.labelled_by(label.id);
            if changed {
                self.auto_save.request_debounced_save();
            }
        });

        ui.label("Trigger:");
        ui.add_enabled_ui(enabled, |ui| {
            ui.vertical_centered_justified(|ui| {
                let always = ui.selectable_value(
                    &mut self.changes.rewards.round_end_reward_gating,
                    RoundEndRewardGating::Always,
                    "Always when threshold met",
                );
                let win_only = ui.selectable_value(
                    &mut self.changes.rewards.round_end_reward_gating,
                    RoundEndRewardGating::OnlyIfTeamWins,
                    "Only if team wins",
                );
                if always.changed() || win_only.changed() {
                    self.auto_save.request_immediate_save();
                }
            });
        });

        ui.horizontal(|ui| {
            ui.set_enabled(enabled);
            ui.label("Sound: ");
            if sound_picker(
                ui,
                "round_end_reward_sound_picker",
                &mut self.changes.rewards.round_end_reward_sound,
                enabled,
            ) {
                self.auto_save.request_immediate_save();
            }
            if ui.button("Preview").clicked() {
                sounds::play(
                    self.changes.rewards.round_end_reward_sound.clone(),
                    self.changes.rewards.round_end_reward_volume_percent,
                );
            }
        });

        ui.horizontal(|ui| {
            let label = ui.label("Volume: ");
            let response = ui.add_enabled(
                enabled,
                DragValue::new(&mut self.changes.rewards.round_end_reward_volume_percent)
                    .speed(1)
                    .clamp_range(0..=VOLUME_PERCENT_MAX)
                    .suffix("%"),
            );
            let changed = response.changed();
            response.labelled_by(label.id);
            if changed {
                self.auto_save.request_debounced_save();
            }
        });
    }

    fn render_setup_banner(&mut self, ui: &mut egui::Ui, summary: &SetupSummary) {
        ui.group(|ui| {
            ui.label(
                "Finish setup to install the CS2 integration and connect your PiShock shocker.",
            );
            ui.label(match summary.current_step() {
                SetupStep::InstallCs2Integration => "Next step: install the CS2 integration file.",
                SetupStep::ConnectPishock => "Next step: enter your PiShock username and API key.",
                SetupStep::ChooseShocker => "Next step: pick which PiShock shocker to use.",
                SetupStep::Complete => "Setup is complete.",
            });
            if ui.button("Finish setup").clicked() {
                self.reopen_setup();
            }
        });
    }

    fn render_setup_install_section(&mut self, ui: &mut egui::Ui, summary: &SetupSummary) {
        egui::CollapsingHeader::new(setup_section_title(
            "1. Install CS2 integration",
            summary.cs2_integration.is_installed(),
        ))
        .id_source(("setup_install_section", self.setup_section_revision))
        .default_open(summary.current_step() == SetupStep::InstallCs2Integration)
        .show(ui, |ui| {
            ui.label("CS2 needs one small file so the game can send live events to CS2Shock.");

            match &summary.cs2_integration {
                Cs2IntegrationStatus::Installed { .. } => {
                    ui.label("The CS2 integration file is installed and points to this app.");
                    if ui.button("Re-check installation").clicked() {
                        self.refresh_cs2_integration_status();
                    }
                }
                Cs2IntegrationStatus::MissingKnownPath { target_path }
                | Cs2IntegrationStatus::RepairRecommended { target_path, .. } => {
                    if let Some(message) = summary.cs2_integration.message() {
                        ui.label(message);
                    }

                    ui.horizontal(|ui| {
                        if ui
                            .button(summary.cs2_integration.install_action_label())
                            .clicked()
                        {
                            match setup::install_cs2_integration(target_path) {
                                Ok(()) => self.refresh_cs2_integration_status(),
                                Err(message) => {
                                    self.cs2_integration_status =
                                        Cs2IntegrationStatus::CheckFailed {
                                            target_path: Some(target_path.clone()),
                                            message,
                                        };
                                }
                            }
                        }

                        if ui.button("Manual instructions").clicked() {
                            self.show_setup_manual_steps = !self.show_setup_manual_steps;
                        }

                        if ui.button("Do this later").clicked() {
                            self.dismiss_setup();
                        }
                    });
                }
                Cs2IntegrationStatus::MissingUnknownPath
                | Cs2IntegrationStatus::CheckFailed { .. } => {
                    if let Some(message) = summary.cs2_integration.message() {
                        ui.label(message);
                    } else {
                        ui.label("CS2 was not found automatically on this computer.");
                    }

                    ui.horizontal(|ui| {
                        if ui.button("Retry detection").clicked() {
                            self.refresh_cs2_integration_status();
                        }

                        if ui.button("Manual instructions").clicked() {
                            self.show_setup_manual_steps = !self.show_setup_manual_steps;
                        }

                        if ui.button("Do this later").clicked() {
                            self.dismiss_setup();
                        }
                    });
                }
            }

            if self.show_setup_manual_steps {
                ui.separator();
                ui.label("Manual install:");
                ui.label(
                    "1. In your Steam Library, select Counter-Strike 2, click on the Settings button and choose 'Properties', then click Installed Files > Browse.",
                );
                ui.label("2. Open the `game/csgo/cfg` folder.");
                ui.label("3. Copy `gamestate_integration_cs2shock.cfg` into that folder.");
                ui.label("4. If you want a manual copy, save the file to Downloads and drag it into the cfg folder.");
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            summary.cs2_integration.target_path().is_some(),
                            Button::new("Open Folder"),
                        )
                        .clicked()
                    {
                        if let Some(target_path) = summary.cs2_integration.target_path() {
                            self.open_cs2_cfg_folder(target_path);
                        }
                    }

                    if ui.button("Save Integration File").clicked() {
                        self.save_cs2_integration_to_downloads();
                    }

                    if ui.button("Re-check installation").clicked() {
                        self.refresh_cs2_integration_status();
                    }
                });

                if let Some(status) = self.setup_install_action_status.as_deref() {
                    ui.label(status);
                }
            }
        });
    }

    fn render_setup_connect_section(&mut self, ui: &mut egui::Ui, summary: &SetupSummary) {
        egui::CollapsingHeader::new(setup_section_title(
            "2. Connect PiShock",
            summary.has_auth_credentials,
        ))
        .id_source(("setup_connect_section", self.setup_section_revision))
        .default_open(summary.current_step() == SetupStep::ConnectPishock)
        .show(ui, |ui| {
            ui.label("Enter your PiShock username and API key.");
            self.render_auth_fields(ui, "setup_username_field", "setup_api_key_field");
            ui.horizontal_wrapped(|ui| {
                ui.hyperlink_to(
                    "Create PiShock API Key",
                    "https://login.pishock.com/account",
                );
            });
        });
    }

    fn render_setup_shocker_section(&mut self, ui: &mut egui::Ui, summary: &SetupSummary) {
        egui::CollapsingHeader::new(setup_section_title(
            "3. Choose a shocker",
            summary.has_selected_target,
        ))
        .id_source(("setup_shocker_section", self.setup_section_revision))
        .default_open(summary.current_step() == SetupStep::ChooseShocker)
        .show(ui, |ui| {
            if !summary.has_auth_credentials {
                ui.label("Finish the PiShock login step first, then load your shockers.");
                return;
            }

            self.render_status_labels(ui);

            self.render_shocker_picker(ui, "setup_shocker_picker");
            ui.horizontal(|ui| self.render_refresh_and_test_buttons(ui, "Refresh shockers"));

            let can_finish = self.setup_summary().is_complete();
            if can_finish {
                ui.label("You are ready to play.");
            }

            if ui
                .add_enabled(can_finish, Button::new("Finish setup"))
                .clicked()
            {
                self.show_setup_manual_steps = false;
                if self.changes.setup_dismissed {
                    self.changes.setup_dismissed = false;
                    self.auto_save.request_immediate_save();
                }
            }
        });
    }

    fn render_setup_modal(&mut self, ctx: &egui::Context) {
        let summary = self.setup_summary();
        let mut open = true;

        egui::Window::new("Finish setup")
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("CS2Shock needs a couple of quick setup steps before it will work.");
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(430.0)
                    .show(ui, |ui| {
                        self.render_setup_install_section(ui, &summary);
                        ui.separator();
                        self.render_setup_connect_section(ui, &summary);
                        ui.separator();
                        self.render_setup_shocker_section(ui, &summary);
                    });
            });

        if !open {
            self.dismiss_setup();
        }
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(result) = self.async_result_rx.try_recv() {
            let previous_changes = self.changes.clone();
            self.session_controller
                .handle_async_result(result, &mut self.changes);
            if self.changes != previous_changes {
                self.auto_save.request_immediate_save();
            }
            ctx.request_repaint();
        }

        self.reset_setup_dismissal_if_complete();
        self.sync_setup_section_revision();

        egui::CentralPanel::default().show(ctx, |ui| {
            let setup_summary = self.setup_summary();
            ui.heading("CS2 Shock");

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
            if setup_summary.needs_setup() && self.changes.setup_dismissed {
                self.render_setup_banner(ui, &setup_summary);
                ui.separator();
            }

            self.render_auth_fields(ui, "username_field", "api_key_field");

            ui.vertical_centered_justified(|ui| {
                ui.separator();
                ui.horizontal(|ui| self.render_refresh_and_test_buttons(ui, "Refresh shockers"));
            });

            self.render_shocker_picker(ui, "pishock_target_picker");
            self.render_status_labels(ui);

            ui.vertical_centered(|ui| {
                ui.separator();
                ui.label("Shock Mode: ");
            });
            ui.vertical_centered_justified(|ui| {
                let random = ui.selectable_value(
                    &mut self.changes.shock_mode,
                    ShockMode::Random,
                    "Random Intensity",
                );
                let last_hit = ui.selectable_value(
                    &mut self.changes.shock_mode,
                    ShockMode::LastHitPercentage,
                    "Last Hit Percentage",
                );
                if random.changed() || last_hit.changed() {
                    self.auto_save.request_immediate_save();
                }
            });
            ui.vertical_centered(|ui| ui.separator());

            ui.vertical_centered(|ui| {
                ui.label("Shock Timing: ");
            });
            ui.vertical_centered_justified(|ui| {
                let immediate = ui.selectable_value(
                    &mut self.changes.shock_timing_mode,
                    ShockTimingMode::Immediate,
                    "Shock immediately on death",
                );
                let round_end = ui.selectable_value(
                    &mut self.changes.shock_timing_mode,
                    ShockTimingMode::EndOfRound,
                    "Shock at round end",
                );
                let round_loss_only = ui.selectable_value(
                    &mut self.changes.shock_timing_mode,
                    ShockTimingMode::EndOfRoundIfTeamLoses,
                    "Shock at round end only if team loses",
                );
                if immediate.changed() || round_end.changed() || round_loss_only.changed() {
                    self.auto_save.request_immediate_save();
                }
            });
            ui.vertical_centered(|ui| {
                ui.separator();
            });

            let beep_on_match_start = ui.add(egui::Checkbox::new(
                &mut self.changes.beep_on_match_start,
                "Beep on match start",
            ));
            if beep_on_match_start.changed() {
                self.auto_save.request_immediate_save();
            }

            let beep_on_round_start = ui.add(egui::Checkbox::new(
                &mut self.changes.beep_on_round_start,
                "Beep on round start",
            ));
            if beep_on_round_start.changed() {
                self.auto_save.request_immediate_save();
            }
            ui.vertical_centered(|ui| {
                ui.separator();
            });

            let prevent_shock_if_round_kills_reached = ui.add(egui::Checkbox::new(
                &mut self.changes.prevent_shock_if_round_kills_reached,
                "Prevent shock if round kills reached",
            ));
            if prevent_shock_if_round_kills_reached.changed() {
                self.auto_save.request_immediate_save();
            }

            ui.horizontal(|ui| {
                let round_kill_label = ui.label("Kill threshold: ");
                let round_kills = ui.add_enabled(
                    self.changes.prevent_shock_if_round_kills_reached,
                    DragValue::new(&mut self.changes.round_kills_to_prevent_shock)
                        .speed(1)
                        .clamp_range(1..=5)
                        .suffix(" kills"),
                );
                let round_kills_changed = round_kills.changed();
                round_kills.labelled_by(round_kill_label.id);
                if round_kills_changed {
                    self.auto_save.request_debounced_save();
                }
            });
            ui.vertical_centered(|ui| {
                ui.separator();
            });

            self.render_rewards_section(ui);

            ui.vertical_centered(|ui| {
                ui.separator();
            });

            let warning_beep_before_shock = ui.add(egui::Checkbox::new(
                &mut self.changes.warning_beep_before_shock,
                "Play warning beep before shock",
            ));
            if warning_beep_before_shock.changed() {
                self.auto_save.request_immediate_save();
            }
            ui.horizontal(|ui| {
                let warning_beep_label = ui.label("Warning duration: ");
                let warning_duration = ui.add_enabled(
                    self.changes.warning_beep_before_shock,
                    DragValue::new(&mut self.changes.warning_beep_duration)
                        .speed(1)
                        .clamp_range(1..=15)
                        .suffix(" sec"),
                );
                let warning_duration_changed = warning_duration.changed();
                warning_duration.labelled_by(warning_beep_label.id);
                if warning_duration_changed {
                    self.auto_save.request_debounced_save();
                }
            });

            ui.horizontal(|ui| {
                let shock_chance_label = ui.label("Chance to shock: ");
                let shock_chance = ui.add(
                    DragValue::new(&mut self.changes.shock_chance)
                        .speed(1)
                        .clamp_range(0..=100)
                        .suffix("%"),
                );
                let shock_chance_changed = shock_chance.changed();
                shock_chance.labelled_by(shock_chance_label.id);
                if shock_chance_changed {
                    self.auto_save.request_debounced_save();
                }
            });

            ui.horizontal(|ui| {
                let indensity_label = ui.label("Shock intensity: ");

                let min_intensity = ui.add(
                    DragValue::new(&mut self.changes.min_intensity)
                        .speed(1)
                        .clamp_range(1..=self.changes.max_intensity)
                        .prefix("Min "),
                );
                let min_intensity_changed = min_intensity.changed();
                min_intensity.labelled_by(indensity_label.id);
                if min_intensity_changed {
                    self.auto_save.request_debounced_save();
                }

                let max_intensity = ui.add(
                    DragValue::new(&mut self.changes.max_intensity)
                        .speed(1)
                        .clamp_range(self.changes.min_intensity.max(1)..=100)
                        .prefix("Max "),
                );
                let max_intensity_changed = max_intensity.changed();
                max_intensity.labelled_by(indensity_label.id);
                if max_intensity_changed {
                    self.auto_save.request_debounced_save();
                }
            });
            ui.horizontal(|ui| {
                let duration_label = ui.label("Shock duration: ");
                let min_duration = ui.add(
                    DragValue::new(&mut self.changes.min_duration)
                        .speed(0.1)
                        .clamp_range(MIN_SHOCK_DURATION..=self.changes.max_duration)
                        .min_decimals(1)
                        .max_decimals(1)
                        .prefix("Min ")
                        .suffix(" sec"),
                );
                let min_duration_changed = min_duration.changed();
                min_duration.labelled_by(duration_label.id);
                if min_duration_changed {
                    self.auto_save.request_debounced_save();
                }

                let max_duration = ui.add(
                    DragValue::new(&mut self.changes.max_duration)
                        .speed(0.1)
                        .clamp_range(
                            self.changes.min_duration.max(MIN_SHOCK_DURATION)..=MAX_SHOCK_DURATION,
                        )
                        .min_decimals(1)
                        .max_decimals(1)
                        .prefix("Max ")
                        .suffix(" sec"),
                );
                let max_duration_changed = max_duration.changed();
                max_duration.labelled_by(duration_label.id);
                if max_duration_changed {
                    self.auto_save.request_debounced_save();
                }
            });

            ui.vertical_centered(|ui| {
                ui.separator();
            });
                });

            if self.auto_save.take_save_due() {
                self.persist_changes_if_needed();
            }

            if ctx.input(|i| i.viewport().close_requested()) {
                info!(target: "GUI", "Closing");
                if self.auto_save.has_pending() {
                    debug!(target: "GUI", "Flushing pending auto-save before close");
                }
                self.persist_changes_if_needed();
                process::exit(0);
            }
        });

        if self.should_show_setup_modal() {
            self.render_setup_modal(ctx);
        }

        // Keep periodic UI ticks for heartbeat freshness and debounce checks.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::{AutoSaveState, AUTO_SAVE_DEBOUNCE};
    use std::time::{Duration, Instant};

    #[test]
    fn immediate_save_triggers_once() {
        let mut state = AutoSaveState::default();
        state.request_immediate_save();

        assert!(state.take_save_due_at(Instant::now()));
        assert!(!state.take_save_due_at(Instant::now()));
    }

    #[test]
    fn debounced_save_waits_for_idle_window() {
        let mut state = AutoSaveState::default();
        let started_at = Instant::now();
        state.request_debounced_save_at(started_at);

        assert!(!state.take_save_due_at(started_at + AUTO_SAVE_DEBOUNCE - Duration::from_millis(1)));
        assert!(state.take_save_due_at(started_at + AUTO_SAVE_DEBOUNCE));
    }

    #[test]
    fn immediate_save_clears_pending_debounced_save() {
        let mut state = AutoSaveState::default();
        let started_at = Instant::now();
        state.request_debounced_save_at(started_at);
        state.request_immediate_save();

        assert!(state.take_save_due_at(started_at));
        assert!(!state.has_pending());
    }
}
