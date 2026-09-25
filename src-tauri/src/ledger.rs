//! The ledger — the "骨" half of 臉＋骨 (D44): long-term truth storage.
//!
//! A NEW database file (`ledger.sqlite`), separate from the legacy
//! `history.sqlite` (which stays on disk untouched, read-only for the old
//! statistics pages until M3/M4 re-wire them). Starting clean avoids the
//! v1 trap of `usage_event` (singular, empty) vs the new `usage_events`
//! coexisting in one file, and keeps the old schema-version gate from
//! fataling old builds (recon: 2026-08-27-M0-db現況與遷移評估).
//!
//! Schema v1 (D48 機械項 11 + 第 4 點「座位＝帳號×組織」):
//!   seats          — (account_uuid, org_uuid) pairs, learned from channel C
//!   truth_samples  — one observed % of one limit (the "truth" timeline)
//!   usage_events   — deduped API calls from the local JSONL transcripts
//!   anchors        — 429 quota-wall events (window-boundary evidence)
//!   gaps           — known observation gaps (Desktop closed, machine off…)
//!   ingest_files   — mtime/size/offset watermarks for incremental JSONL reads
//!   channel_state  — per-channel watermarks (e.g. last absorbed Desktop `t`)

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::collector::model::TruthSample;

/// (seat_id, account_uuid, org_uuid, total_samples, desktop_samples) — the row
/// shape shared by the seat picker queries.
type SeatRow = (String, Option<String>, Option<String>, i64, i64, Option<String>, Option<String>);
/// (seat_id, account_uuid, org_uuid, desktop_sample_count) — the seat a page
/// ends up rendering.
type SelectedSeat = (String, Option<String>, Option<String>, i64);

/// v1.8.11（D89，主人）：全量視角＝不分帳號。趨勢頁把每個座位各自配對的區間全部
/// 合起來算（配對仍按座位——跨座位配對會把 A 的 56% 接到 B 的 3% 當成下降）；期間
/// 消耗／撞牆／來源歸因不套登入時段；歷史頁把每個座位的窗口合成一條時間軸。
/// 拖尾（D88）與最早觀測前的用量在這個視角下都不是問題：反正全部都算。
pub const ALL_SEATS: &str = "*";

/// v1.8.7（D86 蟲 3）：座位的一段「登入中」時段——entrypoint 說是 CLI 側還是 Desktop 側的
/// 登入，[start, end) 為 UTC ISO 字串（end 可能是 9999 年＝延到現在）。
#[derive(Debug, Clone)]
struct TenureSpan {
    entrypoint: &'static str,
    start: String,
    end: String,
}

/// 把登入時段拼成 SQL 條件。`by_entrypoint`＝true 時每段還要對 `entrypoint` 欄
/// （usage_events）；false 只對時間（anchors 沒有帳號／入口欄位，取兩側聯集）。
/// 沒有任何時段 → 條件恆假（這個座位什麼都不該看到）。回傳 (sql, params)。
fn tenure_where(spans: &[TenureSpan], by_entrypoint: bool) -> (String, Vec<String>) {
    if spans.is_empty() {
        return ("0".to_string(), Vec::new());
    }
    let mut parts: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    for s in spans {
        if by_entrypoint {
            parts.push("(entrypoint = ? AND ts >= ? AND ts < ?)".to_string());
            params.push(s.entrypoint.to_string());
        } else {
            parts.push("(ts >= ? AND ts < ?)".to_string());
        }
        params.push(s.start.clone());
        params.push(s.end.clone());
    }
    (parts.join(" OR "), params)
}

/// v2 (M1/D51): B-channel samples quarantined onto (NULL, org) placeholder
/// seats — Desktop can be a different account than the CLI.
/// v3 (M2/D52): B samples now resolve their account through bridge-state's
/// org→account mapping BEFORE seat lookup (confidence stays inferred), so
/// the 30-day Desktop history books onto the right exact seat. Rebuild
/// re-attributes everything; all data is re-derivable from the sources.
const SCHEMA_VERSION: i64 = 3;

/// M5 (D64) estimation constants — one place, mirrored into the payload's
/// `method` block so the UI's method note can never drift from the code.
const MIN_N: usize = 20;
const MIN_DELTA_SUM: f64 = 30.0;
const BOOT_REPS: usize = 1000;
const PERM_REPS: usize = 2000;
const ALPHA: f64 = 0.05;
const SEED_5H: u64 = 0x4d35_0001;
const SEED_7D: u64 = 0x4d35_0007;
const SEED_BASKET: u64 = 0x4d35_00b0;
const SEED_FAMILY: u64 = 0x4d35_00fa;
const SEED_PERM: u64 = 0x4d35_0e00;

/// v1.4.1（D78 #7）方案建議的樣本門檻：近 PLAN_LOOKBACK_DAYS 天內累計 PLAN_MIN_DAYS 天有真值樣本。
const PLAN_LOOKBACK_DAYS: i64 = 60;
const PLAN_MIN_DAYS: usize = 7;

/// Sample gate (D64-3): a month must hold this much before it gets a CI or
/// enters a test. Below it the UI shows the number as 「校準中」.
fn month_qualified(v: &[crate::stats::FamIv]) -> bool {
    v.len() >= MIN_N && v.iter().map(|x| x.delta).sum::<f64>() >= MIN_DELTA_SUM
}

fn month_seed(m: &str) -> u64 {
    m.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))
}

/// One paired truth interval with everything the M5 layer needs.
struct Interval {
    month: String,
    /// Truth timestamps bounding the interval (RFC 3339) — the evidence
    /// export needs them so every row is re-computable.
    t0: String,
    t1: String,
    iv: crate::stats::FamIv,
    /// v1.6（D80）：同一段的 API$ 再按模型 id 拆（`claude-fable-5-1`…），
    /// 回歸估計分模型倍率用；家族只是分組標題。稀疏、依 id 排序。
    by_model: Vec<(String, f64)>,
    turns: i64,
    out_tokens: i64,
    /// Local-time start hour / weekday (Mon=0) for the peak heatmap.
    hour: u32,
    weekday: u32,
}

/// 讓 `stats::trim` 直接修剪 Interval（v1.6 回歸要保留 by_model，不能先降成 FamIv）。
impl crate::stats::Pair for Interval {
    fn delta(&self) -> f64 {
        self.iv.delta
    }
    fn usd(&self) -> f64 {
        self.iv.usd
    }
}

#[derive(Clone)]
pub struct Ledger {
    conn: Arc<Mutex<Connection>>,
    /// v1.8.2 效能：趨勢頁／歷史頁 payload 快取。key＝配方字串；指紋＝帳本幾張表的
    /// 筆數與最新樣本時刻。指紋沒變直接回快取；變了但快取還新鮮就先回舊的、背景重算。
    cache: Arc<std::sync::Mutex<std::collections::HashMap<String, CacheEntry>>>,
    /// 正在背景重算的 key，避免同一份算兩次。
    inflight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// 快取裡一份算好的 payload 與它的來歷。
struct CacheEntry {
    recipe: Recipe,
    fingerprint: String,
    computed_at: std::time::Instant,
    value: serde_json::Value,
}

/// 「怎麼算出這份 payload」——背景重算與開機預熱都靠它重跑。
#[derive(Clone, Debug)]
enum Recipe {
    Dashboard { seat: Option<String>, days: u32 },
    History { seat: Option<String>, unit: String, offset: i32, kind: HistKind },
}

impl Recipe {
    fn key(&self) -> String {
        match self {
            Recipe::Dashboard { seat, days } => format!("dash|{}|{days}", seat.as_deref().unwrap_or("")),
            Recipe::History { seat, unit, offset, kind } => {
                format!("hist|{}|{unit}|{offset}|{}", seat.as_deref().unwrap_or(""), kind.key())
            }
        }
    }
    fn run(&self, conn: &Connection) -> Result<serde_json::Value> {
        match self {
            Recipe::Dashboard { seat, days } => Ledger::dashboard_sync(conn, seat.as_deref(), *days),
            Recipe::History { seat, unit, offset, kind } => {
                Ledger::history_sync(conn, seat.as_deref(), unit, *offset, *kind)
            }
        }
    }
}

/// 指紋變了但快取不到這麼舊 → 先回舊的、背景重算（主人：數字幾分鐘更新一次就夠）。
const CACHE_STALE_OK: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// 背景重算的最短間隔——jsonl-scan 每分鐘都會動指紋，不值得每分鐘燒 3 秒 CPU。
const CACHE_MIN_RECOMPUTE: std::time::Duration = std::time::Duration::from_secs(90);
/// 快取最多留幾份（座位 × 期間 × 歷史頁翻頁很快就會長）。
const CACHE_MAX_ENTRIES: usize = 16;

impl Ledger {
    /// 帳本內容的便宜指紋：幾張表的筆數＋最新樣本時刻。任何寫入都會動到其中之一。
    fn fingerprint_sync(conn: &Connection) -> String {
        conn.query_row(
            "SELECT (SELECT COUNT(*) FROM truth_samples) || ':' || \
                    COALESCE((SELECT MAX(fetched_at) FROM truth_samples), '') || ':' || \
                    (SELECT COUNT(*) FROM usage_events) || ':' || \
                    (SELECT COUNT(*) FROM anchors) || ':' || \
                    (SELECT COUNT(*) FROM seats)",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    }

    async fn fingerprint(&self) -> String {
        let conn = self.conn.lock().await;
        Self::fingerprint_sync(&conn)
    }

    /// 在阻塞執行緒上跑配方——重算 3 秒不佔 async runtime，其他 IPC 與採集不用排隊。
    async fn compute_blocking(&self, recipe: Recipe) -> Result<serde_json::Value> {
        let conn = self.conn.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            recipe.run(&conn)
        })
        .await
        .map_err(|e| anyhow::anyhow!("compute task failed: {e}"))?
    }

    fn cache_store(&self, recipe: Recipe, fingerprint: String, value: serde_json::Value) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_MAX_ENTRIES {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, e)| e.computed_at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            recipe.key(),
            CacheEntry { recipe, fingerprint, computed_at: std::time::Instant::now(), value },
        );
    }

    /// 快取優先的取用：指紋同→直接回；指紋變但還新鮮→回舊的並背景重算；否則現算（阻塞執行緒）。
    async fn cached(&self, recipe: Recipe) -> Result<serde_json::Value> {
        let key = recipe.key();
        let fp = self.fingerprint().await;
        let hit = {
            let cache = self.cache.lock().unwrap();
            cache
                .get(&key)
                .map(|e| (e.fingerprint == fp, e.computed_at.elapsed(), e.value.clone()))
        };
        if let Some((fresh, age, value)) = hit {
            if fresh {
                return Ok(value);
            }
            if age < CACHE_STALE_OK {
                self.spawn_recompute(recipe);
                return Ok(value);
            }
        }
        let value = self.compute_blocking(recipe.clone()).await?;
        self.cache_store(recipe, fp, value.clone());
        Ok(value)
    }

    /// 背景重算一份配方（同 key 同時只跑一個；指紋沒變或剛算過就略過）。
    fn spawn_recompute(&self, recipe: Recipe) {
        let key = recipe.key();
        {
            let mut inflight = self.inflight.lock().unwrap();
            if !inflight.insert(key.clone()) {
                return;
            }
        }
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let fp = me.fingerprint().await;
            let skip = {
                let cache = me.cache.lock().unwrap();
                cache
                    .get(&key)
                    .is_some_and(|e| e.fingerprint == fp || e.computed_at.elapsed() < CACHE_MIN_RECOMPUTE)
            };
            if !skip {
                let started = std::time::Instant::now();
                match me.compute_blocking(recipe.clone()).await {
                    Ok(v) => {
                        log::debug!("cache recompute {key} in {:.0} ms", started.elapsed().as_secs_f64() * 1e3);
                        me.cache_store(recipe, fp, v);
                    }
                    Err(e) => log::warn!("cache recompute {key} failed: {e:#}"),
                }
            }
            me.inflight.lock().unwrap().remove(&key);
        });
    }

    /// 採集完叫一次：把快取裡每一份都在背景更新，主人開頁時已經是新的。
    /// 快取還是空的（剛開機）就先預熱目前座位的趨勢頁（180 天）。
    pub fn warm_cache(&self) {
        let recipes: Vec<Recipe> = {
            let cache = self.cache.lock().unwrap();
            cache.values().map(|e| e.recipe.clone()).collect()
        };
        if recipes.is_empty() {
            let me = self.clone();
            tauri::async_runtime::spawn(async move {
                let seat = me.get_channel_state("current_seat_id").await.ok().flatten();
                me.spawn_recompute(Recipe::Dashboard { seat, days: 180 });
            });
            return;
        }
        for r in recipes {
            self.spawn_recompute(r);
        }
    }
}

