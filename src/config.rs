use std::{fs::OpenOptions, io::Write};

use log::error;
use serde::{Deserialize, Serialize};

use crate::sounds::{BundledSound, SoundChoice, VOLUME_PERCENT_MAX};

pub const CONFIG_FILE_PATH: &str = "cs2love-config.json";

pub const MIN_REWARD_KILL_THRESHOLD: i32 = 1;
pub const MAX_REWARD_KILL_THRESHOLD: i32 = 5;

pub const MAX_VIBRATION_STRENGTH_PERCENT: u32 = 100;
pub const MIN_VIBRATION_DURATION_MS: u32 = 100;
pub const MAX_VIBRATION_DURATION_MS: u32 = 10_000;

pub const DEFAULT_INTIFACE_URL: &str = "ws://127.0.0.1:12345";

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
pub struct VibrationRewardConfig {
    pub kill_vibration_enabled: bool,
    pub kill_vibration_strength_percent: u32,
    pub kill_vibration_duration_ms: u32,

    pub round_end_vibration_enabled: bool,
    pub round_end_vibration_kill_threshold: i32,
    pub round_end_vibration_gating: RoundEndRewardGating,
    pub round_end_vibration_strength_percent: u32,
    pub round_end_vibration_duration_ms: u32,
}

impl VibrationRewardConfig {
    pub fn is_valid(&self) -> bool {
        if self.kill_vibration_strength_percent > MAX_VIBRATION_STRENGTH_PERCENT {
            error!(
                target: "Config",
                "kill_vibration_strength_percent must be between 0 and {MAX_VIBRATION_STRENGTH_PERCENT}"
            );
            return false;
        }

        if self.round_end_vibration_strength_percent > MAX_VIBRATION_STRENGTH_PERCENT {
            error!(
                target: "Config",
                "round_end_vibration_strength_percent must be between 0 and {MAX_VIBRATION_STRENGTH_PERCENT}"
            );
            return false;
        }

        if !(MIN_VIBRATION_DURATION_MS..=MAX_VIBRATION_DURATION_MS)
            .contains(&self.kill_vibration_duration_ms)
        {
            error!(
                target: "Config",
                "kill_vibration_duration_ms must be between {MIN_VIBRATION_DURATION_MS} and {MAX_VIBRATION_DURATION_MS}"
            );
            return false;
        }

        if !(MIN_VIBRATION_DURATION_MS..=MAX_VIBRATION_DURATION_MS)
            .contains(&self.round_end_vibration_duration_ms)
        {
            error!(
                target: "Config",
                "round_end_vibration_duration_ms must be between {MIN_VIBRATION_DURATION_MS} and {MAX_VIBRATION_DURATION_MS}"
            );
            return false;
        }

        if !(MIN_REWARD_KILL_THRESHOLD..=MAX_REWARD_KILL_THRESHOLD)
            .contains(&self.round_end_vibration_kill_threshold)
        {
            error!(
                target: "Config",
                "round_end_vibration_kill_threshold must be between {MIN_REWARD_KILL_THRESHOLD} and {MAX_REWARD_KILL_THRESHOLD}"
            );
            return false;
        }

        true
    }
}

impl Default for VibrationRewardConfig {
    fn default() -> Self {
        Self {
            kill_vibration_enabled: true,
            kill_vibration_strength_percent: 60,
            kill_vibration_duration_ms: 1500,
            round_end_vibration_enabled: true,
            round_end_vibration_kill_threshold: 3,
            round_end_vibration_gating: RoundEndRewardGating::Always,
            round_end_vibration_strength_percent: 100,
            round_end_vibration_duration_ms: 4000,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    pub setup_dismissed: bool,
    pub intiface_websocket_url: String,
    pub selected_toy_identifiers: Vec<String>,
    pub rewards: RewardConfig,
    pub vibrations: VibrationRewardConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            setup_dismissed: false,
            intiface_websocket_url: DEFAULT_INTIFACE_URL.to_string(),
            selected_toy_identifiers: Vec::new(),
            rewards: RewardConfig::default(),
            vibrations: VibrationRewardConfig::default(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> bool {
        if !self.rewards.is_valid() {
            return false;
        }

        if !self.vibrations.is_valid() {
            return false;
        }

        true
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
    use super::{
        Config, RewardConfig, RoundEndRewardGating, VibrationRewardConfig, DEFAULT_INTIFACE_URL,
    };
    use crate::sounds::{BundledSound, SoundChoice};

    #[test]
    fn default_config_matches_application_defaults() {
        let config = Config::default();

        assert!(!config.setup_dismissed);
        assert_eq!(config.intiface_websocket_url, DEFAULT_INTIFACE_URL);
        assert!(config.selected_toy_identifiers.is_empty());
        assert_eq!(config.rewards, RewardConfig::default());
        assert_eq!(config.vibrations, VibrationRewardConfig::default());
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
    fn default_vibrations_match_application_defaults() {
        let vibrations = VibrationRewardConfig::default();

        assert!(vibrations.kill_vibration_enabled);
        assert_eq!(vibrations.kill_vibration_strength_percent, 60);
        assert_eq!(vibrations.kill_vibration_duration_ms, 1500);
        assert!(vibrations.round_end_vibration_enabled);
        assert_eq!(vibrations.round_end_vibration_kill_threshold, 3);
        assert_eq!(
            vibrations.round_end_vibration_gating,
            RoundEndRewardGating::Always
        );
        assert_eq!(vibrations.round_end_vibration_strength_percent, 100);
        assert_eq!(vibrations.round_end_vibration_duration_ms, 4000);
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
    fn validate_rejects_kill_vibration_strength_above_one_hundred() {
        let mut config = Config::default();
        config.vibrations.kill_vibration_strength_percent = 101;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_round_end_vibration_strength_above_one_hundred() {
        let mut config = Config::default();
        config.vibrations.round_end_vibration_strength_percent = 250;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_kill_vibration_duration_below_minimum() {
        let mut config = Config::default();
        config.vibrations.kill_vibration_duration_ms = 50;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_round_end_vibration_duration_above_maximum() {
        let mut config = Config::default();
        config.vibrations.round_end_vibration_duration_ms = 60_000;

        assert!(!config.validate());
    }

    #[test]
    fn validate_rejects_round_end_vibration_threshold_outside_range() {
        let mut config = Config::default();
        config.vibrations.round_end_vibration_kill_threshold = 0;
        assert!(!config.validate());

        config.vibrations.round_end_vibration_kill_threshold = 6;
        assert!(!config.validate());
    }

    #[test]
    fn deserialize_missing_setup_dismissed_defaults_to_false() {
        let json = serde_json::json!({});

        let config: Config = serde_json::from_value(json).unwrap();

        assert!(!config.setup_dismissed);
    }

    #[test]
    fn deserialize_missing_rewards_field_uses_default() {
        let json = serde_json::json!({});

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(config.rewards, RewardConfig::default());
    }

    #[test]
    fn deserialize_missing_vibrations_field_uses_default() {
        let json = serde_json::json!({});

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(config.vibrations, VibrationRewardConfig::default());
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

    #[test]
    fn deserialize_partial_vibrations_fills_missing_fields_from_default() {
        let json = serde_json::json!({
            "vibrations": {
                "kill_vibration_strength_percent": 75
            }
        });

        let config: Config = serde_json::from_value(json).unwrap();

        assert_eq!(config.vibrations.kill_vibration_strength_percent, 75);
        assert_eq!(config.vibrations.kill_vibration_duration_ms, 1500);
    }
}
