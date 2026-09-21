//! Channel A — the `claude -p "/usage"` probe (隨叫真值).
//!
//! The only ACTIVE channel: it asks the CLI, which asks the server, which
//! also refreshes channel C's cache as a side effect (O5) — so one probe
//! buys us both the live percentages AND a fresh structured cache to read
//! resets_at from.
//!
//! Guard rails (D45/D48 決策點 1–2):
//!   - spawned via CreateProcess directly on claude.exe, NEVER through a
//!     shell — Git Bash/MSYS rewrites "/usage" into "C:/Program Files/Git/
//!     usage" and burns a model turn (雷區 #10).
//!   - health check: the JSON envelope must report num_turns == 0 and
//!     total_cost_usd == 0, i.e. the slash command was handled locally.
//!   - one fixed probe session: first run mints a UUID with --session-id,
//!     later runs append with --resume, so the CLI keeps ONE ~4KB transcript
//!     instead of one per call. cwd points at our own empty probe dir so the
//!     transcript never mixes into the user's projects.
//!   - cadence floor (3 min) + failure backoff live in `should_probe` /
//!     `record_failure`; the caller decides WHEN to want a probe (staleness),
//!     this module decides whether one is currently allowed.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use super::model::{SeatConfidence, Source, TruthSample};

/// Minimum spacing between probes (D48 決策點 2). The idle pause lives in
/// the caller (collector::refresh) — this module only owns the floor.
const MIN_INTERVAL_SECS: i64 = 3 * 60;
/// Tighter floor while the 5h window is running hot (≥90%, D45).
const MIN_INTERVAL_HOT_SECS: i64 = 2 * 60;
/// Backoff after a failed probe starts here and doubles per failure…
const BACKOFF_BASE_SECS: i64 = 5 * 60;
/// …capped here.
const BACKOFF_MAX_SECS: i64 = 60 * 60;
/// A hung CLI is killed after this long.
const TIMEOUT_SECS: u64 = 90;

/// Channel-state keys (ledger.channel_state).
const K_SESSION_ID: &str = "probe_session_id";
const K_LAST_RUN: &str = "probe_last_run";
const K_NOT_BEFORE: &str = "probe_not_before";
const K_FAILURES: &str = "probe_consecutive_failures";

pub struct ProbeOutcome {
    /// The three percentages parsed straight out of the text panel.
    pub samples: Vec<TruthSample>,
    pub raw_result: String,
}

/// Minutes since the last probe attempt, None if it never ran.
pub async fn minutes_since_last(
    ledger: &crate::ledger::Ledger,
    now: DateTime<Utc>,
) -> Result<Option<i64>> {
    Ok(ledger
        .get_channel_state(K_LAST_RUN)
        .await?
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .map(|t| (now - t.with_timezone(&Utc)).num_minutes()))
}

