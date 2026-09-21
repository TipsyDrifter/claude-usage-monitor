//! The collection layer (M0 換心手術) — replaces the removed claude.ai
//! scraper with the four-channel design of 規格書 §4:
//!
//!   A `/usage` probe (probe.rs, wired in a later M0 commit)
//!   B Desktop plan-usage-history.json (desktop_file.rs)
//!   C CLI cachedUsageUtilization (cli_cache.rs)
//!   D statusline forwarder (加分項, deferred)
//!
//! `refresh()` keeps the exact contract the scraper had — same `UsageState`
//! shape, same `usage://update` event, same tray/notifier side effects — so
//! the three frontend windows keep working unchanged. What changes is where
//! the numbers come from and that every number now carries a freshness stamp
//! (source + fetched_at) in its `note`: 絕不讓凍結的數字看起來像即時 (規格書 §1).

pub mod bridge;
pub mod cli_cache;
pub mod desktop_file;
pub mod jsonl;
pub mod model;
pub mod probe;

use anyhow::Result;
use chrono::{DateTime, Utc};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;
use crate::types::{ScrapeStatus, UsageItem, UsageSnapshot, UsageState};
use model::{age_label, TruthSample};

/// Channel C's cache counts as stale after this long — beyond it a refresh
/// tries to fire the probe (subject to probe cadence guards).
const CLI_STALE_SECS: i64 = 10 * 60;

/// No transcript activity for this long → the machine is idle → the probe
/// stays quiet (D45 閒置暫停). Passive channels (B/C) are still read.
const IDLE_PAUSE_SECS: i64 = 15 * 60;

/// Cross-account conflict detection (D51): B vs A/C samples of the same
/// limit taken within this co-window…
const CONFLICT_CO_WINDOW_SECS: i64 = 20 * 60;
/// …differing by more than this many percent-points ⇒ Desktop is probably a
/// different account — tell the user instead of silently merging.
const CONFLICT_THRESHOLD_PCT: f32 = 10.0;

/// D54: a refresh caused by a human gesture (立即刷新 button, tray menu) or
/// the webhook doorbell (browser extension observed claude.ai traffic) IS
/// activity — stamp it so the idle pause never blocks these paths.
pub async fn refresh_manual(app: &AppHandle) -> Result<()> {
    let ledger: tauri::State<crate::ledger::Ledger> = app.state();
    let _ = ledger
        .set_channel_state("last_activity_at", &Utc::now().to_rfc3339())
        .await;
    refresh(app).await
}

/// D55 加碼: webhook rings additionally track "futility" — a doorbell that
/// keeps ringing while the CLI numbers never move usually means the browser
/// is logged into a third account we can't see (files/擴充登入第三帳號的情況).
/// The bell itself stays deaf and dumb (合規本錢); we only count outcomes.
pub async fn refresh_webhook(app: &AppHandle) -> Result<()> {
    let ledger: tauri::State<crate::ledger::Ledger> = app.state();
    let _ = ledger
        .set_channel_state("pending_refresh_cause", "webhook")
        .await;
    refresh_manual(app).await
}

