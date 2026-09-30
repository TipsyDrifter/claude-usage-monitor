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

/// Absolute floor between probes regardless of settings (a probe is one
/// `claude.exe` run of ~10 s; anything under a minute would overlap).
/// v1.8.13（D91 T-09）：手動「立即刷新」繞過設定的最少間隔與退避，只剩這條地板。
pub const MIN_INTERVAL_FLOOR_SECS: i64 = 60;
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
/// v1.8.13（D91 T-01）：最近一次失敗是哪一種——"error"（沒回應／沒吐額度行）或
/// "stale"（CLI 回的是它手上的舊快取、沒碰到伺服器）。畫面的提示照這個換說法。
const K_LAST_FAILURE_KIND: &str = "probe_last_failure_kind";

pub struct ProbeOutcome {
    /// The three percentages parsed straight out of the text panel.
    pub samples: Vec<TruthSample>,
    pub raw_result: String,
    /// v1.8.13（D91 T-01）：探針開始的時刻——拿來跟 C 快取的 fetchedAtMs 比，
    /// 看 CLI 這次到底有沒有去伺服器拿新值。
    pub started_at: DateTime<Utc>,
}

/// v1.8.13（D91 T-01）：探針「成功」之後的真假判定。
///
/// 斷網、代理壞、登入失效，或探針落在 CLI 自己的短暫節流內時，`claude -p /usage`
/// 照樣 exit 0、吐三行百分比——但那是 `~/.claude.json` 快取裡的舊值（2026-09-28
/// 斷網對照實驗：46/12/18 vs 真值 50/13/19；主人 log 458 次 probe-ok 有 44 次是這樣）。
/// 唯一分得出來的記號是 C 快取的 `fetchedAtMs`：CLI 真的碰到伺服器才會把它推到探針開始之後。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// C 快取在探針開始之後刷新過＝拿到伺服器新值。
    Fresh,
    /// C 快取沒前進、而且探針吐的百分比就是 C 裡那一組＝這次回的是舊值。
    /// 帶快取年齡（秒，相對探針開始）。
    Stale { cache_age_secs: i64 },
    /// 沒辦法判斷——照舊採用並記 log（`reason` 寫進 collector.log）。
    Unverified { reason: &'static str },
}

/// 純函式：探針開始時刻 vs 重讀到的 C 快取時刻。`c_readable=false`＝重讀失敗；
/// `same_as_cache`＝探針吐的每一條百分比都跟重讀到的 C 一樣（見 `same_as_cache`）。
///
/// v1.8.13（D91 T-01 審查 R-1）：只有「C 沒前進」**而且**「探針值＝C 的值」才判舊值。
/// C 沒前進但值不同＝探針寫的不是我們讀的這一份（設了 `CLAUDE_CONFIG_DIR`、
/// `%USERPROFILE%\.claude.json` 只剩改設定前的舊檔），或 CLI 改版換了寫法——這時探針
/// 反而是唯一的即時來源，判舊值會讓它永遠被丟掉、退避升到上限、⚠ 一直掛著。
pub fn judge(
    started_at: DateTime<Utc>,
    c_readable: bool,
    c_fetched_at: Option<DateTime<Utc>>,
    same_as_cache: bool,
) -> Verdict {
    if !c_readable {
        return Verdict::Unverified { reason: "cache-unreadable" };
    }
    match c_fetched_at {
        Some(t) if t >= started_at => Verdict::Fresh,
        Some(t) if same_as_cache => Verdict::Stale {
            cache_age_secs: (started_at - t).num_seconds(),
        },
        Some(_) => Verdict::Unverified { reason: "cache-not-moved-values-differ" },
        // 讀得到檔、卻沒有 fetchedAtMs（CLI 改版換了欄位名、或還沒寫過快取）：分不出來。
        None => Verdict::Unverified { reason: "no-fetched-at" },
    }
}

/// v1.8.13（D91 T-01 審查 R-1）：探針吐的百分比是不是就是 C 快取裡那一組。
/// 每一條探針樣本都要在 C 找到同種類（週·單一模型再比模型名）且百分比差 ≤ 0.5 的一條——
/// C 存的是整數、面板也印整數，0.5 只是給四捨五入留的縫。探針沒樣本、或有一條在 C
/// 找不到對應，都算「不一樣」。
pub fn same_as_cache(probe: &[TruthSample], cache: &[TruthSample]) -> bool {
    !probe.is_empty()
        && probe.iter().all(|p| {
            cache.iter().any(|c| {
                c.limit_kind == p.limit_kind
                    && match (&c.scope, &p.scope) {
                        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                        (None, None) => true,
                        _ => false,
                    }
                    && (c.percent - p.percent).abs() <= 0.5
            })
        })
}