/// Is a probe allowed right now? (floor + backoff, read from the ledger)
/// `hot` = the 5h window is ≥90%, which tightens the floor to 2 minutes.
pub async fn should_probe(
    ledger: &crate::ledger::Ledger,
    now: DateTime<Utc>,
    hot: bool,
) -> Result<bool> {
    if let Some(nb) = ledger.get_channel_state(K_NOT_BEFORE).await? {
        if let Ok(nb) = DateTime::parse_from_rfc3339(&nb) {
            if now < nb.with_timezone(&Utc) {
                return Ok(false);
            }
        }
    }
    let floor = if hot {
        MIN_INTERVAL_HOT_SECS
    } else {
        MIN_INTERVAL_SECS
    };
    if let Some(last) = ledger.get_channel_state(K_LAST_RUN).await? {
        if let Ok(last) = DateTime::parse_from_rfc3339(&last) {
            if (now - last.with_timezone(&Utc)).num_seconds() < floor {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

async fn record_failure(ledger: &crate::ledger::Ledger, now: DateTime<Utc>) -> Result<()> {
    let failures: i64 = ledger
        .get_channel_state(K_FAILURES)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        + 1;
    let backoff = (BACKOFF_BASE_SECS << (failures - 1).min(4)).min(BACKOFF_MAX_SECS);
    ledger
        .set_channel_state(K_FAILURES, &failures.to_string())
        .await?;
    ledger
        .set_channel_state(
            K_NOT_BEFORE,
            &(now + chrono::Duration::seconds(backoff)).to_rfc3339(),
        )
        .await?;
    Ok(())
}

fn claude_exe() -> std::path::PathBuf {
    // Preferred install location; fall back to PATH resolution by name.
    if let Ok(home) = std::env::var("USERPROFILE") {
        let p = std::path::PathBuf::from(home)
            .join(".local")
            .join("bin")
            .join("claude.exe");
        if p.exists() {
            return p;
        }
    }
    std::path::PathBuf::from("claude.exe")
}

fn probe_cwd() -> Result<std::path::PathBuf> {
    let base = std::env::var("LOCALAPPDATA").context("LOCALAPPDATA not set")?;
    let dir = std::path::PathBuf::from(base)
        .join("ClaudeUsageMonitor")
        .join("probe");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Run one probe. On success the caller should RE-READ channel C (the cache
/// was just refreshed server-side) for structured resets_at values.
pub async fn run(
    ledger: &crate::ledger::Ledger,
    seat: (Option<String>, Option<String>), // (account_uuid, org_uuid) from C
) -> Result<ProbeOutcome> {
    let now = Utc::now();
    ledger
        .set_channel_state(K_LAST_RUN, &now.to_rfc3339())
        .await?;

    let existing_session = ledger.get_channel_state(K_SESSION_ID).await?;
    let mut cmd = tokio::process::Command::new(claude_exe());
    cmd.current_dir(probe_cwd()?)
        .arg("-p")
        .arg("/usage")
        .arg("--output-format")
        .arg("json")
        .arg("--strict-mcp-config")
        .arg("--mcp-config")
        .arg(r#"{"mcpServers":{}}"#)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // CREATE_NO_WINDOW — no console flash from a tray app.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    let minted_session = match &existing_session {
        Some(id) => {
            cmd.arg("--resume").arg(id);
            None
        }
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            cmd.arg("--session-id").arg(&id);
            Some(id)
        }
    };

    let out = match tokio::time::timeout(
        std::time::Duration::from_secs(TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            record_failure(ledger, now).await?;
            return Err(e).context("spawn claude.exe");
        }
        Err(_) => {
            record_failure(ledger, now).await?;
            anyhow::bail!("probe timed out after {TIMEOUT_SECS}s");
        }
    };

    if !out.status.success() {
        record_failure(ledger, now).await?;
        anyhow::bail!(
            "claude.exe exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).chars().take(500).collect::<String>()
        );
    }

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parse probe JSON envelope")?;

    // Health check — a non-zero turn count means the slash command was sent
    // to the model as a prompt (MSYS-style mangling or CLI drift). Treat as
    // failure and back off: burning turns on the user's quota is the one
    // thing this probe must never silently do.
    let num_turns = envelope.get("num_turns").and_then(|v| v.as_i64()).unwrap_or(-1);
    let cost = envelope
        .get("total_cost_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(-1.0);
    if num_turns != 0 || cost != 0.0 {
        record_failure(ledger, now).await?;
        anyhow::bail!(
            "probe health check failed: num_turns={num_turns}, total_cost_usd={cost} — \
             /usage was NOT handled as a slash command"
        );
    }

    let result_text = envelope
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let samples = parse_result_text(&result_text, &seat, now);
    if samples.is_empty() {
        record_failure(ledger, now).await?;
        anyhow::bail!("probe returned no parseable usage lines: {result_text:.200}");
    }

    // Success: commit the minted session id, clear the failure streak.
    if let Some(id) = minted_session {
        ledger.set_channel_state(K_SESSION_ID, &id).await?;
    }
    ledger.set_channel_state(K_FAILURES, "0").await?;
    ledger
        .set_channel_state(K_NOT_BEFORE, &now.to_rfc3339())
        .await?;

    Ok(ProbeOutcome {
        samples,
        raw_result: result_text,
    })
}

/// Parse the text panel:
///   Current session: 43% used · resets Aug 23, 10:09pm (Asia/Taipei)
///   Current week (all models): 29% used · resets …
///   Current week (Fable): 47% used · resets …
/// We take the percentages only — resets_at is read from the refreshed
/// channel C instead (its ISO timestamps beat parsing localized month names).
fn parse_result_text(
    text: &str,
    seat: &(Option<String>, Option<String>),
    now: DateTime<Utc>,
) -> Vec<TruthSample> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let parsed: Option<(&str, Option<String>, &str)> =
            if let Some(rest) = line.strip_prefix("Current session:") {
                Some(("session", None, rest))
            } else if let Some(rest) = line.strip_prefix("Current week (") {
                match rest.split_once("):") {
                    Some((scope_str, tail)) if scope_str.eq_ignore_ascii_case("all models") => {
                        Some(("weekly_all", None, tail))
                    }
                    Some((scope_str, tail)) => {
                        Some(("weekly_scoped", Some(scope_str.to_string()), tail))
                    }
                    None => None,
                }
            } else {
                None
            };
        let Some((kind, scope, tail)) = parsed else {
            continue;
        };

        let Some(percent) = tail
            .trim()
            .split('%')
            .next()
            .and_then(|n| n.trim().parse::<f32>().ok())
        else {
            continue;
        };

        out.push(TruthSample {
            account_uuid: seat.0.clone(),
            org_uuid: seat.1.clone(),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: kind.to_string(),
            scope,
            percent,
            resets_at: None, // structured resets_at comes from re-read C
            source: Source::Probe,
            fetched_at: now,
            raw_json: serde_json::json!({ "line": line }),
        });
    }
    out
}
