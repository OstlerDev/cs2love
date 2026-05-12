use std::{fs::OpenOptions, io::Write};

use log::error;
use serde::{Deserialize, Deserializer, Serialize};

use crate::sounds::{BundledSound, SoundChoice, VOLUME_PERCENT_MAX};

pub const CONFIG_FILE_PATH: &str = "cs2shock-config.json";
pub const MIN_SHOCK_DURATION: f32 = 0.1;
pub const MAX_SHOCK_DURATION: f32 = 5.0;
pub const MIN_REWARD_KILL_THRESHOLD: i32 = 1;
pub const MAX_REWARD_KILL_THRESHOLD: i32 = 5;
const SHOCK_DURATION_STEP: f32 = 0.1;
const SHOCK_DURATION_EPSILON: f32 = 0.000_1;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub enum ShockMode {
    Random,
    LastHitPercentage,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShockTimingMode {
    Immediate,
    EndOfRound,
    #[default]
    EndOfRoundIfTeamLoses,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ShockTimingModeRepr {
    Mode(ShockTimingMode),
    Legacy(bool),
}

fn deserialize_shock_timing_mode<'de, D>(deserializer: D) -> Result<ShockTimingMode, D::Error>
where
    D: Deserializer<'de>,
{
    let value = ShockTimingModeRepr::deserialize(deserializer)?;

    Ok(match value {
        ShockTimingModeRepr::Mode(mode) => mode,
        ShockTimingModeRepr::Legacy(true) => ShockTimingMode::EndOfRoundIfTeamLoses,
        ShockTimingModeRepr::Legacy(false) => ShockTimingMode::Immediate,
    })
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoundEndRewardGating {
    #[default]
    Always,
    OnlyIfTeamWins,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct RewardConfig {
    pub kill_reward_enabled: bool,
    pub kill_reward_sound: SoundChoice,
    pub kill_reward_volume_percent: u32,

    pub round_end_reward_enabled: bool,
    pub round_end_reward_kill_threshold: i32,
    pub round_end_reward_gating: RoundEndRewardGating,
    pub round_end_reward_sound: SoundChoice,
    pub round_end_reward_volume_percent: u32,
}

impl RewardConfig {
    pub fn is_valid(&self) -> bool {
        if self.kill_reward_volume_percent > VOLUME_PERCENT_MAX {
            error!(
                target: "Config",
                "kill_reward_volume_percent must be between 0 and {VOLUME_PERCENT_MAX}"
            );
            return false;
        }

        if self.round_end_reward_volume_percent > VOLUME_PERCENT_MAX {
            error!(
                target: "Config",
                "round_end_reward_volume_percent must be between 0 and {VOLUME_PERCENT_MAX}"
            );
            return false;
        }

        if !(MIN_REWARD_KILL_THRESHOLD..=MAX_REWARD_KILL_THRESHOLD)
            .contains(&self.round_end_reward_kill_threshold)
        {
            error!(
                target: "Config",
                "round_end_reward_kill_threshold must be between {MIN_REWARD_KILL_THRESHOLD} and {MAX_REWARD_KILL_THRESHOLD}"
            );
            return false;
        }

        true
    }
}

impl Default for RewardConfig {
    fn default() -> Self {
        Self {
            kill_reward_enabled: true,
            kill_reward_sound: SoundChoice::Bundled(BundledSound::Clicker),
            kill_reward_volume_percent: 100,
            round_end_reward_enabled: true,
            round_end_reward_kill_threshold: 3,
            round_end_reward_gating: RoundEndRewardGating::Always,
            round_end_reward_sound: SoundChoice::Bundled(BundledSound::GoodPuppy1),
            round_end_reward_volume_percent: 100,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    pub shock_mode: ShockMode,
    pub min_duration: f32,
    pub max_duration: f32,
    pub min_intensity: i32,
    pub max_intensity: i32,
    pub beep_on_match_start: bool,
    pub beep_on_round_start: bool,
    pub warning_beep_before_shock: bool,
    pub warning_beep_duration: i32,
    #[serde(alias = "warning_beep_shock_chance")]
    pub shock_chance: i32,
    #[serde(
        alias = "shock_on_round_loss_only",
        default,
        deserialize_with = "deserialize_shock_timing_mode"
    )]
    pub shock_timing_mode: ShockTimingMode,
    pub prevent_shock_if_round_kills_reached: bool,
    pub round_kills_to_prevent_shock: i32,
    pub username: String,
    pub selected_client_id: Option<u64>,
    pub selected_shocker_id: Option<u64>,
    pub selected_device_name: String,
    pub selected_shocker_name: String,
    pub apikey: String,
    pub setup_dismissed: bool,
    pub rewards: RewardConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shock_mode: ShockMode::Random,
            min_duration: 0.3,
            max_duration: 1.0,
            min_intensity: 1,
            max_intensity: 15,
            beep_on_match_start: true,
            beep_on_round_start: false,
            warning_beep_before_shock: true,
            warning_beep_duration: 2,
            shock_chance: 50,
            shock_timing_mode: ShockTimingMode::default(),
            prevent_shock_if_round_kills_reached: true,
            round_kills_to_prevent_shock: 1,
            username: String::new(),
            selected_client_id: None,
            selected_shocker_id: None,
            selected_device_name: String::new(),
            selected_shocker_name: String::new(),
            apikey: String::new(),
            setup_dismissed: false,
            rewards: RewardConfig::default(),
        }
    }
}

pub fn shock_duration_to_tenths(duration: f32) -> i32 {
    (duration / SHOCK_DURATION_STEP).round() as i32
}

fn is_valid_shock_duration(duration: f32) -> bool {
    let duration_tenths = duration / SHOCK_DURATION_STEP;
    (MIN_SHOCK_DURATION..=MAX_SHOCK_DURATION).contains(&duration)
        && (duration_tenths - duration_tenths.round()).abs() <= SHOCK_DURATION_EPSILON
}

impl Config {
    pub fn validate(&self) -> bool {
        if !is_valid_shock_duration(self.min_duration) {
            error!(target: "Config", "min_duration must be between 0.1 and 5.0 in steps of 0.1");
            return false;
        }

        if !is_valid_shock_duration(self.max_duration) {
            error!(target: "Config", "max_duration must be between 0.1 and 5.0 in steps of 0.1");
            return false;
        }

        if self.warning_beep_duration < 1 || self.warning_beep_duration > 15 {
            error!(target: "Config", "warning_beep_duration must be between 1 and 15");
            return false;
        }

        if self.shock_chance < 0 || self.shock_chance > 100 {
            error!(target: "Config", "shock_chance must be between 0 and 100");
            return false;
        }

        if self.round_kills_to_prevent_shock < 1 || self.round_kills_to_prevent_shock > 5 {
            error!(
                target: "Config",
                "round_kills_to_prevent_shock must be between 1 and 5"
            );
            return false;
        }

        if self.min_duration > self.max_duration {
            error!(target: "Config", "min_duration must be less than or equal to max_duration");
            return false;
        }

        if self.min_intensity < 1 || self.min_intensity > 100 {
            error!(target: "Config", "min_intensity must be between 1 and 100");
            return false;
        }

        if self.max_intensity < 1 || self.max_intensity > 100 {
            error!(target: "Config", "max_intensity must be between 1 and 100");
            return false;
        }

        if self.min_intensity > self.max_intensity {
            error!(target: "Config", "min_intensity must be less than or equal to max_intensity");
            return false;
        }

        if !self.rewards.is_valid() {
            return false;
        }

        return true;
    }

    pub fn try_write_to_file(&self, path: &str) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| format!("Failed to open config file `{path}`: {e}"))?;

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {e}"))?;

        file.write_all(json.as_bytes())
            .map_err(|e| format!("Failed to write config file `{path}`: {e}"))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, RewardConfig, RoundEndRewardGating, ShockTimingMode};
    use crate::sounds::{BundledSound, SoundChoice};

    #[test]
    fn default_config_matches_application_defaults() {
        let config = Config::default();

        assert_eq!(config.min_duration, 0.3);
        assert_eq!(config.max_duration, 1.0);
        assert_eq!(config.min_intensity, 1);
        assert_eq!(config.max_intensity, 15);
        assert!(config.beep_on_match_start);
        assert!(!config.beep_on_round_start);
        assert!(config.warning_beep_before_shock);
        assert_eq!(config.warning_beep_duration, 2);
        assert_eq!(config.shock_chance, 50);
        assert_eq!(
            config.shock_timing_mode,
            ShockTimingMode::EndOfRoundIfTeamLoses
        );
        assert!(config.prevent_shock_if_round_kills_reached);
        assert_eq!(config.round_kills_to_prevent_shock, 1);
        assert!(!config.setup_dismissed);
    }

    #[test]
    fn validate_rejects_zero_intensity() {
        let mut config = Config::default();
        config.min_intensity = 0;

        assert!(!config.validate());
    }

    #[test]
    fn validate_accepts_tenth_second_shock_durations() {
        let mut config = Config::default();
        config.min_duration = 0.3;
        config.max_duration = 5.0;

        assert!(config.validate());
    }

    #[test]
    fn validate_rejects_shock_duration_above_maximum() {
        let mut config = Config::default();
        config.max_duration = 5.1;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_shock_duration_with_more_than_one_decimal_place() {
        let mut config = Config::default();
        config.min_duration = 0.25;

        assert!(!config.validate());
    }

    #[test]
    fn deserialize_legacy_warning_beep_shock_chance_maps_to_shock_chance() {
        let json = serde_json::json!({
            "warning_beep_shock_chance": 35
        });

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(config.shock_chance, 35);
    }

    #[test]
    fn deserialize_missing_setup_dismissed_defaults_to_false() {
        let json = serde_json::json!({
            "username": "player",
            "apikey": "key"
        });

        let config: Config = serde_json::from_value(json).unwrap();

        assert!(!config.setup_dismissed);
    }

    #[test]
    fn deserialize_missing_shock_timing_mode_uses_default_mode() {
        let json = serde_json::json!({
            "username": "player",
            "apikey": "key"
        });

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(
            config.shock_timing_mode,
            ShockTimingMode::EndOfRoundIfTeamLoses
        );
    }

    #[test]
    fn deserialize_legacy_round_loss_only_true_maps_to_end_of_round_if_team_loses() {
        let json = serde_json::json!({
            "shock_on_round_loss_only": true
        });

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(
            config.shock_timing_mode,
            ShockTimingMode::EndOfRoundIfTeamLoses
        );
    }

    #[test]
    fn deserialize_legacy_round_loss_only_false_maps_to_immediate_mode() {
        let json = serde_json::json!({
            "shock_on_round_loss_only": false
        });

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(config.shock_timing_mode, ShockTimingMode::Immediate);
    }

    #[test]
    fn validate_rejects_shock_chance_above_one_hundred() {
        let mut config = Config::default();
        config.shock_chance = 101;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_zero_round_kill_threshold() {
        let mut config = Config::default();
        config.round_kills_to_prevent_shock = 0;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_round_kill_threshold_above_five() {
        let mut config = Config::default();
        config.round_kills_to_prevent_shock = 6;

        assert!(!config.validate());
    }

    #[test]
    fn default_rewards_match_application_defaults() {
        let rewards = RewardConfig::default();

        assert!(rewards.kill_reward_enabled);
        assert_eq!(
            rewards.kill_reward_sound,
            SoundChoice::Bundled(BundledSound::Clicker)
        );
        assert_eq!(rewards.kill_reward_volume_percent, 100);
        assert!(rewards.round_end_reward_enabled);
        assert_eq!(rewards.round_end_reward_kill_threshold, 3);
        assert_eq!(
            rewards.round_end_reward_gating,
            RoundEndRewardGating::Always
        );
        assert_eq!(
            rewards.round_end_reward_sound,
            SoundChoice::Bundled(BundledSound::GoodPuppy1)
        );
        assert_eq!(rewards.round_end_reward_volume_percent, 100);
    }

    #[test]
    fn validate_rejects_kill_volume_above_two_hundred() {
        let mut config = Config::default();
        config.rewards.kill_reward_volume_percent = 201;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_round_end_volume_above_two_hundred() {
        let mut config = Config::default();
        config.rewards.round_end_reward_volume_percent = 500;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_round_end_threshold_below_one() {
        let mut config = Config::default();
        config.rewards.round_end_reward_kill_threshold = 0;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_round_end_threshold_above_five() {
        let mut config = Config::default();
        config.rewards.round_end_reward_kill_threshold = 6;

        assert!(!config.validate());
    }

    #[test]
    fn deserialize_missing_rewards_field_uses_default() {
        let json = serde_json::json!({
            "username": "player",
            "apikey": "key"
        });

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(config.rewards, RewardConfig::default());
    }

    #[test]
    fn deserialize_partial_rewards_fills_missing_fields_from_default() {
        let json = serde_json::json!({
            "rewards": {
                "kill_reward_volume_percent": 175
            }
        });

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(config.rewards.kill_reward_volume_percent, 175);
        assert_eq!(
            config.rewards.kill_reward_sound,
            SoundChoice::Bundled(BundledSound::Clicker)
        );
    }
}
