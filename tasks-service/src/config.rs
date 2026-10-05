use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use common::LogConfig;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub issuer: String,
    pub jwks_url: Option<String>,
    #[serde(default = "default_audience")]
    pub audience: String,
    pub service_secret: String,
    #[serde(default)]
    pub log: LogConfig,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("data_dir", &self.data_dir)
            .field("issuer", &self.issuer)
            .field("jwks_url", &self.jwks_url)
            .field("audience", &self.audience)
            .field("service_secret", &"<redacted>")
            .field("log", &self.log)
            .finish()
    }
}

fn default_audience() -> String {
    "me-api".into()
}

impl Config {
    /// Reads a TOML file, then applies `ME_TASKS__<FIELD>` environment overrides
    /// (`__` descends into tables, e.g. `ME_TASKS__LOG__LEVEL=debug`).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        common::config::load(path, "ME_TASKS__")
    }

    /// Config for integration tests: ephemeral port, temp data dir, known secret.
    pub fn for_tests(data_dir: PathBuf, issuer: &str, service_secret: &str) -> Self {
        Self {
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir,
            issuer: issuer.into(),
            jwks_url: None,
            audience: default_audience(),
            service_secret: service_secret.into(),
            log: LogConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_env_override() {
        let toml = "listen = \"127.0.0.1:8084\"\ndata_dir = \"./data\"\nissuer = \"http://i\"\nservice_secret = \"from-file\"\n";
        let env = [(
            "ME_TASKS__SERVICE_SECRET".to_string(),
            "from-env".to_string(),
        )];
        let cfg = common::config::from_toml::<Config>(toml, "ME_TASKS__", env).unwrap();
        assert_eq!(cfg.service_secret, "from-env");
        assert_eq!(cfg.audience, "me-api");
        assert_eq!(cfg.jwks_url, None);
    }
}
