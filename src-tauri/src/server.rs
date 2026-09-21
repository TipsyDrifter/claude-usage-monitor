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
        .with_state(app)
        .layer(cors);

    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;
    log::info!("Local webhook listening on http://{}/refresh", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
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
        log::warn!(
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
    // D54: the webhook is a doorbell — the browser extension (or a Stop
    // hook) observed real usage happening, so this counts as activity.
    // D55: futile rings (numbers never move) are counted for the health page.
    let _ = crate::collector::refresh_webhook(&app).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true })),
    )
}
