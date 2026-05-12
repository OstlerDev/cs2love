mod api;
mod config;
mod gamestateintegration;
mod gui;
mod pishock;
mod pishock_session_controller;
mod setup;
mod sounds;

use std::{
    fs::File,
    io::{Error, Read},
    sync::Arc,
};

use config::{Config, RoundEndRewardGating, ShockTimingMode, CONFIG_FILE_PATH};
use gamestateintegration::{MapPhase, RoundPhase};
use log::{error, info};
use simple_logger::SimpleLogger;
use sounds::SoundChoice;
use time::macros::format_description;
use tokio::sync::{Mutex, RwLock};

pub const NAME: &str = "CS2 Shocker";

#[derive(Debug, Clone)]
struct AppState {
    game_state: Arc<Mutex<GameState>>,
    config: Arc<RwLock<Config>>,
}

#[derive(Debug, Clone)]
struct GameState {
    round_phase: RoundPhase,
    map_phase: MapPhase,
    steam_id: String,
    player_team: Option<String>,
    player_state: Option<PlayerState>,
    triggered_this_round: bool,
    shocks_disabled_until_next_round: bool,
    pending_round_end_shock: Option<PendingShock>,
    current_round_kills: i32,
    pending_round_end_reward: Option<PendingRoundEndReward>,
}

#[derive(Debug, Clone)]
struct PlayerState {
    health: i32,
    armor: i32,
    kills: i32,
    deaths: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingShock {
    severity: i32,
    timing_mode: ShockTimingMode,
}

#[derive(Debug, Clone, PartialEq)]
struct PendingRoundEndReward {
    sound: SoundChoice,
    volume_percent: u32,
    gating: RoundEndRewardGating,
}

impl Default for GameState {
    fn default() -> Self {
        Self {
            round_phase: RoundPhase::Unknown,
            map_phase: MapPhase::Unknown,
            steam_id: String::new(),
            player_team: None,
            player_state: None,
            triggered_this_round: false,
            shocks_disabled_until_next_round: false,
            pending_round_end_shock: None,
            current_round_kills: 0,
            pending_round_end_reward: None,
        }
    }
}

impl GameState {
    fn reset(&mut self) {
        self.round_phase = RoundPhase::Unknown;
        self.map_phase = MapPhase::Unknown;
        self.player_team = None;
        self.player_state = None;
        self.triggered_this_round = false;
        self.shocks_disabled_until_next_round = false;
        self.pending_round_end_shock = None;
        self.current_round_kills = 0;
        self.pending_round_end_reward = None;
    }
}

#[tokio::main]
async fn main() {
    SimpleLogger::new()
        .env()
        .with_level(log::LevelFilter::Info)
        .with_timestamp_format(format_description!(
            "[[[year]-[month]-[day] [hour]:[minute]:[second]]"
        ))
        .init()
        .expect("Failed to initialize logger");

    // Startup log name and version of app
    info!("{} v{}", NAME, env!("CARGO_PKG_VERSION"));

    let config = || -> Result<Config, Error> {
        let mut file = File::open(CONFIG_FILE_PATH)?;
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        let conf = serde_json::from_str::<Config>(&raw)?;
        info!("Config file loaded");
        Ok(conf)
    }();

    let mut config = if let Ok(c) = config {
        Arc::new(RwLock::new(c))
    } else {
        Arc::new(RwLock::new(Config::default()))
    };

    if !config.read().await.validate() {
        config = Arc::new(RwLock::new(Config::default()));
        error!("Invalid config, using default");
    }

    // Don't log the config to the console unless we are debugging! We don't want to leak the API key!
    // info!("Config: \n{:?}", config);

    let c = config.clone();

    let task = tokio::spawn(async move {
        api::run(c).await;
    });

    gui::run(config.clone()).await;
    task.await.unwrap();
}
