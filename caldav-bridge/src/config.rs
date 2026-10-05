use std::{net::SocketAddr, path::Path};

use common::LogConfig;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub struct Config {
    pub listen: SocketAddr,
    pub auth_url: String,
    pub calendar_url: String,
    pub tasks_url: String,
    pub auth_secret: String,
    pub calendar_secret: String,
    pub tasks_secret: String,
    #[serde(default)]
    pub log: LogConfig,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("auth_url", &self.auth_url)
            .field("calendar_url", &self.calendar_url)
            .field("tasks_url", &self.tasks_url)
            .field("auth_secret", &"<redacted>")
            .field("calendar_secret", &"<redacted>")
            .field("tasks_secret", &"<redacted>")
            .field("log", &self.log)
            .finish()
    }
}

impl Config {
    /// Reads a TOML file, then applies `ME_CALDAV__<FIELD>` environment overrides
    /// (`__` descends into tables, e.g. `ME_CALDAV__LOG__LEVEL=debug`).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        common::config::load(path, "ME_CALDAV__")
    }

    /// Config for integration tests: ephemeral port, the given service URLs and secrets.
    pub fn for_tests(urls: [&str; 3], secrets: [&str; 3]) -> Self {
        Self {
            listen: "127.0.0.1:0".parse().unwrap(),
            auth_url: urls[0].into(),
            calendar_url: urls[1].into(),
            tasks_url: urls[2].into(),
            auth_secret: secrets[0].into(),
            calendar_secret: secrets[1].into(),
            tasks_secret: secrets[2].into(),
            log: LogConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_env_override() {
        let toml = "listen = \"127.0.0.1:8085\"\nauth_url = \"http://a\"\ncalendar_url = \"http://c\"\ntasks_url = \"http://t\"\n\
                    auth_secret = \"from-file\"\ncalendar_secret = \"c\"\ntasks_secret = \"t\"\n";
        let env = [("ME_CALDAV__AUTH_SECRET".to_string(), "from-env".to_string())];
        let cfg = common::config::from_toml::<Config>(toml, "ME_CALDAV__", env).unwrap();
        assert_eq!(cfg.auth_secret, "from-env");
        assert_eq!(cfg.calendar_secret, "c");
        assert_eq!(cfg.log.level, "debug");
        assert!(!format!("{cfg:?}").contains("from-env"));
    }
}
