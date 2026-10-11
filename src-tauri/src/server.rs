use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use std::net::SocketAddr;
use tauri::{AppHandle, Manager};
use tower_http::cors::{Any, CorsLayer};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WebhookBody {
    #[serde(default)]
    pub source: Option<String>,
    /// v1.8.2: the browser extension reports its own version so the log can
    /// tell a stale sideload from a current one.
    #[serde(default, rename = "extensionVersion")]
    pub extension_version: Option<String>,
}

/// `?source=…` — the Claude Code Stop hook is a bare `curl.exe` with no body,
/// so it names itself in the query string instead (see `hook_installer`).
#[derive(Debug, Deserialize, Default)]
pub struct WebhookQuery {
    #[serde(default)]
    pub source: Option<String>,
}

/// v1.8.13（D91 T-08）：門鈴綁不到的 port（空字串＝正常）。collector 的提示讀它。
pub const WEBHOOK_BIND_ERROR_KEY: &str = "webhook_bind_error";

pub async fn run(app: AppHandle, port: u16) -> Result<()> {
    // CORS is permissive because the only legitimate callers are
    // localhost-bound: the browser extension (chrome-extension:// origin)
    // and curl from a Stop hook. The bearer-token check below is what
    // actually keeps random local processes out.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let router = Router::new()
        .route("/refresh", post(handle_refresh))
        .with_state(app.clone())
        .layer(cors)
        // v1.9（D94）：給 Claude Code 的 MCP。自己守門（Bearer＋Origin＋Host），不吃上面寬鬆的 CORS。
        .merge(crate::mcp::router(std::sync::Arc::new(crate::mcp::AppSource {
            app: app.clone(),
        })));

    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;

    // v1.8.13（D91 T-08）：綁不到 port（被別的程式佔走、或 App 開了兩份）以前只寫 stderr——
    // 發行版看不到，門鈴靜靜失效。現在寫 collector.log，並記進 channel_state 讓畫面的提示說出來。
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => {
            let ledger: tauri::State<crate::ledger::Ledger> = app.state();
            let _ = ledger.set_channel_state(WEBHOOK_BIND_ERROR_KEY, "").await;
            l
        }
        Err(e) => {
            crate::applog::write_log(
                &app,
                "webhook-bind-error",
                serde_json::json!({ "port": port, "error": e.to_string() }),
            )
            .await;
            let ledger: tauri::State<crate::ledger::Ledger> = app.state();
            let _ = ledger
                .set_channel_state(WEBHOOK_BIND_ERROR_KEY, &port.to_string())
                .await;
            return Err(anyhow::anyhow!("bind {addr}: {e}"));
        }
    };
    log::info!(
        "Local webhook listening on http://{}/refresh (MCP at {})",
        addr,
        crate::mcp::MCP_PATH
    );
    axum::serve(listener, router).await?;
    Ok(())
}

/// `POST /refresh` — kicks the collection layer to re-pull usage data.
///
/// Auth: requires `Authorization: Bearer <token>` matching `webhook_token`
/// in settings.json. The token is provisioned on first launch
/// (`settings::load`) and revealed in the Settings UI so the browser
/// extension and the Claude Code Stop hook can copy it.
async fn handle_refresh(
    State(app): State<AppHandle>,
    query: Option<Query<WebhookQuery>>,
    headers: HeaderMap,
    body: Option<Json<WebhookBody>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let state: tauri::State<AppState> = app.state();
    let settings = state.get_settings().await;
    let expected = settings.webhook.token;

    // Who rang: JSON body (extension) wins, then `?source=` (Stop hook),
    // then "unknown" — an older hook install predating v1.8.2 lands here.
    let body = body.map(|b| b.0);
    let source = body
        .as_ref()
        .and_then(|b| b.source.clone())
        .or_else(|| query.and_then(|q| q.0.source))
        .unwrap_or_else(|| "unknown".into());
    let ext_version = body.and_then(|b| b.extension_version);

    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    if expected.is_empty() || provided != Some(expected.as_str()) {
        // v1.8.13：warn 以上現在會進 collector.log，而下面已經寫了 refresh-webhook unauthorized——降成 debug 免得一次 401 兩行。
        log::debug!(
            "Webhook /refresh: 401 (token_provided={}, token_configured={})",
            provided.is_some(),
            !expected.is_empty(),
        );
        // v1.8.2: a wrong token is the #1 reason "the doorbell never rings" —
        // leave a trace in collector.log so the user can see it happened.
        crate::applog::write_log(
            &app,
            "refresh-webhook",
            serde_json::json!({
                "ok": false,
                "reason": "unauthorized",
                "source": source,
                "token_provided": provided.is_some(),
            }),
        )
        .await;
        // v1.8.7（D86 蟲 5）：log 沒人看——把「按了門鈴但牌對不上」記進 channel_state，
        // 健康度頁與設定頁的門鈴區塊才有東西可以提醒（連續計數、最近一次、誰按的）。
        // v1.8.8（D87）：**按來源分開計**——1.8.7 用一顆計數器，hook 每輪成功就把擴充的
        // 舊牌 401 歸零，擴充壞了整整三天畫面一句話都沒說。
        {
            let ledger: tauri::State<crate::ledger::Ledger> = app.state();
            let key_n = format!("doorbell_unauth_count:{source}");
            let key_t = format!("doorbell_unauth_last:{source}");
            let n: i64 = ledger
                .get_channel_state(&key_n)
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let _ = ledger.set_channel_state(&key_n, &(n + 1).to_string()).await;
            let _ = ledger
                .set_channel_state(&key_t, &chrono::Utc::now().to_rfc3339())
                .await;
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "ok": false,
                "error": "unauthorized — missing or invalid Bearer token"
            })),
        );
    }

    log::info!("Webhook /refresh from {} (auth ok)", source);
    crate::applog::write_log(
        &app,
        "refresh-webhook",
        serde_json::json!({
            "ok": true,
            "source": source,
            "extension_version": ext_version,
        }),
    )
    .await;
    // v1.8.7：一次成功的響鈴就把「牌對不上」的計數歸零（提醒只在問題還在時出現）。
    // v1.8.8：只歸零**這個來源**的——hook 好了不代表擴充好了。
    {
        let ledger: tauri::State<crate::ledger::Ledger> = app.state();
        let _ = ledger
            .set_channel_state(&format!("doorbell_unauth_count:{source}"), "0")
            .await;
    }
    // D54: the webhook is a doorbell — the browser extension (or a Stop
    // hook) observed real usage happening, so this counts as activity.
    // D55: futile rings (numbers never move) are counted for the health page.
    // v1.8.13（D91 T-16）：以前 await 整輪 refresh（碰上探針要 8～13 秒，實抓到一次 90.6 秒）
    // 才回 200——Claude Code 的 Stop hook 那一輪收尾就跟著等。門鈴只負責「按了」，先回 202
    // （已收下、背景處理），刷新丟到背景跑。擴充看的是 res.ok，202 一樣算成功。
    tauri::async_runtime::spawn(async move {
        let _ = crate::collector::refresh_webhook(&app).await;
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "ok": true })),
    )
}
