//! v1.9（D94）：給 Claude Code 的用量 MCP。
//!
//! 掛在門鈴同一個本機伺服器（`127.0.0.1:<webhook.port>/mcp`），Streamable HTTP、無狀態、
//! 簡單 JSON 回應。工具直接呼叫帳本既有的函式（今日頁／懸浮窗同一套）——**不另寫一份**
//! 「哪些來源算即時」「重置怎麼切」（D90 的漂移蟲就是兩條路徑各寫一份）。
//!
//! 裁決的文字（「會提早見底」「近期偏快」）在前端 `Today.tsx::verdictOf`；這裡只轉交後端的
//! 事實旗標（`etaAt` 只在「兩窗都超速、且觸頂早於重置」時才有值），不在 Rust 另寫一份裁決。
//!
//! 守門（MCP 規格 Streamable HTTP〈Security Warning〉，見 docs/research/2026-10-01-mcp-transport-facts.md）：
//! 只綁 127.0.0.1（server.rs）、Host 只收 loopback（rmcp 預設）、帶 Origin 一律 403
//! （Claude Code 不是瀏覽器；瀏覽器頁面帶 Origin＝DNS rebinding 的路）、Bearer＝門鈴同一張密碼牌。

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use chrono::{DateTime, Local, Utc};
use rmcp::{
    handler::server::wrapper::Parameters,
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::never::NeverSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub const MCP_PATH: &str = "/mcp";

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// MCP 需要的資料來源。正式版是 [`AppSource`]（AppHandle）；測試用假的，協定層才測得到。
pub trait UsageSource: Send + Sync + 'static {
    /// 目前的門鈴密碼牌（每次請求現讀——設定頁換牌後立刻生效）。
    fn token(&self) -> BoxFut<'_, String>;
    /// `Ledger::list_seats` 的原樣輸出。
    fn list_seats(&self) -> BoxFut<'_, anyhow::Result<Value>>;
    /// `Ledger::today_outlook(seat)` 的原樣輸出。
    fn outlook(&self, seat_id: String) -> BoxFut<'_, anyhow::Result<Value>>;
    /// 懸浮窗那行提醒（例如「CLI 0%、Desktop 有數字 → /login」）；只對目前登入的帳號有意義。
    fn notice(&self) -> BoxFut<'_, Option<String>>;
    fn log(&self, phase: &'static str, data: Value) -> BoxFut<'_, ()>;
    /// v1.10（D101）：`Ledger::budget_lines(seat)` 的原樣輸出；預算關掉回 None。
    fn budget(&self, _seat_id: String) -> BoxFut<'_, anyhow::Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }
}

pub struct AppSource {
    pub app: tauri::AppHandle,
}

impl UsageSource for AppSource {
    fn token(&self) -> BoxFut<'_, String> {
        Box::pin(async move {
            use tauri::Manager;
            let state: tauri::State<crate::state::AppState> = self.app.state();
            state.get_settings().await.webhook.token
        })
    }
    fn list_seats(&self) -> BoxFut<'_, anyhow::Result<Value>> {
        Box::pin(async move {
            use tauri::Manager;
            let ledger: tauri::State<crate::ledger::Ledger> = self.app.state();
            ledger.list_seats().await
        })
    }
    fn outlook(&self, seat_id: String) -> BoxFut<'_, anyhow::Result<Value>> {
        Box::pin(async move {
            use tauri::Manager;
            let ledger: tauri::State<crate::ledger::Ledger> = self.app.state();
            ledger.today_outlook(Some(&seat_id)).await
        })
    }
    fn notice(&self) -> BoxFut<'_, Option<String>> {
        Box::pin(async move {
            use tauri::Manager;
            let state: tauri::State<crate::state::AppState> = self.app.state();
            state.get_usage().await.notice
        })
    }
    fn log(&self, phase: &'static str, data: Value) -> BoxFut<'_, ()> {
        Box::pin(async move { crate::applog::write_log(&self.app, phase, data).await })
    }
    fn budget(&self, seat_id: String) -> BoxFut<'_, anyhow::Result<Option<Value>>> {
        Box::pin(async move {
            use tauri::Manager;
            let state: tauri::State<crate::state::AppState> = self.app.state();
            let budget = state.get_settings().await.budget;
            if !budget.enabled {
                return Ok(None);
            }
            let ledger: tauri::State<crate::ledger::Ledger> = self.app.state();
            ledger.budget_lines(Some(&seat_id), budget.day_start_hour).await.map(Some)
        })
    }
}

