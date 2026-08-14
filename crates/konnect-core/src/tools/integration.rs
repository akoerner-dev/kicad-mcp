//! `integration` toolset — JLCPCB parts database, datasheet enrichment, and Freerouting autorouter.
//!
//! JLCPCB tools query a local SQLite cache of the JLCPCB parts database.
//! Freerouting wraps the Freerouting JAR via subprocess.
//! Datasheet enrichment uses the LCSC HTTP API.
//!
//! The three network calls (JLCPCB database download, LCSC datasheet lookups)
//! go through `get_with_backoff`, which retries transient failures (network
//! errors, 429, 5xx) with exponential backoff before giving up.
//!
//! The three JLCPCB query tools (`search_jlcpcb_parts`, `get_jlcpcb_part`,
//! `suggest_jlcpcb_alternatives`) cache results in `ToolContext::jlcpcb_cache`
//! (5-minute TTL) to avoid re-running an identical SQLite query for repeated
//! lookups within a session. Responses carry a `"cached"` field so callers
//! can see whether a given result came from cache.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use anyhow::Context;
use serde_json::json;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "download_jlcpcb_database",
            "Download or update the local JLCPCB component parts database cache (SQLite).",
            json!({
                "type": "object",
                "properties": {
                    "output_path": { "type": "string", "description": "Local path to store the SQLite database file (optional, uses config default)" },
                    "force": { "type": "boolean", "description": "Force re-download even if cache exists", "default": false }
                },
                "required": []
            }),
            |args, ctx| async move { handle_download_jlcpcb(args, ctx).await }
        ),
        tool!(
            "search_jlcpcb_parts",
            "Search the local JLCPCB component database by keyword, value, or category.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search string (MPN, description, or value)" },
                    "category": { "type": "string", "description": "Component category filter (optional)" },
                    "basic_only": { "type": "boolean", "description": "Restrict to JLCPCB Basic Library parts only", "default": false },
                    "in_stock": { "type": "boolean", "description": "Only return parts currently in stock", "default": true },
                    "limit": { "type": "integer", "description": "Maximum number of results", "default": 20 }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_jlcpcb_parts(args, ctx).await }
        ),
        tool!(
            "get_jlcpcb_part",
            "Retrieve full details for a single JLCPCB part by its LCSC part number.",
            json!({
                "type": "object",
                "properties": {
                    "lcsc_id": { "type": "string", "description": "LCSC part number (e.g. 'C14663')" }
                },
                "required": ["lcsc_id"]
            }),
            |args, ctx| async move { handle_get_jlcpcb_part(args, ctx).await }
        ),
        tool!(
            "suggest_jlcpcb_alternatives",
            "Suggest JLCPCB-stocked alternative parts for a given component value and footprint.",
            json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string", "description": "Component value (e.g. '100nF')" },
                    "footprint": { "type": "string", "description": "KiCAD footprint identifier" },
                    "max_price_usd": { "type": "number", "description": "Maximum unit price in USD (optional)" },
                    "limit": { "type": "integer", "description": "Maximum number of suggestions", "default": 5 }
                },
                "required": ["value", "footprint"]
            }),
            |args, ctx| async move { handle_suggest_alternatives(args, ctx).await }
        ),
        tool!(
            "get_jlcpcb_database_stats",
            "Return statistics about the local JLCPCB database cache: part count, last updated, file size.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_jlcpcb_stats(args, ctx).await }
        ),
        tool!(
            "enrich_datasheets",
            "Fetch and cache datasheet URLs for all components in a schematic using the LCSC API.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "overwrite_existing": { "type": "boolean", "description": "Replace existing Datasheet fields", "default": false }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_enrich_datasheets(args, ctx).await }
        ),
        tool!(
            "get_datasheet_url",
            "Retrieve the datasheet URL for a component by MPN or LCSC ID.",
            json!({
                "type": "object",
                "properties": {
                    "mpn": { "type": "string", "description": "Manufacturer part number (optional)" },
                    "lcsc_id": { "type": "string", "description": "LCSC part number (optional)" }
                },
                "required": []
            }),
            |args, ctx| async move { handle_get_datasheet_url(args, ctx).await }
        ),
        tool!(
            "autoroute",
            "Run Freerouting autorouter on the PCB: export DSN → autoroute → import SES result.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "passes": { "type": "integer", "description": "Number of autorouter passes", "default": 3 },
                    "timeout_seconds": { "type": "integer", "description": "Maximum autorouter runtime in seconds", "default": 120 },
                    "jar_path": { "type": "string", "description": "Path to freerouting.jar (optional, uses config default)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_autoroute(args, ctx).await }
        ),
        tool!(
            "check_freerouting",
            "Verify that the Freerouting JAR is available and return its version.",
            json!({
                "type": "object",
                "properties": {
                    "jar_path": { "type": "string", "description": "Path to freerouting.jar (optional, uses config default)" }
                },
                "required": []
            }),
            |args, ctx| async move { handle_check_freerouting(args, ctx).await }
        ),
    ]
}