/// Pull every available channel, rebuild the view-model, push it to state /
/// event / tray / notifier. Same signature and call sites as the old
/// `scraper::refresh`.
pub async fn refresh(app: &AppHandle) -> Result<()> {
    // Mark loading (UI shows the spinner exactly like before).
    let state: tauri::State<AppState> = app.state();
    let mut current = state.get_usage().await;
    current.status = ScrapeStatus::Loading;
    state.set_usage(current.clone()).await;
    let _ = app.emit("usage://update", &current);
    crate::tray::refresh_tray(app, &current).await;

    let now = Utc::now();

    // Channel C — structured limits + seat anchor. May be stale (days!).
    let mut cli = match cli_cache::read() {
        Ok(r) => Some(r),
        Err(e) => {
            crate::applog::write_log(
                app,
                "cli-cache-error",
                serde_json::json!({ "error": format!("{e:#}") }),
            )
            .await;
            None
        }
    };

    // Channel A — fire the probe when C's cache is stale and the cadence
    // guard allows it. A successful probe refreshes C server-side (O5), so
    // we re-read C right after and its resets_at become current too.
    let mut probe_samples: Vec<TruthSample> = Vec::new();
    let cli_is_stale = cli
        .as_ref()
        .and_then(|c| c.fetched_at)
        .map(|t| (now - t).num_seconds() > CLI_STALE_SECS)
        .unwrap_or(true);
    if cli_is_stale {
        let ledger: tauri::State<crate::ledger::Ledger> = app.state();
        // Idle pause: no transcript activity in 15 min → don't probe. An
        // unset marker (fresh install, first scan pending) counts as active.
        let active = match ledger
            .get_channel_state("last_activity_at")
            .await
            .ok()
            .flatten()
        {
            Some(ts) => chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|t| (now - t.with_timezone(&Utc)).num_seconds() < IDLE_PAUSE_SECS)
                .unwrap_or(true),
            None => true,
        };
        // D54 低頻心跳: idle doesn't mean fully silent — the web-side blind
        // spot (claude.ai usage rings no JSONL doorbell) is bounded by one
        // heartbeat. 0 disables and restores the pure idle pause.
        let heartbeat_min = state.get_settings().await.general.idle_heartbeat_minutes;
        let heartbeat_due = !active
            && heartbeat_min > 0
            && probe::minutes_since_last(&ledger, now)
                .await
                .ok()
                .flatten()
                .map(|m| m >= heartbeat_min as i64)
                .unwrap_or(true);
        // ≥90% on the 5h window tightens the probe floor to 2 min (D45).
        let hot = current
            .data
            .as_ref()
            .and_then(|d| d.current_session.used_percent)
            .map(|p| p >= 90.0)
            .unwrap_or(false);
        if (active || heartbeat_due)
            && probe::should_probe(&ledger, now, hot).await.unwrap_or(false)
        {
            let seat = (
                cli.as_ref()
                    .and_then(|c| c.oauth.as_ref())
                    .and_then(|o| o.account_uuid.clone()),
                cli.as_ref()
                    .and_then(|c| c.oauth.as_ref())
                    .and_then(|o| o.organization_uuid.clone()),
            );
            match probe::run(&ledger, seat).await {
                Ok(outcome) => {
                    crate::applog::write_log(
                        app,
                        "probe-ok",
                        serde_json::json!({
                            "samples": outcome.samples.len(),
                            "result_head": outcome.raw_result.chars().take(300).collect::<String>(),
                        }),
                    )
                    .await;
                    probe_samples = outcome.samples;
                    if let Ok(fresh) = cli_cache::read() {
                        cli = Some(fresh);
                    }
                }
                Err(e) => {
                    crate::applog::write_log(
                        app,
                        "probe-error",
                        serde_json::json!({ "error": format!("{e:#}") }),
                    )
                    .await;
                }
            }
        }
    }

    // Channel B — Desktop history file. Fresh while Desktop runs (900s beat).
    let desktop = if desktop_file::exists() {
        match desktop_file::read(None) {
            Ok(r) => {
                if let Some(v) = r.unexpected_version {
                    crate::applog::write_log(
                        app,
                        "desktop-file-version-drift",
                        serde_json::json!({ "version": v }),
                    )
                    .await;
                }
                Some(r)
            }
            Err(e) => {
                // Possibly caught mid-write — next round retries; log only.
                crate::applog::write_log(
                    app,
                    "desktop-file-error",
                    serde_json::json!({ "error": format!("{e:#}") }),
                )
                .await;
                None
            }
        }
    } else {
        None
    };

    let current_org = cli
        .as_ref()
        .and_then(|c| c.oauth.as_ref())
        .and_then(|o| o.organization_uuid.clone());

    // D52/O11: org→account mapping from bridge-state, used to book B samples
    // onto their real exact seat (confidence stays inferred).
    let bridge_map: std::collections::HashMap<String, String> = bridge::read_pairs()
        .unwrap_or_default()
        .into_iter()
        .map(|(account, org)| (org, account))
        .collect();

    // ---- Persist into the ledger (骨). Errors are logged, never fatal to
    // the live display (臉) — a broken disk shouldn't blank the tray.
    let ledger: tauri::State<crate::ledger::Ledger> = app.state();
    let mut persisted = serde_json::json!({});
    if !probe_samples.is_empty() {
        match ledger.insert_truth_samples(&probe_samples).await {
            Ok(n) => persisted["probe_inserted"] = n.into(),
            Err(e) => {
                crate::applog::write_log(
                    app,
                    "ledger-persist-error",
                    serde_json::json!({ "error": format!("{e:#}"), "stage": "probe" }),
                )
                .await;
            }
        }
    }
    if let Err(e) = persist(&ledger, cli.as_ref(), desktop.as_ref(), &bridge_map, &mut persisted).await {
        crate::applog::write_log(
            app,
            "ledger-persist-error",
            serde_json::json!({ "error": format!("{e:#}") }),
        )
        .await;
    }

    // View-model candidates: CLI-account sources ONLY (D51 — 主人拍板
    // 「以資料比較齊全的 CLI 為主」). Channel B is quarantined to the ledger:
    // Desktop can be logged into a DIFFERENT account, so its numbers never
    // reach the display, not even as a fallback.
    let mut candidates: Vec<TruthSample> = Vec::new();
    candidates.extend(probe_samples.iter().cloned());
    if let Some(c) = &cli {
        candidates.extend(c.samples.iter().cloned());
    }

    if candidates.is_empty() {
        let prev = state.get_usage().await;
        let new_state = UsageState {
            status: ScrapeStatus::Error,
            data: prev.data,
            error: Some(
                "找不到 CLI 資料來源（cachedUsageUtilization 與探針皆無資料）——顯示以 CLI 帳號為準"
                    .to_string(),
            ),
            notice: None,
            last_success_at: prev.last_success_at,
            next_refresh_at: None,
        };
        state.set_usage(new_state.clone()).await;
        let _ = app.emit("usage://update", &new_state);
        crate::tray::refresh_tray(app, &new_state).await;
        return Ok(());
    }

    let snapshot = build_snapshot(&candidates, cli.as_ref(), now);
    let cli_account = cli
        .as_ref()
        .and_then(|c| c.oauth.as_ref())
        .and_then(|o| o.account_uuid.clone());

    // v1.3 (D75 決策點 3／4): remember which seat the CLI is logged into and
    // stamp its organisation name / email onto the seats row (display-only
    // fields from oauthAccount — never a token).
    if let Some(o) = cli.as_ref().and_then(|c| c.oauth.as_ref()) {
        let ledger: tauri::State<crate::ledger::Ledger> = app.state();
        if let Err(e) = ledger
            .mark_current_seat(
                o.account_uuid.as_deref(),
                o.organization_uuid.as_deref(),
                o.email_address.as_deref().filter(|s| !s.is_empty()),
                o.organization_name.as_deref().filter(|s| !s.is_empty()),
            )
            .await
        {
            log::warn!("mark_current_seat failed: {e:?}");
        }
    }
    let notice =
        detect_account_notice(&candidates, desktop.as_ref(), &bridge_map, cli_account.as_deref(), now);

    // v1.2 two-level alerts: debounced per limit per window (memory in AppState).
    let settings = state.get_settings().await;
    crate::notifier::check_threshold_crossings(app, &state, &settings, &snapshot).await;

    // D55 門鈴白響計數: CLI session % moved → any doorbell is proven useful,
    // reset. Unmoved AND this refresh was webhook-caused → one more futile
    // ring. (Read-and-clear the cause so scheduler ticks never count.)
    {
        let prev_pct = current
            .data
            .as_ref()
            .and_then(|d| d.current_session.used_percent);
        let new_pct = snapshot.current_session.used_percent;
        let cause = ledger
            .get_channel_state("pending_refresh_cause")
            .await
            .ok()
            .flatten();
        let _ = ledger.set_channel_state("pending_refresh_cause", "").await;
        if prev_pct.is_some() && new_pct.is_some() && prev_pct != new_pct {
            let _ = ledger.set_channel_state("doorbell_futile_count", "0").await;
        } else if cause.as_deref() == Some("webhook") {
            let n: i64 = ledger
                .get_channel_state("doorbell_futile_count")
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let _ = ledger
                .set_channel_state("doorbell_futile_count", &(n + 1).to_string())
                .await;
        }
    }

    let new_state = UsageState {
        status: ScrapeStatus::Ok,
        data: Some(snapshot),
        error: None,
        notice,
        last_success_at: Some(now.to_rfc3339()),
        next_refresh_at: None,
    };
    state.set_usage(new_state.clone()).await;
    let _ = app.emit("usage://update", &new_state);
    crate::tray::refresh_tray(app, &new_state).await;

    let view = new_state.data.as_ref().map(|d| {
        serde_json::json!({
            "session": d.current_session.used_percent,
            "session_src": d.current_session.note,
            "weekly_all": d.weekly_all_models.used_percent,
            "weekly_all_src": d.weekly_all_models.note,
            "fable": d.weekly_fable.used_percent,
            "fable_src": d.weekly_fable.note,
        })
    });
    crate::applog::write_log(
        app,
        "refresh-ok",
        serde_json::json!({
            "candidates": candidates.len(),
            "cli_fetched_at": cli.as_ref().and_then(|c| c.fetched_at).map(|t| t.to_rfc3339()),
            "org": current_org,
            "persisted": persisted,
            "view": view,
        }),
    )
    .await;

    // v1.8.2 效能：採集完就在背景把趨勢頁／歷史頁的快取算新，主人開頁不用等。
    ledger.warm_cache();

    Ok(())
}

