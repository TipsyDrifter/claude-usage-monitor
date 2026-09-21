//! Desktop `bridge-state.json` — a free org→account mapping (read-only).
//!
//! `%APPDATA%\Claude\bridge-state.json` keys look like `"<orgUuid>:<accountUuid>"`,
//! one per account the Desktop app has seen. D48 wanted an org→account
//! learning table; this file simply is one, so we seed the seats table from
//! it instead of asking the user. Values are ignored (opaque bridge state).

use anyhow::{Context, Result};

/// Returns learned (account_uuid, org_uuid) pairs. Empty when the file is
/// missing or has no recognizable keys — never an error path worth blocking on.
pub fn read_pairs() -> Result<Vec<(String, String)>> {
    let appdata = std::env::var("APPDATA").context("APPDATA not set")?;
    let path = std::path::PathBuf::from(appdata)
        .join("Claude")
        .join("bridge-state.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).context("parse bridge-state.json")?;
    let Some(obj) = value.as_object() else {
        return Ok(Vec::new());
    };

    let is_uuid = |s: &str| {
        s.len() == 36 && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
    };

    let mut pairs = Vec::new();
    for key in obj.keys() {
        if let Some((org, account)) = key.split_once(':') {
            if is_uuid(org) && is_uuid(account) {
                pairs.push((account.to_string(), org.to_string()));
            }
        }
    }
    Ok(pairs)
}