// ─── JLCPCB database path helper ─────────────────────────────────────────────

fn default_jlcpcb_db_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("konnect").join("jlcpcb.db")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".konnect").join("jlcpcb.db")
    }
}

fn resolve_db_path(args: &serde_json::Value, ctx: &ToolContext) -> PathBuf {
    if let Some(p) = args["output_path"].as_str() {
        return PathBuf::from(p);
    }
    if let Some(p) = &ctx.config.jlcpcb_db_path {
        return p.clone();
    }
    default_jlcpcb_db_path()
}

// ─── Retry/backoff for external HTTP calls ────────────────────────────────────
//
// JLCPCB database download and LCSC datasheet lookups are the only genuinely
// networked calls in this toolset (everything else queries the local SQLite
// cache). Both are prone to transient failures — timeouts, connection resets,
// rate limiting — that a simple retry clears up without any user action.

/// Retry policy: 3 attempts total, exponential backoff starting at 300ms
/// (300ms, then 600ms between attempts).
const RETRY_MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Whether an HTTP status is worth retrying. 429 (rate limited) and 5xx
/// (server-side) are transient; other 4xx (404, 401, ...) are not — retrying
/// a "not found" or "unauthorized" wastes time and won't change the outcome.
fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Delay before the next attempt, given the attempt number just made (1-based).
fn backoff_delay(attempt: u32) -> std::time::Duration {
    RETRY_BASE_DELAY * 2u32.pow(attempt.saturating_sub(1))
}

/// GET `url` with retry/backoff for transient failures (network-level errors,
/// 429, and 5xx). Returns the last response/error once attempts are exhausted.
async fn get_with_backoff(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if !is_transient_status(status) || attempt >= RETRY_MAX_ATTEMPTS {
                    return Ok(resp);
                }
                tracing::warn!(
                    "[BETA] {} returned {} (attempt {}/{}), retrying",
                    url,
                    status,
                    attempt,
                    RETRY_MAX_ATTEMPTS
                );
            }
            Err(e) => {
                if attempt >= RETRY_MAX_ATTEMPTS {
                    return Err(e.into());
                }
                tracing::warn!(
                    "[BETA] request to {} failed (attempt {}/{}): {}, retrying",
                    url,
                    attempt,
                    RETRY_MAX_ATTEMPTS,
                    e
                );
            }
        }
        tokio::time::sleep(backoff_delay(attempt)).await;
    }
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_download_jlcpcb(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let db_path = resolve_db_path(args, ctx);
    let force = args["force"].as_bool().unwrap_or(false);

    if db_path.exists() && !force {
        let meta = tokio::fs::metadata(&db_path).await?;
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&json!({
                "status": "already_exists",
                "path": db_path.to_str().unwrap_or(""),
                "size_bytes": meta.len(),
                "note": "Use force=true to re-download"
            }))
            .unwrap(),
        ));
    }

    // JLCPCB parts database is distributed as a CSV or SQLite download.
    // The official URL changes — we use a known community mirror format.
    let url = "https://bouni.github.io/kicad-jlcpcb-tools/jlcpcb_parts.db";

    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let resp = get_with_backoff(&client, url).await?;
    if !resp.status().is_success() {
        return Ok(CallToolResult::error(format!(
            "Download failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp.bytes().await?;
    tokio::fs::write(&db_path, &bytes).await?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "path": db_path.to_str().unwrap_or(""),
            "size_bytes": bytes.len()
        }))
        .unwrap(),
    ))
}

/// Build a deterministic cache key from a tool name, the resolved DB path
/// (so pointing at a different `output_path` never serves stale results),
/// and the query parameters that affect the result set.
fn cache_key(tool: &str, db_path: &std::path::Path, parts: &[&str]) -> String {
    format!("{}|{}|{}", tool, db_path.display(), parts.join("|"))
}