impl Ledger {
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create app data dir {}", dir.display()))?;
        let path = dir.join("ledger.sqlite");
        let conn = Connection::open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let version: i64 = conn
            .query_row(
                "SELECT COALESCE((SELECT value FROM meta WHERE key = 'schema_version'), '0')",
                [],
                |r| r.get::<_, String>(0),
            )
            .map(|v| v.parse().unwrap_or(0))
            .unwrap_or(0);

        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "ledger.sqlite schema v{version} is newer than this build supports (v{SCHEMA_VERSION})"
            );
        }
        if version < 3 {
            // Pre-v3 (incl. fresh file): rebuild from scratch — see the
            // SCHEMA_VERSION note for why dropping is safe here.
            conn.execute_batch(
                "DROP TABLE IF EXISTS truth_samples;
                 DROP TABLE IF EXISTS usage_events;
                 DROP TABLE IF EXISTS anchors;
                 DROP TABLE IF EXISTS gaps;
                 DROP TABLE IF EXISTS ingest_files;
                 DROP TABLE IF EXISTS channel_state;
                 DROP TABLE IF EXISTS seats;
                 DROP TABLE IF EXISTS meta;",
            )?;
            conn.execute_batch(SCHEMA_V2)?;
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        })
    }

    /// Find-or-create the seat for an (account, org) pair — EXACT pair match
    /// only. v2/D51: a sample that only knows its org (channel B) gets a
    /// (account=NULL, org) placeholder seat and STAYS there; we never guess
    /// it onto some account's exact seat, because Desktop may be logged into
    /// a different account than the CLI (the very bug v1 had). Merging a
    /// placeholder into a real seat is a future explicit user-confirmed
    /// action, not an automatic one.
    fn seat_id_sync(
        conn: &Connection,
        account_uuid: Option<&str>,
        org_uuid: Option<&str>,
        now_iso: &str,
    ) -> Result<String> {
        if let Ok(id) = conn.query_row(
            "SELECT id FROM seats WHERE account_uuid IS ?1 AND org_uuid IS ?2",
            params![account_uuid, org_uuid],
            |r| r.get::<_, String>(0),
        ) {
            conn.execute(
                "UPDATE seats SET last_seen_at = ?2 WHERE id = ?1",
                params![id, now_iso],
            )?;
            return Ok(id);
        }

        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO seats (id, account_uuid, org_uuid, first_seen_at, last_seen_at) \
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, account_uuid, org_uuid, now_iso],
        )?;
        Ok(id)
    }

    /// Seed the seats table with (account, org) pairs learned from Desktop's
    /// bridge-state.json (read-only) — D48 wanted an org→account mapping
    /// without asking the user; the bridge file simply has it.
    pub async fn seed_seats(&self, pairs: &[(String, String)]) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().await;
        let now_iso = chrono::Utc::now().to_rfc3339();
        for (account, org) in pairs {
            Self::seat_id_sync(&conn, Some(account), Some(org), &now_iso)?;
        }
        Ok(())
    }

    /// Burn-rate stats for the W5 expanded panel (M2). Computed from the
    /// exact-source (probe / cli-cache) session-limit samples of the last
    /// two hours: we take the monotonic tail since the last window reset
    /// (a percent DROP means a new 5h window started — samples before it
    /// describe a different window and must not enter the slope).
    ///
    /// The ▼ pace mark assumes linear burn (W5 自評弱點 4); this is the
    /// "precise language" companion the panel pairs with it.
    pub async fn burn_stats(&self, seat: Option<&str>) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let seat_id = Self::resolve_seat_sync(&conn, seat)?;
        Self::limit_outlook_sync(&conn, &seat_id, &["session", "five_hour"], None, 5.0, 1.0)
    }

    /// v1.3（D75 決策點 4）：查詢要看的座位——明講的 > CLI 目前登入的
    /// （collector 每次 refresh 寫進 channel_state）> 樣本最多的 exact 座位。
    /// 什麼都沒有時回空字串，讓下游查詢自然查無樣本。
    fn resolve_seat_sync(conn: &Connection, seat: Option<&str>) -> Result<String> {
        // v1.8.11：全量視角對「即時面」沒有意義（百分比是每個帳號各自的），退回目前座位。
        if let Some(s) = seat.filter(|s| !s.is_empty() && *s != ALL_SEATS) {
            return Ok(s.to_string());
        }
        if let Ok(s) = conn.query_row(
            "SELECT value FROM channel_state WHERE key = 'current_seat_id'",
            [],
            |r| r.get::<_, String>(0),
        ) {
            if !s.is_empty() {
                return Ok(s);
            }
        }
        let fallback = conn
            .query_row(
                "SELECT s.id FROM seats s WHERE s.account_uuid IS NOT NULL \
                 ORDER BY (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id \
                           AND t.source IN ('probe','cli-cache')) DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(fallback.unwrap_or_default())
    }

    /// v1.3：把 CLI 目前登入的（帳號, 組織）記成「目前座位」，並把組織名／email
    /// 寫進 seats 表（COALESCE：讀到才覆寫，不會被空值洗掉）。回傳 seat id。
    pub async fn mark_current_seat(
        &self,
        account_uuid: Option<&str>,
        org_uuid: Option<&str>,
        email: Option<&str>,
        org_name: Option<&str>,
    ) -> Result<String> {
        let conn = self.conn.lock().await;
        let now_iso = chrono::Utc::now().to_rfc3339();
        let seat_id = Self::seat_id_sync(&conn, account_uuid, org_uuid, &now_iso)?;
        conn.execute(
            "UPDATE seats SET account_email = COALESCE(?2, account_email), \
                              org_name = COALESCE(?3, org_name) WHERE id = ?1",
            params![seat_id, email, org_name],
        )?;
        conn.execute(
            "INSERT INTO channel_state (key, value) VALUES ('current_seat_id', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![seat_id],
        )?;
        Ok(seat_id)
    }

    /// v1.8.9（D88，O10 實證）：切帳號後 CLI 會拖著**上一個帳號**的額度走——`oauthAccount`
    /// 已經是新帳號，`/usage` 卻連續 20 小時回切換前那組數字（09-22 11:55 切到 thomaskuo100
    /// 之後 7d 56／Fable 100 一動不動，直到週重置才歸零），App 照它說的帳號記，就把
    /// 別人的 Fable 100% 記到新帳號頭上。這裡在寫進帳本之前對一次：進來的 CLI 樣本若
    /// 7d（與 Fable）跟「上一個座位最後一筆」一模一樣且非零，就當拖尾——不記、不顯示，
    /// 直到數字真的變了才解除。回傳 Some((上一個座位 id, 7d, Fable)) 表示拖尾。
    /// 狀態放 channel_state：`prev_seat_id`／`seat_switched_at`（超過 7 天自動放棄比對）。
    pub async fn carryover_check(&self, samples: &[TruthSample]) -> Result<Option<(String, f32, Option<f32>)>> {
        let Some(first) = samples.iter().find(|s| s.account_uuid.is_some()) else {
            return Ok(None);
        };
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now();
        let now_iso = now.to_rfc3339();
        let incoming = Self::seat_id_sync(&conn, first.account_uuid.as_deref(), first.org_uuid.as_deref(), &now_iso)?;
        let get = |k: &str| -> Option<String> {
            conn.query_row("SELECT value FROM channel_state WHERE key = ?1", params![k], |r| r.get::<_, String>(0))
                .ok()
                .filter(|v| !v.is_empty())
        };
        let set = |k: &str, v: &str| -> rusqlite::Result<usize> {
            conn.execute(
                "INSERT INTO channel_state (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k, v],
            )
        };
        let current = get("current_seat_id");
        let remembered = get("prev_seat_id");
        let prev: Option<String> = match (current.as_deref(), remembered) {
            (Some(cur), _) if cur != incoming => Some(cur.to_string()), // 正在切換
            (_, Some(p)) if p != incoming => Some(p),                  // 之前切過、還在盯
            _ => None,
        };
        let Some(prev) = prev else {
            set("prev_seat_id", "")?;
            set("seat_switched_at", "")?;
            return Ok(None);
        };
        let switched_at = get("seat_switched_at");
        if let Some(t) = switched_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
            if (now - t.with_timezone(&chrono::Utc)).num_days() > 7 {
                set("prev_seat_id", "")?;
                set("seat_switched_at", "")?;
                return Ok(None);
            }
        }
        let last = |seat: &str, kinds: &str, scope: &str| -> Option<f32> {
            conn.query_row(
                &format!(
                    "SELECT percent FROM truth_samples WHERE seat_id = ?1 AND limit_kind IN ({kinds}) \
                     AND scope = ?2 AND source IN ('probe','cli-cache') ORDER BY fetched_at DESC LIMIT 1"
                ),
                params![seat, scope],
                |r| r.get::<_, f64>(0),
            )
            .ok()
            .map(|v| v as f32)
        };
        let prev_weekly = last(&prev, "'weekly_all','seven_day'", "");
        let prev_fable = last(&prev, "'weekly_scoped'", "Fable");
        let no_scope = |s: &TruthSample| s.scope.as_deref().map(|x| x.is_empty()).unwrap_or(true);
        let in_weekly = samples
            .iter()
            .find(|s| matches!(s.limit_kind.as_str(), "weekly_all" | "seven_day") && no_scope(s))
            .map(|s| s.percent);
        let in_fable = samples
            .iter()
            .find(|s| s.limit_kind == "weekly_scoped" && s.scope.as_deref().is_some_and(|x| x.eq_ignore_ascii_case("fable")))
            .map(|s| s.percent);
        let same = match (prev_weekly, in_weekly) {
            (Some(pw), Some(iw)) if pw > 0.0 && (pw - iw).abs() < 0.5 => match (prev_fable, in_fable) {
                (Some(pf), Some(inf)) => (pf - inf).abs() < 0.5,
                _ => true,
            },
            _ => false,
        };
        if same {
            set("prev_seat_id", &prev)?;
            if switched_at.is_none() {
                set("seat_switched_at", &now_iso)?;
            }
            Ok(Some((prev, prev_weekly.unwrap_or(0.0), prev_fable)))
        } else {
            set("prev_seat_id", "")?;
            set("seat_switched_at", "")?;
            Ok(None)
        }
    }

    /// v1.3 切換器用的座位清單：只列有帳號 uuid 的 exact 座位（決策點 9），
    /// 附名字欄位與 A／C 樣本數；`currentSeatId` 是 CLI 目前登入的那一個。
    pub async fn list_seats(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let current = conn
            .query_row(
                "SELECT value FROM channel_state WHERE key = 'current_seat_id'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.account_uuid, s.org_uuid, s.account_email, s.org_name, s.last_seen_at, \
                    (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id \
                      AND t.source IN ('probe','cli-cache')), \
                    (SELECT MAX(t.fetched_at) FROM truth_samples t WHERE t.seat_id = s.id \
                      AND t.source IN ('probe','cli-cache')) \
             FROM seats s WHERE s.account_uuid IS NOT NULL ORDER BY 7 DESC",
        )?;
        let mut seats = Vec::new();
        for row in stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "accountUuid": r.get::<_, Option<String>>(1)?,
                "orgUuid": r.get::<_, Option<String>>(2)?,
                "email": r.get::<_, Option<String>>(3)?,
                "orgName": r.get::<_, Option<String>>(4)?,
                "lastSeenAt": r.get::<_, String>(5)?,
                "liveSamples": r.get::<_, i64>(6)?,
                "lastSampleAt": r.get::<_, Option<String>>(7)?,
            }))
        })? {
            seats.push(row?);
        }
        Ok(serde_json::json!({ "seats": seats, "currentSeatId": current }))
    }

    /// v1.3（決策點 5）：某座位三條額度的**最後已知值**——每個額度家族各取
    /// probe／cli-cache 的最後一筆。形狀對齊 `UsageSnapshot`，前端的懸浮窗列
    /// 可以直接吃；`scrapedAt`＝三筆裡最新的 fetched_at，前端據此標「N 小時前」。
    pub async fn seat_snapshot(&self, seat_id: &str) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let last = |kinds: &str, scope: &str| -> Result<(Option<f64>, Option<String>, Option<String>)> {
            let sql = format!(
                "SELECT percent, resets_at, fetched_at FROM truth_samples \
                 WHERE seat_id = ?1 AND limit_kind IN ({kinds}) AND scope = ?2 \
                   AND source IN ('probe','cli-cache') \
                 ORDER BY fetched_at DESC LIMIT 1"
            );
            let row = conn
                .query_row(&sql, params![seat_id, scope], |r| {
                    Ok((r.get::<_, f64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?))
                })
                .ok();
            Ok(match row {
                Some((p, reset, at)) => (Some(p), reset, Some(at)),
                None => (None, None, None),
            })
        };
        let session = last("'session','five_hour'", "")?;
        let weekly = last("'weekly_all','seven_day'", "")?;
        let fable = last("'weekly_scoped'", "Fable")?;
        let newest = [&session.2, &weekly.2, &fable.2]
            .into_iter()
            .flatten()
            .max()
            .cloned();
        let item = |v: &(Option<f64>, Option<String>, Option<String>)| {
            serde_json::json!({ "usedPercent": v.0, "resetAt": v.1, "note": v.2.as_ref().map(|_| "最後已知值") })
        };
        Ok(serde_json::json!({
            "seatId": seat_id,
            "planName": null,
            "currentSession": item(&session),
            "weeklyAllModels": item(&weekly),
            "weeklyFable": item(&fable),
            "scrapedAt": newest,
        }))
    }

    /// Outlook for ONE limit family（D71，SRE 式雙窗）：
    /// - 長窗＝整個額度窗至今（用量 ÷ 窗內已過時間）——與 pace ▼ 同一把尺；
    /// - 短窗：7d／Fable＝窗長 ÷ 12（14 小時）；5h＝60 分（D73：25 分只有 2–5 筆整數樣本，
    ///   回放證明報分鐘會抖；60 分跳動 3%、誤差中位數 24 分，比長窗還準）；
    /// - R ＝ burn ÷ par（par ＝ 100% ÷ 窗長）；「會提早見底」只在**兩個 R 都 ≥ 1** 時成立；
    /// - 重置後未滿一個短窗 → insufficient，先不裁決（AWS／GCP 資料不足不預測）。
    fn limit_outlook_sync(
        conn: &Connection,
        seat_id: &str,
        kinds: &[&str],
        scope: Option<&str>,
        window_hours: f64,
        short_hours: f64,
    ) -> Result<serde_json::Value> {
        let since = (chrono::Utc::now()
            - chrono::Duration::seconds((window_hours * 3600.0) as i64))
        .to_rfc3339();
        let kind_list = kinds
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(",");
        let scope_clause = match scope {
            Some(_) => "AND scope = ?3",
            None => "AND scope = ''",
        };
        // v1.3（D75）：按座位算——兩個帳號的樣本不能接成一條曲線。
        let sql = format!(
            "SELECT percent, fetched_at, resets_at FROM truth_samples \
             WHERE seat_id = ?1 AND limit_kind IN ({kind_list}) \
               AND source IN ('probe','cli-cache') \
               AND fetched_at >= ?2 {scope_clause} \
             ORDER BY fetched_at ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<(f64, String, Option<String>)> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        };
        let rows: Vec<(f64, String, Option<String>)> = match scope {
            Some(s) => stmt
                .query_map(params![seat_id, since, s], map_row)?
                .collect::<std::result::Result<_, _>>()?,
            None => stmt
                .query_map(params![seat_id, since], map_row)?
                .collect::<std::result::Result<_, _>>()?,
        };

        // 重置切尾：最後一次 >2% 下跌之後才是本窗。
        let mut tail_start = 0usize;
        for i in 1..rows.len() {
            if rows[i].0 < rows[i - 1].0 - 2.0 {
                tail_start = i;
            }
        }
        let tail = &rows[tail_start..];

        let par = 100.0 / window_hours;
        let short_h = short_hours;
        let round1 = |x: f64| (x * 10.0).round() / 10.0;
        let round2 = |x: f64| (x * 100.0).round() / 100.0;

        let mut out = serde_json::json!({
            "samples": tail.len(),
            "percent": tail.last().map(|r| r.0),
            "fetchedAt": tail.last().map(|r| r.1.clone()),
            "resetsAt": tail.iter().rev().find_map(|r| r.2.clone()),
            "windowHours": window_hours,
            "shortWindowMinutes": (short_h * 60.0).round(),
            "burnPerHour": null,       // 短窗平滑斜率（%/h）——燃燒速度顯示、pace-flow 流速、副句預警
            "paceBurnPerHour": null,   // 長窗平均（%/h）——與 ▼ 同尺
            "rShort": null,
            "rLong": null,
            "projectedAtReset": null,  // 長窗外推到重置時的 %
            "etaShort": null,          // 短窗速度若持續的見底時刻（rShort ≥ 1 才有）
            "etaLong": null,           // 長窗平均外推的見底時刻（rLong ≥ 1 才有）——5h 的數字用它（D72）
            "etaAt": null,             // 裁決見底時刻：兩窗都 ≥ 1 才有
            "insufficient": false,
            "expired": false,          // v1.4.1：最後樣本的窗已過重置時間——之後未知
            "lastKnownPercent": null,
        });
        let Some(last) = tail.last() else {
            // v1.4.2：窗內一筆樣本都沒有（例如 5h 窗、最後一筆是三天前）——
            // 往前找這條額度的最後一筆；它的窗必然已過，標 expired 並附上次的值，
            // 讓「已重置 · 上次 X%」跟 7d／Fable 同一套說法，而不是硬邦邦的「無資料」。
            let sql = format!(
                "SELECT percent, fetched_at, resets_at FROM truth_samples \
                 WHERE seat_id = ?1 AND limit_kind IN ({kind_list}) \
                   AND source IN ('probe','cli-cache') {scope_clause} \
                 ORDER BY fetched_at DESC LIMIT 1"
            );
            let mut stmt = conn.prepare(&sql)?;
            // scope_clause 用的是 ?3，所以 Some 時要佔住 ?2（rusqlite 檢查參數個數）。
            let older: Option<(f64, String, Option<String>)> = match scope {
                Some(s) => stmt.query_row(params![seat_id, "", s], map_row).ok(),
                None => stmt.query_row(params![seat_id], map_row).ok(),
            };
            if let Some((p, at, reset)) = older {
                out["expired"] = serde_json::json!(true);
                out["lastKnownPercent"] = serde_json::json!(p);
                out["fetchedAt"] = serde_json::json!(at);
                out["resetsAt"] = serde_json::json!(reset);
            }
            return Ok(out);
        };
        let t_last = chrono::DateTime::parse_from_rfc3339(&last.1)?;

        // v1.4.1（D78 #1，也收掉 backlog「閒置超過一個窗顯示無資料」）：最後一筆樣本的窗
        // 已經過了重置時間——之後用了多少沒人知道（別的座位沒登入、或目前座位閒置）。
        // 不假裝：標 expired，前端把數字換「—」、寫「HH:MM 已重置」，上次的值留給提示。
        // 百分比與時間仍照常回傳，畫面才有東西可以說「上次是多少」。
        if let Some(reset) = last
            .2
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        {
            if reset < chrono::Utc::now() {
                out["expired"] = serde_json::json!(true);
                out["lastKnownPercent"] = serde_json::json!(last.0);
                return Ok(out);
            }
        }

        // ---- 短窗：取最近 short_h 內的樣本；窗內只剩一筆就往前多拿一筆，
        //      斜率至少跨過一個短窗（Chromium／curl 的固定滑動窗精神）。
        let cutoff = t_last - chrono::Duration::seconds((short_h * 3600.0) as i64);
        let mut i0 = tail.len() - 1;
        while i0 > 0 && chrono::DateTime::parse_from_rfc3339(&tail[i0 - 1].1)? >= cutoff {
            i0 -= 1;
        }
        if i0 == tail.len() - 1 && i0 > 0 {
            i0 -= 1;
        }
        let mut burn_short: Option<f64> = None;
        if i0 < tail.len() - 1 {
            let t0 = chrono::DateTime::parse_from_rfc3339(&tail[i0].1)?;
            let hours = (t_last - t0).num_seconds() as f64 / 3600.0;
            if hours >= 5.0 / 60.0 {
                burn_short = Some((last.0 - tail[i0].0) / hours);
            }
        }
        if let Some(b) = burn_short {
            out["burnPerHour"] = serde_json::json!(round1(b));
            out["rShort"] = serde_json::json!(round2(b / par));
        }

        // ---- 長窗：用量 ÷ 窗內已過時間。
        let reset = last
            .2
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        if let Some(reset) = reset {
            let window_start =
                reset - chrono::Duration::seconds((window_hours * 3600.0) as i64);
            let elapsed_h = (t_last - window_start).num_seconds() as f64 / 3600.0;
            let insufficient = elapsed_h < short_h;
            out["insufficient"] = serde_json::json!(insufficient);
            let mut r_long: Option<f64> = None;
            if elapsed_h >= 5.0 / 60.0 {
                let pace_burn = last.0 / elapsed_h;
                out["paceBurnPerHour"] = serde_json::json!(round1(pace_burn));
                r_long = Some(pace_burn / par);
                out["rLong"] = serde_json::json!(round2(pace_burn / par));
                out["projectedAtReset"] =
                    serde_json::json!((pace_burn * window_hours).min(100.0).round());
                if pace_burn / par >= 1.0 && last.0 < 100.0 {
                    let eta_long = window_start
                        + chrono::Duration::seconds((elapsed_h * 100.0 / last.0 * 3600.0) as i64);
                    if eta_long < reset {
                        out["etaLong"] = serde_json::json!(eta_long.to_rfc3339());
                    }
                }
            }
            if let Some(b) = burn_short {
                if b / par >= 1.0 && last.0 < 100.0 {
                    let eta = t_last
                        + chrono::Duration::seconds(((100.0 - last.0) / b * 3600.0) as i64);
                    out["etaShort"] = serde_json::json!(eta.to_rfc3339());
                    if !insufficient && r_long.is_some_and(|r| r >= 1.0) && eta < reset {
                        out["etaAt"] = serde_json::json!(eta.to_rfc3339());
                    }
                }
            }
        }
        Ok(out)
    }

    /// M3「今天會不會撞牆」decision-layer payload: outlook for all three
    /// limits + what THIS 5h window's local activity looks like. Local JSONL
    /// carries no account field (O10), so the activity table is machine-wide
    /// and labeled as such — the truth percentages remain CLI-account (D51).
    pub async fn today_outlook(&self, seat: Option<&str>) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let seat_id = Self::resolve_seat_sync(&conn, seat)?;
        let session =
            Self::limit_outlook_sync(&conn, &seat_id, &["session", "five_hour"], None, 5.0, 1.0)?;
        let weekly =
            Self::limit_outlook_sync(&conn, &seat_id, &["weekly_all", "seven_day"], None, 168.0, 14.0)?;
        let fable =
            Self::limit_outlook_sync(&conn, &seat_id, &["weekly_scoped"], Some("Fable"), 168.0, 14.0)?;

        // Current 5h window start — from the session limit's resets_at.
        let window_start = session
            .get("resetsAt")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|reset| (reset - chrono::Duration::hours(5)).to_rfc3339());

        let mut by_entrypoint = Vec::new();
        let mut top_projects = Vec::new();
        if let Some(start) = &window_start {
            let mut stmt = conn.prepare(
                "SELECT COALESCE(entrypoint,'unknown'), COUNT(*), SUM(output_tokens), \
                        SUM(cache_creation), SUM(cache_read) \
                 FROM usage_events WHERE ts >= ?1 GROUP BY 1 ORDER BY 3 DESC",
            )?;
            for row in stmt.query_map(params![start], |r| {
                Ok(serde_json::json!({
                    "entrypoint": r.get::<_, String>(0)?,
                    "calls": r.get::<_, i64>(1)?,
                    "outputTokens": r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    "cacheCreation": r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    "cacheRead": r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                }))
            })? {
                by_entrypoint.push(row?);
            }

            let mut stmt = conn.prepare(
                "SELECT COALESCE(project_dir,'?'), COUNT(*), SUM(output_tokens) \
                 FROM usage_events WHERE ts >= ?1 GROUP BY 1 ORDER BY 3 DESC LIMIT 6",
            )?;
            for row in stmt.query_map(params![start], |r| {
                Ok(serde_json::json!({
                    "project": r.get::<_, String>(0)?,
                    "calls": r.get::<_, i64>(1)?,
                    "outputTokens": r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                }))
            })? {
                top_projects.push(row?);
            }
        }

        // ---- D56/M5: this window's % staircase (exact sources) + 429 ticks ----
        let mut window_samples: Vec<serde_json::Value> = Vec::new();
        let mut window_anchors: Vec<String> = Vec::new();
        if let Some(start) = &window_start {
            let mut stmt = conn.prepare(
                "SELECT percent, fetched_at, source FROM truth_samples \
                 WHERE seat_id = ?2 AND limit_kind IN ('session','five_hour') AND scope = '' \
                   AND source IN ('probe','cli-cache') AND fetched_at >= ?1 \
                 ORDER BY fetched_at ASC",
            )?;
            for row in stmt.query_map(params![start, seat_id], |r| {
                Ok(serde_json::json!({
                    "at": r.get::<_, String>(1)?,
                    "percent": r.get::<_, f64>(0)?,
                    "source": r.get::<_, String>(2)?,
                }))
            })? {
                window_samples.push(row?);
            }
            let mut stmt =
                conn.prepare("SELECT ts FROM anchors WHERE ts >= ?1 ORDER BY ts ASC")?;
            for row in stmt.query_map(params![start], |r| r.get::<_, String>(0))? {
                window_anchors.push(row?);
            }
        }

        // ---- D56/M5 方案建議: last 14 days of exact samples, % only ----
        let plan = {
            // v1.4.1（D78 #7）：門檻改「累計 ≥7 天有樣本」——看近 60 天，不再要求 14 天內滿 7 天，
            // 用得不頻繁的人也拿得到建議。變數名沿用。
            let since14 = (chrono::Utc::now() - chrono::Duration::days(PLAN_LOOKBACK_DAYS)).to_rfc3339();
            let mut day_peak: std::collections::BTreeMap<String, f64> = Default::default();
            let mut days_seen: std::collections::BTreeSet<String> = Default::default();
            let mut weekly_peak = 0.0f64;
            let mut fable_peak = 0.0f64;
            let mut stmt = conn.prepare(
                "SELECT limit_kind, scope, percent, substr(fetched_at,1,10) FROM truth_samples \
                 WHERE seat_id = ?2 AND source IN ('probe','cli-cache') AND fetched_at >= ?1",
            )?;
            for row in stmt.query_map(params![since14, seat_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })? {
                let (kind, scope, pct, day) = row?;
                days_seen.insert(day.clone());
                match kind.as_str() {
                    "session" | "five_hour" => {
                        let e = day_peak.entry(day).or_insert(0.0);
                        if pct > *e {
                            *e = pct;
                        }
                    }
                    "weekly_all" | "seven_day" => weekly_peak = weekly_peak.max(pct),
                    "weekly_scoped" if scope == "Fable" => fable_peak = fable_peak.max(pct),
                    _ => {}
                }
            }
            let coverage_days = days_seen.len();
            let sat_days = day_peak.values().filter(|&&p| p >= 100.0).count();
            let mut peaks: Vec<f64> = day_peak.values().copied().collect();
            peaks.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p90 = if peaks.is_empty() { None } else { Some(crate::stats::quantile(&peaks, 0.9)) };
            let (suggestion, reason) = if coverage_days < PLAN_MIN_DAYS {
                ("calibrating", format!("目前有觀測記錄的天數累計 {coverage_days} 天（看最近 {PLAN_LOOKBACK_DAYS} 天），滿 {PLAN_MIN_DAYS} 天再給建議"))
            } else if sat_days >= 4 || weekly_peak >= 90.0 {
                ("upgrade", format!("近 14 天 5h 見頂 {sat_days} 天、週峰值 {weekly_peak:.0}%"))
            } else if p90.map(|p| p < 40.0).unwrap_or(false) && weekly_peak < 40.0 {
                ("downgrade", format!("近 14 天 5h 每日最高 p90 {:.0}%、週峰值 {weekly_peak:.0}%", p90.unwrap_or(0.0)))
            } else {
                ("keep", format!("近 14 天 5h 見頂 {sat_days} 天、每日最高 p90 {:.0}%、週峰值 {weekly_peak:.0}%", p90.unwrap_or(0.0)))
            };
            serde_json::json!({
                "coverageDays": coverage_days,
                "coverageLookbackDays": PLAN_LOOKBACK_DAYS,
                "coverageMinDays": PLAN_MIN_DAYS,
                "sessionSatDays": sat_days,
                "sessionPeakP90": p90,
                "weeklyPeak": weekly_peak,
                "fablePeak": fable_peak,
                "dailyPeaks": day_peak.iter().map(|(d, p)| serde_json::json!({ "day": d, "peak": p })).collect::<Vec<_>>(),
                "suggestion": suggestion,
                "reason": reason,
            })
        };

        Ok(serde_json::json!({
            "seatId": seat_id,
            "session": session,
            "weekly": weekly,
            "fable": fable,
            "windowStart": window_start,
            "windowSamples": window_samples,
            "windowAnchors": window_anchors,
            "plan": plan,
            "byEntrypoint": by_entrypoint,
            "topProjects": top_projects,
        }))
    }

    /// v1.8.7（D86 蟲 3）：這個座位「在哪些時段是登入中的那個帳號」。
    /// CLI 側看 probe／cli-cache 樣本、Desktop 側看 desktop-history 樣本：把該來源
    /// **所有座位**的樣本按時間排好，座位換了就切一段，只留這個座位的段；最後一段
    /// 若還是它就延到現在（登入狀態是階梯函數——沒觀測到換帳號就當沒換）。
    /// 最早一筆樣本之前的用量沒有依據、不歸任何座位（D86 決策點 1，評估後主人可翻）。
    fn seat_tenure_sync(conn: &Connection, seat_id: &str, since: &str) -> Result<Vec<TenureSpan>> {
        const OPEN_END: &str = "9999-12-31T23:59:59Z";
        let mut out: Vec<TenureSpan> = Vec::new();
        for (entrypoint, sources) in [
            ("cli", "'probe','cli-cache'"),
            ("claude-desktop", "'desktop-history'"),
        ] {
            let mut stmt = conn.prepare(&format!(
                "SELECT seat_id, MIN(fetched_at) FROM truth_samples WHERE source IN ({sources}) \
                 GROUP BY seat_id, fetched_at ORDER BY 2 ASC"
            ))?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            let mut run: Option<(String, String)> = None; // (seat, run start)
            for (s, t) in rows {
                match &run {
                    Some((cur, _)) if *cur == s => {}
                    Some((cur, start)) => {
                        if cur == seat_id {
                            out.push(TenureSpan { entrypoint, start: start.clone(), end: t.clone() });
                        }
                        run = Some((s, t));
                    }
                    None => run = Some((s, t)),
                }
            }
            if let Some((cur, start)) = run {
                if cur == seat_id {
                    out.push(TenureSpan { entrypoint, start, end: OPEN_END.to_string() });
                }
            }
        }
        // Clip to the requested period.
        out.retain(|s| s.end.as_str() > since);
        for s in &mut out {
            if s.start.as_str() < since {
                s.start = since.to_owned();
            }
        }
        Ok(out)
    }

    /// Pair consecutive truth samples of ONE seat / limit family into
    /// intervals (≤45 min apart, strictly increasing %), and price the
    /// entrypoint-filtered usage_events inside each with the API table,
    /// split by model family (stats::FAMILIES) so the fixed basket can be
    /// built downstream. Shared by the 5h and 7d ledgers (D64).
    fn pair_intervals(
        conn: &Connection,
        seat_id: &str,
        kinds_sql: &str,
        since: &str,
        entrypoint: &str,
    ) -> Result<Vec<Interval>> {
        use chrono::{Datelike, Timelike};
        let mut stmt = conn.prepare(&format!(
            "SELECT percent, fetched_at FROM truth_samples \
             WHERE seat_id = ?1 AND limit_kind IN ({kinds_sql}) AND scope = '' \
               AND fetched_at >= ?2 ORDER BY fetched_at ASC"
        ))?;
        let samples: Vec<(f64, String)> = stmt
            .query_map(params![seat_id, since], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;

        let mut ev_stmt = conn.prepare(
            "SELECT model, COALESCE(speed,''), COUNT(*), SUM(input_tokens), \
                    SUM(cache_creation), SUM(COALESCE(cache_1h,0)), SUM(cache_read), \
                    SUM(output_tokens), SUM(COALESCE(web_searches,0)) \
             FROM usage_events WHERE ts > ?1 AND ts <= ?2 AND entrypoint = ?3 \
             GROUP BY model, COALESCE(speed,'')",
        )?;
        let mut intervals: Vec<Interval> = Vec::new();
        for w in samples.windows(2) {
            let (p0, t0) = (&w[0].0, &w[0].1);
            let (p1, t1) = (&w[1].0, &w[1].1);
            let (Ok(a), Ok(b)) = (
                chrono::DateTime::parse_from_rfc3339(t0),
                chrono::DateTime::parse_from_rfc3339(t1),
            ) else {
                continue;
            };
            let mins = (b - a).num_minutes();
            if mins <= 0 || mins > 45 || p1 <= p0 {
                continue;
            }
            let delta = p1 - p0;
            let mut usd = 0.0f64;
            let mut by_family = [0.0f64; 5];
            let mut by_model: std::collections::BTreeMap<String, f64> = Default::default();
            let mut turns = 0i64;
            let mut out_tokens = 0i64;
            for row in ev_stmt.query_map(params![t0, t1, entrypoint], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                ))
            })? {
                let (model, speed, calls, inp, cw, cw1, cr, out, ws) = row?;
                let v = price_usd(&model, &speed, inp, cw, cw1, cr, out, ws);
                usd += v;
                if let Some(f) = crate::stats::family_index(&model) {
                    by_family[f] += v;
                    *by_model.entry(model.clone()).or_insert(0.0) += v;
                }
                turns += calls;
                out_tokens += out;
            }
            let local = a.with_timezone(&chrono::Local);
            intervals.push(Interval {
                month: t1[..7].to_string(),
                t0: t0.clone(),
                t1: t1.clone(),
                iv: crate::stats::FamIv { delta, usd, by_family },
                by_model: by_model.into_iter().collect(),
                turns,
                out_tokens,
                hour: local.hour(),
                weekday: local.weekday().num_days_from_monday(),
            });
        }
        Ok(intervals)
    }

    /// Per-month exchange-rate rows (D64-1/2/3): raw + trimmed Δ-weighted
    /// ratio, bootstrap CI only for months past the sample gate.
    fn fx_months(
        ivs: &[Interval],
        seed: u64,
    ) -> (Vec<serde_json::Value>, std::collections::BTreeMap<String, Vec<crate::stats::FamIv>>) {
        use crate::stats;
        let mut months: std::collections::BTreeMap<String, Vec<stats::FamIv>> = Default::default();
        for i in ivs {
            months.entry(i.month.clone()).or_default().push(i.iv.clone());
        }
        let rows = months
            .iter()
            .map(|(m, v)| {
                let n = v.len();
                let dsum: f64 = v.iter().map(|x| x.delta).sum();
                let usum: f64 = v.iter().map(|x| x.usd).sum();
                let qualified = month_qualified(v);
                let rate_raw = (dsum >= 5.0).then(|| usum / dsum);
                let rate = if dsum >= 5.0 { stats::trimmed_ratio(v) } else { None };
                let ci = if qualified {
                    stats::bootstrap_ci(v, stats::trimmed_ratio, BOOT_REPS, seed ^ month_seed(m))
                } else {
                    None
                };
                serde_json::json!({
                    "month": m,
                    "samples": n,
                    "deltaSum": dsum,
                    "usdSum": usum,
                    "rateRaw": rate_raw,
                    "rate": rate,
                    "ci": ci.map(|(a, b)| vec![a, b]),
                    "qualified": qualified,
                })
            })
            .collect();
        (rows, months)
    }

    /// M4 dashboard payload — the research layer (這半年發生了什麼).
    ///
    /// M5 (D64) 匯率轉正: trimmed Δ-weighted ratio + bootstrap CI per month,
    /// permutation month-over-month test, 5h/7d cross-check, fixed-basket
    /// shrink index, per-family quota multipliers, local-time peak heatmap
    /// and truth reconciliation. Pure math lives in `stats.rs`; this
    /// function builds intervals and assembles JSON. Events are entrypoint-
    /// filtered to the seat (Desktop seat ↔ claude-desktop, CLI seat ↔ cli;
    /// 推定 O10). `saturatedDays` (D62) keeps its semantics untouched.
    pub async fn dashboard(
        &self,
        seat_account: Option<&str>,
        days: u32, // D60 自定義期間；0 = 全部（實作上限 3650）
    ) -> Result<serde_json::Value> {
        // v1.8.2 效能：走快取（見 Recipe／cached）。真正的計算在 dashboard_sync。
        self.cached(Recipe::Dashboard { seat: seat_account.map(str::to_owned), days }).await
    }

    fn dashboard_sync(
        conn: &Connection,
        seat_account: Option<&str>,
        days: u32,
    ) -> Result<serde_json::Value> {
        use crate::stats;
        let days = if days == 0 { 3650 } else { days.clamp(7, 3650) };
        // v1.8.2 效能量測：CUM_TIMING=1 時把每一段的耗時印到 stderr（cargo test probe::timing）。
        let timing = std::env::var_os("CUM_TIMING").is_some();
        let t_start = std::time::Instant::now();
        let mut t_last = t_start;
        let mut mark = |label: &str| {
            if timing {
                let now = std::time::Instant::now();
                eprintln!("[timing] {label:<14} {:>7.1} ms (cum {:>7.1} ms)", now.duration_since(t_last).as_secs_f64()*1e3, now.duration_since(t_start).as_secs_f64()*1e3);
                t_last = now;
            }
        };

        // ---- seats (pickable; default = most sampled) ----
        let mut seats: Vec<SeatRow> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.account_uuid, s.org_uuid, \
                        (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id), \
                        (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id \
                          AND t.source = 'desktop-history'), \
                        s.account_email, s.org_name \
                 FROM seats s ORDER BY 4 DESC",
            )?;
            for row in stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })? {
                seats.push(row?);
            }
        }
        let selected = seats
            .iter()
            .find(|s| match seat_account {
                // v1.1.1（D69）：先比 seat id，再退回 account uuid（舊呼叫相容）。
                Some(a) => s.0 == a || s.1.as_deref() == Some(a),
                None => true, // first row = most sampled
            })
            .or(seats.first())
            .cloned();
        // v1.8.11（D89）：全量視角——沒有「選中的座位」，下面凡是按座位的查詢都放寬。
        let all = seat_account == Some(ALL_SEATS);
        let (seat_id, seat_acct, seat_org, desktop_n, seat_email, seat_org_name): (String, Option<String>, Option<String>, i64, Option<String>, Option<String>) = if all {
            if seats.is_empty() {
                return Ok(serde_json::json!({ "seats": [], "empty": true }));
            }
            (ALL_SEATS.to_string(), None, None, 0, None, None)
        } else {
            let Some((id, a, o, _, dn, em, on)) = selected else {
                return Ok(serde_json::json!({ "seats": [], "empty": true }));
            };
            (id, a, o, dn, em, on)
        };
        // v1.4.1（D78 #4）：真值資料的實際起點——期間選 180 天但資料只有 20 天時，畫面要說清楚。
        let data_since: Option<String> = conn
            .query_row(
                "SELECT MIN(fetched_at) FROM truth_samples \
                 WHERE (seat_id = ?1 OR ?1 = '*') AND source IN ('probe','cli-cache')",
                params![seat_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        // Desktop-owned seat gets 'claude-desktop' events; the other 'cli'.
        let entrypoint = if all { "all" } else if desktop_n > 0 { "claude-desktop" } else { "cli" };

        mark("seats");
        // ---- interval pairing over the selected period (5h and 7d ledgers) ----
        let since = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        // 全量：每個座位各自配對（跨座位不能配），再合成一疊；下游統計全部吃這一疊。
        let (iv5, iv7) = if all {
            let mut a: Vec<Interval> = Vec::new();
            let mut b: Vec<Interval> = Vec::new();
            for (id, _, _, _, dn, _, _) in &seats {
                let ep = if *dn > 0 { "claude-desktop" } else { "cli" };
                a.extend(Self::pair_intervals(conn, id, "'session','five_hour'", &since, ep)?);
                b.extend(Self::pair_intervals(conn, id, "'weekly_all','seven_day'", &since, ep)?);
            }
            a.sort_by(|x, y| x.t0.cmp(&y.t0));
            b.sort_by(|x, y| x.t0.cmp(&y.t0));
            (a, b)
        } else {
            (
                Self::pair_intervals(conn, &seat_id, "'session','five_hour'", &since, entrypoint)?,
                Self::pair_intervals(conn, &seat_id, "'weekly_all','seven_day'", &since, entrypoint)?,
            )
        };
        let (fx5, months5) = Self::fx_months(&iv5, SEED_5H);
        let (fx7, months7) = Self::fx_months(&iv7, SEED_7D);

        mark("intervals");
        // ---- fixed basket (縮水指數, D64-5): frozen at the first qualified month ----
        let base_month = months5.iter().find(|(_, v)| month_qualified(v)).map(|(m, _)| m.clone());
        let basket = base_month
            .as_ref()
            .and_then(|m| months5.get(m))
            .map(|v| stats::basket_shares(v));
        let basket_est = |s: &[stats::FamIv], b: &[f64; 5]| stats::basket_index(s, b).map(|x| x.index);
        let shrink_monthly: Vec<serde_json::Value> = match &basket {
            Some(b) => months5
                .iter()
                .map(|(m, v)| {
                    let bi = stats::basket_index(v, b);
                    let ci = if month_qualified(v) && bi.is_some() {
                        stats::bootstrap_ci(v, |s| basket_est(s, b), BOOT_REPS, SEED_BASKET ^ month_seed(m))
                    } else {
                        None
                    };
                    serde_json::json!({
                        "month": m,
                        "index": bi.as_ref().map(|x| x.index),
                        "coverage": bi.as_ref().map(|x| x.coverage),
                        "ci": ci.map(|(a, c)| vec![a, c]),
                    })
                })
                .collect(),
            None => Vec::new(),
        };

        mark("basket");
        // ---- verdict: four gates (D64-6 / 規格 §7.3) ----
        let qualified5: Vec<&String> = months5
            .iter()
            .filter(|(_, v)| month_qualified(v) && stats::trimmed_ratio(v).is_some())
            .map(|(m, _)| m)
            .collect();
        let mut gates: Vec<serde_json::Value> = Vec::new();
        let gate = |key: &str, label: &str, pass: bool, detail: String| {
            serde_json::json!({ "key": key, "label": label, "pass": pass, "detail": detail })
        };
        let mut verdict = serde_json::json!({ "declare": false });
        if qualified5.len() < 2 {
            // v1.7（D81）：四項檢查的名字與說明改白話（畫面與匯出報告共用這些字串；
            // 報告的 method 段仍寫術語）。
            gates.push(gate(
                "sample",
                "樣本數",
                false,
                format!("樣本足夠的月份只有 {} 個，比較需要兩個（每月需 {MIN_N} 筆配對、額度累計掉 {MIN_DELTA_SUM:.0}%）", qualified5.len()),
            ));
        } else {
            let cur_m = qualified5[qualified5.len() - 1].clone();
            let prev_m = qualified5[qualified5.len() - 2].clone();
            let cur = &months5[&cur_m];
            let prev = &months5[&prev_m];
            let r_cur = stats::trimmed_ratio(cur).unwrap();
            let r_prev = stats::trimmed_ratio(prev).unwrap();
            let mom_pct = (r_cur / r_prev - 1.0) * 100.0;
            let wallet_pct = (r_prev / r_cur - 1.0) * 100.0;
            gates.push(gate(
                "sample",
                "樣本數",
                true,
                format!("{prev_m} {} 筆 · {cur_m} {} 筆", prev.len(), cur.len()),
            ));
            let t5 = stats::perm_test(prev, cur, stats::trimmed_ratio, PERM_REPS, SEED_PERM);
            let (p5, d5) = match &t5 {
                Some(t) => (Some(t.p), t.diff),
                None => (None, 0.0),
            };
            let pass5 = p5.map(|p| p < ALPHA).unwrap_or(false);
            gates.push(gate(
                "test5h",
                "5h 匯率變化",
                pass5,
                match p5 {
                    Some(p) => format!(
                        "5h 匯率 ${r_prev:.2} → ${r_cur:.2}，差異{}顯著（p={p:.3}）",
                        if pass5 { "夠" } else { "不夠" }
                    ),
                    None => "算不出來（樣本太少）".into(),
                },
            ));
            // 7d cross-check: same direction and past the same gate.
            let (pass7, detail7, p7) = match (months7.get(&prev_m), months7.get(&cur_m)) {
                (Some(a), Some(b)) if month_qualified(a) && month_qualified(b) => {
                    match stats::perm_test(a, b, stats::trimmed_ratio, PERM_REPS, SEED_PERM ^ 7) {
                        Some(t) => {
                            let same = (t.diff > 0.0) == (d5 > 0.0);
                            let ra = stats::trimmed_ratio(a).unwrap_or(0.0);
                            let rb = stats::trimmed_ratio(b).unwrap_or(0.0);
                            (
                                same && t.p < ALPHA,
                                format!(
                                    "7d 匯率 ${ra:.2} → ${rb:.2}，與 5h {}（p={:.3}）",
                                    if same { "同向" } else { "反向" },
                                    t.p
                                ),
                                Some(t.p),
                            )
                        }
                        None => (false, "7d 算不出來（樣本太少）".into(), None),
                    }
                }
                _ => (false, "7d 樣本不夠".into(), None),
            };
            gates.push(gate("cross7d", "7d 方向一致", pass7, detail7));
            // fixed-basket gate
            let (pass_b, detail_b, p_b) = match &basket {
                Some(b) => match stats::perm_test(prev, cur, |s| basket_est(s, b), PERM_REPS, SEED_PERM ^ 11) {
                    Some(t) => {
                        let same = (t.diff > 0.0) == (d5 > 0.0);
                        let ia = basket_est(prev, b).unwrap_or(0.0);
                        let ib = basket_est(cur, b).unwrap_or(0.0);
                        (
                            same && t.p < ALPHA,
                            format!(
                                "固定模型組合的匯率 ${ia:.2} → ${ib:.2}，{}（p={:.3}）",
                                if same { "同向" } else { "反向" },
                                t.p
                            ),
                            Some(t.p),
                        )
                    }
                    None => (false, "有一個月算不出來（單一模型的記錄不夠）".into(), None),
                },
                None => (false, "沒有可當基準的月份".into(), None),
            };
            gates.push(gate("basket", "固定模型組合", pass_b, detail_b));
            let declare = gates.iter().all(|g| g["pass"].as_bool() == Some(true));
            verdict = serde_json::json!({
                "declare": declare,
                "curMonth": cur_m,
                "prevMonth": prev_m,
                "rateCur": r_cur,
                "ratePrev": r_prev,
                "momPct": mom_pct,
                "walletPct": wallet_pct,
                "p5h": p5,
                "p7d": p7,
                "pBasket": p_b,
            });
        }
        verdict["gates"] = serde_json::Value::Array(gates);

        mark("verdict");
        // ---- per-model quota multipliers over the whole period (D55 → M5 → v1.6 D80 回歸) ----
        let all5: Vec<stats::FamIv> = iv5.iter().map(|i| i.iv.clone()).collect();
        let period_trim = stats::trimmed_ratio(&all5);
        let period_raw = stats::ratio(&all5);
        // 回歸吃跟 house 匯率同一套修剪（砍每段匯率下尾 25%／上尾 5%——本機看不到的用量
        // 把低尾污染了），但修剪對象是 Interval 本身，才留得住 by_model。
        let kept: Vec<&Interval> = stats::trim(&iv5, stats::TRIM_LO, stats::TRIM_HI);
        let design = stats::Design::from_sparse(kept.iter().map(|i| (i.by_model.as_slice(), i.iv.delta)));
        let reg = stats::regress(&design, BOOT_REPS, SEED_FAMILY);
        // 家族綜合費率＝各模型合併成一欄再回歸（主人 2026-09-18：四家族，Sonnet 5 與 4.x
        // 同一家）；同一列也掛純區間法當交叉驗證（舊法保留當對照，D80 決策點 4）。
        let fam_reg = stats::regress(&design.collapse_to_groups(), BOOT_REPS, SEED_FAMILY ^ 0x5a5a);
        let fam_rates = stats::group_rates(&all5);
        let families: Vec<serde_json::Value> = (0..stats::GROUPS.len())
            .filter_map(|f| {
                let pure = fam_rates[f];
                let r = fam_reg.rows.iter().find(|r| r.model == stats::GROUPS[f])?;
                if pure.is_none() && r.n_seg == 0 {
                    return None;
                }
                // 一致＝純區間匯率落在回歸區間內，或兩者差不到 25%
                let agree = match (pure, r.rate, r.ci) {
                    (Some((p, _)), Some(rr), ci) => {
                        let in_ci = ci.map(|(lo, hi)| p >= lo && hi.is_none_or(|h| p <= h)).unwrap_or(false);
                        Some(in_ci || (p / rr).ln().abs() < 0.25f64.ln_1p())
                    }
                    _ => None,
                };
                Some(serde_json::json!({
                    "family": stats::GROUPS[f],
                    "pureRate": pure.map(|(r, _)| r),
                    "pureN": pure.map(|(_, n)| n),
                    "regRate": r.rate,
                    "regCi": r.ci.map(|(lo, hi)| vec![Some(lo), hi]),
                    "regStatus": r.status,
                    "zeroShare": r.zero_share,
                    "nSeg": r.n_seg,
                    // v1.8.7（D86 優化 1）：倍率反轉成「這個模型的 $/1% ÷ 期間 $/1%」——
                    // >1＝同樣 1% 額度換到比平均多的 API 用量（划算），量條越長越好。
                    // 以前是倒數（>1＝傷額度），主人說跟「數值大＝用量多＝划算」的直覺相反。
                    "multiplier": match (period_trim, r.rate) {
                        (Some(p), Some(rate)) if r.status == "ok" && p > 0.0 => Some(rate / p),
                        _ => None,
                    },
                    "agree": agree,
                }))
            })
            .collect();
        let rows: Vec<serde_json::Value> = reg
            .rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "model": r.model,
                    "label": stats::model_label(&r.model),
                    "family": r.family,
                    "rate": r.rate,
                    "ci": r.ci.map(|(lo, hi)| vec![Some(lo), hi]),
                    "nSeg": r.n_seg,
                    "zeroShare": r.zero_share,
                    "status": r.status,
                    "collinearWith": r.collinear_with.as_deref().map(stats::model_label),
                    "multiplier": match (period_trim, r.rate) {
                        (Some(p), Some(rate)) if r.status == "ok" && p > 0.0 => Some(rate / p),
                        _ => None,
                    },
                })
            })
            .collect();
        let by_model = serde_json::json!({
            "method": "wnnls",
            "n": reg.n,
            "r2": reg.r2,
            "rows": rows,
            "families": families,
        });

        mark("regression");
        // ---- peak heatmap (local weekday × hour, Σ positive Δ%) + 6h bands ----
        let mut heat = vec![0.0f64; 7 * 24];
        for i in &iv5 {
            heat[(i.weekday * 24 + i.hour) as usize] += i.iv.delta;
        }
        let bands: Vec<serde_json::Value> = (0..4)
            .map(|b| {
                let sub: Vec<stats::FamIv> = iv5
                    .iter()
                    .filter(|i| i.hour / 6 == b)
                    .map(|i| i.iv.clone())
                    .collect();
                serde_json::json!({
                    "label": format!("{:02}–{:02}", b * 6, b * 6 + 6),
                    "n": sub.len(),
                    "rate": if sub.len() >= MIN_N { stats::trimmed_ratio(&sub) } else { None },
                })
            })
            .collect();

        mark("heatmap");
        // ---- truth reconciliation (D64-7): Δ% the local ledger cannot explain ----
        let zero_delta: f64 = all5.iter().filter(|x| x.usd < 0.01).map(|x| x.delta).sum();
        let all_delta: f64 = all5.iter().map(|x| x.delta).sum();
        let unexplained = match (period_raw, period_trim) {
            (Some(r), Some(t)) if t > 0.0 => Some((1.0 - r / t).clamp(0.0, 1.0)),
            _ => None,
        };

        // work rate from the last 30 days of intervals
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(30)).format("%Y-%m").to_string();
        let (mut wd, mut wt, mut wo) = (0.0f64, 0i64, 0i64);
        for iv in iv5.iter().filter(|i| i.month >= cutoff) {
            wd += iv.iv.delta;
            wt += iv.turns;
            wo += iv.out_tokens;
        }

        mark("reconcile");
        // ---- daily USD + walls + gaps + attribution: this seat's tenure only ----
        // v1.8.7（D86 蟲 3）：以前這三塊只按 entrypoint 過濾（兩個座位都有 Desktop 樣本
        // → 都是 'claude-desktop' → 看到同一批事件），切帳號怎麼切都一樣。現在按
        // 「這個座位在 CLI／Desktop 各自登入中的時段」過濾（見 seat_tenure_sync）。
        let since60 = since.clone();
        let tenure = if all { Vec::new() } else { Self::seat_tenure_sync(conn, &seat_id, &since60)? };
        // 全量視角不套登入時段：條件恆真（含最早觀測之前的用量、含雲端 session）。
        let (tenure_ev_sql, tenure_ev_params) = if all { ("1".to_string(), Vec::new()) } else { tenure_where(&tenure, true) };
        let (tenure_ts_sql, tenure_ts_params) = if all { ("1".to_string(), Vec::new()) } else { tenure_where(&tenure, false) };
        let mut daily: Vec<serde_json::Value> = Vec::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT substr(ts,1,10), model, COALESCE(speed,''), COUNT(*), \
                        SUM(input_tokens), SUM(cache_creation), SUM(COALESCE(cache_1h,0)), \
                        SUM(cache_read), SUM(output_tokens), SUM(COALESCE(web_searches,0)) \
                 FROM usage_events WHERE ts >= ?1 AND ({tenure_ev_sql}) \
                 GROUP BY 1, 2, 3 ORDER BY 1"
            ))?;
            let mut per_day: std::collections::BTreeMap<String, f64> = Default::default();
            let mut p: Vec<String> = vec![since60.clone()];
            p.extend(tenure_ev_params.iter().cloned());
            for row in stmt.query_map(rusqlite::params_from_iter(p.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                ))
            })? {
                let (day, model, speed, inp, cw, cw1, cr, out, ws) = row?;
                *per_day.entry(day).or_default() +=
                    price_usd(&model, &speed, inp, cw, cw1, cr, out, ws);
            }
            for (day, usd) in per_day {
                daily.push(serde_json::json!({ "day": day, "usd": (usd * 100.0).round() / 100.0 }));
            }
        }

        let mut walls: Vec<serde_json::Value> = Vec::new();
        {
            // 429 錨點來自本機 JSONL、沒有帳號欄位——用這個座位的登入時段（CLI＋Desktop 聯集）挑。
            let mut stmt = conn.prepare(&format!(
                "SELECT ts, rate_limit_type FROM anchors WHERE ts >= ?1 AND ({tenure_ts_sql}) ORDER BY ts"
            ))?;
            let mut p: Vec<String> = vec![since60.clone()];
            p.extend(tenure_ts_params.iter().cloned());
            for row in stmt.query_map(rusqlite::params_from_iter(p.iter()), |r| {
                Ok(serde_json::json!({
                    "at": r.get::<_, String>(0)?,
                    "kind": r.get::<_, Option<String>>(1)?,
                }))
            })? {
                walls.push(row?);
            }
        }
        // Days where any limit of the selected seat was observed at 100%
        // (truth samples). Web-side 429s never reach local JSONL anchors,
        // but the probe still sees the saturated percent — this is the
        // wall signal a web-only day would otherwise hide.
        let mut saturated_days: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT substr(fetched_at,1,10) FROM truth_samples \
                 WHERE (seat_id = ?1 OR ?1 = '*') AND percent >= 100 AND fetched_at >= ?2 ORDER BY 1",
            )?;
            for row in stmt.query_map(params![seat_id, since60], |r| r.get::<_, String>(0))? {
                saturated_days.push(row?);
            }
        }
        let mut gaps: Vec<serde_json::Value> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT started_at, ended_at FROM gaps WHERE started_at >= ?1 ORDER BY started_at DESC LIMIT 40",
            )?;
            for row in stmt.query_map(params![since60], |r| {
                Ok(serde_json::json!({
                    "from": r.get::<_, String>(0)?,
                    "to": r.get::<_, String>(1)?,
                }))
            })? {
                gaps.push(row?);
            }
        }

        mark("daily");
        // ---- attribution (this seat's tenure, period, by entrypoint) ----
        let since30 = since.clone();
        let mut attribution: Vec<serde_json::Value> = Vec::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT COALESCE(entrypoint,'unknown'), SUM(output_tokens) \
                 FROM usage_events WHERE ts >= ?1 AND ({tenure_ev_sql}) GROUP BY 1 ORDER BY 2 DESC"
            ))?;
            let mut p: Vec<String> = vec![since30.clone()];
            p.extend(tenure_ev_params.iter().cloned());
            for row in stmt.query_map(rusqlite::params_from_iter(p.iter()), |r| {
                Ok(serde_json::json!({
                    "entrypoint": r.get::<_, String>(0)?,
                    "outputTokens": r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                }))
            })? {
                attribution.push(row?);
            }
        }

        mark("attribution");
        Ok(serde_json::json!({
            "seats": seats.iter().map(|(id, a, o, n, dn, em, on)| serde_json::json!({
                "id": id, "accountUuid": a, "orgUuid": o, "samples": n,
                "isDesktop": *dn > 0, "email": em, "orgName": on,
            })).collect::<Vec<_>>(),
            "selected": {
                "id": seat_id, "accountUuid": seat_acct, "orgUuid": seat_org, "entrypoint": entrypoint,
                "email": seat_email, "orgName": seat_org_name,
            },
            "fx5": fx5,
            "fx7": fx7,
            "dataSince": data_since,
            // v1.8.7（D86 蟲 3）：期間消耗／撞牆／來源歸因只算這些登入時段；最早一段之前
            // 的用量沒有帳號依據、不歸任何座位（前端用 from 說清楚）。
            "tenure": {
                "all": all,
                "spans": tenure.len(),
                "cli": tenure.iter().filter(|s| s.entrypoint == "cli").count(),
                "desktop": tenure.iter().filter(|s| s.entrypoint == "claude-desktop").count(),
                "from": tenure.iter().map(|s| s.start.clone()).min(),
            },
            "verdict": verdict,
            "shrink": {
                "baseMonth": base_month,
                "basket": basket.map(|b| (0..5).filter(|&f| b[f] > 0.0005).map(|f| serde_json::json!({
                    "family": stats::FAMILIES[f], "share": b[f],
                })).collect::<Vec<_>>()),
                "monthly": shrink_monthly,
            },
            "byModel": by_model,
            "heat": { "cells": heat, "bands": bands },
            "reconcile": {
                "rateRaw": period_raw,
                "rateTrim": period_trim,
                "unexplainedShare": unexplained,
                "zeroEventDeltaShare": if all_delta > 0.0 { Some(zero_delta / all_delta) } else { None },
                "intervals": all5.len(),
            },
            "method": {
                "trim": [stats::TRIM_LO, stats::TRIM_HI],
                "bootstrapReps": BOOT_REPS,
                "permReps": PERM_REPS,
                "alpha": ALPHA,
                "minN": MIN_N,
                "minDeltaSum": MIN_DELTA_SUM,
                "pureShare": stats::PURE_SHARE,
                "familyMinN": stats::FAMILY_MIN_N,
                "basketMinCoverage": stats::BASKET_MIN_COVERAGE,
                // v1.6（D80）：分模型倍率的回歸參數
                "regression": {
                    "weight": "1/usd",
                    "intercept": false,
                    "minSegments": stats::REG_MIN_SEG,
                    "zeroShareMax": stats::REG_ZERO_SHARE_MAX,
                    "maxCiRatio": stats::REG_MAX_CI_RATIO,
                    "collinearR": stats::REG_COLLINEAR_R,
                },
            },
            "workRate": {
                "turnsPerPercent": if wd >= 3.0 { Some(wt as f64 / wd) } else { None },
                "outputTokensPerPercent": if wd >= 3.0 { Some(wo as f64 / wd) } else { None },
            },
            "daily": daily,
            "walls": walls,
            "saturatedDays": saturated_days,
            "gaps": gaps,
            "attribution": attribution,
        }))
    }

    /// One-stop data-health snapshot for the M1 健康度 page. Returned as
    /// loose JSON — the page renders whatever is here, so the shape can grow
    /// without an IPC/type-mirror dance on every addition.
    pub async fn health_snapshot(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;

        let count = |sql: &str| -> Result<i64> {
            Ok(conn.query_row(sql, [], |r| r.get(0))?)
        };

        let mut by_source = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT source, COUNT(*), MIN(fetched_at), MAX(fetched_at) \
                 FROM truth_samples GROUP BY source ORDER BY source",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "source": r.get::<_, String>(0)?,
                    "count": r.get::<_, i64>(1)?,
                    "first": r.get::<_, Option<String>>(2)?,
                    "last": r.get::<_, Option<String>>(3)?,
                }))
            })?;
            for row in rows {
                by_source.push(row?);
            }
        }

        let mut seats = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT s.account_uuid, s.org_uuid, s.first_seen_at, \
                        (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id), \
                        s.account_email, s.org_name, s.id \
                 FROM seats s ORDER BY s.last_seen_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "accountUuid": r.get::<_, Option<String>>(0)?,
                    "orgUuid": r.get::<_, Option<String>>(1)?,
                    "firstSeenAt": r.get::<_, String>(2)?,
                    "sampleCount": r.get::<_, i64>(3)?,
                    "email": r.get::<_, Option<String>>(4)?,
                    "orgName": r.get::<_, Option<String>>(5)?,
                    "id": r.get::<_, String>(6)?,
                }))
            })?;
            for row in rows {
                seats.push(row?);
            }
        }

        // Coverage: distinct hours holding ≥1 truth sample over the window's
        // total hours — an honest "how much of the timeline do we actually
        // see" number (Desktop off / machine asleep hours count against it).
        let coverage = |days: i64| -> Result<f64> {
            let since = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
            let hours: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT substr(fetched_at, 1, 13)) FROM truth_samples \
                 WHERE fetched_at >= ?1",
                params![since],
                |r| r.get(0),
            )?;
            Ok((hours as f64 / (days as f64 * 24.0)).min(1.0))
        };

        let mut gaps = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT source, started_at, ended_at FROM gaps \
                 ORDER BY started_at DESC LIMIT 20",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "source": r.get::<_, String>(0)?,
                    "startedAt": r.get::<_, String>(1)?,
                    "endedAt": r.get::<_, String>(2)?,
                }))
            })?;
            for row in rows {
                gaps.push(row?);
            }
        }

        let mut top_models = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT model, COUNT(*), SUM(output_tokens) FROM usage_events \
                 GROUP BY model ORDER BY 3 DESC LIMIT 8",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "model": r.get::<_, String>(0)?,
                    "calls": r.get::<_, i64>(1)?,
                    "outputTokens": r.get::<_, i64>(2)?,
                }))
            })?;
            for row in rows {
                top_models.push(row?);
            }
        }

        let doorbell_futile: i64 = conn
            .query_row(
                "SELECT value FROM channel_state WHERE key = 'doorbell_futile_count'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        // v1.8.7（D86 蟲 5）／v1.8.8（D87 按來源分開）：門鈴按了但密碼牌對不上
        // （server.rs 的 401 路徑記的）。回傳每個來源一列，只列還有計數的。
        let mut doorbell_unauthorized: Vec<serde_json::Value> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT substr(key, 23), value FROM channel_state \
                 WHERE key LIKE 'doorbell_unauth_count:%' AND CAST(value AS INTEGER) > 0",
            )?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (source, n) = row?;
                let last: Option<String> = conn
                    .query_row(
                        "SELECT value FROM channel_state WHERE key = ?1",
                        params![format!("doorbell_unauth_last:{source}")],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
                doorbell_unauthorized.push(serde_json::json!({
                    "source": source,
                    "count": n.parse::<i64>().unwrap_or(0),
                    "last": last,
                }));
            }
        }

        Ok(serde_json::json!({
            "doorbellFutileCount": doorbell_futile,
            "doorbellUnauthorized": doorbell_unauthorized,
            "truthSamples": count("SELECT COUNT(*) FROM truth_samples")?,
            "usageEvents": count("SELECT COUNT(*) FROM usage_events")?,
            "anchors": count("SELECT COUNT(*) FROM anchors")?,
            "gapCount": count("SELECT COUNT(*) FROM gaps")?,
            "bySource": by_source,
            "seats": seats,
            "coverage7d": coverage(7)?,
            "coverage30d": coverage(30)?,
            "recentGaps": gaps,
            "topModels": top_models,
            "eventsSpan": {
                "first": conn.query_row("SELECT MIN(ts) FROM usage_events", [], |r| r.get::<_, Option<String>>(0))?,
                "last": conn.query_row("SELECT MAX(ts) FROM usage_events", [], |r| r.get::<_, Option<String>>(0))?,
            },
        }))
    }

    /// Online backup via `VACUUM INTO` — atomic, compact, safe against
    /// concurrent writers. Keeps the newest `keep` files in `dir`.
    pub async fn backup_into(&self, dir: &std::path::Path, keep: usize) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let name = format!(
            "ledger-{}.sqlite",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        );
        let dest = dir.join(&name);
        {
            let conn = self.conn.lock().await;
            conn.execute(
                "VACUUM INTO ?1",
                params![dest.to_string_lossy().to_string()],
            )?;
        }
        // Prune: newest `keep` stay (names sort chronologically).
        let mut backups: Vec<PathBuf> = std::fs::read_dir(dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with("ledger-") && n.ends_with(".sqlite")
                    })
                    .unwrap_or(false)
            })
            .collect();
        backups.sort();
        while backups.len() > keep {
            let old = backups.remove(0);
            let _ = std::fs::remove_file(old);
        }
        Ok(dest)
    }

    /// Batch-insert truth samples inside ONE transaction (the B backfill is
    /// 4,000+ rows — per-row commits would stall the async runtime behind the
    /// connection mutex). Duplicate observations (same seat/kind/scope/source/
    /// fetched_at) are ignored, which makes re-reading a whole channel file
    /// idempotent. Returns the number of rows actually inserted.
    pub async fn insert_truth_samples(&self, samples: &[TruthSample]) -> Result<usize> {
        if samples.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().await;
        let now_iso = chrono::Utc::now().to_rfc3339();
        let tx = conn.unchecked_transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO truth_samples \
                 (id, seat_id, seat_confidence, limit_kind, scope, percent, \
                  resets_at, source, fetched_at, raw_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for s in samples {
                let seat_id = Self::seat_id_sync(
                    &tx,
                    s.account_uuid.as_deref(),
                    s.org_uuid.as_deref(),
                    &now_iso,
                )?;
                let n = stmt.execute(params![
                    uuid::Uuid::new_v4().to_string(),
                    seat_id,
                    s.seat_confidence.as_str(),
                    s.limit_kind,
                    s.scope.as_deref().unwrap_or(""),
                    s.percent as f64,
                    s.resets_at.map(|t| t.to_rfc3339()),
                    s.source.as_str(),
                    s.fetched_at.to_rfc3339(),
                    serde_json::to_string(&s.raw_json).ok(),
                ])?;
                inserted += n;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Record an observation gap (e.g. Desktop was closed between two B
    /// samples). Idempotent on (source, started_at).
    pub async fn insert_gap(
        &self,
        source: &str,
        started_at: &str,
        ended_at: &str,
        note: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO gaps (id, source, started_at, ended_at, note) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                uuid::Uuid::new_v4().to_string(),
                source,
                started_at,
                ended_at,
                note
            ],
        )?;
        Ok(())
    }

    /// Stored (mtime_ms, size, last_offset) for a transcript file, if we've
    /// ingested it before.
    pub async fn get_ingest_file(&self, path: &str) -> Result<Option<(i64, i64, i64)>> {
        let conn = self.conn.lock().await;
        let row = conn
            .query_row(
                "SELECT mtime_ms, size, last_offset FROM ingest_files WHERE path = ?1",
                params![path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        Ok(row)
    }

    /// Apply one parsed transcript file to the ledger in a single
    /// transaction: upsert its usage events (global message.id dedup keeping
    /// max output_tokens — streaming snapshots only carry the real value on
    /// their last line), insert its 429 anchors, and advance the file's
    /// ingest watermark. Returns (events_upserted, anchors_inserted).
    pub async fn apply_file_batch(
        &self,
        batch: &crate::collector::jsonl::FileBatch,
    ) -> Result<(usize, usize)> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        let mut events_n = 0usize;
        let mut anchors_n = 0usize;
        {
            let mut ev = tx.prepare_cached(
                "INSERT INTO usage_events \
                 (msg_id, request_id, ts, model, session_id, agent_id, project_dir, \
                  entrypoint, cc_version, input_tokens, output_tokens, cache_creation, \
                  cache_read, cache_1h, cache_5m, thinking_tokens, service_tier, speed, \
                  web_searches) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19) \
                 ON CONFLICT(msg_id) DO UPDATE SET \
                   output_tokens = excluded.output_tokens, \
                   ts = excluded.ts, \
                   thinking_tokens = excluded.thinking_tokens, \
                   cache_1h = excluded.cache_1h, \
                   cache_5m = excluded.cache_5m, \
                   web_searches = excluded.web_searches \
                 WHERE excluded.output_tokens >= usage_events.output_tokens",
            )?;
            for e in &batch.events {
                events_n += ev.execute(params![
                    e.msg_id,
                    e.request_id,
                    e.ts,
                    e.model,
                    e.session_id,
                    e.agent_id,
                    e.project_dir,
                    e.entrypoint,
                    e.cc_version,
                    e.input_tokens,
                    e.output_tokens,
                    e.cache_creation,
                    e.cache_read,
                    e.cache_1h,
                    e.cache_5m,
                    e.thinking_tokens,
                    e.service_tier,
                    e.speed,
                    e.web_searches,
                ])?;
            }

            let mut an = tx.prepare_cached(
                "INSERT OR IGNORE INTO anchors \
                 (id, ts, session_id, rate_limit_type, resets_at, overage_status, \
                  overage_disabled_reason, content_text, raw_json) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            for a in &batch.anchors {
                anchors_n += an.execute(params![
                    uuid::Uuid::new_v4().to_string(),
                    a.ts,
                    a.session_id,
                    a.rate_limit_type,
                    a.resets_at,
                    a.overage_status,
                    a.overage_disabled_reason,
                    a.content_text,
                    a.raw_json,
                ])?;
            }

            tx.execute(
                "INSERT INTO ingest_files (path, mtime_ms, size, last_offset) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(path) DO UPDATE SET \
                   mtime_ms = excluded.mtime_ms, \
                   size = excluded.size, \
                   last_offset = excluded.last_offset",
                params![batch.path, batch.mtime_ms, batch.size, batch.new_offset],
            )?;
        }
        tx.commit()?;
        Ok((events_n, anchors_n))
    }

    /// Per-channel watermark (e.g. `desktop_last_t` = the largest absorbed
    /// Desktop epoch-ms). None when the channel has never been absorbed.
    pub async fn get_channel_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let v = conn
            .query_row(
                "SELECT value FROM channel_state WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(v)
    }

    pub async fn set_channel_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO channel_state (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// API 價目表 — 匯率粗估的分子（USD / MTok）。
// 來源：docs/research/subscription-vs-api-value.md §2.1（官方定價頁 2026-08-22
// WebFetch 核對），與 tools/analyze-dead-reckoning.cjs 同一份。欄位：
// input / 5m cache write (×1.25) / 1h cache write (×2) / cache read (×0.1) / output。
// web_search $0.01/次；web_fetch $0。價目表變動時兩處要一起改。
// ---------------------------------------------------------------------------
fn price_table(model: &str) -> Option<[f64; 5]> {
    let m = model.to_lowercase();
    Some(if m.contains("fable") {
        [10.0, 12.50, 20.0, 1.00, 50.0]
    } else if m.contains("opus") {
        // opus 家族（含 opus-5）同一價
        [5.0, 6.25, 10.0, 0.50, 25.0]
    } else if m.contains("sonnet-5") || m.contains("sonnet5") {
        [2.0, 2.50, 4.0, 0.20, 10.0]
    } else if m.contains("sonnet") {
        [3.0, 3.75, 6.0, 0.30, 15.0]
    } else if m.contains("haiku") {
        [1.0, 1.25, 2.0, 0.10, 5.0]
    } else {
        return None; // <synthetic> 等
    })
}

/// Price one aggregated event group. `cw` is total cache_creation, `cw1` the
/// 1h-TTL part of it — the 5m part is the difference (same split the analyze
/// script uses). Opus fast mode bills at Fable rates.
#[allow(clippy::too_many_arguments)]
fn price_usd(
    model: &str,
    speed: &str,
    input: i64,
    cw: i64,
    cw1: i64,
    cr: i64,
    out: i64,
    web_searches: i64,
) -> f64 {
    let Some(mut p) = price_table(model) else {
        return 0.0;
    };
    if speed == "fast" && model.to_lowercase().contains("opus") {
        p = [10.0, 12.50, 20.0, 1.00, 50.0];
    }
    let cw5 = (cw - cw1).max(0);
    (input as f64 * p[0]
        + cw5 as f64 * p[1]
        + cw1 as f64 * p[2]
        + cr as f64 * p[3]
        + out as f64 * p[4])
        / 1e6
        + web_searches as f64 * 0.01
}

const SCHEMA_V2: &str = r#"
BEGIN;

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
INSERT INTO meta (key, value) VALUES ('schema_version', '3');

-- 座位＝（帳號, 組織）這一對 (D48 第 4 點). Either half may be unknown while
-- the other is being learned; the pair is what percentages bind to.
CREATE TABLE seats (
    id            TEXT PRIMARY KEY,          -- uuid
    account_uuid  TEXT,
    org_uuid      TEXT,
    account_email TEXT,
    org_name      TEXT,
    first_seen_at TEXT NOT NULL,             -- UTC ISO-8601
    last_seen_at  TEXT NOT NULL,
    UNIQUE(account_uuid, org_uuid)
);

-- One observed data point of one rate limit. limit_kind is an OPEN string
-- set (D46) stored verbatim from the source; scope is '' when unscoped so
-- the UNIQUE dedup key works (SQLite treats NULLs as distinct).
CREATE TABLE truth_samples (
    id              TEXT PRIMARY KEY,        -- uuid
    seat_id         TEXT NOT NULL REFERENCES seats(id),
    seat_confidence TEXT NOT NULL
        CHECK (seat_confidence IN ('exact','inferred','confirmed','conflict')),
    limit_kind      TEXT NOT NULL,
    scope           TEXT NOT NULL DEFAULT '',
    percent         REAL NOT NULL,
    resets_at       TEXT,                    -- UTC ISO-8601, nullable
    source          TEXT NOT NULL,           -- probe / desktop-history / cli-cache / statusline
    fetched_at      TEXT NOT NULL,           -- UTC ISO-8601, origin time
    raw_json        TEXT,
    UNIQUE(seat_id, limit_kind, scope, source, fetched_at)
);
CREATE INDEX idx_truth_fetched ON truth_samples(fetched_at);
CREATE INDEX idx_truth_seat_kind ON truth_samples(seat_id, limit_kind, fetched_at);

-- Deduped API calls from ~/.claude/projects/**/*.jsonl. Global dedup key is
-- message.id; same-id streaming snapshots resolve to max(output_tokens)
-- (recon: 2026-08-27-M0-JSONL讀取器格式規格 §4).
CREATE TABLE usage_events (
    msg_id          TEXT PRIMARY KEY,        -- message.id (msg_*)
    request_id      TEXT,
    ts              TEXT NOT NULL,           -- UTC ISO-8601
    model           TEXT NOT NULL,
    session_id      TEXT,
    agent_id        TEXT,
    project_dir     TEXT,
    entrypoint      TEXT,
    cc_version      TEXT,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_creation  INTEGER NOT NULL DEFAULT 0,
    cache_read      INTEGER NOT NULL DEFAULT 0,
    cache_1h        INTEGER,
    cache_5m        INTEGER,
    thinking_tokens INTEGER,
    service_tier    TEXT,
    speed           TEXT,
    web_searches    INTEGER
);
CREATE INDEX idx_usage_events_ts ON usage_events(ts);
CREATE INDEX idx_usage_events_session ON usage_events(session_id);

-- 429 quota-wall events — first-hand evidence of window boundaries.
CREATE TABLE anchors (
    id                      TEXT PRIMARY KEY, -- uuid
    ts                      TEXT NOT NULL,    -- UTC ISO-8601
    session_id              TEXT,
    rate_limit_type         TEXT,             -- open set; only 'five_hour' observed so far
    resets_at               TEXT,             -- UTC ISO-8601 (from epoch s)
    overage_status          TEXT,
    overage_disabled_reason TEXT,
    content_text            TEXT,
    raw_json                TEXT,
    UNIQUE(session_id, ts)
);

-- Known observation gaps (Desktop closed, machine asleep, …).
CREATE TABLE gaps (
    id         TEXT PRIMARY KEY,             -- uuid
    source     TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at   TEXT NOT NULL,
    note       TEXT,
    UNIQUE(source, started_at)
);

-- Incremental-read watermarks for the JSONL reader (mtime+size+offset model,
-- same shape Claude Code itself uses in .session_cache.json).
CREATE TABLE ingest_files (
    path        TEXT PRIMARY KEY,
    mtime_ms    INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    last_offset INTEGER NOT NULL
);

-- Per-channel absorption watermarks (e.g. desktop_last_t).
CREATE TABLE channel_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

COMMIT;
"#;

// ---------------------------------------------------------------------------
// M6 (D65) — 歷史檢視窗口切分 ＋ 證據包匯出
// ---------------------------------------------------------------------------

/// One reconstructed limit window (D65-4; v1.5／D79 generalised to 5h／7d／
/// Fable). Windows are cut from the truth timeline itself — a % drop of >2
/// points, a silence longer than the window span, span (+tolerance) elapsed
/// since the window's first non-zero sample, or a `resets_at` that jumps by
/// more than 30 min (cli-cache samples carry it; the probe and the Desktop
/// history don't, so those still rely on the drop rule). The first
/// `resets_at` seen inside the window is attached as `reset`.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistWindow {
    pub start: String,
    pub end: String,
    pub reset: Option<String>,
    pub peak: f64,
    pub samples: usize,
    pub sources: Vec<String>,
    pub anchors: usize,
    pub saturated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<(String, f64)>>,
}

/// v1.5（D79）：歷史頁重建哪一條額度的窗口。三條的樣本種類、scope、窗長、
/// 429 錨點類別各不同，集中在這裡，history 與證據匯出共用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistKind {
    FiveHour,
    SevenDay,
    Fable,
}

impl HistKind {
    /// IPC 字串 → kind；不認得的一律當 5h（舊前端沒傳 kind 也走這裡）。
    pub fn parse(s: &str) -> Self {
        match s {
            "7d" => Self::SevenDay,
            "fable" => Self::Fable,
            _ => Self::FiveHour,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::SevenDay => "7d",
            Self::Fable => "fable",
        }
    }
    /// truth_samples.limit_kind 的 SQL IN 清單（D46：開放字串集，兩種拼法並存）。
    fn kinds_sql(self) -> &'static str {
        match self {
            Self::FiveHour => "'session','five_hour'",
            Self::SevenDay => "'weekly_all','seven_day'",
            Self::Fable => "'weekly_scoped'",
        }
    }
    fn scope(self) -> &'static str {
        match self {
            Self::Fable => "Fable",
            _ => "",
        }
    }
    /// 名目窗長。
    pub fn span(self) -> chrono::Duration {
        match self {
            Self::FiveHour => chrono::Duration::hours(5),
            _ => chrono::Duration::hours(24 * 7),
        }
    }
    /// 「從第一筆算起超過窗長多少就一定是下一窗」的寬容：窗的起點是重置後
    /// 第一筆樣本，永遠晚於真正的重置時刻，所以只會低估已過時間。
    fn tolerance(self) -> chrono::Duration {
        match self {
            Self::FiveHour => chrono::Duration::minutes(15),
            _ => chrono::Duration::hours(1),
        }
    }
    /// anchors.rate_limit_type 的篩選。真帳本目前只見過 NULL（06-15～08-23
    /// 的舊回填）與 'five_hour'，7d／Fable 的型別字串是**假設**（Claude Code
    /// 的 rate_limit_type 開放集，週限歷來叫 seven_day、模型限定加後綴）——
    /// 出現新值時錨點會落到哪一頁，看這裡。
    fn anchor_sql(self) -> &'static str {
        match self {
            Self::FiveHour => "(rate_limit_type IS NULL OR rate_limit_type = 'five_hour')",
            Self::SevenDay => "rate_limit_type = 'seven_day'",
            Self::Fable => "(rate_limit_type LIKE 'seven_day%' AND lower(rate_limit_type) LIKE '%fable%')",
        }
    }
}

