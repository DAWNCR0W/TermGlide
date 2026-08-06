//! Versioned configuration for the public Chrome-backed product surface.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tg_url::{BrowserUrl, SearchEngine};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub browser: BrowserConfig,
    pub render: RenderConfig,
    pub network: NetworkConfig,
    pub javascript: JavaScriptConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            browser: BrowserConfig::default(),
            render: RenderConfig::default(),
            network: NetworkConfig::default(),
            javascript: JavaScriptConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    pub homepage: String,
    pub default_search: String,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            homepage: "about:newtab".to_owned(),
            default_search: "https://duckduckgo.com/?q={query}".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderConfig {
    pub backend: String,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            backend: "auto".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub proxy: String,
    pub no_proxy: Vec<String>,
    pub data_limit_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JavaScriptConfig {
    pub enabled: bool,
}

impl Default for JavaScriptConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("configuration TOML is invalid: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("unsupported configuration version {0}")]
    Version(u32),
    #[error("configuration value is invalid: {0}")]
    Validation(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let bytes = std::fs::read(path)?;
        if bytes.len() > 1024 * 1024 {
            return Err(ConfigError::Validation(
                "configuration exceeds the 1 MiB limit".to_owned(),
            ));
        }
        let source = std::str::from_utf8(&bytes)
            .map_err(|_| ConfigError::Validation("configuration must be UTF-8".to_owned()))?;
        let config: Self = toml::from_str(source)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != 1 {
            return Err(ConfigError::Version(self.version));
        }
        BrowserUrl::parse(&self.browser.homepage)
            .map_err(|error| ConfigError::Validation(error.to_string()))?;
        SearchEngine::new(self.browser.default_search.clone())
            .map_err(|error| ConfigError::Validation(error.to_string()))?;
        if !matches!(
            self.render.backend.as_str(),
            "auto" | "cells" | "halfblock" | "quadrant" | "braille"
        ) {
            return Err(ConfigError::Validation(
                "render.backend must be auto, cells, halfblock, quadrant, or braille".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{Config, ConfigError};

    #[test]
    fn safe_defaults_validate() -> Result<(), Box<dyn Error>> {
        Config::default().validate()?;
        Ok(())
    }

    #[test]
    fn unknown_and_removed_fields_are_rejected() {
        assert!(toml::from_str::<Config>("version=1\nunknown=true").is_err());
        let mut config = Config::default();
        config.render.backend = "kitty".to_owned();
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));
    }
}