async fn handle_search_jlcpcb_parts(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(CallToolResult::error(
            "JLCPCB database not found. Run download_jlcpcb_database first.",
        ));
    }

    let query = args["query"].as_str().unwrap_or("").to_string();
    let basic_only = args["basic_only"].as_bool().unwrap_or(false);
    let in_stock = args["in_stock"].as_bool().unwrap_or(true);
    let limit = args["limit"].as_u64().unwrap_or(20) as usize;
    let category = args["category"].as_str().map(String::from);

    let key = cache_key(
        "search_jlcpcb_parts",
        &db_path,
        &[
            &query,
            category.as_deref().unwrap_or(""),
            &basic_only.to_string(),
            &in_stock.to_string(),
            &limit.to_string(),
        ],
    );
    if let Some(cached) = ctx.jlcpcb_cache.get(&key) {
        let mut body = cached;
        body["cached"] = json!(true);
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&body).unwrap(),
        ));
    }

    let results = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = rusqlite::Connection::open(&db_path)?;

        // The JLCPCB db schema has columns: LCSC, MFR_Part, Package, Solder_Joint,
        // Manufacturer, Library_Type, Description, Datasheet, Price, Stock
        let mut sql = String::from(
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Price, Stock \
             FROM components WHERE (Description LIKE ?1 OR MFR_Part LIKE ?1)"
        );
        if basic_only {
            sql.push_str(" AND Library_Type = 'Basic'");
        }
        if in_stock {
            sql.push_str(" AND Stock > 0");
        }
        if let Some(ref _cat) = category {
            sql.push_str(" AND Category LIKE ?2");
        }
        sql.push_str(&format!(" LIMIT {}", limit));

        let like_query = format!("%{}%", query);
        let mut stmt = conn.prepare(&sql)?;

        let rows: Vec<serde_json::Value> = if category.is_some() {
            let cat_like = format!("%{}%", category.as_deref().unwrap_or(""));
            stmt.query_map(rusqlite::params![like_query, cat_like], row_to_part_json)?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map(rusqlite::params![like_query], row_to_part_json)?
                .filter_map(|r| r.ok())
                .collect()
        };
        Ok(rows)
    })
    .await??;

    let body = json!({
        "query": args["query"].as_str().unwrap_or(""),
        "count": results.len(),
        "results": results
    });
    ctx.jlcpcb_cache.put(key, body.clone());

    let mut body = body;
    body["cached"] = json!(false);
    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&body).unwrap(),
    ))
}

fn row_to_part_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    Ok(json!({
        "lcsc": row.get::<_, String>(0).unwrap_or_default(),
        "mpn": row.get::<_, String>(1).unwrap_or_default(),
        "package": row.get::<_, String>(2).unwrap_or_default(),
        "manufacturer": row.get::<_, String>(3).unwrap_or_default(),
        "library_type": row.get::<_, String>(4).unwrap_or_default(),
        "description": row.get::<_, String>(5).unwrap_or_default(),
        "price": row.get::<_, f64>(6).unwrap_or(0.0),
        "stock": row.get::<_, i64>(7).unwrap_or(0)
    }))
}

async fn handle_get_jlcpcb_part(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(CallToolResult::error(
            "JLCPCB database not found. Run download_jlcpcb_database first.",
        ));
    }
    let lcsc_id = require_str(args, "lcsc_id")
        .map_err(|e| anyhow::anyhow!("{:?}", e))?
        .to_string();

    let key = cache_key("get_jlcpcb_part", &db_path, &[&lcsc_id]);
    if let Some(mut cached) = ctx.jlcpcb_cache.get(&key) {
        cached["cached"] = json!(true);
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&cached).unwrap(),
        ));
    }

    let result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<serde_json::Value>> {
            let conn = rusqlite::Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Price, Stock \
             FROM components WHERE LCSC = ?1 LIMIT 1"
        )?;
            let mut rows = stmt.query_map(rusqlite::params![lcsc_id], row_to_part_json)?;
            Ok(rows.next().and_then(|r| r.ok()))
        })
        .await??;

    match result {
        Some(part) => {
            ctx.jlcpcb_cache.put(key, part.clone());
            let mut part = part;
            part["cached"] = json!(false);
            Ok(CallToolResult::text(
                serde_json::to_string_pretty(&part).unwrap(),
            ))
        }
        None => Ok(CallToolResult::error(format!(
            "Part not found in database: {}",
            args["lcsc_id"].as_str().unwrap_or("")
        ))),
    }
}

async fn handle_suggest_alternatives(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(CallToolResult::error(
            "JLCPCB database not found. Run download_jlcpcb_database first.",
        ));
    }
    let value = args["value"].as_str().unwrap_or("").to_string();
    let footprint = args["footprint"].as_str().unwrap_or("").to_string();
    let max_price = args["max_price_usd"].as_f64();
    let limit = args["limit"].as_u64().unwrap_or(5) as usize;

    // Extract package from footprint (e.g. "Resistor_SMD:R_0402" → "0402")
    let package_hint = footprint
        .split(':')
        .next_back()
        .unwrap_or("")
        .split('_')
        .next_back()
        .unwrap_or("")
        .to_string();

    let key = cache_key(
        "suggest_jlcpcb_alternatives",
        &db_path,
        &[
            &value,
            &footprint,
            &max_price.map(|v| v.to_string()).unwrap_or_default(),
            &limit.to_string(),
        ],
    );
    if let Some(cached) = ctx.jlcpcb_cache.get(&key) {
        let mut body = cached;
        body["cached"] = json!(true);
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&body).unwrap(),
        ));
    }

    let results = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = rusqlite::Connection::open(&db_path)?;
        let like_val = format!("%{}%", value);
        let like_pkg = format!("%{}%", package_hint);

        let mut sql = String::from(
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Price, Stock \
             FROM components WHERE Description LIKE ?1 AND Package LIKE ?2 AND Stock > 0"
        );
        if let Some(max_p) = max_price {
            sql.push_str(&format!(" AND Price <= {}", max_p));
        }
        sql.push_str(&format!(" ORDER BY Price ASC LIMIT {}", limit));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params![like_val, like_pkg], row_to_part_json)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    })
    .await??;

    let body = json!({
        "value": args["value"].as_str().unwrap_or(""),
        "footprint": args["footprint"].as_str().unwrap_or(""),
        "alternatives": results
    });
    ctx.jlcpcb_cache.put(key, body.clone());

    let mut body = body;
    body["cached"] = json!(false);
    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&body).unwrap(),
    ))
}