/// (percent, fetched_at, resets_at, source), ascending by fetched_at.
type SampleRow = (f64, String, Option<String>, String);

/// cli-cache 的 resets_at 在同一窗內會在整點前後 ±1 秒抖（真帳本：
/// 07:59:59.9 ↔ 08:00:00.4 交替），所以「換窗」要差超過這個數才算。
const RESET_JUMP_MIN: i64 = 30;
/// 下降之後往後看這麼多分鐘，期間所有樣本都低才算重置（沒有樣本＝算數）。
/// 30 分鐘：一週額度不可能在半小時內彈回原高度；5h 只有原本就很低（<~17%）
/// 又緊接著猛燒才可能誤合併，可接受。
const LOOKAHEAD_MIN: i64 = 30;
/// 重置時刻跳動要單獨成立，% 得低於這個數（剛換窗不可能已經燒掉一大截）。
const FRESH_WINDOW_MAX_PCT: f64 = 5.0;

fn reset_jumped(known: &Option<String>, new: &Option<String>) -> bool {
    let (Some(a), Some(b)) = (known, new) else { return false };
    let (Ok(a), Ok(b)) = (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) else {
        return false;
    };
    (b - a).num_minutes().abs() > RESET_JUMP_MIN
}

/// 階梯只需要「變了」的點：連續同值只留第一個，最後一筆一定留（階梯要
/// 延伸到窗尾）。7d 的 Desktop 歷史每 10 分鐘一筆、一週幾千筆，去重後
/// 只剩幾十個台階。
fn thin_points(points: Vec<(String, f64)>) -> Vec<(String, f64)> {
    let n = points.len();
    let mut out: Vec<(String, f64)> = Vec::with_capacity(n.min(128));
    for (i, pt) in points.into_iter().enumerate() {
        let changed = out.last().map(|l: &(String, f64)| l.1 != pt.1).unwrap_or(true);
        if changed || i + 1 == n {
            out.push(pt);
        }
    }
    out
}