/// Desktop's write cadence is ~900s; a hole ≥ 3 beats means the app (or the
/// machine) was off — record it as an observation gap.
const DESKTOP_GAP_THRESHOLD_MS: i64 = 3 * 900 * 1000;

/// Write this round's channel readings into the ledger.
///
/// - C: idempotent insert (same fetchedAtMs → UNIQUE dedup ignores).
/// - B: incremental absorption above the `desktop_last_t` watermark — the
///   file is a 30-day rolling window, so the FIRST run backfills all of it
///   (D48 第 5 點: 主人第一天就看得到 30 天曲線) and later runs only absorb
///   the new tail. Gaps in the beat become `gaps` rows.
async fn persist(
    ledger: &crate::ledger::Ledger,
    cli: Option<&cli_cache::CliCacheReading>,
    desktop: Option<&desktop_file::DesktopReading>,
    bridge_map: &std::collections::HashMap<String, String>,
    report: &mut serde_json::Value,
) -> Result<()> {
    if let Some(c) = cli {
        let n = ledger.insert_truth_samples(&c.samples).await?;
        report["cli_inserted"] = n.into();
    }

    let Some(d) = desktop else {
        return Ok(());
    };

    let watermark: Option<i64> = ledger
        .get_channel_state("desktop_last_t")
        .await?
        .and_then(|v| v.parse().ok());

    let fresh: Vec<&TruthSample> = d
        .samples
        .iter()
        .filter(|s| match watermark {
            Some(w) => s.fetched_at.timestamp_millis() > w,
            None => true,
        })
        .collect();
    if fresh.is_empty() {
        return Ok(());
    }

    // D52/O11: resolve each sample's account through the bridge mapping so
    // Desktop history books onto its real (account, org) seat. Unknown orgs
    // keep account=None → placeholder seat, exactly as before.
    let owned: Vec<TruthSample> = fresh
        .iter()
        .map(|s| {
            let mut s = (*s).clone();
            if s.account_uuid.is_none() {
                s.account_uuid = s
                    .org_uuid
                    .as_ref()
                    .and_then(|org| bridge_map.get(org))
                    .cloned();
            }
            s
        })
        .collect();
    let n = ledger.insert_truth_samples(&owned).await?;
    report["desktop_inserted"] = n.into();
    report["desktop_backfill"] = watermark.is_none().into();

    // D54 B 同帳號門鈴: fresh B samples on the CLI account's OWN seat are
    // proof of activity (the user is burning the displayed pool through the
    // Desktop app) — ring the doorbell. Under dual accounts the seats
    // differ and this stays silent, which is exactly right.
    if n > 0 && watermark.is_some() {
        let cli_account = cli
            .and_then(|c| c.oauth.as_ref())
            .and_then(|o| o.account_uuid.as_deref());
        let b_account = owned.last().and_then(|s| s.account_uuid.as_deref());
        if let (Some(a), Some(b)) = (cli_account, b_account) {
            if a == b {
                ledger
                    .set_channel_state("last_activity_at", &chrono::Utc::now().to_rfc3339())
                    .await?;
            }
        }
    }

    // Gap detection over the distinct timestamps of the newly absorbed
    // stretch, seeded with the previous watermark so a hole across restarts
    // is still seen.
    let mut ts: Vec<i64> = fresh.iter().map(|s| s.fetched_at.timestamp_millis()).collect();
    ts.dedup();
    let mut prev = watermark;
    let mut gap_count = 0usize;
    for t in &ts {
        if let Some(p) = prev {
            if t - p > DESKTOP_GAP_THRESHOLD_MS {
                let start = model::from_epoch_ms(p).map(|x| x.to_rfc3339());
                let end = model::from_epoch_ms(*t).map(|x| x.to_rfc3339());
                if let (Some(start), Some(end)) = (start, end) {
                    ledger
                        .insert_gap(
                            "desktop-history",
                            &start,
                            &end,
                            "Desktop 取樣中斷（App 未開或機器休眠）",
                        )
                        .await?;
                    gap_count += 1;
                }
            }
        }
        prev = Some(*t);
    }
    if gap_count > 0 {
        report["desktop_gaps"] = gap_count.into();
    }

    if let Some(max_t) = ts.last() {
        ledger
            .set_channel_state("desktop_last_t", &max_t.to_string())
            .await?;
    }
    Ok(())
}