async fn handle_jlcpcb_stats(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&json!({
                "exists": false,
                "note": "Run download_jlcpcb_database to fetch the parts database"
            }))
            .unwrap(),
        ));
    }

    let meta = tokio::fs::metadata(&db_path).await?;
    let size_bytes = meta.len();

    let count = tokio::task::spawn_blocking({
        let db_path = db_path.clone();
        move || -> anyhow::Result<i64> {
            let conn = rusqlite::Connection::open(&db_path)?;
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM components", [], |r| r.get(0))?;
            Ok(count)
        }
    })
    .await??;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "exists": true,
            "path": db_path.to_str().unwrap_or(""),
            "size_bytes": size_bytes,
            "part_count": count
        }))
        .unwrap(),
    ))
}

async fn handle_enrich_datasheets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let overwrite = args["overwrite_existing"].as_bool().unwrap_or(false);

    let content = tokio::fs::read_to_string(&sch_path).await?;

    // Find all LCSC property values in the schematic
    let mut lcsc_ids: Vec<String> = Vec::new();
    let mut search = content.as_str();
    while let Some(pos) = search.find("(property \"LCSC\" \"") {
        let after = &search[pos + 18..];
        if let Some(end) = after.find('"') {
            lcsc_ids.push(after[..end].to_string());
        }
        search = &search[pos + 1..];
    }
    lcsc_ids.sort();
    lcsc_ids.dedup();

    if lcsc_ids.is_empty() {
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&json!({
                "updated": 0,
                "note": "No LCSC property found in schematic components"
            }))
            .unwrap(),
        ));
    }

    // Query LCSC API for datasheet URLs
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut enriched = 0usize;
    let mut new_content = content.clone();

    for lcsc_id in &lcsc_ids {
        let url = format!(
            "https://wmsc.lcsc.com/ftps/wm/product/detail?productCode={}",
            lcsc_id
        );
        if let Ok(resp) = get_with_backoff(&client, &url).await {
            if resp.status().is_success() {
                if let Ok(json_resp) = resp.json::<serde_json::Value>().await {
                    if let Some(datasheet_url) = json_resp
                        .pointer("/result/dataManualUrl")
                        .and_then(|v| v.as_str())
                    {
                        // Find components with this LCSC ID and update their Datasheet property.
                        // Pattern: find (property "LCSC" "CxxxID") → walk back to symbol block →
                        // find (property "Datasheet" "...") and replace the URL.
                        let lcsc_pat = format!(r#"(property "LCSC" "{}")"#, lcsc_id);
                        let mut search_from = 0usize;
                        while let Some(lcsc_pos) = new_content[search_from..]
                            .find(&lcsc_pat)
                            .map(|i| i + search_from)
                        {
                            // Find the enclosing symbol block
                            let before = &new_content[..lcsc_pos];
                            if let Some(sym_start) = before.rfind("\n  (symbol") {
                                let sym_block = &new_content[sym_start..];
                                // Find Datasheet property within this symbol
                                let ds_pat = r#"(property "Datasheet" ""#;
                                if let Some(ds_offset) = sym_block.find(ds_pat) {
                                    let ds_abs = sym_start + ds_offset + ds_pat.len();
                                    if let Some(ds_end) = new_content[ds_abs..].find('"') {
                                        let existing = &new_content[ds_abs..ds_abs + ds_end];
                                        if overwrite || existing == "~" || existing.is_empty() {
                                            new_content = format!(
                                                "{}{}{}",
                                                &new_content[..ds_abs],
                                                datasheet_url,
                                                &new_content[ds_abs + ds_end..]
                                            );
                                            enriched += 1;
                                        }
                                    }
                                }
                            }
                            search_from = lcsc_pos + 1;
                        }
                    }
                }
            }
        }
    }

    // Write back if anything changed
    if enriched > 0 {
        konnect_sexp::writer::write_atomic(&sch_path, &new_content)?;
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "lcsc_ids_found": lcsc_ids.len(),
            "datasheets_enriched": enriched,
            "schematic": sch_path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_get_datasheet_url(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let mpn = args["mpn"].as_str();
    let lcsc_id = args["lcsc_id"].as_str();

    if mpn.is_none() && lcsc_id.is_none() {
        return Ok(CallToolResult::error("Provide either 'mpn' or 'lcsc_id'"));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    // Try LCSC API with lcsc_id first
    if let Some(id) = lcsc_id {
        let url = format!(
            "https://wmsc.lcsc.com/ftps/wm/product/detail?productCode={}",
            id
        );
        if let Ok(resp) = get_with_backoff(&client, &url).await {
            if resp.status().is_success() {
                if let Ok(json_resp) = resp.json::<serde_json::Value>().await {
                    if let Some(ds_url) = json_resp
                        .pointer("/result/dataManualUrl")
                        .and_then(|v| v.as_str())
                    {
                        return Ok(CallToolResult::text(
                            serde_json::to_string_pretty(&json!({
                                "lcsc_id": id,
                                "datasheet_url": ds_url
                            }))
                            .unwrap(),
                        ));
                    }
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "mpn": mpn,
            "lcsc_id": lcsc_id,
            "datasheet_url": null,
            "note": "Datasheet not found via LCSC API"
        }))
        .unwrap(),
    ))
}

// ─── Freerouting ──────────────────────────────────────────────────────────────
//
// kicad-cli dropped Specctra DSN export / SES import in KiCAD 10, but the
// pcbnew Python module bundled with KiCAD still exposes both as module-level
// functions (`pcbnew.ExportSpecctraDSN`, `pcbnew.ImportSpecctraSES`) even
// though kicad-cli no longer calls them. Autoroute shells out to that
// bundled interpreter for the DSN/SES round-trip and to Freerouting (a
// separate JAR, run via `java`) for the actual routing in between.
//
// VERIFIED against: KiCAD 10.0's bin\python.exe, Freerouting 2.3.0 (which
// requires a Java 25+ runtime — its release jar is compiled to class file
// version 69, one past what Java 21 supports).

fn default_freerouting_jar_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("konnect").join("freerouting.jar")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".konnect").join("freerouting.jar")
    }
}