/// `/mcp` 的子路由：Bearer 守門 → rmcp 的 Streamable HTTP 服務。
pub fn router(source: Arc<dyn UsageSource>) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .enforce_origin_validation();
    let factory_source = source.clone();
    let service = StreamableHttpService::new(
        move || Ok(UsageMcp::new(factory_source.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .route_service(MCP_PATH, service)
        .layer(middleware::from_fn_with_state(source, require_bearer))
}

async fn require_bearer(
    State(source): State<Arc<dyn UsageSource>>,
    req: Request,
    next: Next,
) -> Response {
    let expected = source.token().await;
    let provided = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if expected.is_empty() || provided != Some(expected.as_str()) {
        source
            .log("mcp", json!({ "ok": false, "reason": "unauthorized", "token_provided": provided.is_some() }))
            .await;
        return (StatusCode::UNAUTHORIZED, "unauthorized — missing or invalid Bearer token").into_response();
    }
    next.run(req).await
}

// =============================================================================
// 工具
// =============================================================================

#[derive(Clone)]
pub struct UsageMcp {
    source: Arc<dyn UsageSource>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct GetUsageParams {
    /// Which account to look at: a seat id, or part of the account email / organization name
    /// (from list_accounts). Omit for the account Claude Code is logged into right now.
    #[serde(default)]
    pub account: Option<String>,
}

#[tool_router]
impl UsageMcp {
    pub fn new(source: Arc<dyn UsageSource>) -> Self {
        Self { source }
    }

    #[tool(
        name = "get_usage",
        // 長度要壓在 2048 字元內：Claude Code 2.1.295 之前的版本會把 MCP 工具說明截在 2048，後面的整段看不到。
        description = "Remaining Claude subscription quota, from the Claude Usage Monitor app on this machine. \
limits has fiveHour (5-hour window), weekly (7-day, all models) and weeklyFable (7-day, Fable), each with usedPercent, \
resetsAt, burnPercentPerHour and observedMinutesAgo (the app re-checks on its own schedule; old numbers are a lower bound). \
Read status first: on_track = lasts until reset at this pace; will_hit_limit_before_reset = 100% expected at hitsLimitAt; \
observing = too few samples since a reset or plan change to predict; at_limit = 100% used until resetsAt; \
plan_used_extra_usage_active = plan quota used up, extra usage (paid overage, see extraUsage) is on; \
stale = last sample too old to predict, usedPercent is that old value; reset_since_last_sample = the window reset after \
the last sample, so usedPercent is null and lastKnownPercent holds the pre-reset value (now probably low); \
plan_changed_awaiting_sample = plan just changed, no sample yet, usedPercent null; no_data = never seen; \
not_applicable = this plan has no such limit (not a reset; all fields null). hitsLimitAt is a prediction only under \
will_hit_limit_before_reset; null means \"lasts until reset\" only under on_track. \
notice: a short warning (Traditional Chinese) about the current account, e.g. Claude Code and Desktop on different \
accounts, quota checks failing so numbers are frozen, or the limit list changed; null when fine or for other accounts. \
Relay it before trusting the numbers. unknownLimits: limits the server reports that this app does not track (check claude.ai). \
account.plan: the account's plan (a Max 20x pool is 4x Max 5x, so equal percents differ in amount); plan.known=false = \
not observed yet. With the daily budget on, weekly and weeklyFable carry todayLine: quota left at the start of today spread \
evenly to reset; linePercent = how far usage may go today, leftTodayPercent = today's share left (negative = over), \
the day runs dayStartsAt to dayEndsAt. Times are local with offset.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_usage(&self, Parameters(p): Parameters<GetUsageParams>) -> Result<String, String> {
        let seats = self.source.list_seats().await.map_err(|e| format!("ledger: {e:#}"))?;
        let Some(seat) = pick_seat(&seats, p.account.as_deref()) else {
            return Err(match p.account {
                Some(q) => format!("no account matches {q:?} — call list_accounts to see the accounts this app has seen"),
                None => "the app has not observed any account yet (is Claude Code logged in?)".into(),
            });
        };
        let seat_id = seat["id"].as_str().unwrap_or_default().to_string();
        let outlook = self.source.outlook(seat_id.clone()).await.map_err(|e| format!("ledger: {e:#}"))?;
        let is_current = seats["currentSeatId"].as_str() == Some(seat_id.as_str());
        let notice = if is_current { self.source.notice().await } else { None };
        let mut out = usage_payload(&seat, is_current, &outlook, notice, Utc::now());
        // v1.10（D101 決策點 10）：7d／Fable 今天的線（預算關掉就不帶這個鍵）。
        if let Some(b) = self.source.budget(seat_id.clone()).await.map_err(|e| format!("ledger: {e:#}"))? {
            out["limits"]["weekly"]["todayLine"] = today_line_payload(&b["weekly"]);
            out["limits"]["weeklyFable"]["todayLine"] = today_line_payload(&b["fable"]);
        }
        self.source.log("mcp", json!({ "ok": true, "tool": "get_usage", "seat": seat_id })).await;
        Ok(out.to_string())
    }

    #[tool(
        name = "list_accounts",
        description = "Accounts (account × organization pairs) the Claude Usage Monitor app has observed on this machine, \
which one Claude Code is logged into now, and when each was last sampled. Pass an id or email to get_usage to see another account's last known quota.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_accounts(&self) -> Result<String, String> {
        let seats = self.source.list_seats().await.map_err(|e| format!("ledger: {e:#}"))?;
        let now = Utc::now();
        let current = seats["currentSeatId"].as_str();
        let list: Vec<Value> = seats["seats"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|s| {
                json!({
                    "id": s["id"],
                    "email": s["email"],
                    "organization": s["orgName"],
                    "plan": plan_payload(&s["plan"]),
                    "isCurrent": s["id"].as_str().is_some() && s["id"].as_str() == current,
                    "lastSampleAt": local_time(s["lastSampleAt"].as_str()),
                    "lastSampleMinutesAgo": minutes_since(s["lastSampleAt"].as_str(), now),
                })
            })
            .collect();
        self.source.log("mcp", json!({ "ok": true, "tool": "list_accounts" })).await;
        Ok(json!({ "accounts": list }).to_string())
    }
}

#[tool_handler(
    name = "claude-usage-monitor",
    instructions = "Read-only view of the user's Claude subscription quota (5-hour / 7-day / Fable limits), as recorded by the Claude Usage Monitor desktop app."
)]
impl ServerHandler for UsageMcp {}