/// v1.8.13（D91 T-01）：探針結果定案。以前 `run()` 只要解析到行就清失敗計數、
/// `not_before=now`——CLI 吐快取舊值時 D87 的「連續失敗」提示永遠出不來、退避也不生效。
/// 現在由呼叫端重讀 C、判完真假再呼叫這裡：新值才清計數；舊值記成一種失敗（kind＝stale）。
pub async fn settle(
    ledger: &crate::ledger::Ledger,
    verdict: Verdict,
    started_at: DateTime<Utc>,
) -> Result<()> {
    match verdict {
        Verdict::Fresh | Verdict::Unverified { .. } => {
            ledger.set_channel_state(K_FAILURES, "0").await?;
            ledger
                .set_channel_state(K_NOT_BEFORE, &started_at.to_rfc3339())
                .await?;
            Ok(())
        }
        Verdict::Stale { .. } => record_failure(ledger, Utc::now(), "stale").await,
    }
}

/// v1.8.13（D91 T-09）：手動刷新只看 1 分鐘地板（不看設定的最少間隔、不看退避）。
/// 回 None＝可以探；Some(t)＝最早 t 才能再探。
pub async fn manual_not_before(
    ledger: &crate::ledger::Ledger,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let last = ledger
        .get_channel_state(K_LAST_RUN)
        .await?
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .map(|t| t.with_timezone(&Utc));
    Ok(last
        .map(|t| t + chrono::Duration::seconds(MIN_INTERVAL_FLOOR_SECS))
        .filter(|next| now < *next))
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

/// v1.8.8（D87）：給畫面用的探針健康狀態——連續失敗次數與下次允許的時刻。
/// v1.8.13（D91 T-01）：多回最近一次失敗的種類（"error"／"stale"），提示照它換說法。
pub async fn failure_status(
    ledger: &crate::ledger::Ledger,
) -> Result<(i64, Option<DateTime<Utc>>, String)> {
    let failures: i64 = ledger
        .get_channel_state(K_FAILURES)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let not_before = ledger
        .get_channel_state(K_NOT_BEFORE)
        .await?
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .map(|t| t.with_timezone(&Utc));
    let kind = ledger
        .get_channel_state(K_LAST_FAILURE_KIND)
        .await?
        .unwrap_or_else(|| "error".into());
    Ok((failures, not_before, kind))
}

/// Is a probe allowed right now? (floor + backoff, read from the ledger)
/// `hot` = the 5h window is ≥90%, which tightens the floor to 2 minutes.
/// v1.8.9（D88）：`min_secs`＝設定的「查額度最少間隔」；以前是寫死的 3 分地板。
/// v1.8.10：熱態（5h 到警戒值以上）改用 `hot_secs`（設定「接近警戒時的間隔」），
/// 取兩者較小者；兩個都有 1 分鐘的絕對地板。
pub async fn should_probe(
    ledger: &crate::ledger::Ledger,
    now: DateTime<Utc>,
    hot: bool,
    min_secs: i64,
    hot_secs: i64,
) -> Result<bool> {
    if let Some(nb) = ledger.get_channel_state(K_NOT_BEFORE).await? {
        if let Ok(nb) = DateTime::parse_from_rfc3339(&nb) {
            if now < nb.with_timezone(&Utc) {
                return Ok(false);
            }
        }
    }
    let min_secs = min_secs.max(MIN_INTERVAL_FLOOR_SECS);
    let floor = if hot {
        hot_secs.max(MIN_INTERVAL_FLOOR_SECS).min(min_secs)
    } else {
        min_secs
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

/// v1.8.8（D87）：主人正在用時，探針失敗的退避最多到這裡——2026-09-25 CLI 對某個帳號
/// 三成機率不吐額度行，指數退避一路爬到 40 分鐘，主人一直在用卻一小時沒刷新。
/// 探針 0 額度、一發 11 秒，活躍時 10 分鐘重試一次不算吵；閒置時維持 60 分上限。
const BACKOFF_MAX_ACTIVE_SECS: i64 = 10 * 60;
const ACTIVE_WINDOW_SECS: i64 = 15 * 60;

async fn record_failure(
    ledger: &crate::ledger::Ledger,
    now: DateTime<Utc>,
    kind: &str,
) -> Result<()> {
    let failures: i64 = ledger
        .get_channel_state(K_FAILURES)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        + 1;
    let active = ledger
        .get_channel_state("last_activity_at")
        .await?
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds() < ACTIVE_WINDOW_SECS)
        .unwrap_or(false);
    let cap = if active { BACKOFF_MAX_ACTIVE_SECS } else { BACKOFF_MAX_SECS };
    let backoff = (BACKOFF_BASE_SECS << (failures - 1).min(4)).min(cap);
    ledger
        .set_channel_state(K_FAILURES, &failures.to_string())
        .await?;
    ledger.set_channel_state(K_LAST_FAILURE_KIND, kind).await?;
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
/// v1.8.13（D91 T-01）：成功不再自己清失敗計數——呼叫端重讀 C、用 `judge` 判真假後
/// 呼叫 `settle` 定案（CLI 可能只是吐了快取舊值）。
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
            record_failure(ledger, now, "error").await?;
            return Err(e).context("spawn claude.exe");
        }
        Err(_) => {
            record_failure(ledger, now, "error").await?;
            anyhow::bail!("probe timed out after {TIMEOUT_SECS}s");
        }
    };

    if !out.status.success() {
        record_failure(ledger, now, "error").await?;
        anyhow::bail!(
            "claude.exe exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).chars().take(500).collect::<String>()
        );
    }

    // v1.8.13（D91 T-01 順手）：信封解析失敗以前直接 `?` 出去、沒記失敗——退避不生效。
    let envelope: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            record_failure(ledger, now, "error").await?;
            return Err(e).context("parse probe JSON envelope");
        }
    };

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
        record_failure(ledger, now, "error").await?;
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
        record_failure(ledger, now, "error").await?;
        anyhow::bail!("probe returned no parseable usage lines: {result_text:.200}");
    }

    // CLI 正常跑完：session 已經建起來了，先把 id 記住（不管這次數字是新是舊）。
    // 失敗計數交給呼叫端 `settle`（v1.8.13 D91 T-01）。
    if let Some(id) = minted_session {
        ledger.set_channel_state(K_SESSION_ID, &id).await?;
    }

    Ok(ProbeOutcome {
        samples,
        raw_result: result_text,
        started_at: now,
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

#[cfg(test)]
mod stale_tests {
    //! v1.8.13（D91 T-01）：探針吐快取舊值不算成功——判定與失敗計數的行為測試。
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn fresh_ledger(tag: &str) -> crate::ledger::Ledger {
        let dir = std::env::temp_dir().join(format!("cum-probe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::ledger::Ledger::open(dir).unwrap()
    }

    fn sample(kind: &str, scope: Option<&str>, pct: f32, source: Source) -> TruthSample {
        TruthSample {
            account_uuid: None,
            org_uuid: None,
            seat_confidence: SeatConfidence::Exact,
            limit_kind: kind.into(),
            scope: scope.map(str::to_string),
            percent: pct,
            resets_at: None,
            source,
            fetched_at: at("2026-09-28T13:25:26Z"),
            raw_json: serde_json::Value::Null,
        }
    }
    fn three(s: f32, w: f32, f: f32, source: Source) -> Vec<TruthSample> {
        vec![
            sample("session", None, s, source),
            sample("weekly_all", None, w, source),
            sample("weekly_scoped", Some("Fable"), f, source),
        ]
    }

    #[test]
    fn judge_needs_the_cache_to_move_past_probe_start() {
        let start = at("2026-09-28T13:36:00Z");
        // 斷網對照實驗那一格：快取停在 13:25:26、探針吐的就是快取那組 → 舊值，年齡 634 秒
        assert_eq!(
            judge(start, true, Some(at("2026-09-28T13:25:26Z")), true),
            Verdict::Stale { cache_age_secs: 634 }
        );
        // 在線：CLI 在探針跑的那 10 秒裡寫了快取
        assert_eq!(judge(start, true, Some(at("2026-09-28T13:36:06Z")), true), Verdict::Fresh);
        // 讀不到檔：沒辦法判斷
        assert_eq!(
            judge(start, false, None, false),
            Verdict::Unverified { reason: "cache-unreadable" }
        );
    }

    /// 審查 R-1 第一格：CLI 改版把 fetchedAtMs 改名／拿掉——讀得到檔但沒有時刻。
    /// 以前判舊值、探針永遠被丟；現在分不出來就照舊採用。
    #[test]
    fn a_cache_without_fetched_at_is_unverified_not_stale() {
        let start = at("2026-09-28T13:36:00Z");
        assert_eq!(
            judge(start, true, None, true),
            Verdict::Unverified { reason: "no-fetched-at" }
        );
        assert_eq!(
            judge(start, true, None, false),
            Verdict::Unverified { reason: "no-fetched-at" }
        );
    }

    /// 審查 R-1 第二格：設了 CLAUDE_CONFIG_DIR，我們讀的 %USERPROFILE%\.claude.json 是舊檔、
    /// 永遠不前進；探針寫的是另一份，值跟這份不一樣——那不是快取舊值，照舊採用。
    #[test]
    fn cache_not_moved_but_probe_values_differ_is_unverified() {
        let start = at("2026-09-28T13:36:00Z");
        let old_file = three(46.0, 12.0, 18.0, Source::CliCache);
        let probe_live = three(50.0, 13.0, 19.0, Source::Probe);
        let same = same_as_cache(&probe_live, &old_file);
        assert!(!same);
        assert_eq!(
            judge(start, true, Some(at("2026-09-28T13:25:26Z")), same),
            Verdict::Unverified { reason: "cache-not-moved-values-differ" }
        );
    }

    #[test]
    fn same_as_cache_matches_kind_scope_and_percent() {
        let c = three(46.0, 12.0, 18.0, Source::CliCache);
        // 斷網實驗：探針吐的就是快取那組
        assert!(same_as_cache(&three(46.0, 12.0, 18.0, Source::Probe), &c));
        // 只有一條不同也算不同
        assert!(!same_as_cache(&three(46.0, 12.0, 19.0, Source::Probe), &c));
        // 模型名不同（Fable vs 別的）不能拿來比
        let mut other_model = three(46.0, 12.0, 18.0, Source::Probe);
        other_model[2].scope = Some("Other".into());
        assert!(!same_as_cache(&other_model, &c));
        // C 沒有這條 → 算不同；探針沒樣本 → 算不同
        assert!(!same_as_cache(&three(46.0, 12.0, 18.0, Source::Probe), &c[..2]));
        assert!(!same_as_cache(&[], &c));
    }

    #[tokio::test]
    async fn stale_counts_as_failure_and_does_not_clear_the_streak() {
        let l = fresh_ledger("stale");
        let t0 = at("2026-09-28T13:00:00Z");
        // 先有一次真的失敗（沒回應）
        record_failure(&l, t0, "error").await.unwrap();
        // 接著探針「成功」但 C 沒前進 → 記成第二次失敗，不歸零
        settle(&l, Verdict::Stale { cache_age_secs: 600 }, t0).await.unwrap();
        let (n, nb, kind) = failure_status(&l).await.unwrap();
        assert_eq!(n, 2, "舊值不能把連續失敗歸零（D87 提示要出得來）");
        assert_eq!(kind, "stale");
        assert!(nb.is_some_and(|t| t > Utc::now()), "退避要生效：not_before 在未來");
        // 正常情況下的自動探會被退避擋住
        assert!(!should_probe(&l, Utc::now(), false, 60, 60).await.unwrap());
        // 真的拿到新值 → 歸零
        settle(&l, Verdict::Fresh, Utc::now()).await.unwrap();
        let (n, _, _) = failure_status(&l).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn manual_refresh_only_respects_the_one_minute_floor() {
        let l = fresh_ledger("manual");
        let now = Utc::now();
        // 從沒探過 → 可以探
        assert_eq!(manual_not_before(&l, now).await.unwrap(), None);
        // 30 秒前剛探過 → 地板擋住，告訴畫面 30 秒後
        l.set_channel_state(K_LAST_RUN, &(now - chrono::Duration::seconds(30)).to_rfc3339())
            .await
            .unwrap();
        let nb = manual_not_before(&l, now).await.unwrap().expect("被地板擋住");
        assert_eq!((nb - now).num_seconds(), 30);
        // 退避中（not_before 在一小時後）＋距上次 2 分鐘 → 手動仍可探
        record_failure(&l, now, "error").await.unwrap();
        l.set_channel_state(K_LAST_RUN, &(now - chrono::Duration::seconds(120)).to_rfc3339())
            .await
            .unwrap();
        assert_eq!(manual_not_before(&l, now).await.unwrap(), None);
        assert!(!should_probe(&l, now, false, 30 * 60, 120).await.unwrap(), "自動探仍被退避擋");
    }
}
