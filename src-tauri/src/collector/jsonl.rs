//! Local JSONL reader — turns `~/.claude/projects/**/*.jsonl` into deduped
//! `usage_events` plus 429 `anchors`.
//!
//! Everything here follows the measured spec in
//! `docs/research/2026-08-27-M0-JSONL讀取器格式規格.md`:
//!
//!   - Only `type == "assistant"` rows with a `message.usage` matter.
//!   - One API reply writes SEVERAL lines sharing one `message.id`; streaming
//!     snapshots mean only the最後 line carries the real `output_tokens`
//!     (57% of ids have differing usage across their lines!) → global dedup
//!     on `message.id`, keeping max(output_tokens). Resume/fork copies whole
//!     histories across session files, so the dedup set is global across
//!     ALL files, which also makes full re-reads idempotent.
//!   - `model == "<synthetic>"` rows carry zero usage; the 429 ones among
//!     them (`isApiErrorMessage && apiErrorStatus == 429`) become anchors —
//!     first-hand evidence of 5h-window boundaries (`quotaLimits.resetsAt`).
//!   - Incremental model: mtime+size+byte-offset per file (the same shape
//!     Claude Code itself keeps in `.session_cache.json`, which we do NOT
//!     touch). Size shrank / rewritten → full re-read; growing → read from
//!     the stored offset; a half-written tail line parks the offset at its
//!     line start for the next round.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};

/// One deduped-API-call row (usage_events).
#[derive(Debug, Clone)]
pub struct UsageEventRow {
    pub msg_id: String,
    pub request_id: Option<String>,
    pub ts: String,
    pub model: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub project_dir: Option<String>,
    pub entrypoint: Option<String>,
    pub cc_version: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation: i64,
    pub cache_read: i64,
    pub cache_1h: Option<i64>,
    pub cache_5m: Option<i64>,
    pub thinking_tokens: Option<i64>,
    pub service_tier: Option<String>,
    pub speed: Option<String>,
    pub web_searches: Option<i64>,
}

/// One 429 quota-wall row (anchors).
#[derive(Debug, Clone)]
pub struct AnchorRow {
    pub ts: String,
    pub session_id: Option<String>,
    pub rate_limit_type: Option<String>,
    pub resets_at: Option<String>,
    pub overage_status: Option<String>,
    pub overage_disabled_reason: Option<String>,
    pub content_text: Option<String>,
    pub raw_json: String,
}

pub struct FileBatch {
    pub path: String,
    pub mtime_ms: i64,
    pub size: i64,
    pub new_offset: i64,
    pub events: Vec<UsageEventRow>,
    pub anchors: Vec<AnchorRow>,
}

pub struct ScanStats {
    pub files_seen: usize,
    pub files_read: usize,
    pub events_upserted: usize,
    pub anchors_inserted: usize,
}

fn projects_root() -> Result<PathBuf> {
    let home = std::env::var("USERPROFILE").context("USERPROFILE not set")?;
    Ok(PathBuf::from(home).join(".claude").join("projects"))
}

/// Recursively collect every .jsonl under the projects root. `(conflicted)`
/// pCloud copies are strict subsets of their originals — skipping them just
/// saves IO (the global dedup would eat them safely anyway).
fn collect_jsonl_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_jsonl_files(&path, out);
        } else if ft.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".jsonl") && !name.contains("(conflicted)") {
                out.push(path);
            }
        }
    }
}

/// Scan all transcript files incrementally and apply each file's rows to the
/// ledger in one transaction. Designed to run in a background task — it
/// yields between files so it never starves the runtime.
pub async fn scan(ledger: &crate::ledger::Ledger) -> Result<ScanStats> {
    let root = projects_root()?;
    let mut files = Vec::new();
    collect_jsonl_files(&root, &mut files);

    let mut stats = ScanStats {
        files_seen: files.len(),
        files_read: 0,
        events_upserted: 0,
        anchors_inserted: 0,
    };

    for path in files {
        let path_str = path.to_string_lossy().to_string();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let size = meta.len() as i64;
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let prev = ledger.get_ingest_file(&path_str).await?;
        let start_offset = match &prev {
            Some((p_mtime, p_size, p_offset)) => {
                if *p_mtime == mtime_ms && *p_size == size {
                    continue; // unchanged
                }
                if size > *p_size {
                    *p_offset // grew — resume from the parked offset
                } else {
                    0 // shrank or rewritten — full re-read (dedup is idempotent)
                }
            }
            None => 0,
        };

        let project_dir = path
            .strip_prefix(&root)
            .ok()
            .and_then(|rel| rel.components().next())
            .map(|c| c.as_os_str().to_string_lossy().to_string());

        // Blocking file parse on the blocking pool — files can be 36 MB.
        let parse_path = path.clone();
        let batch = tokio::task::spawn_blocking(move || {
            parse_file(&parse_path, start_offset, project_dir)
        })
        .await
        .context("jsonl parse task panicked")??;

        let batch = FileBatch {
            path: path_str,
            mtime_ms,
            size,
            new_offset: batch.0,
            events: batch.1,
            anchors: batch.2,
        };
        stats.files_read += 1;
        let (ev, an) = ledger.apply_file_batch(&batch).await?;
        stats.events_upserted += ev;
        stats.anchors_inserted += an;
    }

    Ok(stats)
}