fn find_freerouting_jar(args: &serde_json::Value) -> Option<PathBuf> {
    if let Some(p) = args["jar_path"].as_str() {
        return Some(PathBuf::from(p));
    }
    let default_path = default_freerouting_jar_path();
    if default_path.exists() {
        return Some(default_path);
    }
    // Common locations
    let candidates = [
        "freerouting.jar",
        "/usr/local/lib/freerouting/freerouting.jar",
        "/opt/freerouting/freerouting.jar",
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn python_exe_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "python.exe"
    } else {
        "python3"
    }
}

/// Resolve a bare executable name to its full path via the OS's PATH lookup,
/// so a sibling `python.exe` can be located even when `kicad_binary` in
/// config is just a bare name like "kicad.exe" rather than an absolute path.
async fn resolve_on_path(binary: &str) -> Option<PathBuf> {
    let lookup = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };
    let output = tokio::process::Command::new(lookup)
        .arg(binary)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(PathBuf::from(first))
    }
}

/// Fallback for when `kicad_binary` is neither absolute nor found on PATH:
/// scan `%ProgramFiles%\KiCad\<version>\bin\python.exe`, preferring the
/// highest installed version. Versions are compared numerically (not as
/// strings) since "10.0" sorts before "9.0" lexicographically.
#[cfg(target_os = "windows")]
fn scan_windows_kicad_python() -> Option<PathBuf> {
    let program_files =
        std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());
    let kicad_dir = PathBuf::from(program_files).join("KiCad");
    let mut versions: Vec<(f64, PathBuf)> = std::fs::read_dir(&kicad_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?;
            let version: f64 = name.parse().ok()?;
            Some((version, p))
        })
        .collect();
    versions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    versions
        .into_iter()
        .map(|(_, dir)| dir.join("bin").join("python.exe"))
        .find(|p| p.exists())
}

