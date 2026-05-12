use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
};

use reqwest::Url;

use crate::config::Config;

pub const GSI_CFG_FILE_NAME: &str = "gamestate_integration_cs2shock.cfg";
pub const EXPECTED_GSI_URI: &str = "http://127.0.0.1:3000/data";

const CS2_CFG_RELATIVE_PATH: &str =
    "steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo\\cfg";
const STEAM_APP_MANIFEST_RELATIVE_PATH: &str = "steamapps\\appmanifest_730.acf";
const STEAM_LIBRARY_FOLDERS_RELATIVE_PATH: &str = "steamapps\\libraryfolders.vdf";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cs2IntegrationStatus {
    Installed {
        target_path: PathBuf,
    },
    MissingKnownPath {
        target_path: PathBuf,
    },
    MissingUnknownPath,
    RepairRecommended {
        target_path: PathBuf,
        message: String,
    },
    CheckFailed {
        target_path: Option<PathBuf>,
        message: String,
    },
}

impl Cs2IntegrationStatus {
    pub fn is_installed(&self) -> bool {
        matches!(self, Self::Installed { .. })
    }

    pub fn target_path(&self) -> Option<&Path> {
        match self {
            Self::Installed { target_path }
            | Self::MissingKnownPath { target_path }
            | Self::RepairRecommended { target_path, .. } => Some(target_path.as_path()),
            Self::CheckFailed {
                target_path: Some(target_path),
                ..
            } => Some(target_path.as_path()),
            Self::MissingUnknownPath
            | Self::CheckFailed {
                target_path: None, ..
            } => None,
        }
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            Self::RepairRecommended { message, .. } | Self::CheckFailed { message, .. } => {
                Some(message.as_str())
            }
            Self::Installed { .. } | Self::MissingKnownPath { .. } | Self::MissingUnknownPath => {
                None
            }
        }
    }

    pub fn install_action_label(&self) -> &'static str {
        match self {
            Self::RepairRecommended { .. } => "Repair automatically",
            _ => "Install automatically",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetupStep {
    InstallCs2Integration,
    ConnectPishock,
    ChooseShocker,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupSummary {
    pub cs2_integration: Cs2IntegrationStatus,
    pub has_auth_credentials: bool,
    pub has_selected_target: bool,
}

impl SetupSummary {
    pub fn from_config(config: &Config, cs2_integration: Cs2IntegrationStatus) -> Self {
        Self {
            cs2_integration,
            has_auth_credentials: has_auth_credentials(config),
            has_selected_target: has_selected_target(config),
        }
    }

    pub fn current_step(&self) -> SetupStep {
        if !self.cs2_integration.is_installed() {
            return SetupStep::InstallCs2Integration;
        }

        if !self.has_auth_credentials {
            return SetupStep::ConnectPishock;
        }

        if !self.has_selected_target {
            return SetupStep::ChooseShocker;
        }

        SetupStep::Complete
    }

    pub fn is_complete(&self) -> bool {
        self.current_step() == SetupStep::Complete
    }

    pub fn needs_setup(&self) -> bool {
        !self.is_complete()
    }
}

pub fn has_auth_credentials(config: &Config) -> bool {
    !config.username.trim().is_empty() && !config.apikey.trim().is_empty()
}

pub fn has_selected_target(config: &Config) -> bool {
    config.selected_client_id.is_some() && config.selected_shocker_id.is_some()
}

pub fn detect_cs2_integration() -> Cs2IntegrationStatus {
    match detect_cs2_cfg_target_path() {
        Ok(Some(target_path)) => inspect_cs2_integration_at(&target_path),
        Ok(None) => Cs2IntegrationStatus::MissingUnknownPath,
        Err(message) => Cs2IntegrationStatus::CheckFailed {
            target_path: None,
            message,
        },
    }
}

pub fn install_cs2_integration(target_path: &Path) -> Result<(), String> {
    let Some(parent) = target_path.parent() else {
        return Err("The detected CS2 config path did not have a parent folder.".into());
    };

    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Failed to create the CS2 cfg folder `{}`: {error}",
            parent.display()
        )
    })?;

    fs::write(target_path, expected_gsi_cfg_contents())
        .map_err(|error| format!("Failed to write `{}`: {error}", target_path.display()))
}

