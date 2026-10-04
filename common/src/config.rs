//! Layered configuration: a TOML file with environment overrides.
use std::path::Path;

use serde::de::DeserializeOwned;

/// Reads a TOML file, then applies `<prefix><FIELD>` environment overrides
/// (`__` descends into tables, e.g. `ME_AUTH__LOG__LEVEL=debug` for prefix `ME_AUTH__`).
pub fn load<T: DeserializeOwned>(path: &Path, prefix: &str) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    from_toml(&text, prefix, std::env::vars())
}

// ponytail: env overrides are strings only (fine for secrets, urls, levels); typed values go in the file
pub fn from_toml<T: DeserializeOwned>(
    text: &str,
    prefix: &str,
    env: impl IntoIterator<Item = (String, String)>,
) -> anyhow::Result<T> {
    let mut doc: toml::Table = text.parse()?;
    for (key, value) in env {
        let Some(rest) = key.strip_prefix(prefix) else {
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
