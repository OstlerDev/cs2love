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
        Config, RoundEndRewardGating, CONFIG_FILE_PATH, MAX_REWARD_KILL_THRESHOLD,
        MAX_VIBRATION_DURATION_MS, MAX_VIBRATION_STRENGTH_PERCENT, MIN_REWARD_KILL_THRESHOLD,
        MIN_VIBRATION_DURATION_MS,
    },
    intiface,
    intiface_session_controller::{IntifaceSessionController, SessionAsyncResult},
    setup::{self, Cs2IntegrationStatus, SetupStep, SetupSummary},
    sounds::{self, SoundChoice, VOLUME_PERCENT_MAX},
};

const AUTO_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
const TEST_VIBRATION_STRENGTH_PERCENT: u32 = 50;
const TEST_VIBRATION_DURATION_MS: u64 = 1000;

pub async fn run(config: Arc<RwLock<Config>>) {
    let png_bytes = include_bytes!("../assets/icon.png");
    let viewport = ViewportBuilder::default()
        .with_inner_size([360.0, 720.0])
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
        "CS2 Love",
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
    async_result_tx: mpsc::Sender<SessionAsyncResult>,
    async_result_rx: mpsc::Receiver<SessionAsyncResult>,
    session_controller: IntifaceSessionController,
    auto_save: AutoSaveState,
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
        let mut session_controller = IntifaceSessionController::new(&changes);
        session_controller.sync_startup(&async_result_tx, &changes);

        Self {
            config,
            changes,
            cs2_integration_status,
            show_setup_manual_steps: false,
            setup_install_action_status: None,
            setup_section_revision: 0,
            last_setup_step: initial_setup_step,
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

    fn render_intiface_url_field(&mut self, ui: &mut egui::Ui, edit_id_source: &'static str) {
        let response = text_row(
            ui,
            "Intiface URL: ",
            &mut self.changes.intiface_websocket_url,
            ui.make_persistent_id(edit_id_source),
        );

        if response.lost_focus() {
            self.session_controller
                .refresh_after_url_commit(&self.async_result_tx, &mut self.changes);
            self.auto_save.request_immediate_save();
        }
    }

    fn send_test_vibration(&self) {
        info!(target: "GUI", "Sending test vibration");
        let toys = self.changes.selected_toy_identifiers.clone();
        tokio::spawn(async move {
            intiface::vibrate_for(toys, TEST_VIBRATION_STRENGTH_PERCENT, TEST_VIBRATION_DURATION_MS)
                .await;
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

    fn render_test_vibration_button(&mut self, ui: &mut egui::Ui) {
        if ui
            .add_enabled(
                !self.changes.selected_toy_identifiers.is_empty(),
                Button::new("Test vibrate"),
            )
            .clicked()
        {
            self.send_test_vibration();
        }
    }

    fn render_toy_checklist(&mut self, ui: &mut egui::Ui) {
        let available = self.session_controller.available_toys();
        let mut all_names: Vec<String> = available.iter().map(|t| t.name.clone()).collect();
        for selected in &self.changes.selected_toy_identifiers {
            if !all_names.iter().any(|name| name == selected) {
                all_names.push(selected.clone());
            }
        }

        if all_names.is_empty() {
            ui.label("No toys connected. Pair a toy in Intiface Central.");
            return;
        }

        let mut changed = false;
        for name in all_names {
            let mut selected = self.changes.selected_toy_identifiers.contains(&name);
            let label = if available.iter().any(|t| t.name == name) {
                name.clone()
            } else {
                format!("{name} (offline)")
            };
            if ui.checkbox(&mut selected, label).changed() {
                if selected {
                    if !self.changes.selected_toy_identifiers.contains(&name) {
                        self.changes.selected_toy_identifiers.push(name.clone());
                    }
                } else {
                    self.changes
                        .selected_toy_identifiers
                        .retain(|n| n != &name);
                }
                changed = true;
            }
        }

        if changed {
            self.auto_save.request_immediate_save();
        }
    }

    fn render_intiface_status(&self, ui: &mut egui::Ui) {
        ui.label(self.session_controller.connection_status_label());
    }

    fn render_intiface_section(&mut self, ui: &mut egui::Ui) {
        ui.label("Intiface Central");
        self.render_intiface_url_field(ui, "intiface_url_field");
        ui.horizontal(|ui| self.render_test_vibration_button(ui));
        self.render_toy_checklist(ui);
        self.render_intiface_status(ui);
    }

    fn render_rewards_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Sound Rewards")
            .id_source("sound_rewards_section")
            .default_open(false)
            .show(ui, |ui| {
                self.render_kill_reward_block(ui);
                ui.separator();
                self.render_round_end_reward_block(ui);
            });

        egui::CollapsingHeader::new("Vibration Rewards")
            .id_source("vibration_rewards_section")
            .default_open(true)
            .show(ui, |ui| {
                self.render_kill_vibration_block(ui);
                ui.separator();
                self.render_round_end_vibration_block(ui);
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

    fn render_kill_vibration_block(&mut self, ui: &mut egui::Ui) {
        ui.label("Instant vibration on every kill");

        let enabled_changed = ui
            .add(egui::Checkbox::new(
                &mut self.changes.vibrations.kill_vibration_enabled,
                "Vibrate on each kill",
            ))
            .changed();
        if enabled_changed {
            self.auto_save.request_immediate_save();
        }

        let enabled = self.changes.vibrations.kill_vibration_enabled;
        render_vibration_strength_row(
            ui,
            enabled,
            "kill_vibration_strength",
            &mut self.changes.vibrations.kill_vibration_strength_percent,
            &mut self.auto_save,
        );
        render_vibration_duration_row(
            ui,
            enabled,
            "kill_vibration_duration",
            &mut self.changes.vibrations.kill_vibration_duration_ms,
            &mut self.auto_save,
        );
    }

    fn render_round_end_vibration_block(&mut self, ui: &mut egui::Ui) {
        ui.label("End-of-round vibration when kill threshold is met");

        let enabled_changed = ui
            .add(egui::Checkbox::new(
                &mut self.changes.vibrations.round_end_vibration_enabled,
                "Vibrate at round end if threshold is met",
            ))
            .changed();
        if enabled_changed {
            self.auto_save.request_immediate_save();
        }

        let enabled = self.changes.vibrations.round_end_vibration_enabled;

        ui.horizontal(|ui| {
            let label = ui.label("Kill threshold: ");
            let response = ui.add_enabled(
                enabled,
                DragValue::new(&mut self.changes.vibrations.round_end_vibration_kill_threshold)
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
                    &mut self.changes.vibrations.round_end_vibration_gating,
                    RoundEndRewardGating::Always,
                    "Always when threshold met",
                );
                let win_only = ui.selectable_value(
                    &mut self.changes.vibrations.round_end_vibration_gating,
                    RoundEndRewardGating::OnlyIfTeamWins,
                    "Only if team wins",
                );
                if always.changed() || win_only.changed() {
                    self.auto_save.request_immediate_save();
                }
            });
        });

        render_vibration_strength_row(
            ui,
            enabled,
            "round_end_vibration_strength",
            &mut self.changes.vibrations.round_end_vibration_strength_percent,
            &mut self.auto_save,
        );
        render_vibration_duration_row(
            ui,
            enabled,
            "round_end_vibration_duration",
            &mut self.changes.vibrations.round_end_vibration_duration_ms,
            &mut self.auto_save,
        );
    }

    fn render_setup_banner(&mut self, ui: &mut egui::Ui, summary: &SetupSummary) {
        ui.group(|ui| {
            ui.label("Finish setup to install the CS2 integration and connect Intiface.");
            ui.label(match summary.current_step() {
                SetupStep::InstallCs2Integration => "Next step: install the CS2 integration file.",
                SetupStep::ConnectIntiface => "Next step: enter the Intiface Central URL.",
                SetupStep::ChooseToy => "Next step: choose at least one toy to use.",
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
            ui.label("CS2 needs one small file so the game can send live events to CS2 Love.");

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
                ui.label("3. Copy `gamestate_integration_cs2love.cfg` into that folder.");
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

    fn render_setup_intiface_section(&mut self, ui: &mut egui::Ui, summary: &SetupSummary) {
        egui::CollapsingHeader::new(setup_section_title(
            "2. Connect Intiface",
            summary.has_intiface_url,
        ))
        .id_source(("setup_intiface_section", self.setup_section_revision))
        .default_open(summary.current_step() == SetupStep::ConnectIntiface)
        .show(ui, |ui| {
            ui.label("Install Intiface Central, press 'Start Server', then enter its WebSocket URL below.");
            self.render_intiface_url_field(ui, "setup_intiface_url_field");
            ui.horizontal_wrapped(|ui| {
                ui.hyperlink_to("Download Intiface Central", "https://intiface.com/central");
            });
        });
    }

    fn render_setup_toy_section(&mut self, ui: &mut egui::Ui, summary: &SetupSummary) {
        egui::CollapsingHeader::new(setup_section_title(
            "3. Choose toys",
            summary.has_selected_toys,
        ))
        .id_source(("setup_toy_section", self.setup_section_revision))
        .default_open(summary.current_step() == SetupStep::ChooseToy)
        .show(ui, |ui| {
            if !summary.has_intiface_url {
                ui.label("Enter the Intiface Central URL above before scanning.");
                return;
            }

            self.render_intiface_status(ui);
            self.render_toy_checklist(ui);
            ui.horizontal(|ui| self.render_test_vibration_button(ui));

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
                ui.label("CS2 Love needs a couple of quick setup steps before it will work.");
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(430.0)
                    .show(ui, |ui| {
                        self.render_setup_install_section(ui, &summary);
                        ui.separator();
                        self.render_setup_intiface_section(ui, &summary);
                        ui.separator();
                        self.render_setup_toy_section(ui, &summary);
                    });
            });

        if !open {
            self.dismiss_setup();
        }
    }
}

fn render_vibration_strength_row(
    ui: &mut egui::Ui,
    enabled: bool,
    id_source: &'static str,
    value: &mut u32,
    auto_save: &mut AutoSaveState,
) {
    ui.horizontal(|ui| {
        let label = ui.label("Strength: ");
        let response = ui.add_enabled(
            enabled,
            DragValue::new(value)
                .speed(1)
                .clamp_range(0..=MAX_VIBRATION_STRENGTH_PERCENT)
                .suffix("%"),
        );
        let changed = response.changed();
        response.labelled_by(label.id).on_hover_text(format!(
            "0-{} percent of maximum vibrator output",
            MAX_VIBRATION_STRENGTH_PERCENT
        ));
        if changed {
            auto_save.request_debounced_save();
        }
        let _ = id_source;
    });
}

fn render_vibration_duration_row(
    ui: &mut egui::Ui,
    enabled: bool,
    id_source: &'static str,
    value: &mut u32,
    auto_save: &mut AutoSaveState,
) {
    ui.horizontal(|ui| {
        let label = ui.label("Duration: ");
        let response = ui.add_enabled(
            enabled,
            DragValue::new(value)
                .speed(50)
                .clamp_range(MIN_VIBRATION_DURATION_MS..=MAX_VIBRATION_DURATION_MS)
                .suffix(" ms"),
        );
        let changed = response.changed();
        response.labelled_by(label.id);
        if changed {
            auto_save.request_debounced_save();
        }
        let _ = id_source;
    });
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

        self.session_controller
            .pump(&self.async_result_tx, &self.changes);

        self.reset_setup_dismissal_if_complete();
        self.sync_setup_section_revision();

        egui::CentralPanel::default().show(ctx, |ui| {
            let setup_summary = self.setup_summary();
            ui.heading("CS2 Love");

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    if setup_summary.needs_setup() && self.changes.setup_dismissed {
                        self.render_setup_banner(ui, &setup_summary);
                        ui.separator();
                    }

                    self.render_intiface_section(ui);
                    ui.separator();
                    self.render_rewards_section(ui);
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

        // Keep periodic UI ticks for connection-status freshness and debounce checks.
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