pub fn save_cs2_integration_to_downloads() -> Result<PathBuf, String> {
    let downloads_dir = downloads_dir()?;
    let download_path = downloads_dir.join(GSI_CFG_FILE_NAME);
    fs::write(&download_path, expected_gsi_cfg_contents())
        .map_err(|error| format!("Failed to write `{}`: {error}", download_path.display()))?;

    open_path_in_file_manager(&downloads_dir)?;
    Ok(download_path)
}

pub fn open_cs2_cfg_folder(target_path: &Path) -> Result<(), String> {
    let Some(folder_path) = target_path.parent() else {
        return Err("The detected CS2 config path did not have a parent folder.".into());
    };
    open_path_in_file_manager(folder_path)
}

pub fn expected_gsi_cfg_contents() -> &'static str {
    include_str!("../gamestate_integration_cs2shock.cfg")
}

fn inspect_cs2_integration_at(target_path: &Path) -> Cs2IntegrationStatus {
    match fs::read_to_string(target_path) {
        Ok(contents) => match validate_installed_gsi_cfg(&contents) {
            Ok(()) => Cs2IntegrationStatus::Installed {
                target_path: target_path.to_path_buf(),
            },
            Err(message) => Cs2IntegrationStatus::RepairRecommended {
                target_path: target_path.to_path_buf(),
                message,
            },
        },
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Cs2IntegrationStatus::MissingKnownPath {
                target_path: target_path.to_path_buf(),
            }
        }
        Err(error) => Cs2IntegrationStatus::CheckFailed {
            target_path: Some(target_path.to_path_buf()),
            message: format!(
                "Failed to read the CS2 integration file at `{}`: {error}",
                target_path.display()
            ),
        },
    }
}

fn validate_installed_gsi_cfg(contents: &str) -> Result<(), String> {
    let Some(uri) = parse_vdf_string_value(contents, "uri") else {
        return Err("The installed file does not define a Game State Integration URI.".into());
    };

    if !is_expected_gsi_uri(&uri) {
        return Err(format!(
            "The installed file points to `{uri}` instead of `{EXPECTED_GSI_URI}`."
        ));
    }

    Ok(())
}

fn is_expected_gsi_uri(uri: &str) -> bool {
    let Ok(parsed) = Url::parse(uri.trim()) else {
        return false;
    };

    matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"))
        && parsed.scheme() == "http"
        && parsed.port_or_known_default() == Some(3000)
        && parsed.path() == "/data"
}

fn detect_cs2_cfg_target_path() -> Result<Option<PathBuf>, String> {
    detect_cs2_cfg_target_path_from_roots(&steam_install_roots())
}