/// Locate KiCAD's bundled Python interpreter (see module doc comment for why
/// autoroute needs it instead of kicad-cli).
async fn find_kicad_python(ctx: &ToolContext) -> Option<PathBuf> {
    let configured = PathBuf::from(&ctx.config.kicad_binary);
    let bin_dir = if configured.is_absolute() {
        configured.parent().map(|p| p.to_path_buf())
    } else {
        resolve_on_path(&ctx.config.kicad_binary)
            .await
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    };

    if let Some(dir) = bin_dir {
        let candidate = dir.join(python_exe_name());
        if candidate.exists() {
            return Some(candidate);
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(p) = scan_windows_kicad_python() {
            return Some(p);
        }
    }

    None
}

const EXPORT_DSN_PY: &str = r#"import sys
import pcbnew
board = pcbnew.LoadBoard(sys.argv[1])
if not pcbnew.ExportSpecctraDSN(board, sys.argv[2]):
    print("EXPORT_FAILED")
    sys.exit(1)
print("EXPORT_OK")
"#;

const IMPORT_SES_PY: &str = r#"import sys
import pcbnew
board = pcbnew.LoadBoard(sys.argv[1])
if not pcbnew.ImportSpecctraSES(board, sys.argv[2]):
    print("IMPORT_FAILED")
    sys.exit(1)
board.Save(sys.argv[1])
print("IMPORT_OK")
"#;

struct SubprocessOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run `program` with `args`, capturing stdout/stderr, killed if it exceeds
/// `timeout_dur`. `kill_on_drop(true)` is what makes the kill-on-timeout
/// actually happen: `wait_with_output()` consumes the `Child` handle, so once
/// the timeout future is dropped there's no handle left to call `.kill()` on
/// directly — Tokio does it for us on drop instead. Without this a timed-out
/// `java` (or `python`) process would leak as an orphan.
async fn run_subprocess(
    program: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    timeout_dur: Duration,
) -> anyhow::Result<SubprocessOutput> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().context("failed to spawn subprocess")?;

    let output = tokio::time::timeout(timeout_dur, child.wait_with_output())
        .await
        .with_context(|| format!("subprocess timed out after {:?}", timeout_dur))?
        .context("subprocess process failed")?;

    Ok(SubprocessOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

/// Best-effort extraction of Freerouting's own "final score: N (X unrouted
/// and Y violations)" summary line, so the tool result surfaces routing
/// quality without the caller having to grep the raw log. Returns `None`
/// (rather than erroring) if the log format doesn't match — this is a
/// convenience field, not load-bearing for success/failure.
fn parse_freerouting_summary(log: &str) -> Option<String> {
    let line = log.lines().rev().find(|l| l.contains("final score:"))?;
    let start = line.find('(')?;
    let end = line[start..].find(')')? + start;
    Some(line[start + 1..end].trim().to_string())
}

/// Truncate `s` to its last `max_chars` characters, safely (char boundaries,
/// not byte offsets — a raw byte slice can panic mid-UTF-8-sequence).
fn tail(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        trimmed.to_string()
    } else {
        let skip = count - max_chars;
        format!("...{}", trimmed.chars().skip(skip).collect::<String>())
    }
}

async fn handle_autoroute(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    if !board_path.exists() {
        anyhow::bail!("Board file not found: {}", board_path.display());
    }

    let jar_path = match find_freerouting_jar(args) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(
                "freerouting.jar not found. Pass jar_path explicitly, place it at \
                 %APPDATA%\\konnect\\freerouting.jar (~/.konnect/freerouting.jar on \
                 macOS/Linux), or download it from \
                 https://github.com/freerouting/freerouting/releases",
            ));
        }
    };

    let python = match find_kicad_python(ctx).await {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(
                "KiCAD's bundled python.exe was not found next to the KiCAD binary or in \
                 the standard install locations. Autoroute needs it to export/import \
                 Specctra DSN/SES files, since kicad-cli no longer supports that in KiCAD 10.",
            ));
        }
    };

    let passes = args["passes"].as_i64().unwrap_or(3).max(1);
    let timeout_secs = args["timeout_seconds"].as_i64().unwrap_or(120).max(1) as u64;

    let work_dir = std::env::temp_dir().join(format!("konnect_autoroute_{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&work_dir).await?;

    let board_str = board_path.to_str().unwrap_or_default();
    let dsn_path = work_dir.join("board.dsn");
    let ses_path = work_dir.join("board.ses");
    let export_script = work_dir.join("export_dsn.py");
    let import_script = work_dir.join("import_ses.py");
    tokio::fs::write(&export_script, EXPORT_DSN_PY).await?;
    tokio::fs::write(&import_script, IMPORT_SES_PY).await?;

    // Step 1: export Specctra DSN via KiCAD's bundled pcbnew Python module.
    let export_result = run_subprocess(
        &python,
        &[
            export_script.to_str().unwrap_or_default(),
            board_str,
            dsn_path.to_str().unwrap_or_default(),
        ],
        Duration::from_secs(60),
    )
    .await?;

    if !export_result.success || !dsn_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Specctra DSN export failed.\nstdout: {}\nstderr: {}",
            tail(&export_result.stdout, 2000),
            tail(&export_result.stderr, 2000)
        )));
    }

    // Step 2: autoroute with Freerouting. `-mt 1` (single-threaded route
    // optimization) is forced rather than exposed as an option: Freerouting
    // 2.3.0 itself warns at runtime that its multi-threaded optimizer "is
    // broken and it is known to generate clearance violations" — a
    // short-circuit risk, not just a quality tradeoff.
    let passes_str = passes.to_string();
    let route_result = run_subprocess(
        "java",
        &[
            "-jar",
            jar_path.to_str().unwrap_or_default(),
            "-de",
            dsn_path.to_str().unwrap_or_default(),
            "-do",
            ses_path.to_str().unwrap_or_default(),
            "-mp",
            &passes_str,
            "-mt",
            "1",
        ],
        Duration::from_secs(timeout_secs),
    )
    .await?;

    if !ses_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Freerouting did not produce a session file.\nstdout: {}\nstderr: {}",
            tail(&route_result.stdout, 4000),
            tail(&route_result.stderr, 2000)
        )));
    }

    let summary = parse_freerouting_summary(&route_result.stdout);

    // Step 3: import the routed session back and overwrite the board in place.
    let import_result = run_subprocess(
        &python,
        &[
            import_script.to_str().unwrap_or_default(),
            board_str,
            ses_path.to_str().unwrap_or_default(),
        ],
        Duration::from_secs(60),
    )
    .await?;

    if !import_result.success {
        return Ok(CallToolResult::error(format!(
            "Specctra SES import failed.\nstdout: {}\nstderr: {}",
            tail(&import_result.stdout, 2000),
            tail(&import_result.stderr, 2000)
        )));
    }

    tokio::fs::remove_dir_all(&work_dir).await.ok();

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "status": "routed",
            "board": board_str,
            "passes": passes,
            "summary": summary,
            "note": "Board file was overwritten in place via a separate pcbnew subprocess, \
                     not through KiCAD's IPC session. If the board is open in KiCAD, close \
                     and reopen it (or File > Revert) to see the routed traces."
        }))
        .unwrap(),
    ))
}

