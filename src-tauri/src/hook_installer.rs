//! Manages the Claude Code `Stop` hook in `~/.claude/settings.json`.
//!
//! Installing / refreshing the hook is a write to a file the user owns and
//! probably edits by hand, so we go to some pains to preserve their existing
//! configuration:
//!
//!   - Read existing JSON and merge into it; only the `hooks.Stop` array is
//!     touched, every other top-level field is left untouched
//!   - Remove any prior hooks pointing at our webhook (so repeated installs
//!     don't accumulate duplicates — also catches token rotations)
//!   - Write atomically via temp-file + rename (so an interrupted write
//!     can't leave a half-written settings.json)
//!
//! The hook command itself is a single `curl.exe` POST with a Bearer token —
//! see `hook_command()` for the exact string we synthesize.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

/// Path to `~/.claude/settings.json`. Returns an error if we can't resolve
/// the user's home directory.
fn claude_settings_path(app: &AppHandle) -> Result<PathBuf> {
    let home = app
        .path()
        .home_dir()
        .context("could not resolve home directory")?;
    Ok(home.join(".claude").join("settings.json"))
}

/// The exact shell command we inject into the Stop hook.
///
/// Uses `curl.exe` (not `curl`) to bypass the PowerShell alias of `curl` to
/// `Invoke-WebRequest`. No body — the bearer token is the only thing the
/// server needs; `?source=claude-code-hook` just names the caller in
/// collector.log (v1.8.2) and is quoting-safe, unlike a JSON body.
fn hook_command(token: &str, port: u16) -> String {
    format!(
        "curl.exe -s -X POST -H \"Authorization: Bearer {token}\" \"http://localhost:{port}/refresh?source=claude-code-hook\""
    )
}

/// The command string of the Stop hook currently pointing at our webhook
/// (`None` = never installed, or the user removed it). Read-only.
pub fn installed_command(app: &AppHandle, port: u16) -> Result<Option<String>> {
    let path = claude_settings_path(app)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {:?}", path))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let root: Value = serde_json::from_str(&text).with_context(|| format!("{:?} is not valid JSON", path))?;
    let needle = format!("localhost:{port}/refresh");
    let found = root["hooks"]["Stop"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| entry["hooks"].as_array().cloned().unwrap_or_default())
        .filter_map(|h| h["command"].as_str().map(str::to_owned))
        .find(|c| c.contains(&needle));
    Ok(found)
}

/// v1.8.7（D86 蟲 5）：密碼牌換過（重新產生、或啟動時空牌補鑄）而 hook 手上還是舊牌，
/// 從那一刻起 Claude Code 每按一次門鈴都是 401——主人這台 2026-09-21 起整整一天
/// 就是這樣，log 裡 `refresh-webhook … unauthorized` 幾十行沒人看。
/// 已裝且命令對不上 → 用現在的牌重寫那一條（只動我們自己那一條）；沒裝就不碰。
/// 回傳 true＝真的改寫了。任何失敗只記 log，不打斷啟動。
pub async fn repair_if_installed(app: &AppHandle, token: &str, port: u16) -> bool {
    if token.is_empty() {
        return false;
    }
    let want = hook_command(token, port);
    let have = match installed_command(app, port) {
        Ok(v) => v,
        Err(e) => {
            crate::applog::write_log(
                app,
                "hook-repair-error",
                serde_json::json!({ "stage": "read", "error": format!("{e:#}") }),
            )
            .await;
            return false;
        }
    };
    let Some(have) = have else {
        return false;
    };
    if have == want {
        return false;
    }
    match install_stop_hook(app, token, port) {
        Ok(path) => {
            crate::applog::write_log(
                app,
                "hook-token-repaired",
                serde_json::json!({ "path": path.to_string_lossy() }),
            )
            .await;
            true
        }
        Err(e) => {
            crate::applog::write_log(
                app,
                "hook-repair-error",
                serde_json::json!({ "stage": "write", "error": format!("{e:#}") }),
            )
            .await;
            false
        }
    }
}

/// Install (or refresh) the Stop hook. Returns the path that was written.
pub fn install_stop_hook(app: &AppHandle, token: &str, port: u16) -> Result<PathBuf> {
    if token.is_empty() {
        anyhow::bail!("webhook_token is empty — cannot install hook");
    }

    let path = claude_settings_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let existing_text = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("read {:?}", path))?
    } else {
        String::from("{}")
    };

    // Tolerate a fully-empty file (treat as `{}`); reject anything else
    // that doesn't parse, rather than silently overwriting.
    let mut root: Value = if existing_text.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(&existing_text)
            .with_context(|| format!("{:?} is not valid JSON", path))?
    };

    let root_obj = root
        .as_object_mut()
        .context("~/.claude/settings.json root must be a JSON object")?;

    let hooks_obj = root_obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("`hooks` must be a JSON object")?;

    let stop_arr = hooks_obj
        .entry("Stop")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("`hooks.Stop` must be a JSON array")?;

    // Anything pointing at our webhook gets stripped first. Match on the
    // localhost:<port>/refresh substring so old hooks installed without a
    // token (or with a previous token) are evicted cleanly.
    let needle = format!("localhost:{port}/refresh");
    stop_arr.retain(|entry| {
        let serialized = serde_json::to_string(entry).unwrap_or_default();
        !serialized.contains(&needle)
    });

    // Fresh entry mirrors the shape Claude Code expects:
    //   { "hooks": [{ "type": "command", "command": "..." }] }
    let new_entry = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": hook_command(token, port),
        }]
    });
    stop_arr.push(new_entry);

    let serialized = serde_json::to_string_pretty(&root)?;

    // Atomic write: temp file in same dir → rename. Cross-device-safe
    // because both paths are inside `<home>/.claude/`.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serialized).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {:?} → {:?}", tmp, path))?;

    log::info!("Stop hook installed in {:?}", path);
    Ok(path)
}
