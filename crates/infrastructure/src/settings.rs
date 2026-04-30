use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use nocturne_domain::AudioSettings;

const CONFIG_DIR_ENV: &str = "NOCTURNE_CONFIG_DIR";
const SETTINGS_FILE_NAME: &str = "audio-settings.json";

#[derive(Debug)]
pub struct LocalAudioSettingsStore {
    path: PathBuf,
}

impl LocalAudioSettingsStore {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            path: default_settings_path()?,
        })
    }

    #[must_use]
    pub fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> io::Result<AudioSettings> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str::<AudioSettings>(&contents)
                .map(AudioSettings::clamped)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AudioSettings::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, settings: &AudioSettings) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = serde_json::to_vec_pretty(&settings.clamped())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&self.path, payload)
    }
}

fn default_settings_path() -> io::Result<PathBuf> {
    let config_dir = if let Some(path) = env::var_os(CONFIG_DIR_ENV) {
        PathBuf::from(path)
    } else if cfg!(windows) {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or(env::current_dir()?.join(".nocturne"))
            .join("Nocturne")
    } else if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(path).join("nocturne")
    } else if let Some(path) = env::var_os("HOME") {
        PathBuf::from(path).join(".config").join("nocturne")
    } else {
        env::current_dir()?.join(".nocturne")
    };

    Ok(config_dir.join(SETTINGS_FILE_NAME))
}
