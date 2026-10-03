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
        description = "Remaining Claude subscription quota, from the Claude Usage Monitor app running on this machine. \
Returns the three limits — fiveHour (5-hour session window), weekly (7-day, all models) and weeklyFable (7-day, Fable) — \
each with usedPercent, when it resets, the recent burn rate (%/hour), and hitsLimitAt: when 100% is reached if both the \
recent and the whole-window pace stay above par (null = expected to last until reset). observedMinutesAgo tells how old \
the numbers are; the app re-checks on its own schedule, so treat old numbers as a lower bound. Times are local with offset.",
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
        let out = usage_payload(&seat, is_current, &outlook, notice, Utc::now());
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
fn limit_payload(o: &Value, now: DateTime<Utc>) -> Value {
    let expired = o["expired"].as_bool().unwrap_or(false);
    let percent = o["percent"].as_f64();
    let eta = o["etaAt"].as_str();
    let status = if expired {
        "reset_since_last_sample"
    } else if percent.is_none() {
        "no_data"
    } else if percent.is_some_and(|p| p >= 100.0) {
        "at_limit"
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
        "hitsLimitInMinutes": minutes_until(eta, now),
        "hitsLimitAtIfRecentPaceContinues": local_time(o["etaShort"].as_str()),
    })
}

fn usage_payload(seat: &Value, is_current: bool, outlook: &Value, notice: Option<String>, now: DateTime<Utc>) -> Value {
    json!({
        "account": {
            "id": seat["id"],
            "email": seat["email"],
            "organization": seat["orgName"],
            "isCurrent": is_current,
        },
        "now": now.with_timezone(&Local).to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "notice": notice,
        "limits": {
            "fiveHour": limit_payload(&outlook["session"], now),
            "weekly": limit_payload(&outlook["weekly"], now),
            "weeklyFable": limit_payload(&outlook["fable"], now),
        },
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
        let names: Vec<String> = rpc_result(&body)["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"get_usage".to_string()) && names.contains(&"list_accounts".to_string()), "{names:?}");
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
