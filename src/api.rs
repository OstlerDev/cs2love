use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use log::{debug, info};
use rand::{thread_rng, Rng};
use tokio::sync::{Mutex, RwLock};

use crate::{
    config::{self, shock_duration_to_tenths, Config, RewardConfig, RoundEndRewardGating, ShockTimingMode},
    gamestateintegration::{MapPhase, Payload, RoundPhase},
    pishock, sounds,
    setup::EXPECTED_GSI_URI,
    AppState, GameState, PendingRoundEndReward, PendingShock, PlayerState,
};

pub async fn run(config: Arc<RwLock<Config>>) {
    let state = AppState {
        game_state: Arc::from(Mutex::from(GameState::default())),
        config: config.clone(),
    };

    let app = Router::new()
        .route("/data", post(read_data))
        .with_state(state);

    info!("Starting server on {}", EXPECTED_GSI_URI);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn calculate_shock_severity(config: &Config, health_before_death: i32) -> i32 {
    match config.shock_mode {
        config::ShockMode::Random => thread_rng().gen_range(1..=100),
        config::ShockMode::LastHitPercentage => health_before_death.clamp(1, 100),
    }
}

fn scale_severity_value(severity: i32, min_value: i32, max_value: i32) -> i32 {
    if min_value >= max_value {
        return min_value;
    }

    let scale = (severity.clamp(1, 100) - 1) as f32 / 99.0;
    let scaled = min_value as f32 + scale * (max_value - min_value) as f32;

    scaled.round() as i32
}

fn warning_beep_delay(config: &Config) -> Option<Duration> {
    config
        .warning_beep_before_shock
        .then(|| Duration::from_secs(config.warning_beep_duration as u64))
}

fn should_send_shock_after_roll(shock_chance: i32, roll: i32) -> bool {
    roll <= shock_chance
}

fn should_send_shock(config: &Config) -> bool {
    if config.shock_chance >= 100 {
        return true;
    }

    if config.shock_chance <= 0 {
        return false;
    }

    let roll = thread_rng().gen_range(1..=100);
    should_send_shock_after_roll(config.shock_chance, roll)
}

fn should_trigger_death_sequence(
    previous_deaths: i32,
    current_deaths: i32,
    triggered_this_round: bool,
    shocks_disabled_until_next_round: bool,
    round_phase: &RoundPhase,
) -> bool {
    current_deaths > previous_deaths
        && !triggered_this_round
        && !shocks_disabled_until_next_round
        && *round_phase == RoundPhase::Live
}

fn should_prevent_shock_for_round_kills(config: &Config, round_kills: i32) -> bool {
    config.prevent_shock_if_round_kills_reached
        && round_kills >= config.round_kills_to_prevent_shock
}

fn should_trigger_kill_reward(
    rewards: &RewardConfig,
    previous_kills: i32,
    current_kills: i32,
    map_phase: &MapPhase,
    round_phase: &RoundPhase,
) -> bool {
    rewards.kill_reward_enabled
        && current_kills > previous_kills
        && *map_phase == MapPhase::Live
        && *round_phase == RoundPhase::Live
}

fn arm_round_end_reward_if_eligible(
    rewards: &RewardConfig,
    round_kills: i32,
) -> Option<PendingRoundEndReward> {
    if !rewards.round_end_reward_enabled {
        return None;
    }
    if round_kills < rewards.round_end_reward_kill_threshold {
        return None;
    }
    Some(PendingRoundEndReward {
        sound: rewards.round_end_reward_sound.clone(),
        volume_percent: rewards.round_end_reward_volume_percent,
        gating: rewards.round_end_reward_gating,
    })
}

fn resolve_death_shock(
    timing_mode: ShockTimingMode,
    severity: i32,
) -> (Option<PendingShock>, Option<i32>) {
    match timing_mode {
        ShockTimingMode::Immediate => (None, Some(severity)),
        ShockTimingMode::EndOfRound | ShockTimingMode::EndOfRoundIfTeamLoses => (
            Some(PendingShock {
                severity,
                timing_mode,
            }),
            None,
        ),
    }
}

fn normalize_team_name(team: &str) -> String {
    team.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn did_player_win_round(player_team: Option<&str>, round_winner: Option<&str>) -> bool {
    let Some(player_team) = player_team else {
        return false;
    };
    let Some(round_winner) = round_winner else {
        return false;
    };

    let normalized_player_team = normalize_team_name(player_team);
    let normalized_round_winner = normalize_team_name(round_winner);

    normalized_player_team == normalized_round_winner
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundOutcome {
    Won,
    Lost,
    Unknown,
}

fn round_outcome_for_player(player_team: Option<&str>, round_winner: Option<&str>) -> RoundOutcome {
    let Some(player_team) = player_team else {
        return RoundOutcome::Unknown;
    };
    let Some(round_winner) = round_winner else {
        return RoundOutcome::Unknown;
    };

    if did_player_win_round(Some(player_team), Some(round_winner)) {
        RoundOutcome::Won
    } else {
        RoundOutcome::Lost
    }
}

fn scale_severity_duration_ms(severity: i32, min_duration: f32, max_duration: f32) -> u64 {
    let duration_tenths = scale_severity_value(
        severity,
        shock_duration_to_tenths(min_duration),
        shock_duration_to_tenths(max_duration),
    );
    duration_tenths as u64 * 100
}

fn resolve_shock_values(config: &Config, severity: i32) -> (i32, u64) {
    let intensity = scale_severity_value(severity, config.min_intensity, config.max_intensity);
    let duration_ms =
        scale_severity_duration_ms(severity, config.min_duration, config.max_duration);

    (intensity, duration_ms)
}

fn resolve_deferred_round_end_shock(
    game_state: &mut GameState,
    round_winner: Option<&str>,
) -> Option<i32> {
    let Some(pending_shock) = game_state.pending_round_end_shock else {
        return None;
    };
    let player_team = game_state.player_team.clone();
    let round_outcome = round_outcome_for_player(player_team.as_deref(), round_winner);

    debug!(
        "Round result available: player_team={:?}, round_winner={:?}, outcome={:?}, timing_mode={:?}",
        player_team, round_winner, round_outcome, pending_shock.timing_mode
    );

    match (pending_shock.timing_mode, round_outcome) {
        (_, RoundOutcome::Unknown) => None,
        (ShockTimingMode::EndOfRoundIfTeamLoses, RoundOutcome::Won) => {
            if !game_state.shocks_disabled_until_next_round {
                debug!(
                    "Suppressed shocks until next round: player_team={:?}, round_winner={:?}",
                    player_team, round_winner
                );
                info!("Round won, disabling shocks until next round");
            }

            game_state.shocks_disabled_until_next_round = true;

            if game_state.pending_round_end_shock.take().is_some() {
                info!("Cancelled deferred shock because your team won the round");
            }

            None
        }
        (ShockTimingMode::EndOfRound | ShockTimingMode::Immediate, _)
        | (ShockTimingMode::EndOfRoundIfTeamLoses, RoundOutcome::Lost) => {
            if let Some(deferred_shock) = game_state.pending_round_end_shock.take() {
                let trigger_message = match pending_shock.timing_mode {
                    ShockTimingMode::EndOfRoundIfTeamLoses => {
                        "Round lost after death, triggering deferred shock"
                    }
                    ShockTimingMode::EndOfRound | ShockTimingMode::Immediate => {
                        "Round ended after death, triggering deferred shock"
                    }
                };
                info!("{}", trigger_message);
                game_state.shocks_disabled_until_next_round = true;
                Some(deferred_shock.severity)
            } else {
                None
            }
        }
    }
}

fn resolve_pending_round_end_reward(
    game_state: &mut GameState,
    round_winner: Option<&str>,
) -> Option<(crate::sounds::SoundChoice, u32)> {
    let pending = game_state.pending_round_end_reward.as_ref()?;
    let outcome = round_outcome_for_player(game_state.player_team.as_deref(), round_winner);

    match (pending.gating, outcome) {
        (RoundEndRewardGating::Always, _) => {
            let reward = game_state.pending_round_end_reward.take()?;
            Some((reward.sound, reward.volume_percent))
        }
        (RoundEndRewardGating::OnlyIfTeamWins, RoundOutcome::Won) => {
            let reward = game_state.pending_round_end_reward.take()?;
            Some((reward.sound, reward.volume_percent))
        }
        (RoundEndRewardGating::OnlyIfTeamWins, RoundOutcome::Lost) => {
            game_state.pending_round_end_reward.take();
            None
        }
        (RoundEndRewardGating::OnlyIfTeamWins, RoundOutcome::Unknown) => None,
    }
}

async fn send_shock_sequence(config_handle: Arc<RwLock<Config>>, severity: i32) {
    let warning_config = config_handle.read().await.clone();
    if let Some(delay) = warning_beep_delay(&warning_config) {
        info!(
            "Sending warning beep before shock (duration: {})",
            warning_config.warning_beep_duration
        );
        pishock::beep(config_handle.clone(), warning_config.warning_beep_duration).await;
        tokio::time::sleep(delay).await;
    }

    let config = config_handle.read().await.clone();
    if !should_send_shock(&config) {
        info!(
            "Skipping shock because the shock chance roll failed (shock chance: {}%)",
            config.shock_chance
        );
        return;
    }

    let (intensity, duration_ms) = resolve_shock_values(&config, severity);
    pishock::shock(config_handle, intensity, duration_ms).await;
}

async fn read_data(State(state): State<AppState>, Json(payload): Json<Payload>) -> StatusCode {
    let mut game_state = state.game_state.lock().await;
    let config = state.config.read().await.clone();
    let previous_map_phase = game_state.map_phase.clone();
    let previous_round_phase = game_state.round_phase.clone();
    let payload_timestamp = payload.provider.as_ref().map(|provider| provider.timestamp);
    let payload_map_phase = payload.map.as_ref().map(|map| map.phase.clone());
    let payload_round_phase = payload.round.as_ref().map(|round| round.phase.clone());
    let payload_round_winner = payload
        .round
        .as_ref()
        .and_then(|round| round.win_team.clone());
    let map_phase_changed = payload_map_phase
        .as_ref()
        .map(|phase| phase != &previous_map_phase)
        .unwrap_or(false);
    let round_phase_changed = payload_round_phase
        .as_ref()
        .map(|phase| phase != &previous_round_phase)
        .unwrap_or(false);
    let mut should_beep_on_match_start = false;
    let mut should_beep_on_round_start = false;
    let mut pending_shock = None;
    let mut pending_kill_reward: Option<(crate::sounds::SoundChoice, u32)> = None;
    let mut pending_reward_to_play: Option<(crate::sounds::SoundChoice, u32)> = None;

    if map_phase_changed
        || round_phase_changed
        || payload_map_phase
            .as_ref()
            .map(|phase| phase == &MapPhase::GameOver)
            .unwrap_or(false)
        || payload_round_phase
            .as_ref()
            .map(|phase| phase == &RoundPhase::Over)
            .unwrap_or(false)
    {
        info!(
            "GSI transition ts={:?} map={:?}->{:?} round={:?}->{:?} win_team={:?} player_team={:?} pending_round_end_shock={} triggered_this_round={} shocks_disabled_until_next_round={}",
            payload_timestamp,
            previous_map_phase,
            payload_map_phase,
            previous_round_phase,
            payload_round_phase,
            payload_round_winner,
            game_state.player_team,
            game_state.pending_round_end_shock.is_some(),
            game_state.triggered_this_round,
            game_state.shocks_disabled_until_next_round,
        );
    }

    if payload_map_phase
        .as_ref()
        .map(|phase| phase == &MapPhase::GameOver)
        .unwrap_or(false)
        || payload_round_phase
            .as_ref()
            .map(|phase| phase == &RoundPhase::Over)
            .unwrap_or(false)
    {
        info!("GSI terminal payload: {:?}", &payload);
    }

    if let Some(provider) = payload.provider {
        game_state.steam_id = provider.steamid;
    }

    if let Some(map) = payload.map {
        if game_state.map_phase == MapPhase::Warmup && map.phase == MapPhase::Live {
            info!("Match started");

            if config.beep_on_match_start {
                should_beep_on_match_start = true;
            }

            // Reset game state to default
            game_state.reset();
        }

        game_state.map_phase = map.phase;
    }

    if let Some(round) = payload.round.as_ref() {
        if let Some(severity) =
            resolve_deferred_round_end_shock(&mut game_state, round.win_team.as_deref())
        {
            pending_shock = Some(severity);
        }

        if game_state.round_phase == RoundPhase::Live && round.phase == RoundPhase::Over {
            if let Some(reward) =
                arm_round_end_reward_if_eligible(&config.rewards, game_state.current_round_kills)
            {
                info!(
                    "Round ended with {} kill(s); arming round-end reward",
                    game_state.current_round_kills
                );
                game_state.pending_round_end_reward = Some(reward);
            }
        }

        if let Some(reward) =
            resolve_pending_round_end_reward(&mut game_state, round.win_team.as_deref())
        {
            pending_reward_to_play = Some(reward);
        }

        if game_state.round_phase != round.phase && round.phase == RoundPhase::Freezetime {
            game_state.triggered_this_round = false;
            game_state.shocks_disabled_until_next_round = false;
            game_state.current_round_kills = 0;
            if game_state.pending_round_end_shock.take().is_some() {
                debug!("Cleared deferred round-end shock at freezetime");
            }
            if game_state.pending_round_end_reward.take().is_some() {
                debug!("Cleared deferred round-end reward at freezetime");
            }
        }

        if game_state.round_phase == RoundPhase::Freezetime && round.phase == RoundPhase::Live {
            game_state.triggered_this_round = false;
            game_state.shocks_disabled_until_next_round = false;
            game_state.current_round_kills = 0;
            if game_state.pending_round_end_shock.take().is_some() {
                debug!("Cleared deferred round-end shock at round start");
            }
            if game_state.pending_round_end_reward.take().is_some() {
                debug!("Cleared deferred round-end reward at round start");
            }

            if config.beep_on_round_start {
                info!("Round started");
                should_beep_on_round_start = true;
            }
        }

        game_state.round_phase = round.phase.clone();
    }

    if game_state.map_phase == MapPhase::Live {
        if let Some(player) = payload.player {
            if player.steamid == game_state.steam_id {
                game_state.player_team = player.team.clone();
                game_state.current_round_kills = player.state.round_kills;
                let already_triggered_this_round = game_state.triggered_this_round;
                let shocks_disabled_until_next_round = game_state.shocks_disabled_until_next_round;
                let round_phase = if previous_round_phase == RoundPhase::Live
                    || game_state.round_phase == RoundPhase::Live
                {
                    RoundPhase::Live
                } else {
                    game_state.round_phase.clone()
                };

                let map_phase_now = game_state.map_phase.clone();
                if let Some(player_state) = &mut game_state.player_state {
                    let mut should_mark_triggered_this_round = false;
                    let mut deferred_round_end_shock = None;

                    if should_trigger_kill_reward(
                        &config.rewards,
                        player_state.kills,
                        player.match_stats.kills,
                        &map_phase_now,
                        &round_phase,
                    ) {
                        info!(
                            "Player got a kill ({}), playing kill reward",
                            player.match_stats.kills
                        );
                        pending_kill_reward = Some((
                            config.rewards.kill_reward_sound.clone(),
                            config.rewards.kill_reward_volume_percent,
                        ));
                    }

                    if player_state.health > player.state.health && player.state.health > 0 {
                        // Took damage and survived

                        /*
                        println!("Player took damage, vibrating");

                        let diff = player_state.health - player.state.health;

                        let res = pishock::post(
                            &config,
                            pishock::PiShockOp::Vibrate {
                                intensity: diff,
                                duration: 1,
                            },
                        )
                        .await;

                        match res {
                            Ok(code) => println!("Vibrated with code {}", code),
                            Err(e) => println!("Error while vibrating: {}", e),
                        };
                         */
                    }

                    if should_trigger_death_sequence(
                        player_state.deaths,
                        player.match_stats.deaths,
                        already_triggered_this_round,
                        shocks_disabled_until_next_round,
                        &round_phase,
                    ) {
                        should_mark_triggered_this_round = true;

                        if should_prevent_shock_for_round_kills(&config, player.state.round_kills) {
                            info!(
                                "Player died after reaching round kill threshold ({}), skipping shock",
                                player.state.round_kills
                            );
                        } else {
                            let severity = calculate_shock_severity(&config, player_state.health);

                            let (deferred_shock, immediate_shock) =
                                resolve_death_shock(config.shock_timing_mode, severity);
                            match config.shock_timing_mode {
                                ShockTimingMode::Immediate => {
                                    info!("Player died, shocking immediately");
                                }
                                ShockTimingMode::EndOfRound => {
                                    info!("Player died, deferring shock until round end");
                                }
                                ShockTimingMode::EndOfRoundIfTeamLoses => {
                                    info!("Player died, deferring shock until round result");
                                }
                            }
                            deferred_round_end_shock = deferred_shock;
                            if let Some(severity) = immediate_shock {
                                pending_shock = Some(severity);
                            }
                        }
                    }

                    player_state.health = player.state.health;
                    player_state.armor = player.state.armor;
                    player_state.kills = player.match_stats.kills;
                    player_state.deaths = player.match_stats.deaths;

                    if should_mark_triggered_this_round {
                        game_state.triggered_this_round = true;
                    }

                    if let Some(deferred_shock) = deferred_round_end_shock {
                        game_state.pending_round_end_shock = Some(deferred_shock);
                    }
                } else {
                    println!("Player state initialized");

                    game_state.player_state = Some(PlayerState {
                        health: player.state.health,
                        armor: player.state.armor,
                        kills: player.match_stats.kills,
                        deaths: player.match_stats.deaths,
                    });
                }
            }
        }
    }

    drop(game_state);

    if should_beep_on_match_start {
        pishock::beep(state.config.clone(), 2).await;
    }

    if should_beep_on_round_start {
        pishock::beep(state.config.clone(), 1).await;
    }

    if let Some((sound, volume)) = pending_kill_reward {
        sounds::play(sound, volume);
    }

    if let Some((sound, volume)) = pending_reward_to_play {
        sounds::play(sound, volume);
    }

    if let Some(severity) = pending_shock {
        send_shock_sequence(state.config.clone(), severity).await;
    }

    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::{
        arm_round_end_reward_if_eligible, calculate_shock_severity, did_player_win_round,
        resolve_death_shock, resolve_deferred_round_end_shock, resolve_pending_round_end_reward,
        resolve_shock_values, round_outcome_for_player, scale_severity_value,
        should_prevent_shock_for_round_kills, should_send_shock, should_send_shock_after_roll,
        should_trigger_death_sequence, should_trigger_kill_reward, warning_beep_delay,
        RoundOutcome,
    };
    use crate::gamestateintegration::{MapPhase, RoundPhase};
    use crate::sounds::{BundledSound, SoundChoice};
    use crate::{
        config::{Config, RewardConfig, RoundEndRewardGating, ShockMode, ShockTimingMode},
        GameState, PendingRoundEndReward, PendingShock,
    };
    use std::time::Duration;

    #[test]
    fn scale_severity_value_anchors_to_configured_bounds() {
        assert_eq!(scale_severity_value(1, 20, 40), 20);
        assert_eq!(scale_severity_value(100, 20, 40), 40);
    }

    #[test]
    fn scale_severity_value_uses_shared_percentage_across_range() {
        assert_eq!(scale_severity_value(25, 20, 40), 25);
        assert_eq!(scale_severity_value(50, 20, 40), 30);
    }

    #[test]
    fn warning_beep_delay_uses_default_duration() {
        assert_eq!(
            warning_beep_delay(&Config::default()),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn warning_beep_delay_uses_configured_duration() {
        let mut config = Config::default();
        config.warning_beep_before_shock = true;
        config.warning_beep_duration = 3;

        assert_eq!(warning_beep_delay(&config), Some(Duration::from_secs(3)));
    }

    #[test]
    fn should_send_shock_after_roll_respects_probability() {
        assert!(should_send_shock_after_roll(50, 50));
        assert!(!should_send_shock_after_roll(50, 51));
    }

    #[test]
    fn should_send_shock_respects_zero_percent_chance_without_warning_beep() {
        let mut config = Config::default();
        config.warning_beep_before_shock = false;
        config.shock_chance = 0;

        assert!(!should_send_shock(&config));
    }

    #[test]
    fn should_send_shock_respects_hundred_percent_chance_without_warning_beep() {
        let mut config = Config::default();
        config.warning_beep_before_shock = false;
        config.shock_chance = 100;

        assert!(should_send_shock(&config));
    }

    #[test]
    fn should_send_shock_respects_zero_percent_chance_with_warning_beep() {
        let mut config = Config::default();
        config.warning_beep_before_shock = true;
        config.shock_chance = 0;

        assert!(!should_send_shock(&config));
    }

    #[test]
    fn should_send_shock_respects_hundred_percent_chance_with_warning_beep() {
        let mut config = Config::default();
        config.warning_beep_before_shock = true;
        config.shock_chance = 100;

        assert!(should_send_shock(&config));
    }

    #[test]
    fn should_trigger_death_sequence_only_once_per_round() {
        assert!(should_trigger_death_sequence(
            3,
            4,
            false,
            false,
            &RoundPhase::Live
        ));
        assert!(!should_trigger_death_sequence(
            3,
            4,
            true,
            false,
            &RoundPhase::Live
        ));
        assert!(!should_trigger_death_sequence(
            3,
            3,
            false,
            false,
            &RoundPhase::Live
        ));
    }

    #[test]
    fn should_not_trigger_death_sequence_after_round_win() {
        assert!(!should_trigger_death_sequence(
            3,
            4,
            false,
            true,
            &RoundPhase::Live
        ));
        assert!(!should_trigger_death_sequence(
            3,
            4,
            false,
            false,
            &RoundPhase::Over
        ));
    }

    #[test]
    fn round_kill_threshold_suppression_uses_default_threshold() {
        assert!(!should_prevent_shock_for_round_kills(&Config::default(), 0));
        assert!(should_prevent_shock_for_round_kills(&Config::default(), 1));
    }

    #[test]
    fn round_kill_threshold_suppression_waits_until_threshold_is_reached() {
        let mut config = Config::default();
        config.prevent_shock_if_round_kills_reached = true;
        config.round_kills_to_prevent_shock = 3;

        assert!(!should_prevent_shock_for_round_kills(&config, 2));
    }

    #[test]
    fn round_kill_threshold_suppression_triggers_at_threshold() {
        let mut config = Config::default();
        config.prevent_shock_if_round_kills_reached = true;
        config.round_kills_to_prevent_shock = 3;

        assert!(should_prevent_shock_for_round_kills(&config, 3));
    }

    #[test]
    fn round_kill_threshold_suppression_triggers_above_threshold() {
        let mut config = Config::default();
        config.prevent_shock_if_round_kills_reached = true;
        config.round_kills_to_prevent_shock = 3;

        assert!(should_prevent_shock_for_round_kills(&config, 4));
    }

    #[test]
    fn did_player_win_round_matches_team_names() {
        assert!(did_player_win_round(Some("CT"), Some("CT")));
        assert!(did_player_win_round(
            Some("Counter-Terrorist"),
            Some("counter_terrorist")
        ));
        assert!(!did_player_win_round(Some("T"), Some("CT")));
    }

    #[test]
    fn round_outcome_for_player_detects_win_loss_and_unknown() {
        assert_eq!(
            round_outcome_for_player(Some("CT"), Some("CT")),
            RoundOutcome::Won
        );
        assert_eq!(
            round_outcome_for_player(Some("T"), Some("CT")),
            RoundOutcome::Lost
        );
        assert_eq!(
            round_outcome_for_player(None, Some("CT")),
            RoundOutcome::Unknown
        );
    }

    #[test]
    fn resolve_death_shock_returns_immediate_shock_for_immediate_mode() {
        let (pending_shock, immediate_shock) = resolve_death_shock(ShockTimingMode::Immediate, 42);

        assert!(pending_shock.is_none());
        assert_eq!(immediate_shock, Some(42));
    }

    #[test]
    fn resolve_death_shock_defers_shock_for_end_of_round_modes() {
        for timing_mode in [
            ShockTimingMode::EndOfRound,
            ShockTimingMode::EndOfRoundIfTeamLoses,
        ] {
            let (pending_shock, immediate_shock) = resolve_death_shock(timing_mode, 42);

            assert_eq!(
                pending_shock,
                Some(PendingShock {
                    severity: 42,
                    timing_mode,
                })
            );
            assert_eq!(immediate_shock, None);
        }
    }

    #[test]
    fn resolve_deferred_round_end_shock_triggers_on_known_win_for_end_of_round_mode() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("CT".into());
        game_state.pending_round_end_shock = Some(PendingShock {
            severity: 42,
            timing_mode: ShockTimingMode::EndOfRound,
        });

        let severity = resolve_deferred_round_end_shock(&mut game_state, Some("CT"));

        assert_eq!(severity, Some(42));
        assert!(game_state.pending_round_end_shock.is_none());
        assert!(game_state.shocks_disabled_until_next_round);
    }

    #[test]
    fn resolve_deferred_round_end_shock_triggers_on_known_loss() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("T".into());
        game_state.pending_round_end_shock = Some(PendingShock {
            severity: 42,
            timing_mode: ShockTimingMode::EndOfRoundIfTeamLoses,
        });

        let severity = resolve_deferred_round_end_shock(&mut game_state, Some("CT"));

        assert_eq!(severity, Some(42));
        assert!(game_state.pending_round_end_shock.is_none());
        assert!(game_state.shocks_disabled_until_next_round);
    }

    #[test]
    fn resolve_deferred_round_end_shock_cancels_on_known_win_for_team_loss_mode() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("CT".into());
        game_state.pending_round_end_shock = Some(PendingShock {
            severity: 42,
            timing_mode: ShockTimingMode::EndOfRoundIfTeamLoses,
        });

        let severity = resolve_deferred_round_end_shock(&mut game_state, Some("CT"));

        assert_eq!(severity, None);
        assert!(game_state.pending_round_end_shock.is_none());
        assert!(game_state.shocks_disabled_until_next_round);
    }

    #[test]
    fn resolve_deferred_round_end_shock_keeps_pending_when_winner_unknown() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("T".into());
        game_state.pending_round_end_shock = Some(PendingShock {
            severity: 42,
            timing_mode: ShockTimingMode::EndOfRound,
        });

        let severity = resolve_deferred_round_end_shock(&mut game_state, None);

        assert_eq!(severity, None);
        assert_eq!(
            game_state
                .pending_round_end_shock
                .as_ref()
                .map(|pending_shock| pending_shock.severity),
            Some(42)
        );
        assert!(!game_state.shocks_disabled_until_next_round);
    }

    #[test]
    fn calculate_shock_severity_uses_last_hit_percentage_mode() {
        let mut config = Config::default();
        config.shock_mode = ShockMode::LastHitPercentage;

        assert_eq!(calculate_shock_severity(&config, 25), 25);
        assert_eq!(calculate_shock_severity(&config, 0), 1);
        assert_eq!(calculate_shock_severity(&config, 150), 100);
    }

    #[test]
    fn calculate_shock_severity_random_mode_stays_in_bounds() {
        let config = Config::default();

        for _ in 0..128 {
            let severity = calculate_shock_severity(&config, 25);
            assert!((1..=100).contains(&severity));
        }
    }

    #[test]
    fn resolve_shock_values_respects_configured_bounds() {
        let mut config = Config::default();
        config.min_intensity = 20;
        config.max_intensity = 40;
        config.min_duration = 1.0;
        config.max_duration = 1.0;

        assert_eq!(resolve_shock_values(&config, 25), (25, 1000));
        assert_eq!(resolve_shock_values(&config, 1), (20, 1000));
        assert_eq!(resolve_shock_values(&config, 100), (40, 1000));
    }

    #[test]
    fn resolve_shock_values_returns_constant_when_bounds_match() {
        let mut config = Config::default();
        config.min_intensity = 22;
        config.max_intensity = 22;
        config.min_duration = 0.8;
        config.max_duration = 0.8;

        assert_eq!(resolve_shock_values(&config, 1), (22, 800));
        assert_eq!(resolve_shock_values(&config, 50), (22, 800));
        assert_eq!(resolve_shock_values(&config, 100), (22, 800));
    }

    #[test]
    fn random_mode_shared_severity_keeps_shock_within_bounds() {
        let mut config = Config::default();
        config.shock_mode = ShockMode::Random;
        config.min_intensity = 20;
        config.max_intensity = 40;
        config.min_duration = 0.4;
        config.max_duration = 1.2;

        for _ in 0..128 {
            let severity = calculate_shock_severity(&config, 25);
            let (intensity, duration_ms) = resolve_shock_values(&config, severity);
            assert!((20..=40).contains(&intensity));
            assert!((400..=1200).contains(&duration_ms));
        }
    }

    #[test]
    fn pending_round_end_shock_resolves_using_current_config_bounds() {
        let pending_shock = PendingShock {
            severity: 100,
            timing_mode: ShockTimingMode::EndOfRound,
        };
        let mut config = Config::default();
        config.min_intensity = 5;
        config.max_intensity = 10;
        config.min_duration = 0.2;
        config.max_duration = 0.3;

        assert_eq!(
            resolve_shock_values(&config, pending_shock.severity),
            (10, 300)
        );
    }

    #[test]
    fn should_trigger_kill_reward_fires_on_positive_kill_delta_during_live_round() {
        let rewards = RewardConfig::default();
        assert!(should_trigger_kill_reward(
            &rewards,
            2,
            3,
            &MapPhase::Live,
            &RoundPhase::Live
        ));
    }

    #[test]
    fn should_trigger_kill_reward_ignores_zero_or_negative_delta() {
        let rewards = RewardConfig::default();
        assert!(!should_trigger_kill_reward(
            &rewards,
            3,
            3,
            &MapPhase::Live,
            &RoundPhase::Live
        ));
        assert!(!should_trigger_kill_reward(
            &rewards,
            3,
            2,
            &MapPhase::Live,
            &RoundPhase::Live
        ));
    }

    #[test]
    fn should_trigger_kill_reward_suppressed_outside_live_phase() {
        let rewards = RewardConfig::default();
        assert!(!should_trigger_kill_reward(
            &rewards,
            0,
            1,
            &MapPhase::Warmup,
            &RoundPhase::Live
        ));
        assert!(!should_trigger_kill_reward(
            &rewards,
            0,
            1,
            &MapPhase::Live,
            &RoundPhase::Freezetime
        ));
        assert!(!should_trigger_kill_reward(
            &rewards,
            0,
            1,
            &MapPhase::Live,
            &RoundPhase::Over
        ));
    }

    #[test]
    fn should_trigger_kill_reward_respects_disabled_setting() {
        let mut rewards = RewardConfig::default();
        rewards.kill_reward_enabled = false;
        assert!(!should_trigger_kill_reward(
            &rewards,
            0,
            1,
            &MapPhase::Live,
            &RoundPhase::Live
        ));
    }

    #[test]
    fn arm_round_end_reward_returns_pending_when_threshold_met() {
        let rewards = RewardConfig::default();
        let pending = arm_round_end_reward_if_eligible(&rewards, 3).expect("should arm");
        assert_eq!(pending.sound, rewards.round_end_reward_sound);
        assert_eq!(pending.volume_percent, rewards.round_end_reward_volume_percent);
        assert_eq!(pending.gating, rewards.round_end_reward_gating);
    }

    #[test]
    fn arm_round_end_reward_returns_none_below_threshold() {
        let rewards = RewardConfig::default();
        assert!(arm_round_end_reward_if_eligible(&rewards, 2).is_none());
    }

    #[test]
    fn arm_round_end_reward_returns_none_when_disabled() {
        let mut rewards = RewardConfig::default();
        rewards.round_end_reward_enabled = false;
        assert!(arm_round_end_reward_if_eligible(&rewards, 5).is_none());
    }

    fn pending_reward(gating: RoundEndRewardGating) -> PendingRoundEndReward {
        PendingRoundEndReward {
            sound: SoundChoice::Bundled(BundledSound::GoodPuppy1),
            volume_percent: 100,
            gating,
        }
    }

    #[test]
    fn resolve_pending_round_end_reward_fires_unconditionally_when_gating_is_always() {
        for winner in [Some("CT"), Some("T"), None] {
            let mut game_state = GameState::default();
            game_state.player_team = Some("CT".into());
            game_state.pending_round_end_reward = Some(pending_reward(RoundEndRewardGating::Always));

            let result = resolve_pending_round_end_reward(&mut game_state, winner);

            assert!(result.is_some(), "winner={:?} should fire reward", winner);
            assert!(game_state.pending_round_end_reward.is_none());
        }
    }

    #[test]
    fn resolve_pending_round_end_reward_fires_only_on_win_for_only_if_team_wins() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("CT".into());
        game_state.pending_round_end_reward =
            Some(pending_reward(RoundEndRewardGating::OnlyIfTeamWins));

        let result = resolve_pending_round_end_reward(&mut game_state, Some("CT"));
        assert!(result.is_some());
        assert!(game_state.pending_round_end_reward.is_none());
    }

    #[test]
    fn resolve_pending_round_end_reward_clears_silently_on_loss_for_only_if_team_wins() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("T".into());
        game_state.pending_round_end_reward =
            Some(pending_reward(RoundEndRewardGating::OnlyIfTeamWins));

        let result = resolve_pending_round_end_reward(&mut game_state, Some("CT"));
        assert!(result.is_none());
        assert!(game_state.pending_round_end_reward.is_none());
    }

    #[test]
    fn resolve_pending_round_end_reward_keeps_pending_when_winner_unknown() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("CT".into());
        game_state.pending_round_end_reward =
            Some(pending_reward(RoundEndRewardGating::OnlyIfTeamWins));

        let result = resolve_pending_round_end_reward(&mut game_state, None);
        assert!(result.is_none());
        assert!(game_state.pending_round_end_reward.is_some());
    }

    #[test]
    fn resolve_pending_round_end_reward_returns_none_when_nothing_pending() {
        let mut game_state = GameState::default();
        let result = resolve_pending_round_end_reward(&mut game_state, Some("CT"));
        assert!(result.is_none());
    }
}