pub fn segment_windows(rows: &[SampleRow], keep_points: bool, kind: HistKind) -> Vec<HistWindow> {
    struct Cur {
        start: chrono::DateTime<chrono::FixedOffset>,
        last_t: chrono::DateTime<chrono::FixedOffset>,
        last_p: f64,
        peak: f64,
        n: usize,
        reset: Option<String>,
        sources: Vec<String>,
        points: Vec<(String, f64)>,
    }
    let span = kind.span();
    let span_tol = span + kind.tolerance();
    let mut out = Vec::new();
    let mut cur: Option<Cur> = None;
    let flush = |c: Cur, out: &mut Vec<HistWindow>| {
        if c.peak <= 0.0 {
            return; // idle stretch, not a window
        }
        out.push(HistWindow {
            start: c.start.to_rfc3339(),
            end: c.last_t.to_rfc3339(),
            reset: c.reset,
            peak: c.peak,
            samples: c.n,
            sources: c.sources,
            anchors: 0,
            saturated: c.peak >= 100.0,
            points: if keep_points { Some(thin_points(c.points)) } else { None },
        });
    };
    for (i, (p, t, r, s)) in rows.iter().enumerate() {
        let Ok(tt) = chrono::DateTime::parse_from_rfc3339(t) else { continue };
        let mut outlier = false;
        let start_new = match &cur {
            None => true,
            Some(c) => {
                // 下降要「站得住」才算重置：真的重置之後 % 會一直低，而來源之間
                // 打架（探針＋CLI 快取連續半小時報 0、Desktop 同時報 11；真帳本
                // 09-11 早上）過一會就彈回原來的高度。往後看 30 分鐘內的樣本，
                // 全都低才切——用時間不用筆數，兩個來源的取樣節奏才不會左右結果。
                let floor = c.last_p - 2.0;
                let dropped = *p < floor;
                let horizon = tt + chrono::Duration::minutes(LOOKAHEAD_MIN);
                let drop_confirmed = dropped
                    && rows[i + 1..]
                        .iter()
                        .take_while(|n| {
                            chrono::DateTime::parse_from_rfc3339(&n.1)
                                .map(|nt| nt <= horizon)
                                .unwrap_or(false)
                        })
                        .all(|n| n.0 < floor);
                // 重置時刻跳動：真的換窗時 % 一定接近 0；% 還高卻換了重置時刻＝
                // 快取的重置欄位不可信（同一批資料裡出現過 09-13T01 → 09-16T08），
                // 不切、改記新值。
                let jumped = reset_jumped(&c.reset, r);
                let jump_confirmed = jumped && *p <= FRESH_WINDOW_MAX_PCT;
                let cut = drop_confirmed
                    || jump_confirmed
                    || tt - c.last_t > span
                    || tt - c.start > span_tol;
                outlier = !cut && dropped;
                cut
            }
        };
        if start_new {
            if let Some(c) = cur.take() {
                flush(c, &mut out);
            }
            cur = Some(Cur {
                start: tt,
                last_t: tt,
                last_p: *p,
                peak: *p,
                n: 1,
                reset: r.clone(),
                sources: vec![s.clone()],
                points: vec![(t.clone(), *p)],
            });
            continue;
        }
        let c = cur.as_mut().unwrap();
        if !c.sources.contains(s) {
            c.sources.push(s.clone());
        }
        c.n += 1;
        if outlier {
            // 孤立的低值：算樣本，但不讓它拉動窗的尾巴、階梯與峰值。
            continue;
        }
        if c.peak <= 0.0 && *p <= 0.0 {
            // still idle: the window hasn't started — slide the start along
            c.start = tt;
            c.points.clear();
        }
        c.last_t = tt;
        c.last_p = *p;
        c.peak = c.peak.max(*p);
        if c.reset.is_none() || reset_jumped(&c.reset, r) {
            // 第一次看到、或（沒切窗的）跳動＝後來的值比較可信
            c.reset = r.clone();
        }
        c.points.push((t.clone(), *p));
    }
    if let Some(c) = cur.take() {
        flush(c, &mut out);
    }
    out
}