// =============================================================================
// 純函式（測試直接打）
// =============================================================================

/// `account` 沒給 → 目前登入的座位（沒有就取樣本最多的，與 `resolve_seat_sync` 同序）。
/// 有給 → 先比 seat id，再比 email／組織名（不分大小寫、部分符合）；多個符合取第一個。
fn pick_seat(seats: &Value, account: Option<&str>) -> Option<Value> {
    let list = seats["seats"].as_array()?;
    match account.map(str::trim).filter(|q| !q.is_empty()) {
        None => {
            let current = seats["currentSeatId"].as_str();
            list.iter()
                .find(|s| current.is_some() && s["id"].as_str() == current)
                .or_else(|| list.first())
                .cloned()
        }
        Some(q) => {
            let q = q.to_lowercase();
            list.iter()
                .find(|s| s["id"].as_str().is_some_and(|id| id.to_lowercase() == q))
                .or_else(|| {
                    list.iter().find(|s| {
                        ["email", "orgName"].iter().any(|k| {
                            s[*k].as_str().is_some_and(|v| v.to_lowercase().contains(&q))
                        })
                    })
                })
                .cloned()
        }
    }
}

fn parse_utc(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc))
}

fn local_time(s: Option<&str>) -> Value {
    match parse_utc(s) {
        Some(t) => json!(t.with_timezone(&Local).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)),
        None => Value::Null,
    }
}

fn minutes_since(s: Option<&str>, now: DateTime<Utc>) -> Value {
    parse_utc(s).map(|t| json!((now - t).num_minutes())).unwrap_or(Value::Null)
}

fn minutes_until(s: Option<&str>, now: DateTime<Utc>) -> Value {
    parse_utc(s).map(|t| json!((t - now).num_minutes())).unwrap_or(Value::Null)
}