#[cfg(test)]
mod live_autoroute_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    /// Live end-to-end check of the DSN export -> Freerouting -> SES import
    /// pipeline against a real board (via the TEST_PCB env var). Skipped
    /// cleanly when unset, so the normal `cargo test` run stays green
    /// without KiCAD/Java/Freerouting installed — mirrors the pattern in
    /// `cli.rs::live_drc_tests`. Copies the board into its own scratch dir
    /// first since `handle_autoroute` overwrites its input in place.
    ///
    /// Verified locally: routes a real board, segment count increases,
    /// result carries a `summary` parsed from Freerouting's own log.
    #[tokio::test]
    async fn live_autoroute_routes_a_real_board() {
        let Ok(pcb) = std::env::var("TEST_PCB") else {
            eprintln!("SKIP: set TEST_PCB to run the live autoroute test");
            return;
        };

        let scratch_dir =
            std::env::temp_dir().join(format!("konnect_test_autoroute_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch_dir).unwrap();
        let scratch_board = scratch_dir.join("test.kicad_pcb");
        std::fs::copy(&pcb, &scratch_board).expect("copy TEST_PCB into scratch dir");

        let ctx = ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
            },
            Arc::new(ToolRouter::new()),
        );

        let segments_before = std::fs::read_to_string(&scratch_board)
            .unwrap()
            .matches("(segment")
            .count();

        let args = json!({
            "board": scratch_board.to_str().unwrap(),
            "passes": 2,
            "timeout_seconds": 120
        });

        let result = handle_autoroute(&args, &ctx)
            .await
            .expect("handle_autoroute should not error at the Rust level");

        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        eprintln!("LIVE AUTOROUTE result: {}", text);
        assert!(!result.is_error, "autoroute failed: {}", text);

        let segments_after = std::fs::read_to_string(&scratch_board)
            .unwrap()
            .matches("(segment")
            .count();
        eprintln!(
            "segments before={} after={}",
            segments_before, segments_after
        );
        assert!(
            segments_after > segments_before,
            "expected autoroute to add copper traces to the board"
        );

        std::fs::remove_dir_all(&scratch_dir).ok();
    }
}

async fn handle_check_freerouting(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let jar = find_freerouting_jar(args);

    match jar {
        None => Ok(CallToolResult::text(
            serde_json::to_string_pretty(&json!({
                "available": false,
                "note": "freerouting.jar not found. Download from https://github.com/freerouting/freerouting/releases"
            }))
            .unwrap(),
        )),
        Some(jar_path) => {
            // Try to get version from java -jar freerouting.jar --version
            let output = tokio::process::Command::new("java")
                .args(["-jar", jar_path.to_str().unwrap_or(""), "--version"])
                .output()
                .await;

            let version = match output {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    format!("{}{}", stdout.trim(), stderr.trim())
                }
                Err(e) => format!("java not available: {e}"),
            };

            Ok(CallToolResult::text(
                serde_json::to_string_pretty(&json!({
                    "available": true,
                    "jar_path": jar_path.to_str().unwrap_or(""),
                    "version_output": version
                }))
                .unwrap(),
            ))
        }
    }
}

#[cfg(test)]
mod retry_backoff_tests {
    use super::*;