/// Local-time span for the history pager.
fn history_span(unit: &str, offset: i32) -> (chrono::NaiveDate, chrono::NaiveDate, String) {
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    match unit {
        "day" => {
            let d = today + chrono::Duration::days(offset as i64);
            (d, d + chrono::Duration::days(1), d.format("%Y-%m-%d（%a）").to_string())
        }
        "week" => {
            let monday = today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64)
                + chrono::Duration::weeks(offset as i64);
            let sunday = monday + chrono::Duration::days(6);
            (
                monday,
                monday + chrono::Duration::days(7),
                format!("{} – {}", monday.format("%m-%d"), sunday.format("%m-%d")),
            )
        }
        _ => {
            let total = today.year() * 12 + (today.month0() as i32) + offset;
            let (y, m0) = (total.div_euclid(12), total.rem_euclid(12));
            let first = chrono::NaiveDate::from_ymd_opt(y, m0 as u32 + 1, 1).unwrap();
            let next = if m0 == 11 {
                chrono::NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap()
            } else {
                chrono::NaiveDate::from_ymd_opt(y, m0 as u32 + 2, 1).unwrap()
            };
            (first, next, first.format("%Y 年 %m 月").to_string())
        }
    }
}

fn local_midnight_utc(d: chrono::NaiveDate) -> String {
    d.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .earliest()
        .map(|t| t.with_timezone(&chrono::Utc).to_rfc3339())
        .unwrap_or_else(|| format!("{d}T00:00:00+00:00"))
}

