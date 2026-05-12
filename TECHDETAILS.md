# Technical Details & Advanced Usage

This document contains technical information about how CS2Shock operates under the hood, how to manually configure it, and how to build it from source.

## PiShock Integration

CS2Shock uses the PiShock V2 websocket authentication with `Username + ApiKey` only.

- **Broker connection**: `wss://broker.pishock.com/v2?Username={username}&ApiKey={apikey}`
- The app keeps one long-running broker websocket alive and reuses it across commands to minimize latency.
- Commands are sent as V2 JSON `PUBLISH` envelopes.
- Owned device metadata is resolved through PiShock's HTTP API before sending commands.
- No browser login flow or website session token is used.

This matches the documented PiShock V2 websocket flow in the [PiShock WebSocket API documentation](https://docs.pishock.com/pishock/api-documentation/websocket-api-documentation.html) and uses PiShock's owned-device lookup from the [PiShock API documentation](https://docs.pishock.com/pishock/api-documentation/pishock-api-documentation.html).

## Game State Integration (GSI)

Counter-Strike 2 sends game events via HTTP POST requests. CS2Shock starts a local listener on `http://127.0.0.1:3000/data`. 

When you click "Install CS2 Integration" in the app, it places a file named `gamestate_integration_cs2shock.cfg` into your `game/csgo/cfg` folder, which tells CS2 to send event data to that local port that CS2Shock is listening on.

## Configuration File

The app automatically saves your settings to `cs2shock-config.json` in the current working directory (usually right next to `cs2shock.exe`). You can manually edit this file if the app is closed.

### Available Config Fields:

- `shock_mode`: `"Random"` or `"LastHitPercentage"`
- `min_duration`: minimum shock duration in seconds, `0.1` to `5.0`
- `max_duration`: maximum shock duration in seconds, `0.1` to `5.0`
- `min_intensity`: minimum shock intensity, `1` to `100`
- `max_intensity`: maximum shock intensity, `1` to `100`
- `beep_on_match_start`: boolean, beep when a live match begins
- `beep_on_round_start`: boolean, beep when a live round begins
- `warning_beep_before_shock`: boolean, beep before sending a shock
- `warning_beep_duration`: warning beep duration in seconds, `1` to `15`
- `shock_chance`: final percent chance that an otherwise eligible shock is actually sent, `0` to `100`
- `shock_timing_mode`: `"Immediate"`, `"EndOfRound"`, or `"EndOfRoundIfTeamLoses"`, controls whether shocks happen immediately, at round end, or only at round end after a loss
- `prevent_shock_if_round_kills_reached`: boolean, if enabled, deaths are ignored once you have reached the configured kill threshold for the current round
- `round_kills_to_prevent_shock`: round kill threshold, `1` to `5`
- `username`: your PiShock username
- `apikey`: your PiShock API key
- `selected_client_id`: the selected PiShock device ID
- `selected_shocker_id`: the selected PiShock shocker ID
- `selected_device_name`: the selected PiShock device name
- `selected_shocker_name`: the selected PiShock shocker name
- `setup_dismissed`: boolean, whether the first-run setup modal was dismissed
- `rewards`: nested object controlling sound rewards. See [Reward Config](#reward-config-rewards) below.

Older config files that still use `shock_on_round_loss_only` are accepted for backward compatibility and map to the equivalent timing mode automatically. Older configs that omit `rewards` entirely are accepted and fall back to the documented defaults.

### Reward Config (`rewards`)

- `kill_reward_enabled`: boolean, whether to play a sound on every in-round kill (default `true`)
- `kill_reward_sound`: tagged sound choice for the instant kill reward (default `{"kind": "bundled", "value": "clicker"}`)
- `kill_reward_volume_percent`: integer `0` to `200`, playback volume for the kill reward (default `100`)
- `round_end_reward_enabled`: boolean, whether to play a sound at round end when the kill threshold is met (default `true`)
- `round_end_reward_kill_threshold`: integer `1` to `5`, in-round kills required to trigger the round-end reward (default `3`)
- `round_end_reward_gating`: `"Always"` or `"OnlyIfTeamWins"`, controls whether the round-end reward fires regardless of outcome or only on a round win (default `"Always"`)
- `round_end_reward_sound`: tagged sound choice for the round-end reward (default `{"kind": "bundled", "value": "goodpuppy1"}`)
- `round_end_reward_volume_percent`: integer `0` to `200`, playback volume for the round-end reward (default `100`)

Sound choices are tagged JSON objects:

- Bundled assets: `{"kind": "bundled", "value": "<tag>"}` where `<tag>` is one of `clicker`, `goodpuppy1`, `goodpuppy2`, `goodpuppy3`, `goodpuppy4`, `goodboy1`, `goodgirl1`. These WAV files are embedded in `cs2shock.exe` at compile time, so the app does not need an `assets/` folder at runtime.
- Custom files: `{"kind": "custom", "value": "<absolute path to .wav/.mp3/.ogg/.flac>"}`. Missing files are logged and silently skipped at playback time.

## Official Release Builds

Official Windows release binaries are built by GitHub Actions from the tagged commit and uploaded to the GitHub release page. Releases also include a `SHA256SUMS.txt` checksum file and GitHub build provenance attestation so users can verify the published binary matches the repository source and CI build.

## Build From Source

If you want to compile the application yourself, you will need the [Rust toolchain](https://rustup.rs/) installed.

1. Clone the repository:
```bash
git clone https://github.com/OstlerDev/cs2shock.git
cd cs2shock
```

2. Build a release binary:
```bash
cargo build --release
```

The compiled executable will be available at `target/release/cs2shock.exe`.

## Run From Source

To run the app directly during development:
```bash
cargo run
```