/// Synonym sets for the view-model merge. Ingestion keeps source-verbatim
/// kinds (D46); only the display layer folds them together.
fn is_session_kind(k: &str) -> bool {
    k == "session" || k == "five_hour"
}
fn is_weekly_all_kind(k: &str) -> bool {
    k == "weekly_all" || k == "seven_day"
}

/// Pick the freshest sample for one limit. Candidates are CLI-account
/// sources only (D51) — a fresh probe beats the C cache, that's all the
/// tie-breaking this needs now.
fn freshest(
    candidates: &[TruthSample],
    pred: impl Fn(&TruthSample) -> bool,
) -> Option<&TruthSample> {
    candidates
        .iter()
        .filter(|s| pred(s))
        .max_by_key(|s| s.fetched_at)
}

/// B is considered "actively writing" when its newest sample is at most this
/// old — the different-account reminder only shows for a live Desktop.
const DESKTOP_ACTIVE_SECS: i64 = 30 * 60;

/// D52: B's two auxiliary roles, keyed on whether Desktop's account (resolved
/// through the bridge mapping) matches the CLI's.
///   Same account  → cross-validation: hard divergence in a tight co-window
///                   means one side's cache is lying — warn.
///   Other account → reminder: its usage is being recorded separately and the
///                   display follows the CLI account.
fn detect_account_notice(
    candidates: &[TruthSample],
    desktop: Option<&desktop_file::DesktopReading>,
    bridge_map: &std::collections::HashMap<String, String>,
    cli_account: Option<&str>,
    now: chrono::DateTime<Utc>,
) -> Option<String> {
    let d = desktop?;
    let latest_b = d.samples.iter().max_by_key(|s| s.fetched_at)?;
    let b_active = (now - latest_b.fetched_at).num_seconds() <= DESKTOP_ACTIVE_SECS;
    let b_org = latest_b.org_uuid.as_deref();
    let b_account = b_org.and_then(|org| bridge_map.get(org)).map(String::as_str);

    let same_account = match (b_account, cli_account) {
        (Some(b), Some(c)) => b == c,
        _ => false, // unmapped org / unknown CLI account → treat as different
    };

    if !same_account {
        if b_active {
            return Some(format!(
                "Desktop 登入的是另一個帳號（org {}…），其用量已另行記錄；顯示數字以 CLI 帳號為準。",
                b_org.map(|o| &o[..8.min(o.len())]).unwrap_or("未知")
            ));
        }
        return None;
    }

    // Same account: cross-validate the two channels (D52 互相驗證).
    type KindPred<'a> = &'a dyn Fn(&TruthSample) -> bool;
    let pairs: [(KindPred, &str, &str); 2] = [
        (&|s: &TruthSample| is_session_kind(&s.limit_kind), "five_hour", "5h"),
        (
            &|s: &TruthSample| is_weekly_all_kind(&s.limit_kind),
            "seven_day",
            "7d",
        ),
    ];
    for (pred, b_kind, label) in pairs {
        let Some(ac) = freshest(candidates, pred) else {
            continue;
        };
        let Some(b) = d
            .samples
            .iter()
            .filter(|s| s.limit_kind == b_kind)
            .max_by_key(|s| s.fetched_at)
        else {
            continue;
        };
        let co_window =
            (ac.fetched_at - b.fetched_at).num_seconds().abs() <= CONFLICT_CO_WINDOW_SECS;
        let diverges = (ac.percent - b.percent).abs() > CONFLICT_THRESHOLD_PCT;
        if co_window && diverges {
            return Some(format!(
                "同帳號下 Desktop 與 CLI 的 {label} 數據不一致（相差 {:.0} 個百分點）——\
                 可能有一邊的快取過期，顯示以 CLI 為準。",
                (ac.percent - b.percent).abs()
            ));
        }
    }
    None
}