fn local_day_of(iso: &str) -> Option<chrono::NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|t| t.with_timezone(&chrono::Local).date_naive())
}

impl Ledger {
    /// Pick the seat the research pages work on: explicit account, else the
    /// most-sampled one. Returns (seat_id, account, org, desktop_sample_count).
    fn select_seat_sync(
        conn: &Connection,
        seat_account: Option<&str>,
    ) -> Result<(Vec<serde_json::Value>, Option<SelectedSeat>)> {
        let mut seats: Vec<SeatRow> = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.account_uuid, s.org_uuid, \
                    (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id), \
                    (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id \
                      AND t.source = 'desktop-history'), \
                    s.account_email, s.org_name \
             FROM seats s ORDER BY 4 DESC",
        )?;
        for row in stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))? {
            seats.push(row?);
        }
        let selected = seats
            .iter()
            .find(|s| match seat_account {
                // v1.1.1（D69）：先比 seat id，再退回 account uuid（舊呼叫相容）。
                Some(a) => s.0 == a || s.1.as_deref() == Some(a),
                None => true,
            })
            .or(seats.first())
            .map(|s| (s.0.clone(), s.1.clone(), s.2.clone(), s.4));
        let seats_json = seats
            .iter()
            .map(|(id, a, o, n, dn, em, on)| {
                serde_json::json!({ "id": id, "accountUuid": a, "orgUuid": o, "samples": n, "isDesktop": *dn > 0, "email": em, "orgName": on })
            })
            .collect();
        Ok((seats_json, selected))
    }

    /// v1.5（D79）：一條額度在 [from, to) 的窗口重建——history 頁與證據匯出
    /// 共用。回 (look-back 起的樣本列, 窗口, 期間內該類 429 時刻)。
    ///
    /// look-back＝窗長＋6h：7d 的窗跨進期間之前就開始了，不往前撈起點與峰值
    /// 都會錯（5h 原本就往前撈 6h 對付跨午夜）。窗口只留「尾巴落在期間內」
    /// 的；階梯點裁到期間內，但保留期間前最後一個台階、時間夾到 from，
    /// 階梯才會從正確的高度進場。
    fn windows_sync(
        conn: &Connection,
        seat_id: &str,
        kind: HistKind,
        from_utc: &str,
        to_utc: &str,
        keep_points: bool,
    ) -> Result<(Vec<SampleRow>, Vec<HistWindow>, Vec<String>)> {
        let from_probe = chrono::DateTime::parse_from_rfc3339(from_utc)
            .map(|t| (t - kind.span() - chrono::Duration::hours(6)).to_rfc3339())
            .unwrap_or_else(|_| from_utc.to_string());

        let mut stmt = conn.prepare(&format!(
            "SELECT percent, fetched_at, resets_at, source FROM truth_samples \
             WHERE seat_id = ?1 AND limit_kind IN ({}) AND scope = ?2 \
               AND fetched_at >= ?3 AND fetched_at < ?4 ORDER BY fetched_at ASC",
            kind.kinds_sql()
        ))?;
        let rows: Vec<SampleRow> = stmt
            .query_map(params![seat_id, kind.scope(), from_probe, to_utc], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut windows: Vec<HistWindow> = segment_windows(&rows, keep_points, kind)
            .into_iter()
            .filter(|w| w.end.as_str() >= from_utc)
            .collect();
        for w in windows.iter_mut() {
            if let Some(pts) = w.points.as_mut() {
                let first_in = pts.iter().position(|(t, _)| t.as_str() >= from_utc).unwrap_or(pts.len());
                if first_in > 0 {
                    let carry = pts[first_in - 1].1;
                    let mut kept: Vec<(String, f64)> = vec![(from_utc.to_string(), carry)];
                    kept.extend(pts.drain(first_in..));
                    *pts = kept;
                }
            }
        }

        let mut anchors: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT ts FROM anchors WHERE {} AND ts >= ?1 AND ts < ?2 ORDER BY ts ASC",
                kind.anchor_sql()
            ))?;
            for row in stmt.query_map(params![from_probe, to_utc], |r| r.get::<_, String>(0))? {
                anchors.push(row?);
            }
        }
        for a in &anchors {
            // attach to the window it fell in (15 min grace after the last sample)
            if let Some(w) = windows.iter_mut().find(|w| {
                let end_grace = chrono::DateTime::parse_from_rfc3339(&w.end)
                    .map(|t| (t + chrono::Duration::minutes(15)).to_rfc3339())
                    .unwrap_or_else(|_| w.end.clone());
                a.as_str() >= w.start.as_str() && a.as_str() <= end_grace.as_str()
            }) {
                w.anchors += 1;
            }
        }
        Ok((rows, windows, anchors))
    }

    /// M6 歷史檢視 payload: one local-time span (day / week / month, paged by
    /// `offset` ≤ 0) of reconstructed windows of ONE limit (v1.5／D79: 5h／7d／
    /// Fable, `kind`) for one seat, plus per-day aggregates. truth_samples +
    /// anchors only (D56: never the transcripts).
    pub async fn history(
        &self,
        seat_account: Option<&str>,
        unit: &str,
        offset: i32,
        kind: HistKind,
    ) -> Result<serde_json::Value> {
        // v1.8.2 效能：走快取（歷史頁本身只要幾十 ms，快取主要是讓翻頁零等待）。
        self.cached(Recipe::History {
            seat: seat_account.map(str::to_owned),
            unit: unit.to_owned(),
            offset,
            kind,
        })
        .await
    }

    fn history_sync(
        conn: &Connection,
        seat_account: Option<&str>,
        unit: &str,
        offset: i32,
        kind: HistKind,
    ) -> Result<serde_json::Value> {
        let (seats, selected) = Self::select_seat_sync(conn, seat_account)?;
        // v1.8.11（D89）：全量視角——每個座位各自重建窗口，合成一條時間軸（窗口帶 seat）。
        let all = seat_account == Some(ALL_SEATS);
        let (seat_id, seat_acct, seat_org): (String, Option<String>, Option<String>) = if all {
            if seats.is_empty() {
                return Ok(serde_json::json!({ "seats": [], "empty": true }));
            }
            (ALL_SEATS.to_string(), None, None)
        } else {
            let Some((id, a, o, _)) = selected else {
                return Ok(serde_json::json!({ "seats": [], "empty": true }));
            };
            (id, a, o)
        };
        let (from_d, to_d, label) = history_span(unit, offset.min(0));
        let from_utc = local_midnight_utc(from_d);
        let to_utc = local_midnight_utc(to_d);
        // 階梯點：5h 只在日檢視畫（一週幾十個窗擠不下）；7d／Fable 一週才一個
        // 窗、爬升就是重點，三個尺度都畫（去重後每窗只剩台階數個點）。
        let keep_points = unit == "day" || kind != HistKind::FiveHour;
        let (rows, windows, anchors, windows_json): (Vec<SampleRow>, Vec<HistWindow>, Vec<String>, Vec<serde_json::Value>) = if all {
            let mut rows = Vec::new();
            let mut wins = Vec::new();
            let mut anchors = Vec::new();
            let mut wj = Vec::new();
            for s in &seats {
                let Some(id) = s["id"].as_str() else { continue };
                let (r, w, a) = Self::windows_sync(conn, id, kind, &from_utc, &to_utc, keep_points)?;
                for win in &w {
                    let mut v = serde_json::to_value(win)?;
                    v["seat"] = serde_json::Value::String(id.to_string());
                    wj.push(v);
                }
                rows.extend(r);
                wins.extend(w);
                anchors = a; // 429 錨點不分座位，每次回的都一樣
            }
            wins.sort_by(|x, y| x.start.cmp(&y.start));
            wj.sort_by(|x, y| x["start"].as_str().cmp(&y["start"].as_str()));
            (rows, wins, anchors, wj)
        } else {
            let (r, w, a) = Self::windows_sync(conn, &seat_id, kind, &from_utc, &to_utc, keep_points)?;
            let wj = w.iter().map(serde_json::to_value).collect::<std::result::Result<Vec<_>, _>>()?;
            (r, w, a, wj)
        };

        // 三條額度在期間內的峰值（脈絡列：看 7d 時也知道 5h／Fable 到哪）
        let peak_of = |k: HistKind| -> Result<Option<f64>> {
            Ok(conn.query_row(
                &format!(
                    "SELECT MAX(percent) FROM truth_samples WHERE (seat_id = ?1 OR ?1 = '*') \
                     AND limit_kind IN ({}) AND scope = ?2 AND fetched_at >= ?3 AND fetched_at < ?4",
                    k.kinds_sql()
                ),
                params![seat_id, k.scope(), from_utc, to_utc],
                |r| r.get::<_, Option<f64>>(0),
            )?)
        };
        let peaks = serde_json::json!({
            "5h": peak_of(HistKind::FiveHour)?,
            "7d": peak_of(HistKind::SevenDay)?,
            "fable": peak_of(HistKind::Fable)?,
        });

        // per local day aggregates
        let mut days: Vec<serde_json::Value> = Vec::new();
        let mut d = from_d;
        while d < to_d {
            let day_rows: Vec<&SampleRow> = rows
                .iter()
                .filter(|r| local_day_of(&r.1) == Some(d))
                .collect();
            let peak = day_rows.iter().map(|r| r.0).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |x| x.max(v))));
            let n_win = windows.iter().filter(|w| local_day_of(&w.start) == Some(d)).count();
            let n_anchor = anchors.iter().filter(|a| local_day_of(a) == Some(d)).count();
            days.push(serde_json::json!({
                "day": d.to_string(),
                "peak": peak,
                "samples": day_rows.len(),
                "windows": n_win,
                "anchors": n_anchor,
                "saturated": peak.map(|p| p >= 100.0).unwrap_or(false),
            }));
            d += chrono::Duration::days(1);
        }

        let sat = windows.iter().filter(|w| w.saturated).count();
        let samples: usize = rows.iter().filter(|r| r.1.as_str() >= from_utc.as_str()).count();

        // v1.8.8（D87，主人：歷史頁也要看得到模型與專案分佈）：期間內本機事件按模型／
        // 專案分，只計這個座位登入中的時段（規則同趨勢頁 D86）。
        let tenure = if all { Vec::new() } else { Self::seat_tenure_sync(conn, &seat_id, &from_utc)? };
        let (tsql, tparams) = if all { ("1".to_string(), Vec::new()) } else { tenure_where(&tenure, true) };
        let dist = |sql_col: &str, key: &str, limit: usize| -> Result<Vec<serde_json::Value>> {
            let mut stmt = conn.prepare(&format!(
                "SELECT COALESCE({sql_col},'?'), COUNT(*), SUM(output_tokens) \
                 FROM usage_events WHERE ts >= ?1 AND ts < ?2 AND ({tsql}) \
                 GROUP BY 1 ORDER BY 3 DESC LIMIT {limit}"
            ))?;
            let mut p: Vec<String> = vec![from_utc.clone(), to_utc.clone()];
            p.extend(tparams.iter().cloned());
            let mut out = Vec::new();
            for row in stmt.query_map(rusqlite::params_from_iter(p.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0)))
            })? {
                let (name, calls, out_tokens) = row?;
                let mut v = serde_json::json!({ key: name, "calls": calls, "outputTokens": out_tokens });
                if key == "model" {
                    v["label"] = serde_json::Value::String(crate::stats::model_label(&name));
                }
                out.push(v);
            }
            Ok(out)
        };
        let usage = serde_json::json!({
            "byModel": dist("model", "model", 8)?,
            "byProject": dist("project_dir", "project", 8)?,
            "tenureSpans": tenure.len(),
            "all": all,
        });

        Ok(serde_json::json!({
            "unit": unit,
            "offset": offset.min(0),
            "kind": kind.key(),
            "usage": usage,
            "label": label,
            "from": from_utc,
            "to": to_utc,
            "fromLocal": from_d.to_string(),
            "toLocal": to_d.to_string(),
            "seats": seats,
            "selected": { "id": seat_id, "accountUuid": seat_acct, "orgUuid": seat_org },
            "windows": windows_json,
            "days": days,
            "peaks": peaks,
            "totals": {
                "samples": samples,
                "windows": windows.len(),
                "saturated": sat,
                "anchors": anchors.iter().filter(|a| a.as_str() >= from_utc.as_str()).count(),
            },
        }))
    }

    /// M6 證據包 (D65-2): JSON + two CSVs + Markdown, written by the ledger
    /// itself so the numbers never pass through the UI. Returns the paths.
    pub async fn export_evidence(
        &self,
        seat_account: Option<&str>,
        days: u32,
        dir: &std::path::Path,
        app_version: &str,
    ) -> Result<Vec<PathBuf>> {
        use std::fmt::Write as _;
        let dash = self.dashboard(seat_account, days).await?;
        if dash.get("empty").and_then(|v| v.as_bool()) == Some(true) {
            anyhow::bail!("帳本裡還沒有任何座位，沒東西可匯出");
        }
        // raw intervals — the re-computable evidence
        let all = seat_account == Some(ALL_SEATS);
        let intervals: Vec<Interval> = {
            let conn = self.conn.lock().await;
            let (seats_json, selected) = Self::select_seat_sync(&conn, seat_account)?;
            let d = if days == 0 { 3650 } else { days.clamp(7, 3650) };
            let since = (chrono::Utc::now() - chrono::Duration::days(d as i64)).to_rfc3339();
            if all {
                // v1.8.11：全量＝每個座位各自配對再合起來（同趨勢頁）
                let mut v = Vec::new();
                for s in &seats_json {
                    let Some(id) = s["id"].as_str() else { continue };
                    let ep = if s["isDesktop"].as_bool().unwrap_or(false) { "claude-desktop" } else { "cli" };
                    v.extend(Self::pair_intervals(&conn, id, "'session','five_hour'", &since, ep)?);
                }
                v.sort_by(|x, y| x.t0.cmp(&y.t0));
                v
            } else {
                let (seat_id, _, _, desktop_n) = selected.unwrap();
                let entrypoint = if desktop_n > 0 { "claude-desktop" } else { "cli" };
                Self::pair_intervals(&conn, &seat_id, "'session','five_hour'", &since, entrypoint)?
            }
        };

        std::fs::create_dir_all(dir)?;
        let now = chrono::Local::now();
        let stamp = now.format("%Y%m%d-%H%M").to_string();
        let seat8 = dash["selected"]["accountUuid"]
            .as_str()
            .map(|s| s.chars().take(8).collect::<String>())
            .unwrap_or_else(|| if all { "all-accounts".into() } else { "unknown".into() });
        let period = if days == 0 { "all".to_string() } else { format!("{days}d") };
        let base = format!("evidence-{seat8}-{period}-{stamp}");
        let generated_at = now.to_rfc3339();

        // ---- JSON ----
        let fields = serde_json::json!({
            "fx5[].rate": "修剪 Δ-加權匯率（USD 每 1% 5h 額度）：砍每段區間匯率下尾 25%／上尾 5% 後 Σusd÷Σδ",
            "fx5[].rateRaw": "未修剪 Σusd÷Σδ",
            "fx5[].ci": "rate 的 bootstrap 95% 信心區間 [lo, hi]（區間層級重抽，固定種子）",
            "fx5[].samples": "該月配對區間數 n",
            "fx5[].deltaSum": "該月 Δ% 合計（分母）",
            "fx5[].usdSum": "該月本機事件 API 計價 USD 合計（分子）",
            "fx5[].qualified": "是否達樣本閘（n≥minN 且 Σδ≥minDeltaSum）",
            "fx7": "同 fx5，但分母是 7d 週額度的 Δ%",
            "verdict.declare": "四閘全過才 true；gates[] 逐閘 pass 與說明",
            "verdict.momPct": "匯率方向：本月匯率÷上月−1（正＝1% 買到更多）",
            "verdict.walletPct": "荷包方向：上月匯率÷本月−1（正＝同樣工作要多花額度）",
            "shrink": "固定籃指數：籃＝基期月各模型家族 API$ 佔比；index＝1÷Σ(w_m÷r_m)",
            "byModel": "分模型倍率（v1.6 回歸法）：rows[] 每個模型 id 一列——rate＝$/1%（加權 NNLS 係數的倒數，修剪後區間、w=1/usd、無截距）、ci＝bootstrap 95%（上界 null＝碰到 β→0 無上界）、nSeg 有花錢的段數、zeroShare bootstrap 卡 0 的比例、status ok/calibrating/collinear、multiplier＝rate÷期間修剪匯率（v1.8.7 起；>1＝同樣 1% 額度換到的 API 用量比平均多、較划算，只有 ok 才給；v1.6～v1.8.6 是倒數）；families[] 家族層級的純區間法 vs 回歸交叉驗證（agree）",
            "reconcile.unexplainedShare": "1−rateRaw÷rateTrim：找不到本機事件的 Δ% 佔比（推估）",
            "intervals[]": "每段配對區間：t0/t1 真值時刻、deltaPct、usd（本機事件 API 計價）、byFamily 各家族 USD",
            "windows.<5h|7d|fable>[]": "期間內重建的額度窗口（與歷史頁同一套切法）：start/end 首末樣本、reset 重置時刻（CLI 快取才有）、peak 峰值 %、samples、anchors 窗內 429、saturated 見頂",
            "method": "所有門檻與重抽次數，與畫面方法註記同源",
        });
        // v1.5（D79）：歷史頁三窗的重建同步進證據包——見頂幾次、每窗峰值是
        // 「額度縮水」最直白的證據。與歷史頁走同一個 windows_sync。
        let windows_by_kind: Vec<(HistKind, Vec<HistWindow>)> = {
            let conn = self.conn.lock().await;
            let seat_id = dash["selected"]["id"].as_str().unwrap_or_default().to_string();
            let d = if days == 0 { 3650 } else { days.clamp(7, 3650) };
            let now_utc = chrono::Utc::now();
            let from_utc = (now_utc - chrono::Duration::days(d as i64)).to_rfc3339();
            let to_utc = now_utc.to_rfc3339();
            let mut v = Vec::new();
            for k in [HistKind::FiveHour, HistKind::SevenDay, HistKind::Fable] {
                if all {
                    // v1.8.11：全量＝每個座位各自重建再合起來
                    let mut merged: Vec<HistWindow> = Vec::new();
                    for s in dash["seats"].as_array().into_iter().flatten() {
                        let Some(id) = s["id"].as_str() else { continue };
                        let (_, w, _) = Self::windows_sync(&conn, id, k, &from_utc, &to_utc, false)?;
                        merged.extend(w);
                    }
                    merged.sort_by(|x, y| x.start.cmp(&y.start));
                    v.push((k, merged));
                } else {
                    let (_, w, _) = Self::windows_sync(&conn, &seat_id, k, &from_utc, &to_utc, false)?;
                    v.push((k, w));
                }
            }
            v
        };
        let windows_json: serde_json::Map<String, serde_json::Value> = windows_by_kind
            .iter()
            .map(|(k, w)| (k.key().to_string(), serde_json::to_value(w).unwrap_or_default()))
            .collect();
        let iv_json: Vec<serde_json::Value> = intervals
            .iter()
            .map(|i| {
                serde_json::json!({
                    "t0": i.t0, "t1": i.t1, "deltaPct": i.iv.delta, "usd": i.iv.usd,
                    "byFamily": (0..5).map(|f| (crate::stats::FAMILIES[f].to_string(), serde_json::json!(i.iv.by_family[f]))).collect::<serde_json::Map<String, serde_json::Value>>(),
                })
            })
            .collect();
        let json = serde_json::json!({
            "schema": "cum-evidence/1",
            "generatedAt": generated_at,
            "appVersion": app_version,
            "periodDays": days,
            "fields": fields,
            "seat": dash["selected"],
            "fx5": dash["fx5"], "fx7": dash["fx7"], "verdict": dash["verdict"], "shrink": dash["shrink"],
            "byModel": dash["byModel"], "reconcile": dash["reconcile"], "method": dash["method"],
            "workRate": dash["workRate"], "walls": dash["walls"], "saturatedDays": dash["saturatedDays"],
            "intervals": iv_json,
            "windows": windows_json,
        });
        let mut paths = Vec::new();
        let p = dir.join(format!("{base}.json"));
        std::fs::write(&p, serde_json::to_string_pretty(&json)?)?;
        paths.push(p);

        // ---- monthly.csv ----
        let mut csv = String::from("window,month,samples,deltaSum,usdSum,rateRaw,rateTrim,ciLo,ciHi,qualified\n");
        for (win, key) in [("5h", "fx5"), ("7d", "fx7")] {
            for m in dash[key].as_array().into_iter().flatten() {
                let f = |k: &str| m[k].as_f64().map(|v| format!("{v:.4}")).unwrap_or_default();
                let ci = m["ci"].as_array();
                let _ = writeln!(
                    csv,
                    "{win},{},{},{},{},{},{},{},{},{}",
                    m["month"].as_str().unwrap_or(""),
                    m["samples"].as_u64().unwrap_or(0),
                    f("deltaSum"),
                    f("usdSum"),
                    f("rateRaw"),
                    f("rate"),
                    ci.and_then(|c| c.first()).and_then(|v| v.as_f64()).map(|v| format!("{v:.4}")).unwrap_or_default(),
                    ci.and_then(|c| c.get(1)).and_then(|v| v.as_f64()).map(|v| format!("{v:.4}")).unwrap_or_default(),
                    m["qualified"].as_bool().unwrap_or(false),
                );
            }
        }
        let p = dir.join(format!("{base}-monthly.csv"));
        std::fs::write(&p, csv)?;
        paths.push(p);

        // ---- intervals.csv ----
        let mut csv = String::from("t0,t1,deltaPct,usd,fable,opus,sonnet5,sonnet,haiku\n");
        for i in &intervals {
            let b = &i.iv.by_family;
            let _ = writeln!(
                csv,
                "{},{},{:.2},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4}",
                i.t0, i.t1, i.iv.delta, i.iv.usd, b[0], b[1], b[2], b[3], b[4]
            );
        }
        let p = dir.join(format!("{base}-intervals.csv"));
        std::fs::write(&p, csv)?;
        paths.push(p);

        // ---- windows.csv（v1.5／D79）----
        let mut csv = String::from("limit,start,end,reset,peak,samples,anchors,saturated,sources\n");
        for (k, ws) in &windows_by_kind {
            for w in ws {
                let _ = writeln!(
                    csv,
                    "{},{},{},{},{:.0},{},{},{},{}",
                    k.key(),
                    w.start,
                    w.end,
                    w.reset.as_deref().unwrap_or(""),
                    w.peak,
                    w.samples,
                    w.anchors,
                    w.saturated,
                    w.sources.join("+"),
                );
            }
        }
        let p = dir.join(format!("{base}-windows.csv"));
        std::fs::write(&p, csv)?;
        paths.push(p);

        // ---- report.md ----
        let p = dir.join(format!("{base}.md"));
        let mut md = evidence_markdown(&dash, days, intervals.len(), app_version, &generated_at);
        md.push_str(&windows_markdown(&windows_by_kind));
        std::fs::write(&p, md)?;
        paths.push(p);
        Ok(paths)
    }
}

