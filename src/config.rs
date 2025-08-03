use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mount_point: PathBuf,
    pub source_paths: Vec<SourcePath>,
    pub filename_patterns: Vec<String>,
    pub heic_settings: HeicSettings,
    pub cache: CacheSettings,
    #[serde(default)]
    pub fuse: FuseSettings,
    pub logging: LoggingSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcePath {
    pub path: PathBuf,
    pub recursive: bool,
    /// Name to appear in the FUSE mount (e.g., "pictures", "downloads")
    pub mount_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeicSettings {
    pub quality: u8,
    pub speed: u8,
    pub chroma: u16,
    /// Maximum pixel resolution - images larger than this will be resized
    /// Format: "width,height" or "2560,1440" for 1440p. None = no limit
    pub max_resolution: Option<String>,
}

impl HeicSettings {
    /// Parse max_resolution string into (width, height) tuple
    /// Returns None if no limit is set or parsing fails
    pub fn get_max_resolution(&self) -> Option<(u32, u32)> {
        if let Some(ref res_str) = self.max_resolution {
            if let Some((width_str, height_str)) = res_str.split_once(',') {
                if let (Ok(width), Ok(height)) =
                    (width_str.trim().parse(), height_str.trim().parse())
                {
                    return Some((width, height));
                }
            }
        }
        None
    }

    /// Check if image dimensions exceed the configured limit
    pub fn should_resize(&self, width: u32, height: u32) -> bool {
        if let Some((max_width, max_height)) = self.get_max_resolution() {
            width > max_width || height > max_height
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheSettings {
    pub max_size_mb: u64,
    pub cache_dir: Option<PathBuf>,
    /// Enable cache file encryption using the source filepath as the encryption key
    /// Default: true for security
    #[serde(default = "default_encryption")]
    pub enable_encryption: bool,
}

fn default_encryption() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuseSettings {
    /// How long FUSE should cache filesystem operations (seconds)
    pub cache_timeout: u64,
}

impl Default for FuseSettings {
    fn default() -> Self {
        Self {
            cache_timeout: 60, // Cache for 1 minute
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingSettings {
    pub level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mount_point: PathBuf::from("/tmp/fuse-img2heic"),
            source_paths: vec![
                SourcePath {
                    path: PathBuf::from(format!(
                        "{}/Pictures",
                        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
                    )),
                    recursive: true,
                    mount_name: "pictures".to_string(),
                },
                SourcePath {
                    path: PathBuf::from(format!(
                        "{}/Downloads",
                        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
                    )),
                    recursive: false,
                    mount_name: "downloads".to_string(),
                },
            ],
            fuse: FuseSettings::default(),
            filename_patterns: vec![r".*\.(jpg|jpeg|png|gif|heic)$".to_string()],
            heic_settings: HeicSettings {
                quality: 50,
                speed: 4,
                chroma: 420,
                max_resolution: None, // No limit by default
            },
            cache: CacheSettings {
                max_size_mb: 1024,
                cache_dir: None,         // Will use default XDG cache dir
                enable_encryption: true, // Enable by default
            },
            logging: LoggingSettings {
                level: "warn".to_string(),
            },
        }
    }
}

impl Config {
    pub fn load(config_path: &Path) -> Result<Self> {
        if config_path.exists() {
            let content = fs::read_to_string(config_path)
                .with_context(|| format!("Failed to read config file: {config_path:?}"))?;

            let mut config: Config = serde_yaml::from_str(&content)
                .with_context(|| format!("Failed to parse config file: {config_path:?}"))?;

            // Set cache directory to XDG cache dir if not specified
            if config.cache.cache_dir.is_none() {
                config.cache.cache_dir = Some(Self::get_cache_dir()?);
            }

            Ok(config)
        } else {
            log::warn!("Config file not found at {config_path:?}, creating default config");
            let config = Self::default();
            config.save(config_path)?;
            Ok(config)
        }
    }

    pub fn save(&self, config_path: &Path) -> Result<()> {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {parent:?}"))?;
        }

        let content = serde_yaml::to_string(self).context("Failed to serialize config")?;

        fs::write(config_path, content)
            .with_context(|| format!("Failed to write config file: {config_path:?}"))?;

        Ok(())
    }

    pub fn get_default_config_path() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME environment variable not set")?;

        // Use XDG_CONFIG_HOME if set, otherwise ~/.config
        let config_home =
            std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));

        Ok(PathBuf::from(config_home)
            .join("fuse-img2heic-rs")
            .join("config.yaml"))
    }

    pub fn get_cache_dir() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME environment variable not set")?;

        // Use XDG_CACHE_HOME if set, otherwise ~/.cache
        let cache_home =
            std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| format!("{home}/.cache"));

        let cache_dir = PathBuf::from(cache_home).join("fuse-img2heic-rs");

        // Create cache directory if it doesn't exist
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("Failed to create cache directory: {cache_dir:?}"))?;

        Ok(cache_dir)
    }

    pub fn get_cache_dir_from_config(&self) -> Result<PathBuf> {
        match &self.cache.cache_dir {
            Some(dir) => {
                fs::create_dir_all(dir)
                    .with_context(|| format!("Failed to create cache directory: {dir:?}"))?;
                Ok(dir.clone())
            }
            None => Self::get_cache_dir(),
        }
    }
}
