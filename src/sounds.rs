use std::{
    fs::File,
    io::{BufReader, Cursor},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread,
};

use log::{debug, error, warn};
use rodio::{stream::MixerDeviceSink, Decoder, Player};
use serde::{Deserialize, Serialize};

pub const VOLUME_PERCENT_MAX: u32 = 200;

macro_rules! bundled_sound_table {
    ($( ($variant:ident, $tag:literal, $file:literal) ),+ $(,)?) => {
        #[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
        pub enum BundledSound {
            $(
                #[serde(rename = $tag)]
                $variant,
            )+
        }

        impl BundledSound {
            pub fn tag(self) -> &'static str {
                match self {
                    $( Self::$variant => $tag, )+
                }
            }

            fn bytes(self) -> &'static [u8] {
                match self {
                    $( Self::$variant => include_bytes!(concat!("../assets/sounds/", $file)), )+
                }
            }

            pub const ALL: &'static [BundledSound] = &[ $( Self::$variant, )+ ];
        }
    };
}

bundled_sound_table! {
    (Clicker,    "clicker",    "clicker.wav"),
    (GoodPuppy1, "goodpuppy1", "goodpuppy1.wav"),
    (GoodPuppy2, "goodpuppy2", "goodpuppy2.wav"),
    (GoodPuppy3, "goodpuppy3", "goodpuppy3.wav"),
    (GoodPuppy4, "goodpuppy4", "goodpuppy4.wav"),
    (GoodBoy1,   "goodboy1",   "goodboy1.wav"),
    (GoodGirl1,  "goodgirl1",  "goodgirl1.wav"),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum SoundChoice {
    Bundled(BundledSound),
    Custom(PathBuf),
}

impl SoundChoice {
    pub fn display_label(&self) -> String {
        match self {
            Self::Bundled(b) => b.tag().to_string(),
            Self::Custom(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Custom file".to_string()),
        }
    }
}

pub fn bundled_sounds() -> &'static [BundledSound] {
    BundledSound::ALL
}

fn clamp_volume_percent(volume_percent: u32) -> u32 {
    volume_percent.min(VOLUME_PERCENT_MAX)
}

static AUDIO: OnceLock<Mutex<Option<MixerDeviceSink>>> = OnceLock::new();

fn audio_lock() -> &'static Mutex<Option<MixerDeviceSink>> {
    AUDIO.get_or_init(|| Mutex::new(None))
}

fn play_with_handle(handle: &MixerDeviceSink, choice: &SoundChoice, volume_percent: u32) -> Result<(), String> {
    let player = Player::connect_new(handle.mixer());
    player.set_volume((clamp_volume_percent(volume_percent) as f32) / 100.0);

    match choice {
        SoundChoice::Bundled(b) => {
            let source = Decoder::try_from(Cursor::new(b.bytes()))
                .map_err(|e| format!("decode bundled `{}`: {e}", b.tag()))?;
            player.append(source);
        }
        SoundChoice::Custom(path) => {
            let file = File::open(path).map_err(|e| format!("open `{}`: {e}", path.display()))?;
            let source = Decoder::try_from(BufReader::new(file))
                .map_err(|e| format!("decode `{}`: {e}", path.display()))?;
            player.append(source);
        }
    }

    player.detach();
    Ok(())
}

pub fn play(choice: SoundChoice, volume_percent: u32) {
    thread::spawn(move || {
        let lock = audio_lock();
        let mut guard = match lock.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                warn!(target: "Sounds", "Audio mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };

        if guard.is_none() {
            match rodio::stream::DeviceSinkBuilder::open_default_sink() {
                Ok(handle) => *guard = Some(handle),
                Err(e) => {
                    error!(target: "Sounds", "Failed to open audio output: {e}");
                    return;
                }
            }
        }

        let Some(handle) = guard.as_ref() else { return };
        if let Err(message) = play_with_handle(handle, &choice, volume_percent) {
            error!(target: "Sounds", "Failed to play sound: {message}");
        } else {
            debug!(target: "Sounds", "Playing {} at {}%", choice.display_label(), clamp_volume_percent(volume_percent));
        }
    });
}

pub fn pick_custom_sound_file(starting_dir: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new()
        .add_filter("Audio", &["wav", "mp3", "ogg", "flac"])
        .set_title("Choose a custom sound");
    if let Some(dir) = starting_dir {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_sounds_list_is_non_empty_and_unique() {
        let sounds = bundled_sounds();
        assert!(!sounds.is_empty());

        let mut tags: Vec<&str> = sounds.iter().map(|s| s.tag()).collect();
        tags.sort();
        let original_len = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), original_len, "bundled sound tags must be unique");
    }

    #[test]
    fn every_bundled_sound_has_non_empty_payload() {
        for sound in bundled_sounds() {
            assert!(!sound.bytes().is_empty(), "{} bytes empty", sound.tag());
        }
    }

    #[test]
    fn bundled_sound_round_trips_through_serde_by_tag() {
        for sound in bundled_sounds() {
            let json = serde_json::to_string(sound).unwrap();
            assert_eq!(json, format!("\"{}\"", sound.tag()));
            let parsed: BundledSound = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *sound);
        }
    }

    #[test]
    fn unknown_bundled_tag_fails_to_deserialize() {
        let result: Result<BundledSound, _> = serde_json::from_str("\"not-a-real-sound\"");
        assert!(result.is_err());
    }

    #[test]
    fn sound_choice_round_trips_for_bundled_and_custom() {
        let bundled = SoundChoice::Bundled(BundledSound::Clicker);
        let json = serde_json::to_string(&bundled).unwrap();
        let parsed: SoundChoice = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, bundled);

        let custom = SoundChoice::Custom(PathBuf::from("C:\\sounds\\foo.wav"));
        let json = serde_json::to_string(&custom).unwrap();
        let parsed: SoundChoice = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, custom);
    }

    #[test]
    fn clamp_volume_percent_caps_at_max() {
        assert_eq!(clamp_volume_percent(0), 0);
        assert_eq!(clamp_volume_percent(100), 100);
        assert_eq!(clamp_volume_percent(VOLUME_PERCENT_MAX), VOLUME_PERCENT_MAX);
        assert_eq!(clamp_volume_percent(500), VOLUME_PERCENT_MAX);
    }

    #[test]
    fn display_label_uses_tag_for_bundled_and_filename_for_custom() {
        assert_eq!(
            SoundChoice::Bundled(BundledSound::Clicker).display_label(),
            "clicker"
        );
        let custom = SoundChoice::Custom(PathBuf::from("/tmp/example.wav"));
        assert_eq!(custom.display_label(), "example.wav");
    }
}