/// v1.5（D79）：證據報告的「額度窗口」段——三條額度各幾個窗、見頂幾次、
/// 最高峰值、窗內 429。數字與歷史頁同源（同一個 windows_sync）。
fn windows_markdown(by_kind: &[(HistKind, Vec<HistWindow>)]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "\n## 額度窗口（重建）\n");
    let _ = writeln!(s, "與歷史檢視頁同一套切法：真值 % 下降 >2、靜默超過窗長、窗長到期、或重置時刻跳動即為新窗；峰值＝窗內最高真值。\n");
    let _ = writeln!(s, "| 額度 | 窗口數 | 見頂窗（100%） | 最高峰值 | 窗內 429 | 帶重置時刻的窗 |\n|---|---|---|---|---|---|");
    for (k, ws) in by_kind {
        let name = match k {
            HistKind::FiveHour => "5h",
            HistKind::SevenDay => "7d",
            HistKind::Fable => "Fable 7d",
        };
        let peak = ws.iter().map(|w| w.peak).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |x| x.max(v))));
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} |",
            name,
            ws.len(),
            ws.iter().filter(|w| w.saturated).count(),
            peak.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "—".into()),
            ws.iter().map(|w| w.anchors).sum::<usize>(),
            ws.iter().filter(|w| w.reset.is_some()).count(),
        );
    }
    let _ = writeln!(s, "\n逐窗明細在 `-windows.csv`（limit／start／end／reset／peak／samples／anchors／saturated／sources）。");
    s
}

/// The paste-able report (D65-2). Every number comes from the dashboard
/// payload — the same one the UI renders — so report and screen never drift.
fn evidence_markdown(dash: &serde_json::Value, days: u32, n_intervals: usize, app_version: &str, generated_at: &str) -> String {
    use std::fmt::Write as _;
    let f2 = |v: &serde_json::Value| v.as_f64().map(|x| format!("{x:.2}")).unwrap_or_else(|| "—".into());
    let mut s = String::new();
    let seat = dash["selected"]["accountUuid"].as_str().map(|a| a.chars().take(8).collect::<String>()).unwrap_or_default();
    let ep = if dash["selected"]["entrypoint"] == "claude-desktop" { "Desktop" } else { "CLI" };
    let period = if days == 0 { "全部資料".to_string() } else { format!("近 {days} 天") };
    let _ = writeln!(s, "# Claude 訂閱額度匯率 · 證據報告\n");
    let _ = writeln!(s, "- 產生時間：{generated_at}（Claude Usage Monitor v{app_version}）");
    let _ = writeln!(s, "- 座位：{seat}（{ep} 帳號）· 期間：{period} · 配對區間 {n_intervals} 段");
    let _ = writeln!(s, "- 匯率 R ＝「1% 的 5h 額度約當多少美金 API 用量」。**R 下降＝同樣 1% 買到的變少＝對使用者是壞消息。**\n");

    let v = &dash["verdict"];
    let _ = writeln!(s, "## 結論\n");
    if v["declare"].as_bool() == Some(true) {
        let wallet = v["walletPct"].as_f64().unwrap_or(0.0);
        let _ = writeln!(
            s,
            "**{} 月比 {} 月{} {:.1}%**（匯率 ${} → ${}，四道閘全過：樣本、5h 置換檢定、7d 雙視窗同向、固定籃指數）。\n",
            v["curMonth"].as_str().map(|m| &m[5..]).unwrap_or(""),
            v["prevMonth"].as_str().map(|m| &m[5..]).unwrap_or(""),
            if wallet > 0.0 { "貴" } else { "便宜" },
            wallet.abs(),
            f2(&v["ratePrev"]),
            f2(&v["rateCur"]),
        );
    } else {
        let failed = v["gates"].as_array().and_then(|g| g.iter().find(|x| x["pass"].as_bool() != Some(true)));
        let _ = writeln!(
            s,
            "**不宣告縮水或放寬。** 卡在「{}」——{}。\n",
            failed.and_then(|g| g["label"].as_str()).unwrap_or("檢定"),
            failed.and_then(|g| g["detail"].as_str()).unwrap_or(""),
        );
        if let (Some(mom), Some(wallet)) = (v["momPct"].as_f64(), v["walletPct"].as_f64()) {
            let _ = writeln!(
                s,
                "資料本身：匯率 ${} → ${}，較上月{} {:.1}%（同樣的工作要{}花 {:.1}% 的額度）——未過檢定，當雜訊看。\n",
                f2(&v["ratePrev"]), f2(&v["rateCur"]),
                if mom < 0.0 { "跌" } else { "升" }, mom.abs(),
                if wallet > 0.0 { "多" } else { "少" }, wallet.abs(),
            );
        }
    }
    if let Some(gates) = v["gates"].as_array() {
        let _ = writeln!(s, "| 閘 | 結果 | 說明 |\n|---|---|---|");
        for g in gates {
            let _ = writeln!(
                s,
                "| {} | {} | {} |",
                g["label"].as_str().unwrap_or(""),
                if g["pass"].as_bool() == Some(true) { "✅ 過" } else { "❌ 未過" },
                g["detail"].as_str().unwrap_or("")
            );
        }
        s.push('\n');
    }

    for (title, key, unit) in [("5h 額度月匯率", "fx5", "5h"), ("7d 額度月匯率（只比方向）", "fx7", "7d")] {
        let _ = writeln!(s, "## {title}\n");
        let _ = writeln!(s, "| 月 | n | Δ% 合計 | USD 合計 | 未修剪 | 修剪匯率 $/1% {unit} | 95% CI | 樣本閘 |\n|---|---|---|---|---|---|---|---|");
        for m in dash[key].as_array().into_iter().flatten() {
            let ci = m["ci"].as_array().map(|c| format!("{}–{}", f2(&c[0]), f2(&c[1]))).unwrap_or_else(|| "—".into());
            let _ = writeln!(
                s,
                "| {} | {} | {:.0} | {:.0} | {} | {} | {} | {} |",
                m["month"].as_str().unwrap_or(""),
                m["samples"].as_u64().unwrap_or(0),
                m["deltaSum"].as_f64().unwrap_or(0.0),
                m["usdSum"].as_f64().unwrap_or(0.0),
                f2(&m["rateRaw"]),
                f2(&m["rate"]),
                ci,
                if m["qualified"].as_bool() == Some(true) { "過" } else { "校準中" },
            );
        }
        s.push('\n');
    }

    let sh = &dash["shrink"];
    if let Some(basket) = sh["basket"].as_array() {
        let b: Vec<String> = basket
            .iter()
            .map(|x| format!("{} {:.0}%", x["family"].as_str().unwrap_or(""), x["share"].as_f64().unwrap_or(0.0) * 100.0))
            .collect();
        let _ = writeln!(s, "## 固定籃指數（縮水指數）\n");
        let _ = writeln!(s, "籃＝{} 月的模型組合（{}），凍結不動；展示匯率跟著用法走，固定籃只看廠商給的有沒有變。\n", sh["baseMonth"].as_str().map(|m| &m[5..]).unwrap_or(""), b.join("／"));
        let _ = writeln!(s, "| 月 | 指數 $/1% | 95% CI | 籃覆蓋 |\n|---|---|---|---|");
        for m in sh["monthly"].as_array().into_iter().flatten() {
            let ci = m["ci"].as_array().map(|c| format!("{}–{}", f2(&c[0]), f2(&c[1]))).unwrap_or_else(|| "—".into());
            let _ = writeln!(s, "| {} | {} | {} | {} |", m["month"].as_str().unwrap_or(""), f2(&m["index"]), ci,
                m["coverage"].as_f64().map(|c| format!("{:.0}%", c * 100.0)).unwrap_or_else(|| "—".into()));
        }
        s.push('\n');
    }

    if let Some(rows) = dash["byModel"]["rows"].as_array().filter(|a| !a.is_empty()) {
        let bm = &dash["byModel"];
        let _ = writeln!(s, "## 模型別額度倍率（回歸法，v1.6）\n");
        let _ = writeln!(
            s,
            "加權非負最小平方：每段區間 Δ% ＝ Σ 各模型 API$ × 係數（係數 ≥0、無截距、權重 1/usd、與匯率同一套修剪）；$/1% ＝ 係數倒數。修剪後 n={}，R²（無截距）{}。\n",
            bm["n"].as_u64().unwrap_or(0),
            f2(&bm["r2"])
        );
        let ci_of = |m: &serde_json::Value, key: &str| {
            m[key].as_array().map(|c| {
                let hi = c.get(1).and_then(|v| v.as_f64()).map(|v| format!("{v:.2}")).unwrap_or_else(|| "∞".into());
                format!("{}–{}", f2(&c[0]), hi)
            }).unwrap_or_else(|| "—".into())
        };
        let status_of = |st: &str, with: Option<&str>| match st {
            "ok" => "✅".to_string(),
            "collinear" => format!("共線（與 {} 一起看）", with.unwrap_or("?")),
            _ => "校準中".to_string(),
        };
        let mult_of = |m: &serde_json::Value| m["multiplier"].as_f64().map(|v| format!("{v:.2}×")).unwrap_or_else(|| "—".into());
        let fam_label = |f: &str| -> String {
            match f { "fable" => "Fable", "opus" => "Opus", "sonnet" => "Sonnet", "haiku" => "Haiku", other => other }.to_string()
        };
        let _ = writeln!(s, "| 模型 | $/1% | 95% CI | 有花錢的段數 | 狀態 | 每 1% 額度換到的 API 用量（vs 平均，>1 較划算） |\n|---|---|---|---|---|---|");
        // 家族綜合費率（各模型合併再回歸）粗體一列，底下縮排該家族的模型
        let empty = Vec::new();
        let fams = bm["families"].as_array().unwrap_or(&empty);
        for fam in ["fable", "opus", "sonnet", "haiku"] {
            let members: Vec<&serde_json::Value> = rows.iter().filter(|m| m["family"].as_str() == Some(fam)).collect();
            let f = fams.iter().find(|x| x["family"].as_str() == Some(fam));
            if members.is_empty() && f.is_none() {
                continue;
            }
            if let Some(f) = f {
                let agree = match f["agree"].as_bool() {
                    Some(true) => "、純區間法一致",
                    Some(false) => "、與純區間法不一致",
                    None => "",
                };
                let _ = writeln!(
                    s,
                    "| **{}（合併）** | **{}** | {} | {} | {}{} | **{}** |",
                    fam_label(fam),
                    f2(&f["regRate"]),
                    ci_of(f, "regCi"),
                    f["nSeg"].as_u64().unwrap_or(0),
                    status_of(f["regStatus"].as_str().unwrap_or(""), None),
                    agree,
                    mult_of(f)
                );
            }
            for m in members {
                let _ = writeln!(
                    s,
                    "| ・{} | {} | {} | {} | {} | {} |",
                    m["label"].as_str().unwrap_or(""),
                    f2(&m["rate"]),
                    ci_of(m, "ci"),
                    m["nSeg"].as_u64().unwrap_or(0),
                    status_of(m["status"].as_str().unwrap_or(""), m["collinearWith"].as_str()),
                    mult_of(m)
                );
            }
        }
        if !fams.is_empty() {
            let _ = writeln!(s, "\n交叉驗證（家族層級：純區間法 vs 回歸）：");
            for f in fams {
                let agree = match f["agree"].as_bool() {
                    Some(true) => "一致",
                    Some(false) => "不一致",
                    None => "純區間不足，無法對照",
                };
                let _ = writeln!(
                    s,
                    "- {}：純區間 ${}（n={}）· 回歸 ${} → {}",
                    fam_label(f["family"].as_str().unwrap_or("")),
                    f2(&f["pureRate"]),
                    f["pureN"].as_u64().unwrap_or(0),
                    f2(&f["regRate"]),
                    agree
                );
            }
        }
        s.push('\n');
    }

    let rc = &dash["reconcile"];
    let _ = writeln!(s, "## 真值對帳\n");
    let _ = writeln!(
        s,
        "期間未修剪匯率 ${}、修剪匯率 ${}；找不到本機事件的 Δ% 佔比（推估）{}，其中完全零事件的區間佔 {}。\n",
        f2(&rc["rateRaw"]), f2(&rc["rateTrim"]),
        rc["unexplainedShare"].as_f64().map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "—".into()),
        rc["zeroEventDeltaShare"].as_f64().map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "—".into()),
    );

    let m = &dash["method"];
    let _ = writeln!(s, "## 方法與已知弱點\n");
    let _ = writeln!(s, "- 分子＝本機 Claude 對話記錄（JSONL）的 API 計價 USD（官方價目表，含快取寫入／讀取、web search）；分母＝訂閱額度真值百分比的 Δ%（探針 `/usage`、CLI 快取、Desktop 歷史檔），同一座位、≤45 分鐘、嚴格遞增的相鄰樣本配成一段區間。");
    let _ = writeln!(s, "- 估計量＝Δ-加權比值 Σusd÷Σδ；非對稱修剪：砍每段區間匯率下尾 {:.0}%／上尾 {:.0}%（下尾被「本機看不到的用量」污染）。", m["trim"][0].as_f64().unwrap_or(0.25) * 100.0, (1.0 - m["trim"][1].as_f64().unwrap_or(0.95)) * 100.0);
    let _ = writeln!(s, "- 信心區間＝區間層級 bootstrap {} 次（固定種子）；月差＝置換檢定 {} 次、α={}；樣本閘 n≥{} 且 Σδ≥{}。", m["bootstrapReps"], m["permReps"], m["alpha"], m["minN"], m["minDeltaSum"]);
    let _ = writeln!(s, "- 縮水宣告要四道閘全過：樣本閘、5h 檢定、7d 雙視窗同向且顯著、固定籃指數同向且顯著。任一沒過只呈現數據。");
    let _ = writeln!(s, "- **已知弱點**：區間彼此有時序相關，p 值偏樂觀（雜訊閘，不是證明）；真值百分比實質精度 1 個百分點；網頁端與其他裝置的用量不在分子裡（對帳段已估其佔比）；權重＝API 價目表常數，未從資料估。");
    let _ = writeln!(s, "- 所有數字與畫面同源、可由 `-intervals.csv` 重算。工具：Claude Usage Monitor（自用、本機、不爬網頁）。");
    s
}