    /// End-to-end check against a real (hand-rolled) flaky HTTP server: two
    /// 503s followed by a 200 should be retried through to success, with
    /// real backoff delays elapsed in between — not just the status-code
    /// decision logic in isolation.
    #[tokio::test]
    async fn get_with_backoff_recovers_after_transient_failures() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            for resp in [
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                socket.write_all(resp.as_bytes()).await.unwrap();
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{}/x", addr);

        let start = std::time::Instant::now();
        let resp = get_with_backoff(&client, &url).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        // Two retries at 300ms + 600ms = 900ms minimum before the 3rd (successful) attempt.
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "expected backoff delays to have elapsed, got {:?}",
            elapsed
        );
    }

    /// A persistent (non-transient) failure should return immediately after
    /// the first attempt — no wasted retries on a 404.
    #[tokio::test]
    async fn get_with_backoff_does_not_retry_client_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            // If get_with_backoff retried, it would try to accept() again here
            // and this task would hang until the test times out.
        });

        let client = reqwest::Client::new();
        let url = format!("http://{}/x", addr);

        let start = std::time::Instant::now();
        let resp = get_with_backoff(&client, &url).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "expected no retry delay for a 404, took {:?}",
            elapsed
        );
    }

    #[test]
    fn transient_on_rate_limit_and_server_errors() {
        assert!(is_transient_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_transient_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_transient_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_transient_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(is_transient_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
    }

    #[test]
    fn not_transient_on_client_errors() {
        // Retrying a 404/401/403/400 wastes time — the request itself is
        // wrong, not the server having a bad moment.
        assert!(!is_transient_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_transient_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_transient_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_transient_status(reqwest::StatusCode::NOT_FOUND));
    }

    #[test]
    fn not_transient_on_success() {
        assert!(!is_transient_status(reqwest::StatusCode::OK));
        assert!(!is_transient_status(reqwest::StatusCode::NO_CONTENT));
    }

    #[test]
    fn backoff_delay_doubles_each_attempt() {
        assert_eq!(backoff_delay(1), std::time::Duration::from_millis(300));
        assert_eq!(backoff_delay(2), std::time::Duration::from_millis(600));
        assert_eq!(backoff_delay(3), std::time::Duration::from_millis(1200));
    }

    #[test]
    fn backoff_delay_never_panics_on_zero_attempt() {
        // attempt is 1-based in normal use, but the saturating_sub guards
        // against an accidental 0 causing an underflow panic.
        assert_eq!(backoff_delay(0), std::time::Duration::from_millis(300));
    }
}

#[cfg(test)]
mod jlcpcb_cache_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// Builds a temp SQLite file with a `components` table matching the
    /// schema the handlers query, seeded with one part.
    fn seed_test_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("jlcpcb.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        conn.execute(
            "CREATE TABLE components (
                LCSC TEXT, MFR_Part TEXT, Package TEXT, Manufacturer TEXT,
                Library_Type TEXT, Description TEXT, Price REAL, Stock INTEGER
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO components VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "C14663",
                "RC0402FR-0710KL",
                "0402",
                "YAGEO",
                "Basic",
                "10k resistor 0402",
                0.01,
                5000
            ],
        )
        .unwrap();
        (dir, db_path)
    }

    #[tokio::test]
    async fn search_jlcpcb_parts_caches_repeated_query() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();
        let args = json!({
            "query": "10k",
            "output_path": db_path.to_str().unwrap()
        });

        let first = handle_search_jlcpcb_parts(&args, &ctx).await.unwrap();
        let second = handle_search_jlcpcb_parts(&args, &ctx).await.unwrap();

        let first_body = response_json(&first);
        let second_body = response_json(&second);
        assert_eq!(first_body["cached"], json!(false));
        assert_eq!(second_body["cached"], json!(true));
        assert_eq!(first_body["results"], second_body["results"]);
        assert_eq!(first_body["count"], json!(1));
    }

    #[tokio::test]
    async fn search_jlcpcb_parts_different_query_is_a_cache_miss() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();

        let args_a = json!({ "query": "10k", "output_path": db_path.to_str().unwrap() });
        let args_b = json!({ "query": "100nF", "output_path": db_path.to_str().unwrap() });

        handle_search_jlcpcb_parts(&args_a, &ctx).await.unwrap();
        let second = handle_search_jlcpcb_parts(&args_b, &ctx).await.unwrap();

        assert_eq!(response_json(&second)["cached"], json!(false));
    }

    #[tokio::test]
    async fn get_jlcpcb_part_caches_repeated_lookup() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();
        let args = json!({
            "lcsc_id": "C14663",
            "output_path": db_path.to_str().unwrap()
        });

        let first = handle_get_jlcpcb_part(&args, &ctx).await.unwrap();
        let second = handle_get_jlcpcb_part(&args, &ctx).await.unwrap();

        assert_eq!(response_json(&first)["cached"], json!(false));
        assert_eq!(response_json(&second)["cached"], json!(true));
        assert_eq!(response_json(&first)["lcsc"], json!("C14663"));
    }

    #[tokio::test]
    async fn suggest_alternatives_caches_repeated_query() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();
        let args = json!({
            "value": "10k",
            "footprint": "Resistor_SMD:R_0402",
            "output_path": db_path.to_str().unwrap()
        });

        let first = handle_suggest_alternatives(&args, &ctx).await.unwrap();
        let second = handle_suggest_alternatives(&args, &ctx).await.unwrap();

        assert_eq!(response_json(&first)["cached"], json!(false));
        assert_eq!(response_json(&second)["cached"], json!(true));
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        }
    }
}