type ParseOut = (i64, Vec<UsageEventRow>, Vec<AnchorRow>);

/// Parse one file from `start_offset`. Returns (new_offset, events, anchors).
/// The new offset stops at the start of a trailing half-written line so the
/// next round re-reads only that line.
fn parse_file(path: &Path, start_offset: i64, project_dir: Option<String>) -> Result<ParseOut> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.seek(SeekFrom::Start(start_offset.max(0) as u64))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);

    let mut offset = start_offset.max(0);
    let mut events: Vec<UsageEventRow> = Vec::new();
    let mut anchors: Vec<AnchorRow> = Vec::new();
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);

    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break; // clean EOF
        }
        let complete_line = buf.last() == Some(&b'\n');

        // Cheap prefilter before the JSON parse — only ~5% of bytes matter.
        let text = String::from_utf8_lossy(&buf);
        let is_candidate = text.contains("\"type\":\"assistant\"");
        if is_candidate {
            match serde_json::from_str::<serde_json::Value>(text.trim_end()) {
                Ok(v) => {
                    if let Some(row) = extract(&v, project_dir.as_deref()) {
                        match row {
                            Extracted::Event(e) => events.push(e),
                            Extracted::Anchor(a) => anchors.push(a),
                        }
                    }
                }
                Err(_) if !complete_line => {
                    // Half-written tail — park the offset here for next round.
                    return Ok((offset, events, anchors));
                }
                Err(_) => { /* malformed full line — skip, keep going */ }
            }
        } else if !complete_line {
            // Non-candidate half line: also park (it may become a candidate
            // once the rest of it lands).
            return Ok((offset, events, anchors));
        }

        offset += n as i64;
    }

    Ok((offset, events, anchors))
}

enum Extracted {
    Event(UsageEventRow),
    Anchor(AnchorRow),
}

fn extract(v: &serde_json::Value, project_dir: Option<&str>) -> Option<Extracted> {
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let msg = v.get("message")?;
    let model = msg.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let ts = v.get("timestamp").and_then(|t| t.as_str())?.to_string();
    let session_id = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .map(str::to_string);

    if model == "<synthetic>" {
        // Zero-usage local message. The 429s among them are anchors.
        let is_429 = v.get("isApiErrorMessage").and_then(|b| b.as_bool()) == Some(true)
            && v.get("apiErrorStatus").and_then(|s| s.as_i64()) == Some(429);
        if !is_429 {
            return None;
        }
        let q = v.get("quotaLimits");
        let resets_at = q
            .and_then(|q| q.get("resetsAt"))
            .and_then(|r| r.as_i64())
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
            .map(|t| t.to_rfc3339());
        let content_text = msg
            .pointer("/content/0/text")
            .and_then(|t| t.as_str())
            .map(str::to_string);
        return Some(Extracted::Anchor(AnchorRow {
            ts,
            session_id,
            rate_limit_type: q
                .and_then(|q| q.get("rateLimitType"))
                .and_then(|r| r.as_str())
                .map(str::to_string),
            resets_at,
            overage_status: q
                .and_then(|q| q.get("overageStatus"))
                .and_then(|r| r.as_str())
                .map(str::to_string),
            overage_disabled_reason: q
                .and_then(|q| q.get("overageDisabledReason"))
                .and_then(|r| r.as_str())
                .map(str::to_string),
            content_text,
            raw_json: serde_json::json!({ "quotaLimits": q, "error": v.get("error") })
                .to_string(),
        }));
    }

    let usage = msg.get("usage")?;
    let msg_id = msg.get("id").and_then(|i| i.as_str())?.to_string();
    let geti = |key: &str| usage.get(key).and_then(|x| x.as_i64());

    Some(Extracted::Event(UsageEventRow {
        msg_id,
        request_id: v
            .get("requestId")
            .and_then(|r| r.as_str())
            .map(str::to_string),
        ts,
        model: model.to_string(),
        session_id,
        agent_id: v.get("agentId").and_then(|a| a.as_str()).map(str::to_string),
        project_dir: project_dir.map(str::to_string),
        entrypoint: v
            .get("entrypoint")
            .and_then(|e| e.as_str())
            .map(str::to_string),
        cc_version: v
            .get("version")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        input_tokens: geti("input_tokens").unwrap_or(0),
        output_tokens: geti("output_tokens").unwrap_or(0),
        cache_creation: geti("cache_creation_input_tokens").unwrap_or(0),
        cache_read: geti("cache_read_input_tokens").unwrap_or(0),
        cache_1h: usage
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(|x| x.as_i64()),
        cache_5m: usage
            .pointer("/cache_creation/ephemeral_5m_input_tokens")
            .and_then(|x| x.as_i64()),
        thinking_tokens: usage
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(|x| x.as_i64()),
        service_tier: usage
            .get("service_tier")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        speed: usage.get("speed").and_then(|x| x.as_str()).map(str::to_string),
        web_searches: usage
            .pointer("/server_tool_use/web_search_requests")
            .and_then(|x| x.as_i64()),
    }))
}