#[cfg(test)]
mod segment_tests {
    //! v1.5（D79）：三窗切分規則的單元測試。樣本形態照真帳本：cli-cache 的
    //! resets_at 在整點前後 ±1 秒抖；Desktop 歷史沒有 resets_at 只能靠下降。
    use super::{segment_windows, thin_points, HistKind};

    fn t(h: i64, m: i64) -> String {
        // 2026-09-01 起算的 UTC 時刻
        (chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00+00:00").unwrap()
            + chrono::Duration::minutes(h * 60 + m))
            .to_rfc3339()
    }
    fn row(p: f64, h: i64, m: i64, reset: Option<&str>, src: &str) -> (f64, String, Option<String>, String) {
        (p, t(h, m), reset.map(|s| s.to_string()), src.to_string())
    }

    #[test]
    fn reset_jitter_does_not_split_a_window() {
        let rows = vec![
            row(10.0, 0, 0, Some("2026-09-01T05:00:00.4+00:00"), "cli-cache"),
            row(12.0, 0, 10, Some("2026-09-01T04:59:59.7+00:00"), "cli-cache"),
            row(15.0, 0, 20, Some("2026-09-01T05:00:00.1+00:00"), "cli-cache"),
        ];
        let w = segment_windows(&rows, false, HistKind::FiveHour);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].peak, 15.0);
        assert_eq!(w[0].samples, 3);
        assert_eq!(w[0].reset.as_deref(), Some("2026-09-01T05:00:00.4+00:00"));
    }

    #[test]
    fn reset_jump_with_a_fresh_percent_splits_even_without_a_big_drop() {
        // 4% → 2% 只降 2（下降規則不切），但 resets_at 往後跳了 5 小時且 % 很低＝新窗
        let rows = vec![
            row(4.0, 0, 0, Some("2026-09-01T05:00:00+00:00"), "cli-cache"),
            row(4.0, 1, 0, Some("2026-09-01T05:00:00+00:00"), "cli-cache"),
            row(2.0, 2, 0, Some("2026-09-01T10:00:00+00:00"), "cli-cache"),
            row(3.0, 2, 10, Some("2026-09-01T10:00:00+00:00"), "cli-cache"),
        ];
        let w = segment_windows(&rows, false, HistKind::FiveHour);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].samples, 2);
        assert_eq!(w[1].samples, 2);
        assert_eq!(w[1].reset.as_deref(), Some("2026-09-01T10:00:00+00:00"));
    }

    #[test]
    fn reset_jump_with_a_high_percent_is_a_bad_cache_field_not_a_new_window() {
        // 真帳本 09-11：17% 帶著 09-13T01 的重置，五分鐘後同樣 17% 改帶 09-16T08——
        // 剛換窗不可能已經 17%，所以不切，重置欄改記後來的值。
        let rows = vec![
            row(17.0, 0, 0, Some("2026-09-13T01:00:00+00:00"), "cli-cache"),
            row(17.0, 0, 5, Some("2026-09-16T08:00:00+00:00"), "cli-cache"),
            row(25.0, 3, 0, Some("2026-09-16T08:00:00+00:00"), "cli-cache"),
        ];
        let w = segment_windows(&rows, false, HistKind::SevenDay);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].reset.as_deref(), Some("2026-09-16T08:00:00+00:00"));
    }

    #[test]
    fn a_lone_dip_between_two_sources_is_not_a_reset() {
        // 真帳本 09-11 08:54–09:21：Desktop 報 10、11、12，探針＋CLI 快取連續四筆 0
        // 又彈回來——半小時內彈回原高度，不是重置。
        let rows = vec![
            row(10.0, 0, 0, None, "desktop-history"),
            row(0.0, 0, 3, Some("2026-09-13T01:00:00+00:00"), "cli-cache"),
            row(11.0, 0, 15, None, "desktop-history"),
            row(0.0, 0, 19, None, "probe"),
            row(0.0, 0, 19, Some("2026-09-13T01:00:00+00:00"), "cli-cache"),
            row(0.0, 0, 29, None, "probe"),
            row(0.0, 0, 29, Some("2026-09-13T01:00:00+00:00"), "cli-cache"),
            row(12.0, 0, 30, None, "desktop-history"),
            row(31.0, 30, 0, None, "desktop-history"),
        ];
        let w = segment_windows(&rows, true, HistKind::SevenDay);
        assert_eq!(w.len(), 1, "連續幾筆孤立的 0 不是重置：{w:?}");
        assert_eq!(w[0].samples, 9, "孤立值仍算樣本");
        assert_eq!(w[0].peak, 31.0);
        let pts = w[0].points.as_ref().unwrap();
        assert!(pts.iter().all(|(_, p)| *p > 0.0), "孤立值不進階梯：{pts:?}");
        assert!(w[0].reset.is_none(), "孤立值帶的重置時刻不採信");
        // 但連續三筆都低就是真的重置
        let rows2 = vec![
            row(40.0, 0, 0, None, "desktop-history"),
            row(0.0, 1, 0, None, "desktop-history"),
            row(1.0, 1, 10, None, "desktop-history"),
            row(2.0, 1, 20, None, "desktop-history"),
            row(3.0, 1, 30, None, "desktop-history"),
        ];
        assert_eq!(segment_windows(&rows2, false, HistKind::SevenDay).len(), 2);
    }

    #[test]
    fn seven_day_window_survives_days_of_silence_and_cuts_on_drop() {
        // Desktop 歷史：沒有 resets_at，三天沒樣本也還是同一窗；% 掉回 0 才換窗。
        let rows = vec![
            row(10.0, 0, 0, None, "desktop-history"),
            row(40.0, 24, 0, None, "desktop-history"),
            row(58.0, 24 * 4, 0, None, "desktop-history"), // 3 天靜默（5h 規則會切，7d 不會）
            row(0.0, 24 * 7 + 1, 0, None, "desktop-history"), // 重置後第一筆
            row(12.0, 24 * 7 + 2, 0, None, "desktop-history"),
        ];
        let w7 = segment_windows(&rows, false, HistKind::SevenDay);
        assert_eq!(w7.len(), 2, "7d: 一週一窗，中間的靜默不切");
        assert_eq!(w7[0].peak, 58.0);
        assert_eq!(w7[0].samples, 3);
        assert_eq!(w7[1].peak, 12.0);
        let w5 = segment_windows(&rows, false, HistKind::FiveHour);
        assert!(w5.len() >= 4, "5h: 每段靜默 >5h 都切開，{} 窗", w5.len());
    }

    #[test]
    fn seven_day_span_expiry_cuts_a_stale_climb() {
        // 沒有下降、沒有 resets_at，但從第一筆算起已過 7 天又 2 小時＝一定是下一窗
        let rows = vec![
            row(20.0, 0, 0, None, "desktop-history"),
            row(50.0, 24 * 6, 0, None, "desktop-history"),
            row(55.0, 24 * 7 + 2, 0, None, "desktop-history"),
        ];
        let w = segment_windows(&rows, false, HistKind::SevenDay);
        assert_eq!(w.len(), 2);
        assert_eq!(w[1].peak, 55.0);
    }

    #[test]
    fn fable_uses_the_seven_day_span() {
        assert_eq!(HistKind::Fable.span(), HistKind::SevenDay.span());
        assert_eq!(HistKind::parse("fable"), HistKind::Fable);
        assert_eq!(HistKind::parse("7d"), HistKind::SevenDay);
        assert_eq!(HistKind::parse("whatever"), HistKind::FiveHour);
    }

    #[test]
    fn thinning_keeps_steps_and_the_last_point() {
        let pts: Vec<(String, f64)> = [(0, 10.0), (1, 10.0), (2, 10.0), (3, 20.0), (4, 20.0), (5, 20.0)]
            .iter()
            .map(|(i, p)| (t(0, *i), *p))
            .collect();
        let out = thin_points(pts);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].1, 10.0);
        assert_eq!(out[1].1, 20.0);
        assert_eq!(out[2].0, t(0, 5), "最後一筆一定留，階梯才延伸到窗尾");
        // keep_points 走同一條去重
        let rows = vec![
            row(10.0, 0, 0, None, "probe"),
            row(10.0, 0, 10, None, "probe"),
            row(30.0, 0, 20, None, "probe"),
            row(30.0, 0, 30, None, "probe"),
        ];
        let w = segment_windows(&rows, true, HistKind::FiveHour);
        assert_eq!(w[0].points.as_ref().unwrap().len(), 3);
        assert_eq!(w[0].samples, 4, "去重只動階梯點，樣本數照舊");
    }
}

#[cfg(test)]
mod probe {
    //! Real-ledger probe (not a unit test): `CUM_LEDGER_DIR=<dir> cargo test
    //! --lib probe -- --nocapture` dumps the M5 payloads to CUM_PROBE_OUT.
    /// v1.8.2 效能量測：`CUM_LEDGER_DIR=<dir> CUM_TIMING=1 cargo test --lib probe::timing -- --nocapture`
    /// 印出趨勢頁／歷史頁／今日頁各算一次要多久（第二次呼叫看快取有沒有接住）。
    #[tokio::test]
    async fn timing() {
        let Ok(dir) = std::env::var("CUM_LEDGER_DIR") else { return };
        let ledger = super::Ledger::open(dir.into()).unwrap();
        let t = |label: &str, d: std::time::Duration| eprintln!("[probe] {label:<22} {:>8.1} ms", d.as_secs_f64() * 1e3);
        let s = std::time::Instant::now();
        let _ = ledger.dashboard(None, 180).await.unwrap();
        t("dashboard 180d #1", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.dashboard(None, 180).await.unwrap();
        t("dashboard 180d #2", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.dashboard(None, 30).await.unwrap();
        t("dashboard 30d", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.history(None, "week", 0, super::HistKind::FiveHour).await.unwrap();
        t("history week 5h", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.history(None, "month", 0, super::HistKind::SevenDay).await.unwrap();
        t("history month 7d", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.today_outlook(None).await.unwrap();
        t("today_outlook", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.burn_stats(None).await.unwrap();
        t("burn_stats", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.health_snapshot().await.unwrap();
        t("health_snapshot", s.elapsed());
    }

    #[tokio::test]
    async fn dump_m5_payloads() {
        let Ok(dir) = std::env::var("CUM_LEDGER_DIR") else { return };
        let out = std::env::var("CUM_PROBE_OUT").unwrap_or_else(|_| "probe-out".into());
        let ledger = super::Ledger::open(dir.into()).unwrap();
        let dash = ledger.dashboard(None, 180).await.unwrap();
        let dash_cli = ledger.dashboard(Some("646c6f2e-eb7b-431b-8c89-911546a9e437"), 180).await.unwrap();
        // v1.8.11（D89）：全量視角也倒一份，驗「所有座位合起來」的數字比單一座位大
        let dash_all = ledger.dashboard(Some(super::ALL_SEATS), 180).await.unwrap();
        let today = ledger.today_outlook(None).await.unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(format!("{out}/dashboard.json"), serde_json::to_string_pretty(&dash).unwrap()).unwrap();
        std::fs::write(format!("{out}/dashboard-cli.json"), serde_json::to_string_pretty(&dash_cli).unwrap()).unwrap();
        std::fs::write(format!("{out}/dashboard-all.json"), serde_json::to_string_pretty(&dash_all).unwrap()).unwrap();
        for kind in [super::HistKind::FiveHour, super::HistKind::SevenDay, super::HistKind::Fable] {
            let h = ledger.history(Some(super::ALL_SEATS), "week", -1, kind).await.unwrap();
            std::fs::write(format!("{out}/history-week-{}-all.json", kind.key()), serde_json::to_string_pretty(&h).unwrap()).unwrap();
        }
        std::fs::write(format!("{out}/today.json"), serde_json::to_string_pretty(&today).unwrap()).unwrap();
        // M6: history spans + evidence export
        // D72 原型回放：CUM_PROBE_SAMPLES=1 時倒出 5h／7d 的真值樣本原始列，
        // 給 prototypes/predictor-replay.cjs 比較長窗／短窗預測器。
        if std::env::var("CUM_PROBE_SAMPLES").is_ok() {
            let conn = ledger.conn.lock().await;
            for (name, kinds) in [
                ("samples-5h", "'session','five_hour'"),
                ("samples-7d", "'weekly_all','seven_day'"),
            ] {
                let sql = format!(
                    "SELECT percent, fetched_at, resets_at, source, seat_id FROM truth_samples \
                     WHERE limit_kind IN ({kinds}) \
                     ORDER BY fetched_at ASC"
                );
                let mut stmt = conn.prepare(&sql).unwrap();
                let rows: Vec<serde_json::Value> = stmt
                    .query_map([], |r| {
                        Ok(serde_json::json!({
                            "percent": r.get::<_, f64>(0)?,
                            "fetchedAt": r.get::<_, String>(1)?,
                            "resetsAt": r.get::<_, Option<String>>(2)?,
                            "source": r.get::<_, String>(3)?,
                            "seatId": r.get::<_, Option<String>>(4)?,
                        }))
                    })
                    .unwrap()
                    .collect::<std::result::Result<_, _>>()
                    .unwrap();
                std::fs::write(format!("{out}/{name}.json"), serde_json::to_string(&rows).unwrap()).unwrap();
                println!("{name}: {} rows", rows.len());
            }
            let mut stmt = conn.prepare("SELECT ts, rate_limit_type FROM anchors ORDER BY ts ASC").unwrap();
            let rows: Vec<serde_json::Value> = stmt
                .query_map([], |r| Ok(serde_json::json!({ "ts": r.get::<_, String>(0)?, "kind": r.get::<_, Option<String>>(1)? })))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            std::fs::write(format!("{out}/anchors.json"), serde_json::to_string(&rows).unwrap()).unwrap();
            println!("anchors: {} rows", rows.len());
        }
        // v1.1.1（D69）：CUM_PROBE_SEAT=<seat id> 時多倒一份指定席位的歷史，驗「按 id 挑席位」。
        if let Ok(seat) = std::env::var("CUM_PROBE_SEAT") {
            let h = ledger.history(Some(&seat), "day", 0, super::HistKind::FiveHour).await.unwrap();
            std::fs::write(format!("{out}/history-seat.json"), serde_json::to_string_pretty(&h).unwrap()).unwrap();
            // v1.4.1：指定席位的今日頁 payload——驗 expired 旗標（切換器看別的座位）。
            let t = ledger.today_outlook(Some(&seat)).await.unwrap();
            std::fs::write(format!("{out}/today-seat.json"), serde_json::to_string_pretty(&t).unwrap()).unwrap();
        }
        // v1.5（D79）：三條額度 × 三個尺度都倒，檔名 history-{unit}-{kind}.json；
        // history-{unit}.json 仍是 5h（舊 fixture 名）。
        for kind in [super::HistKind::FiveHour, super::HistKind::SevenDay, super::HistKind::Fable] {
            for (unit, off) in [("day", -6), ("week", -1), ("month", -1)] {
                let h = ledger.history(None, unit, off, kind).await.unwrap();
                let s = serde_json::to_string_pretty(&h).unwrap();
                std::fs::write(format!("{out}/history-{unit}-{}.json", kind.key()), &s).unwrap();
                if kind == super::HistKind::FiveHour {
                    std::fs::write(format!("{out}/history-{unit}.json"), &s).unwrap();
                }
            }
        }
        let paths = ledger
            .export_evidence(None, 180, std::path::Path::new(&format!("{out}/exports")), "probe")
            .await
            .unwrap();
        println!("exported: {paths:?}");
    }
}

/// v1.8.9（D88）：切帳號拖尾守門的行為測試——用真的 Ledger（temp 目錄）跑整條路。
#[cfg(test)]
mod carryover_tests {
    use crate::collector::model::{SeatConfidence, Source, TruthSample};

    fn sample(acct: &str, kind: &str, scope: Option<&str>, pct: f32, at: &str) -> TruthSample {
        TruthSample {
            account_uuid: Some(acct.to_string()),
            org_uuid: Some(format!("org-{acct}")),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: kind.to_string(),
            scope: scope.map(str::to_string),
            percent: pct,
            resets_at: None,
            source: Source::Probe,
            fetched_at: chrono::DateTime::parse_from_rfc3339(at).unwrap().with_timezone(&chrono::Utc),
            raw_json: serde_json::json!({}),
        }
    }
    fn triple(acct: &str, s: f32, w: f32, f: f32, at: &str) -> Vec<TruthSample> {
        vec![
            sample(acct, "session", None, s, at),
            sample(acct, "weekly_all", None, w, at),
            sample(acct, "weekly_scoped", Some("Fable"), f, at),
        ]
    }
    async fn fresh_ledger(tag: &str) -> super::Ledger {
        let dir = std::env::temp_dir().join(format!("cum-carryover-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        super::Ledger::open(dir).unwrap()
    }

    #[tokio::test]
    async fn identical_weekly_and_fable_after_a_switch_is_carryover_until_they_move() {
        let l = fresh_ledger("a").await;
        // 座位 A 在用，最後一筆 1／56／100（就是 09-22 的真數字）
        l.insert_truth_samples(&triple("A", 1.0, 56.0, 100.0, "2026-09-22T08:14:00Z")).await.unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        // 切到 B：CLI 回的還是 1／56／100 → 拖尾
        let v = l.carryover_check(&triple("B", 1.0, 56.0, 100.0, "2026-09-22T12:02:00Z")).await.unwrap();
        assert!(matches!(v, Some((ref p, w, Some(f))) if p == &l.get_channel_state("current_seat_id").await.unwrap().unwrap() && w == 56.0 && f == 100.0));
        // 之後 mark_current_seat 換成 B；5h 自己重置成 0 也還是拖尾（只看 7d／Fable）
        l.mark_current_seat(Some("B"), Some("org-B"), None, None).await.unwrap();
        let v = l.carryover_check(&triple("B", 0.0, 56.0, 100.0, "2026-09-23T07:24:00Z")).await.unwrap();
        assert!(v.is_some(), "5h 歸零但 7d／Fable 沒動仍是拖尾");
        assert!(!l.get_channel_state("prev_seat_id").await.unwrap().unwrap().is_empty());
        // 週重置後變 0／0／0 → 數字動了 → 解除
        let v = l.carryover_check(&triple("B", 0.0, 0.0, 0.0, "2026-09-23T10:00:00Z")).await.unwrap();
        assert!(v.is_none());
        assert!(l.get_channel_state("prev_seat_id").await.unwrap().unwrap_or_default().is_empty());
    }

    #[tokio::test]
    async fn different_numbers_right_after_a_switch_are_booked_normally() {
        let l = fresh_ledger("b").await;
        l.insert_truth_samples(&triple("A", 20.0, 5.0, 8.0, "2026-09-20T09:30:00Z")).await.unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        // 09-20 那次切換：切完立刻是不同數字
        let v = l.carryover_check(&triple("B", 11.0, 3.0, 4.0, "2026-09-20T09:43:00Z")).await.unwrap();
        assert!(v.is_none());
    }

    #[tokio::test]
    async fn zero_weekly_on_the_previous_seat_never_counts_as_carryover() {
        let l = fresh_ledger("c").await;
        l.insert_truth_samples(&triple("A", 0.0, 0.0, 0.0, "2026-09-25T03:10:00Z")).await.unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let v = l.carryover_check(&triple("B", 0.0, 0.0, 0.0, "2026-09-25T03:20:00Z")).await.unwrap();
        assert!(v.is_none(), "兩邊都 0 分不出誰是誰，不能當拖尾");
    }
}