/// 一條額度：把 `limit_outlook_sync` 的欄位換成自己會說話的名字。狀態只照後端旗標的優先序
/// （與 `Today.tsx::verdictOf` 同序：已重置 → 無資料 → 已滿 → 觀察中 → 會提早見底），不另做判斷。
/// v1.9.5（D100 E2／F1）：最優先是 `not_applicable`（伺服器的額度清單連續 3 次沒有這條＝此方案沒有它，
/// 不是「重置過了」）；滿格時開了超額付費（`extra_active`）改 `plan_used_extra_usage_active`。
fn limit_payload(o: &Value, extra_active: bool, now: DateTime<Utc>) -> Value {
    if o["notApplicable"].as_bool().unwrap_or(false) {
        // 32r N5：鍵跟其他狀態一模一樣（值全空），只多一句 note——靠「鍵在不在」判斷的 consumer 不會踩空。
        let mut p = limit_payload(&json!({ "expired": false }), false, now);
        if let Some(m) = p.as_object_mut() {
            for v in m.values_mut() {
                *v = Value::Null;
            }
        }
        p["status"] = json!("not_applicable");
        p["resetTimeIsUpperBound"] = json!(false);
        p["note"] = json!(
            "This plan has no such limit (or it has been turned off): the server's limit list has left it out \
several times in a row. This is not a reset — there is nothing to track."
        );
        return p;
    }
    let expired = o["expired"].as_bool().unwrap_or(false);
    let percent = o["percent"].as_f64();
    let eta = o["etaAt"].as_str();
    // v1.9.4（D99 D2）：後端說資料太舊（最後一筆樣本比短窗還老、或預測的見底時刻早已過去）——
    // 不給見底時刻（etaAt 已是 null），狀態是 stale；絕不出現負的分鐘數。
    let stale = o["stale"].as_bool().unwrap_or(false);
    let status = if expired {
        "reset_since_last_sample"
    } else if o["awaitingAfterPlanChange"].as_bool().unwrap_or(false) {
        // 剛換方案、新方案還沒有第一筆樣本：不是已重置，也不是沒資料
        "plan_changed_awaiting_sample"
    } else if percent.is_none() {
        "no_data"
    } else if percent.is_some_and(|p| p >= 100.0) {
        // v1.9.5（D100 F1）：開了超額付費、還沒花到上限——方案額度用完，但不一定被擋（U5 沒驗，只換說法）
        if extra_active {
            "plan_used_extra_usage_active"
        } else {
            "at_limit"
        }
    } else if stale {
        "stale"
    } else if o["insufficient"].as_bool().unwrap_or(false) {
        "observing"
    } else if eta.is_some() {
        "will_hit_limit_before_reset"
    } else {
        "on_track"
    };
    let resets = o["resetsAt"].as_str().or_else(|| o["resetsAtLatest"].as_str());
    let used = if expired { None } else { percent };
    json!({
        "status": status,
        "usedPercent": used,
        "remainingPercent": used.map(|p| (100.0 - p).max(0.0)),
        "lastKnownPercent": if expired { o["lastKnownPercent"].clone() } else { Value::Null },
        "resetsAt": local_time(resets),
        "resetsInMinutes": minutes_until(resets, now),
        "resetTimeIsUpperBound": o["resetsAt"].is_null() && !o["resetsAtLatest"].is_null(),
        "observedAt": local_time(o["fetchedAt"].as_str()),
        "observedMinutesAgo": minutes_since(o["fetchedAt"].as_str(), now),
        "burnPercentPerHour": o["burnPerHour"],
        "burnWindowMinutes": o["shortWindowMinutes"],
        "projectedPercentAtReset": o["projectedAtReset"],
        "hitsLimitAt": local_time(eta),
        // 夾在 0 以上：就算 ETA 在讀取到現在之間過去了，也不回負數（後端已把過去的 ETA 清掉，這是第二道）
        "hitsLimitInMinutes": minutes_until(eta, now).as_i64().map(|m| json!(m.max(0))).unwrap_or(Value::Null),
        "hitsLimitAtIfRecentPaceContinues": local_time(o["etaShort"].as_str()),
        // 換方案後切掉了舊池的樣本（剛換方案，重新觀察）：{at, fromLabel, toLabel}
        "planChange": o["planChange"].clone(),
    })
}

/// v1.10（D101）：今天的線——剩下的額度平均分到重置，今天最多用到幾 %。
fn today_line_payload(l: &Value) -> Value {
    let line = l["line"].as_f64();
    let used = l["percent"].as_f64();
    let status = match l["status"].as_str() {
        Some("ok") => "ok",
        Some("lastDay") => "last_day_use_the_rest",
        Some("noReset") => "unknown_reset_time",
        _ => "unavailable",
    };
    if status == "unavailable" {
        // 審查 #6：算不出線時不帶看似有意義的起訖時刻。
        return json!({ "status": status });
    }
    json!({
        "status": status,
        // dayStart＝起點就是一天開始時的值；windowStart＝今天剛重置、從 0 起算；
        // planChange＝今天換方案、從換方案後第一筆起算；firstAfter＝App 當時沒開、用今天第一筆。
        "startedFrom": match l["anchorKind"].as_str() {
            Some("dayStart") => json!("day_start"),
            Some("windowStart") => json!("reset_today"),
            Some("planChange") => json!("plan_changed_today"),
            Some("firstAfter") => json!("first_sample_today"),
            _ => Value::Null,
        },
        "startedAt": local_time(l["anchorAt"].as_str()),
        "linePercent": line,
        "over": l["over"],
        "leftTodayPercent": match (line, used) { (Some(a), Some(b)) => json!(((a - b) * 10.0).round() / 10.0), _ => Value::Null },
        "dayStartsAt": local_time(l["dayStart"].as_str()),
        "dayEndsAt": local_time(l["dayEnd"].as_str()),
        "startedAtPercent": l["anchorPercent"],
    })
}

/// 方案（等級字串＋自何時起）。`known=false`＝這個帳號的方案從沒被記過（舊資料「方案未知」）。
/// `sinceObserved` 是 App 偵測到這個方案的時刻（不是訂閱起日——不回溯）。
fn plan_payload(plan: &Value) -> Value {
    json!({
        "known": plan["known"].as_bool().unwrap_or(false),
        "name": plan["label"],
        "kind": plan["key"],
        "tier": plan["tier"],
        "organizationType": plan["orgType"],
        "seatTier": plan["seatTier"],
        "sinceObserved": local_time(plan["since"].as_str()),
        "lastReadAt": local_time(plan["observedAt"].as_str()),
        "previousName": plan["previous"]["label"],
    })
}