fn detect_cs2_cfg_target_path_from_roots(roots: &[PathBuf]) -> Result<Option<PathBuf>, String> {
    let mut libraries = BTreeSet::new();
    let mut first_error = None;

    for root in roots {
        if !root.exists() {
            continue;
        }

        libraries.insert(root.to_path_buf());

        let libraryfolders_path = root.join(STEAM_LIBRARY_FOLDERS_RELATIVE_PATH);
        match read_steam_library_paths(&libraryfolders_path) {
            Ok(paths) => {
                libraries.extend(paths);
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    for library in libraries {
        if library.join(STEAM_APP_MANIFEST_RELATIVE_PATH).is_file() {
            return Ok(Some(
                library.join(CS2_CFG_RELATIVE_PATH).join(GSI_CFG_FILE_NAME),
            ));
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok(None)
}

fn read_steam_library_paths(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let contents = fs::read_to_string(path).map_err(|error| {
        format!(
            "Failed to read Steam library list `{}`: {error}",
            path.display()
        )
    })?;

    Ok(parse_steam_library_paths(&contents))
}

fn parse_steam_library_paths(contents: &str) -> Vec<PathBuf> {
    contents
        .lines()
        .filter_map(|line| parse_vdf_string_value(line, "path"))
        .map(|path| PathBuf::from(path.replace("\\\\", "\\")))
        .collect()
}

fn parse_vdf_string_value(contents: &str, key: &str) -> Option<String> {
    for line in contents.lines() {
        let tokens: Vec<_> = line.split('"').skip(1).step_by(2).collect();
        if tokens.len() >= 2 && tokens[0] == key {
            return Some(tokens[1].to_owned());
        }
    }

    None
}

fn steam_install_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();

    if let Some(registry_root) = steam_install_root_from_registry() {
        roots.insert(registry_root);
    }

    roots.insert(PathBuf::from(r"C:\Program Files (x86)\Steam"));
    roots.insert(PathBuf::from(r"C:\Program Files\Steam"));

    roots.into_iter().collect()
}

fn downloads_dir() -> Result<PathBuf, String> {
    let Some(home_dir) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    else {
        return Err("Could not locate the current user's home folder.".into());
    };

    Ok(downloads_dir_from_home(Path::new(&home_dir)))
}

fn downloads_dir_from_home(home_dir: &Path) -> PathBuf {
    home_dir.join("Downloads")
}

#[cfg(windows)]
fn open_path_in_file_manager(path: &Path) -> Result<(), String> {
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map_err(|error| format!("Failed to open `{}` in Explorer: {error}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_path_in_file_manager(path: &Path) -> Result<(), String> {
    Command::new("open")
        .arg(path)
        .spawn()
        .map_err(|error| format!("Failed to open `{}` in Finder: {error}", path.display()))?;
    Ok(())
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn open_path_in_file_manager(path: &Path) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map_err(|error| {
            format!(
                "Failed to open `{}` in the file manager: {error}",
                path.display()
            )
        })?;
    Ok(())
}

#[cfg(windows)]
fn steam_install_root_from_registry() -> Option<PathBuf> {
    const REGISTRY_KEYS: [&str; 3] = [
        r"HKLM\SOFTWARE\Wow6432Node\Valve\Steam",
        r"HKLM\SOFTWARE\Valve\Steam",
        r"HKCU\SOFTWARE\Valve\Steam",
    ];

    for key in REGISTRY_KEYS {
        let output = Command::new("reg")
            .args(["query", key, "/v", "InstallPath"])
            .output()
            .ok()?;

        if !output.status.success() {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if !line.contains("InstallPath") {
                continue;
            }

            let value = line.split("REG_SZ").nth(1)?.trim();
            return Some(PathBuf::from(value));
        }
    }

    None
}

#[cfg(not(windows))]
fn steam_install_root_from_registry() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::{
        detect_cs2_cfg_target_path_from_roots, downloads_dir_from_home, expected_gsi_cfg_contents,
        has_auth_credentials, inspect_cs2_integration_at, install_cs2_integration,
        is_expected_gsi_uri, parse_steam_library_paths, Cs2IntegrationStatus, SetupStep,
        SetupSummary, EXPECTED_GSI_URI, GSI_CFG_FILE_NAME,
    };
    use crate::config::Config;
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn missing_cfg_reports_known_path() {
        let temp_dir = TestDir::new("missing-cfg");
        let target_path = temp_dir.path().join(GSI_CFG_FILE_NAME);

        assert_eq!(
            inspect_cs2_integration_at(&target_path),
            Cs2IntegrationStatus::MissingKnownPath { target_path }
        );
    }

    #[test]
    fn installed_cfg_with_expected_uri_is_accepted() {
        let temp_dir = TestDir::new("valid-cfg");
        let target_path = temp_dir.path().join(GSI_CFG_FILE_NAME);
        fs::write(&target_path, expected_gsi_cfg_contents()).unwrap();

        assert_eq!(
            inspect_cs2_integration_at(&target_path),
            Cs2IntegrationStatus::Installed { target_path }
        );
    }

    #[test]
    fn installed_cfg_with_wrong_uri_requests_repair() {
        let temp_dir = TestDir::new("wrong-uri");
        let target_path = temp_dir.path().join(GSI_CFG_FILE_NAME);
        let contents =
            expected_gsi_cfg_contents().replace(EXPECTED_GSI_URI, "http://127.0.0.1:4000/data");
        fs::write(&target_path, contents).unwrap();

        match inspect_cs2_integration_at(&target_path) {
            Cs2IntegrationStatus::RepairRecommended {
                target_path: actual_path,
                message,
            } => {
                assert_eq!(actual_path, target_path);
                assert!(message.contains("4000"));
            }
            status => panic!("expected repair recommendation, got {status:?}"),
        }
    }

    #[test]
    fn localhost_uri_is_treated_as_equivalent() {
        assert!(is_expected_gsi_uri("http://localhost:3000/data"));
        assert!(is_expected_gsi_uri(EXPECTED_GSI_URI));
    }

    #[test]
    fn install_writes_expected_cfg_contents() {
        let temp_dir = TestDir::new("install");
        let target_path = temp_dir.path().join("nested").join(GSI_CFG_FILE_NAME);

        install_cs2_integration(&target_path).unwrap();

        assert_eq!(
            fs::read_to_string(&target_path).unwrap(),
            expected_gsi_cfg_contents()
        );
    }

    #[test]
    fn downloads_dir_is_resolved_under_home_directory() {
        assert_eq!(
            downloads_dir_from_home(Path::new(r"C:\Users\Sky")),
            PathBuf::from(r"C:\Users\Sky\Downloads")
        );
    }

    #[test]
    fn steam_library_detection_finds_counter_strike_manifest() {
        let temp_dir = TestDir::new("steam-detect");
        let steam_root = temp_dir.path().join("Steam");
        let library_root = temp_dir.path().join("Games");

        fs::create_dir_all(steam_root.join("steamapps")).unwrap();
        fs::create_dir_all(library_root.join("steamapps")).unwrap();

        fs::write(
            steam_root.join("steamapps").join("libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n    \"1\"\n    {{\n        \"path\"        \"{}\"\n    }}\n}}\n",
                library_root.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        fs::write(
            library_root.join("steamapps").join("appmanifest_730.acf"),
            "\"AppState\"{}",
        )
        .unwrap();

        let detected = detect_cs2_cfg_target_path_from_roots(&[steam_root]).unwrap();

        assert_eq!(
            detected,
            Some(
                library_root
                    .join("steamapps/common/Counter-Strike Global Offensive/game/csgo/cfg")
                    .join(GSI_CFG_FILE_NAME)
            )
        );
    }

    #[test]
    fn parse_steam_library_paths_unescapes_windows_paths() {
        let parsed = parse_steam_library_paths(
            "\"libraryfolders\"\n{\n    \"0\"\n    {\n        \"path\"        \"D:\\\\SteamLibrary\"\n    }\n}\n",
        );

        assert_eq!(parsed, vec![PathBuf::from(r"D:\SteamLibrary")]);
    }

    #[test]
    fn setup_summary_prioritizes_cs2_installation() {
        let config = configured_setup();
        let summary = SetupSummary::from_config(&config, Cs2IntegrationStatus::MissingUnknownPath);

        assert_eq!(summary.current_step(), SetupStep::InstallCs2Integration);
        assert!(summary.needs_setup());
    }

    #[test]
    fn setup_summary_requires_auth_after_cs2_is_installed() {
        let summary = SetupSummary::from_config(
            &Config::default(),
            Cs2IntegrationStatus::Installed {
                target_path: PathBuf::from("cfg").join(GSI_CFG_FILE_NAME),
            },
        );

        assert_eq!(summary.current_step(), SetupStep::ConnectPishock);
    }

    #[test]
    fn setup_summary_requires_shocker_after_auth() {
        let mut config = Config::default();
        config.username = "player".into();
        config.apikey = "key".into();

        let summary = SetupSummary::from_config(
            &config,
            Cs2IntegrationStatus::Installed {
                target_path: PathBuf::from("cfg").join(GSI_CFG_FILE_NAME),
            },
        );

        assert!(has_auth_credentials(&config));
        assert_eq!(summary.current_step(), SetupStep::ChooseShocker);
    }

    #[test]
    fn setup_summary_is_complete_when_all_requirements_are_met() {
        let summary = SetupSummary::from_config(
            &configured_setup(),
            Cs2IntegrationStatus::Installed {
                target_path: PathBuf::from("cfg").join(GSI_CFG_FILE_NAME),
            },
        );

        assert!(summary.is_complete());
        assert_eq!(summary.current_step(), SetupStep::Complete);
    }

    fn configured_setup() -> Config {
        let mut config = Config::default();
        config.username = "player".into();
        config.apikey = "key".into();
        config.selected_client_id = Some(1);
        config.selected_shocker_id = Some(2);
        config
    }

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("cs2shock-{label}-{unique}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
