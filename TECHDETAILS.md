# Technical Details & Advanced Usage

This document contains technical information about how CS2 Love operates under the hood, how to manually configure it, and how to build it from source.

## Buttplug / Intiface Integration

CS2 Love uses the [Buttplug v10 protocol](https://buttplug.io/) via the official [`buttplug`](https://crates.io/crates/buttplug) Rust crate (a metacrate that re-exports `buttplug_client` and `buttplug_transport_websocket_tungstenite`). [Intiface Central](https://intiface.com/central) acts as the bridge between CS2 Love and your physical hardware, owning the Bluetooth/USB stack and exposing a Buttplug WebSocket server.

- **Server URL** (default): `ws://127.0.0.1:12345`
- The app keeps one long-running `ButtplugClient` alive and reuses it across all vibration commands.
- Vibration is sent as a [`ClientDeviceOutputCommand::Vibrate(ClientDeviceCommandValue::Percent(0.0..=1.0))`](https://docs.rs/buttplug/latest/buttplug/device/enum.ClientDeviceOutputCommand.html), followed by a configurable sleep, then a `device.stop()`.
- Selected toys are persisted by the human-readable name reported by Intiface (`device.name()`) so re-scans and reconnects don't invalidate the selection.
- Per-toy commands are serialized behind a Tokio `Mutex` so back-to-back kills queue up cleanly instead of cancelling each other mid-vibration.

## Game State Integration (GSI)

Counter-Strike 2 sends game events via HTTP POST requests. CS2 Love starts a local listener on `http://127.0.0.1:3001/data`. The port (`3001`) is intentionally distinct from CS2Shock's `3000`, so both apps can run side-by-side without conflict.

When you click "Install CS2 Integration" in the app, it places a file named `gamestate_integration_cs2love.cfg` into your `game/csgo/cfg` folder, which tells CS2 to send event data to that local port that CS2 Love is listening on.

## Configuration File

The app automatically saves your settings to `cs2love-config.json` in the current working directory (usually right next to `cs2love.exe`). You can manually edit this file if the app is closed.

### Available Config Fields

- `setup_dismissed`: boolean, whether the first-run setup modal was dismissed
- `intiface_websocket_url`: WebSocket URL of your running Intiface Central server (default `"ws://127.0.0.1:12345"`)
- `selected_toy_identifiers`: array of toy display names (as reported by Intiface) that vibration rewards target
- `rewards`: nested object controlling sound rewards. See [Reward Config](#reward-config-rewards) below.
- `vibrations`: nested object controlling vibration rewards. See [Vibration Config](#vibration-config-vibrations) below.

Missing top-level fields and missing nested fields are filled in from the documented defaults at load time.

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

- Bundled assets: `{"kind": "bundled", "value": "<tag>"}` where `<tag>` is one of `clicker`, `goodpuppy1`, `goodpuppy2`, `goodpuppy3`, `goodpuppy4`, `goodboy1`, `goodgirl1`. These WAV files are embedded in `cs2love.exe` at compile time, so the app does not need an `assets/` folder at runtime.
- Custom files: `{"kind": "custom", "value": "<absolute path to .wav/.mp3/.ogg/.flac>"}`. Missing files are logged and silently skipped at playback time.

### Vibration Config (`vibrations`)

- `kill_vibration_enabled`: boolean, whether to vibrate on every in-round kill (default `true`)
- `kill_vibration_strength_percent`: integer `0` to `100`, percentage of maximum vibrator output for kill rewards (default `60`)
- `kill_vibration_duration_ms`: integer `100` to `10000`, kill-reward vibration duration in milliseconds (default `1500`)
- `round_end_vibration_enabled`: boolean, whether to vibrate at round end when the kill threshold is met (default `true`)
- `round_end_vibration_kill_threshold`: integer `1` to `5`, in-round kills required to trigger the round-end vibration (default `3`)
- `round_end_vibration_gating`: `"Always"` or `"OnlyIfTeamWins"`, mirrors the sound-reward gating (default `"Always"`)
- `round_end_vibration_strength_percent`: integer `0` to `100`, percentage of maximum vibrator output for round-end rewards (default `100`)
- `round_end_vibration_duration_ms`: integer `100` to `10000`, round-end vibration duration in milliseconds (default `4000`)

## Future Work

The full CS2 game state - kills, deaths, round outcome, last-hit health, and player team - is parsed and tracked by `read_data` even when not strictly needed by the MVP rewards. That makes the following features one-or-two-line additions on top of the existing pipeline:

- **Keep buzzing until death**: hold the vibration on until the player's death counter increments, instead of using a fixed duration.
- **Negative reinforcement on missed threshold**: short cool-down vibration silence when the player fails to hit the round-kill threshold.
- **Strength scaling by last-hit health**: scale the kill-reward strength to the percentage of HP the killing blow took.
- **Per-toy strength overrides**: per-device strength multipliers for users with multiple toys.
- **Battery readout in the toy list**: the Buttplug client API already exposes `device.battery()`.

## Official Release Builds

Official Windows release binaries are built by GitHub Actions from the tagged commit and uploaded to the GitHub release page. Releases also include a `SHA256SUMS.txt` checksum file and GitHub build provenance attestation so users can verify the published binary matches the repository source and CI build.

## Build From Source

If you want to compile the application yourself, you will need the [Rust toolchain](https://rustup.rs/) installed.

1. Clone the repository:
```bash
git clone https://github.com/OstlerDev/cs2love.git
cd cs2love
```

2. Build a release binary:
```bash
cargo build --release
```

The compiled executable will be available at `target/release/cs2love.exe`.

## Run From Source

To run the app directly during development:
```bash
cargo run
```