fn usage_payload(seat: &Value, is_current: bool, outlook: &Value, notice: Option<String>, now: DateTime<Utc>) -> Value {
    // v1.9.5（D100 F1）：跟今日頁同一份（帳本只對目前登入的座位給 extraUsage）
    let extra = outlook.get("extraUsage").cloned().unwrap_or(Value::Null);
    let extra_active = extra["active"].as_bool().unwrap_or(false);
    // v1.9.5（D100 E1）：伺服器有、App 還不會畫的額度；時刻換成本地時間，跟其他欄位一致
    let unknown: Vec<Value> = outlook["unknownLimits"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|u| {
            json!({ "kind": u["kind"], "scope": u["scope"], "percent": u["percent"], "resetsAt": local_time(u["resetsAt"].as_str()) })
        })
        .collect();
    json!({
        "account": {
            "id": seat["id"],
            "email": seat["email"],
            "organization": seat["orgName"],
            "isCurrent": is_current,
            // 池的大小跟方案有關（Max 20x 的額度是 Max 5x 的 4 倍）——agent 要靠這個判斷百分比的分量
            "plan": plan_payload(&seat["plan"]),
        },
        "now": now.with_timezone(&Local).to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "notice": notice,
        "limits": {
            "fiveHour": limit_payload(&outlook["session"], extra_active, now),
            "weekly": limit_payload(&outlook["weekly"], extra_active, now),
            "weeklyFable": limit_payload(&outlook["fable"], extra_active, now),
        },
        // {enabled, active, spendLimitReached, disabledReason}；不知道（別的帳號、沒有 CLI 快取）是 null
        "extraUsage": extra,
        "unknownLimits": unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    const TOKEN: &str = "test-token";

    struct Fake {
        seats: Value,
        outlook: Value,
    }

    impl UsageSource for Fake {
        fn token(&self) -> BoxFut<'_, String> {
            Box::pin(async { TOKEN.to_string() })
        }
        fn list_seats(&self) -> BoxFut<'_, anyhow::Result<Value>> {
            Box::pin(async { Ok(self.seats.clone()) })
        }
        fn outlook(&self, _seat_id: String) -> BoxFut<'_, anyhow::Result<Value>> {
            Box::pin(async { Ok(self.outlook.clone()) })
        }
        fn notice(&self) -> BoxFut<'_, Option<String>> {
            Box::pin(async { Some("提醒".to_string()) })
        }
        fn log(&self, _phase: &'static str, _data: Value) -> BoxFut<'_, ()> {
            Box::pin(async {})
        }
    }

    fn fake() -> Arc<dyn UsageSource> {
        let now = Utc::now();
        let iso = |m: i64| (now + chrono::Duration::minutes(m)).to_rfc3339();
        Arc::new(Fake {
            seats: json!({
                "currentSeatId": "seat-b",
                "seats": [
                    { "id": "seat-a", "email": "first@example.com", "orgName": "First Org", "lastSampleAt": iso(-600) },
                    { "id": "seat-b", "email": "second@example.com", "orgName": "Second Org", "lastSampleAt": iso(-3) },
                ],
            }),
            outlook: json!({
                "session": { "percent": 62.0, "fetchedAt": iso(-3), "resetsAt": iso(90), "burnPerHour": 30.0,
                             "shortWindowMinutes": 60.0, "etaAt": iso(76), "etaShort": iso(76), "insufficient": false, "expired": false },
                "weekly": { "percent": 41.0, "fetchedAt": iso(-3), "resetsAt": iso(3000), "burnPerHour": 0.4,
                            "shortWindowMinutes": 1440.0, "projectedAtReset": 61.0, "etaAt": null, "insufficient": false, "expired": false },
                "fable": { "percent": null, "fetchedAt": iso(-5000), "resetsAt": iso(-200), "lastKnownPercent": 88.0, "expired": true },
            }),
        })
    }

    fn post(body: Value, token: Option<&str>, origin: Option<&str>, host: &str) -> Request {
        let mut b = axum::http::Request::post(MCP_PATH)
            .header("host", host)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    async fn call(req: Request) -> (StatusCode, String) {
        let res = router(fake()).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// 回應可能是 JSON，也可能是 SSE（`data: {...}`）——兩種都解。
    fn rpc_result(body: &str) -> Value {
        let text = body
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .unwrap_or(body)
            .trim();
        serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON-RPC: {e}: {body}"))
    }

    fn initialize() -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" } } })
    }

    #[tokio::test]
    async fn initialize_and_list_tools() {
        let (s, body) = call(post(initialize(), Some(TOKEN), None, "127.0.0.1:17819")).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let v = rpc_result(&body);
        // serverInfo.name 寫在 #[tool_handler] 的字面值裡（巨集只吃字面值）
        assert_eq!(v["result"]["serverInfo"]["name"], "claude-usage-monitor");
        assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");

        let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
        let mut req = post(list, Some(TOKEN), None, "127.0.0.1:17819");
        req.headers_mut().insert("mcp-protocol-version", "2025-06-18".parse().unwrap());
        let (s, body) = call(req).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let tools = rpc_result(&body)["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<String> = tools.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert!(names.contains(&"get_usage".to_string()) && names.contains(&"list_accounts".to_string()), "{names:?}");
        for t in &tools {
            // Claude Code 2.1.295 之前會把工具說明截在 2048（UTF-16 單位），超過的部分模型看不到。
            let d = t["description"].as_str().unwrap();
            assert!(d.encode_utf16().count() <= 2048, "{} 的說明 {} 字元，會被截掉", t["name"], d.encode_utf16().count());
        }
        // 說明要把 limit_payload 會產出的每種 status 都講到——別的專案的模型是照這段說明讀額度的。
        let get_usage = tools.iter().find(|t| t["name"] == "get_usage").unwrap()["description"].as_str().unwrap();
        for s in [
            "on_track", "will_hit_limit_before_reset", "observing", "at_limit", "plan_used_extra_usage_active",
            "stale", "reset_since_last_sample", "plan_changed_awaiting_sample", "no_data", "not_applicable",
        ] {
            assert!(get_usage.contains(s), "get_usage 的說明沒講到 {s}");
        }
    }

    #[tokio::test]
    async fn get_usage_returns_current_seat_with_freshness() {
        let req_body = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "get_usage", "arguments": {} } });
        let mut req = post(req_body, Some(TOKEN), None, "127.0.0.1:17819");
        req.headers_mut().insert("mcp-protocol-version", "2025-06-18".parse().unwrap());
        let (s, body) = call(req).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let v = rpc_result(&body);
        assert_ne!(v["result"]["isError"], true, "{v}");
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        let u: Value = serde_json::from_str(text).unwrap();
        assert_eq!(u["account"]["id"], "seat-b");
        assert_eq!(u["account"]["isCurrent"], true);
        assert_eq!(u["notice"], "提醒");
        let five = &u["limits"]["fiveHour"];
        assert_eq!(five["status"], "will_hit_limit_before_reset");
        assert_eq!(five["usedPercent"], 62.0);
        assert_eq!(five["remainingPercent"], 38.0);
        assert_eq!(five["observedMinutesAgo"], 3);
        assert!(five["hitsLimitInMinutes"].as_i64().is_some_and(|m| (74..=76).contains(&m)), "{five}");
        assert_eq!(u["limits"]["weekly"]["status"], "on_track");
        assert_eq!(u["limits"]["weekly"]["hitsLimitAt"], Value::Null);
        let fable = &u["limits"]["weeklyFable"];
        assert_eq!(fable["status"], "reset_since_last_sample");
        assert_eq!(fable["usedPercent"], Value::Null);
        assert_eq!(fable["lastKnownPercent"], 88.0);
    }

    #[tokio::test]
    async fn rejects_missing_or_wrong_token() {
        assert_eq!(call(post(initialize(), None, None, "127.0.0.1:17819")).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(post(initialize(), Some("nope"), None, "127.0.0.1:17819")).await.0, StatusCode::UNAUTHORIZED);
    }

    /// 規格〈Security Warning〉：瀏覽器頁面（帶 Origin）與 DNS rebinding（Host 不是 loopback）一律擋——
    /// 就算密碼牌對也一樣。
    #[tokio::test]
    async fn rejects_browser_origin_and_foreign_host() {
        let (s, _) = call(post(initialize(), Some(TOKEN), Some("https://evil.example"), "127.0.0.1:17819")).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(post(initialize(), Some(TOKEN), None, "evil.example:17819")).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(post(initialize(), Some(TOKEN), None, "localhost:17819")).await;
        assert_eq!(s, StatusCode::OK);
    }

    /// 真帳本 probe（不是單元測試）：`CUM_LEDGER_DIR=<帳本複本> cargo test --lib mcp::tests::probe_real_ledger -- --nocapture`
    /// 印出 get_usage／list_accounts 對真資料會回什麼。
    #[tokio::test]
    async fn probe_real_ledger() {
        let Ok(dir) = std::env::var("CUM_LEDGER_DIR") else { return };
        let ledger = crate::ledger::Ledger::open(dir.into()).unwrap();
        let seats = ledger.list_seats().await.unwrap();
        for s in seats["seats"].as_array().unwrap() {
            let id = s["id"].as_str().unwrap().to_string();
            let outlook = ledger.today_outlook(Some(&id)).await.unwrap();
            let is_current = seats["currentSeatId"].as_str() == Some(id.as_str());
            let out = usage_payload(s, is_current, &outlook, None, Utc::now());
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            println!("bytes={}", out.to_string().len());
        }
    }

    /// 端到端（手動）：拿帳本複本在別的 port 起 `/mcp`，給真的 Claude Code 連——不碰安裝版。
    /// `CUM_LEDGER_DIR=<複本> CUM_MCP_PORT=17899 CUM_MCP_SECS=300 cargo test --lib mcp::tests::serve_real_ledger -- --ignored --nocapture`
    /// 密碼牌固定是 `test-token`。
    #[tokio::test]
    #[ignore]
    async fn serve_real_ledger() {
        struct LedgerSource(crate::ledger::Ledger);
        impl UsageSource for LedgerSource {
            fn token(&self) -> BoxFut<'_, String> {
                Box::pin(async { TOKEN.to_string() })
            }
            fn list_seats(&self) -> BoxFut<'_, anyhow::Result<Value>> {
                Box::pin(self.0.list_seats())
            }
            fn outlook(&self, seat_id: String) -> BoxFut<'_, anyhow::Result<Value>> {
                Box::pin(async move { self.0.today_outlook(Some(&seat_id)).await })
            }
            fn notice(&self) -> BoxFut<'_, Option<String>> {
                Box::pin(async { None })
            }
            fn log(&self, phase: &'static str, data: Value) -> BoxFut<'_, ()> {
                Box::pin(async move { println!("[{phase}] {data}") })
            }
        }
        let dir = std::env::var("CUM_LEDGER_DIR").expect("CUM_LEDGER_DIR");
        let port: u16 = std::env::var("CUM_MCP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(17899);
        let secs: u64 = std::env::var("CUM_MCP_SECS").ok().and_then(|p| p.parse().ok()).unwrap_or(300);
        let source: Arc<dyn UsageSource> = Arc::new(LedgerSource(crate::ledger::Ledger::open(dir.into()).unwrap()));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        println!("serving http://127.0.0.1:{port}{MCP_PATH} for {secs}s");
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(secs),
            axum::serve(listener, router(source)),
        )
        .await;
    }

    /// D2：資料太舊就是 stale，不給見底時刻；就算 ETA 已經過去，分鐘數也不會是負的。
    #[test]
    fn stale_outlook_never_reports_a_past_hit_time() {
        let now = Utc::now();
        let iso = |m: i64| (now + chrono::Duration::minutes(m)).to_rfc3339();
        let stale = json!({ "percent": 62.0, "fetchedAt": iso(-600), "resetsAt": iso(30), "etaAt": null,
                            "etaShort": null, "stale": true, "insufficient": false, "expired": false });
        let p = limit_payload(&stale, false, now);
        assert_eq!(p["status"], "stale");
        assert_eq!(p["hitsLimitAt"], Value::Null);
        assert_eq!(p["hitsLimitInMinutes"], Value::Null);
        assert_eq!(p["observedMinutesAgo"], 600);
        // 防守第二道：後端萬一漏清，ETA 在過去也夾到 0，不回負數
        let leaked = json!({ "percent": 62.0, "fetchedAt": iso(-60), "resetsAt": iso(30), "etaAt": iso(-30),
                             "insufficient": false, "expired": false });
        assert_eq!(limit_payload(&leaked, false, now)["hitsLimitInMinutes"], 0);
        // 已滿的牆仍然是牆（滿格在重置前不會自己掉）——不被 stale 蓋掉
        let full = json!({ "percent": 100.0, "fetchedAt": iso(-600), "stale": true, "expired": false });
        assert_eq!(limit_payload(&full, false, now)["status"], "at_limit");
        // 剛換方案、新方案還沒有第一筆樣本
        let awaiting = json!({ "percent": null, "awaitingAfterPlanChange": true, "expired": false });
        assert_eq!(limit_payload(&awaiting, false, now)["status"], "plan_changed_awaiting_sample");
    }

    /// v1.9.5（D100 E1／E2／F1）：三種新說法——此方案沒有這條（蓋過「已重置」）、滿格＋超額付費中、
    /// 不認得的額度清單；主人現況（超額沒開）滿格照舊 at_limit。
    #[test]
    fn not_applicable_extra_usage_and_unknown_limits() {
        let now = Utc::now();
        let iso = |m: i64| (now + chrono::Duration::minutes(m)).to_rfc3339();
        let outlook = |extra: Value| {
            json!({
                "session": { "percent": 100.0, "fetchedAt": iso(-3), "resetsAt": iso(90), "expired": false },
                "weekly": { "percent": 41.0, "fetchedAt": iso(-3), "resetsAt": iso(3000), "expired": false },
                // 以前這裡是 reset_since_last_sample（agent 讀成「週限重置過了」）
                "fable": { "percent": null, "fetchedAt": iso(-5000), "resetsAt": iso(-200), "lastKnownPercent": 88.0,
                           "expired": true, "notApplicable": true },
                "extraUsage": extra,
                "unknownLimits": [{ "kind": "weekly_scoped", "scope": "Opus", "percent": 37.0, "resetsAt": iso(3000) }],
            })
        };
        let seat = json!({ "id": "s" });
        let on = json!({ "enabled": true, "active": true, "spendLimitReached": false, "disabledReason": null });
        let u = usage_payload(&seat, true, &outlook(on.clone()), None, now);
        assert_eq!(u["limits"]["weeklyFable"]["status"], "not_applicable");
        assert_eq!(u["limits"]["weeklyFable"]["usedPercent"], Value::Null);
        assert_eq!(u["limits"]["weeklyFable"]["lastKnownPercent"], Value::Null, "不再把舊值當成「上次」");
        assert!(u["limits"]["weeklyFable"]["note"].as_str().unwrap().contains("not a reset"));
        // N5：NA 的鍵集合＝一般狀態的鍵集合＋note
        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<std::collections::BTreeSet<_>>();
        let mut want = keys(&u["limits"]["weekly"]);
        want.insert("note".into());
        assert_eq!(keys(&u["limits"]["weeklyFable"]), want);
        assert_eq!(u["limits"]["weeklyFable"]["burnPercentPerHour"], Value::Null);
        assert_eq!(u["limits"]["fiveHour"]["status"], "plan_used_extra_usage_active");
        assert_eq!(u["limits"]["weekly"]["status"], "on_track");
        assert_eq!(u["extraUsage"], on);
        assert_eq!(u["unknownLimits"][0]["kind"], "weekly_scoped");
        assert_eq!(u["unknownLimits"][0]["scope"], "Opus");
        assert_eq!(u["unknownLimits"][0]["percent"], 37.0);
        assert!(u["unknownLimits"][0]["resetsAt"].is_string());
        // 主人現況：超額沒開（out_of_credits）→ 滿格照舊 at_limit
        let off = json!({ "enabled": false, "active": false, "spendLimitReached": false, "disabledReason": "out_of_credits" });
        assert_eq!(usage_payload(&seat, true, &outlook(off), None, now)["limits"]["fiveHour"]["status"], "at_limit");
        // 開了但花到上限＝不成立
        let capped = json!({ "enabled": true, "active": false, "spendLimitReached": true });
        assert_eq!(usage_payload(&seat, true, &outlook(capped), None, now)["limits"]["fiveHour"]["status"], "at_limit");
        // 舊帳本／別的座位：沒有這兩個欄位 → extraUsage null、unknownLimits 空陣列
        let bare = usage_payload(&seat, false, &json!({}), None, now);
        assert_eq!(bare["extraUsage"], Value::Null);
        assert_eq!(bare["unknownLimits"], json!([]));
    }

    /// A12：get_usage／list_accounts 帶方案（等級字串＋自何時起），沒觀測過就說 known=false。
    #[test]
    fn plan_rides_along_with_the_account() {
        let now = Utc::now();
        let seat = json!({ "id": "s", "email": "a@x", "orgName": "Home",
            "plan": { "known": true, "label": "Claude Max 20x", "key": "max_20x", "tier": "default_claude_max_20x",
                      "orgType": "claude_max", "seatTier": null, "since": now.to_rfc3339(), "observedAt": now.to_rfc3339(),
                      "previous": { "label": "Claude Max 5x" } } });
        let u = usage_payload(&seat, true, &json!({}), None, now);
        assert_eq!(u["account"]["plan"]["name"], "Claude Max 20x");
        assert_eq!(u["account"]["plan"]["kind"], "max_20x");
        assert_eq!(u["account"]["plan"]["tier"], "default_claude_max_20x");
        assert_eq!(u["account"]["plan"]["known"], true);
        assert_eq!(u["account"]["plan"]["previousName"], "Claude Max 5x");
        assert!(u["account"]["plan"]["sinceObserved"].is_string());
        let unknown = usage_payload(&json!({ "id": "t", "plan": { "known": false } }), false, &json!({}), None, now);
        assert_eq!(unknown["account"]["plan"]["known"], false);
        assert_eq!(unknown["account"]["plan"]["name"], Value::Null);
    }

    #[test]
    fn pick_seat_by_id_email_or_org() {
        let seats = json!({ "currentSeatId": "b", "seats": [
            { "id": "a", "email": "one@x.com", "orgName": "School" },
            { "id": "b", "email": "two@x.com", "orgName": "Home" } ] });
        assert_eq!(pick_seat(&seats, None).unwrap()["id"], "b");
        assert_eq!(pick_seat(&seats, Some("A")).unwrap()["id"], "a");
        assert_eq!(pick_seat(&seats, Some("one@")).unwrap()["id"], "a");
        assert_eq!(pick_seat(&seats, Some("school")).unwrap()["id"], "a");
        assert!(pick_seat(&seats, Some("nobody")).is_none());
        let no_current = json!({ "currentSeatId": null, "seats": [{ "id": "a" }] });
        assert_eq!(pick_seat(&no_current, None).unwrap()["id"], "a");
        assert!(pick_seat(&json!({ "seats": [] }), None).is_none());
    }
}
