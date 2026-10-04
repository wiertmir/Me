use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SignupMode {
    #[default]
    Open,
    Disabled,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub format: LogFormat,
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Pretty,
            level: "debug".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    /// Plain connection upgraded with STARTTLS (usually port 587).
    #[default]
    Starttls,
    /// TLS from the first byte (usually port 465).
    Implicit,
    /// Plain SMTP, no encryption. Local development and testing only.
    None,
}

#[derive(Clone, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub from: String,
    #[serde(default)]
    pub tls: SmtpTls,
}

fn default_smtp_port() -> u16 {
    587
}

#[derive(Clone, Deserialize)]
pub struct ProviderConfig {
    pub client_id: String,
    pub client_secret: String,
    // Endpoint overrides, for tests that point a provider at a stub.
    pub auth_url: Option<String>,
    pub token_url: Option<String>,
    pub userinfo_url: Option<String>,
    /// GitHub only: the `/user/emails` endpoint.
    pub emails_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientConfig {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
}

#[derive(Clone, Deserialize)]
pub struct Config {
    pub issuer: String,
    pub web_url: String,
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub service_secret: String,
    #[serde(default)]
    pub signup: SignupMode,
    pub seed_username: String,
    pub seed_email: Option<String>,
    #[serde(default = "default_audience")]
    pub audience: String,
    #[serde(default)]
    pub log: LogConfig,
    pub smtp: Option<SmtpConfig>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub clients: Vec<ClientConfig>,
}

impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("from", &self.from)
            .field("tls", &self.tls)
            .finish()
    }
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("issuer", &self.issuer)
            .field("web_url", &self.web_url)
            .field("listen", &self.listen)
            .field("data_dir", &self.data_dir)
            .field("service_secret", &"<redacted>")
            .field("signup", &self.signup)
            .field("seed_username", &self.seed_username)
            .field("seed_email", &self.seed_email)
            .field("audience", &self.audience)
            .field("log", &self.log)
            .field("smtp", &self.smtp)
            .field("providers", &self.providers)
            .field("clients", &self.clients)
            .finish()
    }
}

fn default_audience() -> String {
    "me-api".into()
}

impl Config {
    /// Reads a TOML file, then applies `ME_AUTH__<FIELD>` environment overrides
    /// (`__` descends into tables, e.g. `ME_AUTH__LOG__LEVEL=debug`).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        Self::from_toml(&text, std::env::vars())
    }

    // ponytail: env overrides are strings only (fine for secrets, urls, levels); typed values go in the file
    pub fn from_toml(
        text: &str,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> anyhow::Result<Self> {
        let mut doc: toml::Table = text.parse()?;
        for (key, value) in env {
            let Some(rest) = key.strip_prefix("ME_AUTH__") else {
                continue;
            };
            let parts: Vec<String> = rest.split("__").map(str::to_lowercase).collect();
            let (last, parents) = parts.split_last().expect("split yields at least one part");
            let mut table = &mut doc;
            for p in parents {
                table = table
                    .entry(p.clone())
                    .or_insert_with(|| toml::Value::Table(Default::default()))
                    .as_table_mut()
                    .ok_or_else(|| anyhow::anyhow!("{key}: {p} is not a table"))?;
            }
            table.insert(last.clone(), toml::Value::String(value));
        }
        Ok(doc.try_into()?)
    }

    /// Config for integration tests: ephemeral port, temp data dir, known secret.
    pub fn for_tests(data_dir: PathBuf, service_secret: &str) -> Self {
        Self {
            issuer: "http://localhost".into(),
            web_url: "http://localhost:5080".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir,
            service_secret: service_secret.into(),
            signup: SignupMode::Open,
            seed_username: "wiertmir".into(),
            seed_email: None,
            audience: default_audience(),
            log: LogConfig::default(),
            smtp: None,
            providers: HashMap::new(),
            clients: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
issuer = "http://localhost:8081"
web_url = "http://localhost:5080"
listen = "127.0.0.1:8081"
data_dir = "./data"
service_secret = "from-file"
seed_username = "wiertmir"
"#;

    #[test]
    fn defaults_and_env_override() {
        let env = [
            (
                "ME_AUTH__SERVICE_SECRET".to_string(),
                "from-env".to_string(),
            ),
            ("ME_AUTH__LOG__FORMAT".to_string(), "json".to_string()),
            ("UNRELATED".to_string(), "x".to_string()),
        ];
        let cfg = Config::from_toml(BASE, env).unwrap();
        assert_eq!(cfg.service_secret, "from-env");
        assert_eq!(cfg.log.format, LogFormat::Json);
        assert_eq!(cfg.audience, "me-api");
        assert_eq!(cfg.signup, SignupMode::Open);
    }
}