fn to_item(sample: Option<&TruthSample>, now: DateTime<Utc>) -> UsageItem {
    match sample {
        Some(s) => UsageItem {
            used_percent: Some(s.percent),
            reset_at: s.resets_at.map(|t| t.to_rfc3339()),
            note: Some(format!(
                "{} · {}",
                s.source.label(),
                age_label(s.fetched_at, now)
            )),
        },
        None => UsageItem::default(),
    }
}

fn build_snapshot(
    candidates: &[TruthSample],
    cli: Option<&cli_cache::CliCacheReading>,
    now: DateTime<Utc>,
) -> UsageSnapshot {
    let session = freshest(candidates, |s| is_session_kind(&s.limit_kind));
    let weekly_all = freshest(candidates, |s| is_weekly_all_kind(&s.limit_kind));
    // Scoped weekly limits only ever come from channel A/C. Today the only
    // scope observed is "Fable"; other scopes would surface once M3's
    // dashboard reads truth_samples directly.
    let fable = freshest(candidates, |s| {
        s.limit_kind == "weekly_scoped"
            && s.scope.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("fable"))
    });

    UsageSnapshot {
        plan_name: cli.and_then(|c| c.oauth.as_ref()).map(plan_label),
        current_session: to_item(session, now),
        weekly_all_models: to_item(weekly_all, now),
        weekly_fable: to_item(fable, now),
        scraped_at: now.to_rfc3339(),
    }
}

/// "default_claude_max_20x" → "Max 20x"-style human label, falling back to
/// organizationType / organizationName.
fn plan_label(oauth: &cli_cache::OauthAccount) -> String {
    if let Some(tier) = oauth
        .organization_rate_limit_tier
        .as_deref()
        .filter(|t| !t.is_empty())
    {
        let trimmed = tier.trim_start_matches("default_");
        let pretty = trimmed
            .split('_')
            .map(|w| {
                let mut chars = w.chars();
                match chars.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        return pretty; // e.g. "Claude Max 20x"
    }
    if let Some(t) = oauth.organization_type.as_deref().filter(|t| !t.is_empty()) {
        return match t {
            "claude_max" => "Claude Max".to_string(),
            "claude_pro" => "Claude Pro".to_string(),
            other => other.to_string(),
        };
    }
    oauth
        .organization_name
        .clone()
        .unwrap_or_else(|| "Claude".to_string())
}
