use std::sync::Arc;

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use log::{debug, info};
use tokio::sync::{Mutex, RwLock};

use crate::{
    config::{Config, RewardConfig, RoundEndRewardGating, VibrationRewardConfig},
    gamestateintegration::{MapPhase, Payload, RoundPhase},
    intiface, sounds,
    setup::EXPECTED_GSI_URI,
    AppState, GameState, PendingRoundEndReward, PendingRoundEndVibration, PlayerState,
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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3001")
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
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

    normalize_team_name(player_team) == normalize_team_name(round_winner)
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

fn should_trigger_kill_vibration(
    vibrations: &VibrationRewardConfig,
    previous_kills: i32,
    current_kills: i32,
    map_phase: &MapPhase,
    round_phase: &RoundPhase,
) -> bool {
    vibrations.kill_vibration_enabled
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

fn arm_round_end_vibration_if_eligible(
    vibrations: &VibrationRewardConfig,
    round_kills: i32,
) -> Option<PendingRoundEndVibration> {
    if !vibrations.round_end_vibration_enabled {
        return None;
    }
    if round_kills < vibrations.round_end_vibration_kill_threshold {
        return None;
    }
    Some(PendingRoundEndVibration {
        strength_percent: vibrations.round_end_vibration_strength_percent,
        duration_ms: vibrations.round_end_vibration_duration_ms,
        gating: vibrations.round_end_vibration_gating,
    })
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

fn resolve_pending_round_end_vibration(
    game_state: &mut GameState,
    round_winner: Option<&str>,
) -> Option<(u32, u32)> {
    let pending = game_state.pending_round_end_vibration.as_ref()?;
    let outcome = round_outcome_for_player(game_state.player_team.as_deref(), round_winner);

    match (pending.gating, outcome) {
        (RoundEndRewardGating::Always, _) => {
            let vibration = game_state.pending_round_end_vibration.take()?;
            Some((vibration.strength_percent, vibration.duration_ms))
        }
        (RoundEndRewardGating::OnlyIfTeamWins, RoundOutcome::Won) => {
            let vibration = game_state.pending_round_end_vibration.take()?;
            Some((vibration.strength_percent, vibration.duration_ms))
        }
        (RoundEndRewardGating::OnlyIfTeamWins, RoundOutcome::Lost) => {
            game_state.pending_round_end_vibration.take();
            None
        }
        (RoundEndRewardGating::OnlyIfTeamWins, RoundOutcome::Unknown) => None,
    }
}

async fn read_data(State(state): State<AppState>, Json(payload): Json<Payload>) -> StatusCode {
    let mut game_state = state.game_state.lock().await;
    let config = state.config.read().await.clone();
    let previous_round_phase = game_state.round_phase.clone();
    let mut pending_kill_reward: Option<(crate::sounds::SoundChoice, u32)> = None;
    let mut pending_kill_vibration: Option<(u32, u32)> = None;
    let mut pending_reward_to_play: Option<(crate::sounds::SoundChoice, u32)> = None;
    let mut pending_vibration_to_play: Option<(u32, u32)> = None;

    if let Some(provider) = payload.provider {
        game_state.steam_id = provider.steamid;
    }

    if let Some(map) = payload.map {
        if game_state.map_phase == MapPhase::Warmup && map.phase == MapPhase::Live {
            info!("Match started");
            game_state.reset();
        }

        game_state.map_phase = map.phase;
    }

    if let Some(round) = payload.round.as_ref() {
        if game_state.round_phase == RoundPhase::Live && round.phase == RoundPhase::Over {
            if let Some(reward) =
                arm_round_end_reward_if_eligible(&config.rewards, game_state.current_round_kills)
            {
                info!(
                    "Round ended with {} kill(s); arming round-end sound reward",
                    game_state.current_round_kills
                );
                game_state.pending_round_end_reward = Some(reward);
            }

            if let Some(vibration) = arm_round_end_vibration_if_eligible(
                &config.vibrations,
                game_state.current_round_kills,
            ) {
                info!(
                    "Round ended with {} kill(s); arming round-end vibration",
                    game_state.current_round_kills
                );
                game_state.pending_round_end_vibration = Some(vibration);
            }
        }

        if let Some(reward) =
            resolve_pending_round_end_reward(&mut game_state, round.win_team.as_deref())
        {
            pending_reward_to_play = Some(reward);
        }

        if let Some(vibration) =
            resolve_pending_round_end_vibration(&mut game_state, round.win_team.as_deref())
        {
            pending_vibration_to_play = Some(vibration);
        }

        if game_state.round_phase != round.phase
            && (round.phase == RoundPhase::Freezetime || round.phase == RoundPhase::Live)
        {
            game_state.current_round_kills = 0;
            if game_state.pending_round_end_reward.take().is_some() {
                debug!("Cleared deferred round-end reward at round transition");
            }
            if game_state.pending_round_end_vibration.take().is_some() {
                debug!("Cleared deferred round-end vibration at round transition");
            }
        }

        game_state.round_phase = round.phase.clone();
    }

    if game_state.map_phase == MapPhase::Live {
        if let Some(player) = payload.player {
            if player.steamid == game_state.steam_id {
                game_state.player_team = player.team.clone();
                game_state.current_round_kills = player.state.round_kills;

                let round_phase = if previous_round_phase == RoundPhase::Live
                    || game_state.round_phase == RoundPhase::Live
                {
                    RoundPhase::Live
                } else {
                    game_state.round_phase.clone()
                };

                let map_phase_now = game_state.map_phase.clone();
                if let Some(player_state) = &mut game_state.player_state {
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

                    if should_trigger_kill_vibration(
                        &config.vibrations,
                        player_state.kills,
                        player.match_stats.kills,
                        &map_phase_now,
                        &round_phase,
                    ) {
                        info!(
                            "Player got a kill ({}), arming kill vibration",
                            player.match_stats.kills
                        );
                        pending_kill_vibration = Some((
                            config.vibrations.kill_vibration_strength_percent,
                            config.vibrations.kill_vibration_duration_ms,
                        ));
                    }

                    player_state.health = player.state.health;
                    player_state.armor = player.state.armor;
                    player_state.kills = player.match_stats.kills;
                    player_state.deaths = player.match_stats.deaths;
                } else {
                    debug!("Player state initialized");
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

    if let Some((sound, volume)) = pending_kill_reward {
        sounds::play(sound, volume);
    }

    if let Some((sound, volume)) = pending_reward_to_play {
        sounds::play(sound, volume);
    }

    if let Some((strength, duration_ms)) = pending_kill_vibration {
        let toys = config.selected_toy_identifiers.clone();
        tokio::spawn(async move {
            intiface::vibrate_for(toys, strength, duration_ms as u64).await;
        });
    }

    if let Some((strength, duration_ms)) = pending_vibration_to_play {
        let toys = config.selected_toy_identifiers.clone();
        tokio::spawn(async move {
            intiface::vibrate_for(toys, strength, duration_ms as u64).await;
        });
    }

    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::{
        arm_round_end_reward_if_eligible, arm_round_end_vibration_if_eligible,
        did_player_win_round, resolve_pending_round_end_reward,
        resolve_pending_round_end_vibration, round_outcome_for_player, should_trigger_kill_reward,
        should_trigger_kill_vibration, RoundOutcome,
    };
    use crate::config::{RewardConfig, RoundEndRewardGating, VibrationRewardConfig};
    use crate::gamestateintegration::{MapPhase, RoundPhase};
    use crate::sounds::{BundledSound, SoundChoice};
    use crate::{GameState, PendingRoundEndReward, PendingRoundEndVibration};

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
    fn should_trigger_kill_vibration_fires_on_positive_kill_delta_during_live_round() {
        let vibrations = VibrationRewardConfig::default();
        assert!(should_trigger_kill_vibration(
            &vibrations,
            2,
            3,
            &MapPhase::Live,
            &RoundPhase::Live
        ));
    }

    #[test]
    fn should_trigger_kill_vibration_ignores_zero_or_negative_delta() {
        let vibrations = VibrationRewardConfig::default();
        assert!(!should_trigger_kill_vibration(
            &vibrations,
            3,
            3,
            &MapPhase::Live,
            &RoundPhase::Live
        ));
        assert!(!should_trigger_kill_vibration(
            &vibrations,
            3,
            2,
            &MapPhase::Live,
            &RoundPhase::Live
        ));
    }

    #[test]
    fn should_trigger_kill_vibration_suppressed_outside_live_phase() {
        let vibrations = VibrationRewardConfig::default();
        assert!(!should_trigger_kill_vibration(
            &vibrations,
            0,
            1,
            &MapPhase::Warmup,
            &RoundPhase::Live
        ));
        assert!(!should_trigger_kill_vibration(
            &vibrations,
            0,
            1,
            &MapPhase::Live,
            &RoundPhase::Freezetime
        ));
        assert!(!should_trigger_kill_vibration(
            &vibrations,
            0,
            1,
            &MapPhase::Live,
            &RoundPhase::Over
        ));
    }

    #[test]
    fn should_trigger_kill_vibration_respects_disabled_setting() {
        let mut vibrations = VibrationRewardConfig::default();
        vibrations.kill_vibration_enabled = false;
        assert!(!should_trigger_kill_vibration(
            &vibrations,
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

    #[test]
    fn arm_round_end_vibration_returns_pending_when_threshold_met() {
        let vibrations = VibrationRewardConfig::default();
        let pending = arm_round_end_vibration_if_eligible(&vibrations, 3).expect("should arm");
        assert_eq!(
            pending.strength_percent,
            vibrations.round_end_vibration_strength_percent
        );
        assert_eq!(
            pending.duration_ms,
            vibrations.round_end_vibration_duration_ms
        );
        assert_eq!(pending.gating, vibrations.round_end_vibration_gating);
    }

    #[test]
    fn arm_round_end_vibration_returns_none_below_threshold() {
        let vibrations = VibrationRewardConfig::default();
        assert!(arm_round_end_vibration_if_eligible(&vibrations, 2).is_none());
    }

    #[test]
    fn arm_round_end_vibration_returns_none_when_disabled() {
        let mut vibrations = VibrationRewardConfig::default();
        vibrations.round_end_vibration_enabled = false;
        assert!(arm_round_end_vibration_if_eligible(&vibrations, 5).is_none());
    }

    fn pending_reward(gating: RoundEndRewardGating) -> PendingRoundEndReward {
        PendingRoundEndReward {
            sound: SoundChoice::Bundled(BundledSound::GoodPuppy1),
            volume_percent: 100,
            gating,
        }
    }

    fn pending_vibration(gating: RoundEndRewardGating) -> PendingRoundEndVibration {
        PendingRoundEndVibration {
            strength_percent: 80,
            duration_ms: 3000,
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

    #[test]
    fn resolve_pending_round_end_vibration_fires_unconditionally_when_gating_is_always() {
        for winner in [Some("CT"), Some("T"), None] {
            let mut game_state = GameState::default();
            game_state.player_team = Some("CT".into());
            game_state.pending_round_end_vibration =
                Some(pending_vibration(RoundEndRewardGating::Always));

            let result = resolve_pending_round_end_vibration(&mut game_state, winner);

            assert_eq!(result, Some((80, 3000)), "winner={:?} should fire", winner);
            assert!(game_state.pending_round_end_vibration.is_none());
        }
    }

    #[test]
    fn resolve_pending_round_end_vibration_fires_only_on_win_for_only_if_team_wins() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("CT".into());
        game_state.pending_round_end_vibration =
            Some(pending_vibration(RoundEndRewardGating::OnlyIfTeamWins));

        let result = resolve_pending_round_end_vibration(&mut game_state, Some("CT"));
        assert_eq!(result, Some((80, 3000)));
        assert!(game_state.pending_round_end_vibration.is_none());
    }

    #[test]
    fn resolve_pending_round_end_vibration_clears_silently_on_loss_for_only_if_team_wins() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("T".into());
        game_state.pending_round_end_vibration =
            Some(pending_vibration(RoundEndRewardGating::OnlyIfTeamWins));

        let result = resolve_pending_round_end_vibration(&mut game_state, Some("CT"));
        assert!(result.is_none());
        assert!(game_state.pending_round_end_vibration.is_none());
    }

    #[test]
    fn resolve_pending_round_end_vibration_keeps_pending_when_winner_unknown() {
        let mut game_state = GameState::default();
        game_state.player_team = Some("CT".into());
        game_state.pending_round_end_vibration =
            Some(pending_vibration(RoundEndRewardGating::OnlyIfTeamWins));

        let result = resolve_pending_round_end_vibration(&mut game_state, None);
        assert!(result.is_none());
        assert!(game_state.pending_round_end_vibration.is_some());
    }

    #[test]
    fn resolve_pending_round_end_vibration_returns_none_when_nothing_pending() {
        let mut game_state = GameState::default();
        let result = resolve_pending_round_end_vibration(&mut game_state, Some("CT"));
        assert!(result.is_none());
    }
}
