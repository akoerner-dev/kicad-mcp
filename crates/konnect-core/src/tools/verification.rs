//! `verification` toolset — DRC, design rules, KiCAD UI management, routing utilities.
//!
//! DRC delegates to `kicad-cli`. Design rules are read/written as S-expressions.
//! KiCAD UI management uses process inspection + subprocess spawning.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use konnect_sexp::writer::write_atomic;
use serde_json::json;
use tokio::task;

use super::cli;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "run_drc",
            "Run the Design Rule Check on the PCB and return structured violation results, \
             with separate error and warning counts in the summary. Prefer this over \
             `get_drc_violations` (pcb_export toolset) — they run the same underlying \
             kicad-cli check, but `run_drc` returns a cleaner breakdown.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Optional path to write DRC report JSON" },
                    "schematic_parity": {
                        "type": "boolean",
                        "description": "Also run board-vs-schematic parity (--schematic-parity). Requires the sibling .kicad_sch next to the board. Default false.",
                        "default": false
                    },
                    "refill_zones": {
                        "type": "boolean",
                        "description": "Refill copper zones before checking (--refill-zones), matching the KiCAD GUI's 'Refill all zones before DRC'. Without it, stale zone fills produce phantom clearance/short/mask violations. Default false.",
                        "default": false
                    },
                    "severity": {
                        "type": "string",
                        "description": "Minimum violation severity to include: 'error', 'warning' (default), 'info'",
                        "default": "warning"
                    },
                    "tests": {
                        "type": "array",
                        "description": "Specific DRC test IDs to run (empty = all tests)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_run_drc(args, ctx).await }
        ),
        tool!(
            "set_design_rules",
            "Set board-level design rules (clearance, trace width, via size). Writes to the \
             .kicad_pcb for legacy (pre-KiCAD-7) boards; for modern boards, whose constraints \
             live in the sibling .kicad_pro project file instead, writes there.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "min_clearance": { "type": "number", "description": "Minimum clearance in mm" },
                    "min_trace_width": { "type": "number", "description": "Minimum trace width in mm" },
                    "min_via_drill": { "type": "number", "description": "Minimum via drill diameter in mm" },
                    "min_via_size": { "type": "number", "description": "Minimum via pad diameter in mm" },
                    "min_hole_to_hole": { "type": "number", "description": "Minimum hole-to-hole clearance in mm" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_set_design_rules(args, ctx).await }
        ),
        tool!(
            "get_design_rules",
            "Return the current design rule constraints, reading from the .kicad_pcb (legacy \
             boards) or the sibling .kicad_pro project file (modern boards), whichever the \
             board actually uses.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_design_rules(args, ctx).await }
        ),
        tool!(
            "check_kicad_ui",
            "Check whether the KiCAD GUI application is running and responsive.",
            json!({
                "type": "object",
                "properties": {
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Timeout for the health check in seconds",
                        "default": 5
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_check_kicad_ui(args, ctx).await }
        ),
        tool!(
            "launch_kicad_ui",
            "Launch the KiCAD GUI application and optionally open a project file.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro file to open (optional)" },
                    "wait_ready": {
                        "type": "boolean",
                        "description": "Wait until KiCAD IPC is responsive before returning",
                        "default": true
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Maximum wait time in seconds",
                        "default": 30
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_launch_kicad_ui(args, ctx).await }
        ),
        tool!(
            "copy_routing_pattern",
            "Copy a routing pattern (traces and vias) from one region of the board to another.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "src_x1": { "type": "number", "description": "Source region bounding box min X" },
                    "src_y1": { "type": "number", "description": "Source region bounding box min Y" },
                    "src_x2": { "type": "number", "description": "Source region bounding box max X" },
                    "src_y2": { "type": "number", "description": "Source region bounding box max Y" },
                    "dest_x": { "type": "number", "description": "Destination anchor X (maps to src_x1)" },
                    "dest_y": { "type": "number", "description": "Destination anchor Y (maps to src_y1)" },
                    "net_map": {
                        "type": "object",
                        "description": "Optional mapping from source net names to destination net names"
                    }
                },
                "required": ["board", "src_x1", "src_y1", "src_x2", "src_y2", "dest_x", "dest_y"]
            }),
            |args, ctx| async move { handle_copy_routing_pattern(args, ctx).await }
        ),
        tool!(
            "set_layer_constraints",
            "Set per-layer design constraints (e.g. min trace width, clearance) in the board setup section.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "layer": { "type": "string", "description": "Layer name (e.g. 'F.Cu', 'B.Cu')" },
                    "min_clearance": { "type": "number", "description": "Minimum clearance for this layer in mm" },
                    "min_trace_width": { "type": "number", "description": "Minimum trace width for this layer in mm" }
                },
                "required": ["board", "layer"]
            }),
            |args, ctx| async move { handle_set_layer_constraints(args, ctx).await }
        ),
        tool!(
            "check_clearance",
            "Check the physical clearance (distance) between two components on the PCB.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "ref1":  { "type": "string", "description": "First component reference (e.g. 'U1')" },
                    "ref2":  { "type": "string", "description": "Second component reference (e.g. 'C1')" }
                },
                "required": ["board", "ref1", "ref2"]
            }),
            |args, ctx| async move { handle_check_clearance(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

fn severity_rank(s: &str) -> u8 {
    match s {
        "error" => 2,
        "warning" => 1,
        _ => 0,
    }
}

async fn handle_run_drc(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let severity_filter = args["severity"].as_str().unwrap_or("warning");
    let min_rank = severity_rank(severity_filter);

    let refill = args["refill_zones"].as_bool().unwrap_or(false);
    let parity = args["schematic_parity"].as_bool().unwrap_or(false);
    let violations = cli::run_drc(&ctx.config.kicad_cli, &board, refill, parity).await?;

    // Optionally write report
    if let Some(out_path) = args["output"].as_str() {
        let report = serde_json::to_string_pretty(&violations)?;
        tokio::fs::write(out_path, report).await?;
    }

    let filtered: Vec<_> = violations
        .iter()
        .filter(|v| severity_rank(&v.severity) >= min_rank)
        .collect();

    let errors = filtered.iter().filter(|v| v.severity == "error").count();
    let warnings = filtered.iter().filter(|v| v.severity == "warning").count();
    // Break out the connectivity/parity sections so callers can assert the
    // MVP acceptance criterion: 0 unconnected, parity clean.
    let unconnected = filtered.iter().filter(|v| v.kind == "unconnected").count();
    let parity_issues = filtered.iter().filter(|v| v.kind == "parity").count();

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "total_violations": violations.len(),
            "filtered_count": filtered.len(),
            "errors": errors,
            "warnings": warnings,
            "unconnected": unconnected,
            "parity": parity_issues,
            "schematic_parity_checked": parity,
            "severity_filter": severity_filter,
            "violations": filtered.iter().map(|v| json!({
                "severity": v.severity,
                "kind": v.kind,
                "type": v.rule_type,
                "description": v.description,
                "pos": v.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y })),
                "items": v.items.iter().map(|it| json!({
                    "description": it.description,
                    "pos": it.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y })),
                    "uuid": it.uuid
                })).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        }))
        .unwrap(),
    ))
}

// ─── Design rules: modern (.kicad_pro JSON) project fallback ─────────────────
//
// KiCAD 7+ moved board-level constraints out of the .kicad_pcb entirely into
// the sibling .kicad_pro project file, under `board.design_settings.rules`.
// Confirmed live (2026-08-15) against a real KiCad 10 board: its .kicad_pcb
// has zero `min_*` constraint fields anywhere — `read_constraint`/
// `set_constraint` below were silently blind on any current-generation
// project, never erroring, just never finding (or ever effectively writing)
// anything. Each tool-facing arg key maps to the modern JSON key with the
// same meaning; `min_via_drill` maps to `min_through_hole_diameter` — KiCad
// 10 merged the old separate "via drill" and "through-hole diameter" floors
// into one constraint (confirmed against KiCad's own source: drc_engine.cpp
// builds its HOLE_SIZE_CONSTRAINT lower bound from
// board_design_settings.cpp's `m_MinThroughDrill`, applied to both via and
// pad holes) — not "no modern equivalent" as a first pass at this assumed.
const JSON_RULE_KEYS: &[(&str, &str)] = &[
    ("min_clearance", "min_clearance"),
    ("min_trace_width", "min_track_width"),
    ("min_via_drill", "min_through_hole_diameter"),
    ("min_via_size", "min_via_diameter"),
    ("min_hole_to_hole", "min_hole_to_hole"),
];

fn sibling_project_path(board: &std::path::Path) -> Option<std::path::PathBuf> {
    let candidate = board.with_extension("kicad_pro");
    candidate.exists().then_some(candidate)
}

fn read_project_rule(project_path: &std::path::Path, json_key: &str) -> Option<f64> {
    let content = std::fs::read_to_string(project_path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&content).ok()?;
    root["board"]["design_settings"]["rules"][json_key].as_f64()
}

/// Upsert `json_key` under `board.design_settings.rules` in the project file.
/// Errors (rather than silently no-op) if that path isn't an object — a
/// malformed/unexpected project file should surface, not fail quietly.
fn write_project_rule(
    project_path: &std::path::Path,
    json_key: &str,
    value: f64,
) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(project_path)?;
    let mut root: serde_json::Value = serde_json::from_str(&content)?;
    let rules = root
        .pointer_mut("/board/design_settings/rules")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project file has no board.design_settings.rules object: {}",
                project_path.display()
            )
        })?;
    rules.insert(json_key.to_string(), json!(value));
    // Trailing newline matches what KiCad's own writer produces — keeps a
    // one-field edit from also flipping the file's EOF-newline status.
    write_atomic(
        project_path,
        &format!("{}\n", serde_json::to_string_pretty(&root)?),
    )?;
    Ok(())
}

// ─── Design rules S-expression helpers ───────────────────────────────────────

/// Read a rule value from `(setup (rules (rule_severity key val) ...))`.
/// KiCAD stores rules in: `(setup ... (rules (rule_severity "..." ...) ...))`
/// But simple constraints are in `(setup (constraints ...))`.
fn read_constraint(content: &str, key: &str) -> Option<f64> {
    // Pattern: `(constraint clearance (min VAL))` or `(min_clearance VAL)` in setup
    // Try legacy format first: `(key VAL)` inside setup section
    let pat = format!("({} ", key);
    if let Some(pos) = content.find(&pat) {
        let after = &content[pos + pat.len()..];
        if let Some(end) = after.find(')') {
            return after[..end].trim().parse::<f64>().ok();
        }
    }
    // Try constraint format: `(constraint min_clearance (min VAL))`
    let cpat = format!("(constraint {} (min ", key);
    if let Some(pos) = content.find(&cpat) {
        let after = &content[pos + cpat.len()..];
        if let Some(end) = after.find(')') {
            return after[..end].trim().parse::<f64>().ok();
        }
    }
    None
}

/// Set or insert a rule inside the `(setup ...)` section.
fn set_constraint(content: &str, key: &str, value: f64) -> String {
    let pat = format!("({} ", key);

    if let Some(pos) = content.find(&pat) {
        // Replace existing value
        let end = content[pos..]
            .find(')')
            .map(|i| pos + i + 1)
            .unwrap_or(content.len());
        let new_entry = format!("({} {})", key, value);
        format!("{}{}{}", &content[..pos], new_entry, &content[end..])
    } else {
        // Insert into setup section before its closing paren
        if let Some(setup_pos) = content.find("(setup") {
            let setup_end = {
                let mut depth = 0i32;
                let mut end = setup_pos;
                for (i, ch) in content[setup_pos..].char_indices() {
                    match ch {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = setup_pos + i;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                end
            };
            let new_entry = format!("\n  ({} {})", key, value);
            format!(
                "{}{}{}",
                &content[..setup_end],
                new_entry,
                &content[setup_end..]
            )
        } else {
            content.to_string()
        }
    }
}

async fn handle_set_design_rules(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let mut content = tokio::fs::read_to_string(&board).await?;
    let project_path = sibling_project_path(&board);

    let mut changed = Vec::new();
    let mut content_dirty = false;

    let rules: &[(&str, &str)] = &[
        ("min_clearance", "min_clearance"),
        ("min_track_width", "min_trace_width"),
        ("min_via_drill", "min_via_drill"),
        ("min_via_size", "min_via_size"),
        ("min_hole_to_hole", "min_hole_to_hole"),
    ];

    for (sexp_key, arg_key) in rules {
        let Some(val) = args[arg_key].as_f64() else {
            continue;
        };
        let json_key = JSON_RULE_KEYS
            .iter()
            .find(|(k, _)| k == arg_key)
            .map(|(_, j)| *j);

        // A legacy field already present in THIS board's .kicad_pcb means
        // it's an old-format board actively using it — keep updating it in
        // place rather than switching source of truth out from under it.
        if read_constraint(&content, sexp_key).is_some() {
            content = set_constraint(&content, sexp_key, val);
            content_dirty = true;
            changed.push(format!("{sexp_key} = {val} (board setup)"));
            continue;
        }

        // No legacy field: prefer the modern project file when one exists
        // and this key has a JSON equivalent (every key does — see
        // JSON_RULE_KEYS).
        if let (Some(project_path), Some(json_key)) = (&project_path, json_key) {
            write_project_rule(project_path, json_key, val)?;
            changed.push(format!("{json_key} = {val} (project rules)"));
            continue;
        }

        // No sibling project file, or no JSON equivalent for this key:
        // fall back to the legacy insert, same as before this fix.
        content = set_constraint(&content, sexp_key, val);
        content_dirty = true;
        changed.push(format!("{sexp_key} = {val} (board setup)"));
    }

    if content_dirty {
        write_atomic(&board, &content)?;
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "changed": changed
        }))
        .unwrap(),
    ))
}

async fn handle_get_design_rules(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let content = tokio::fs::read_to_string(&board).await?;
    let project_path = sibling_project_path(&board);

    let rule = |sexp_key: &str, arg_key: &str| -> Option<f64> {
        read_constraint(&content, sexp_key).or_else(|| {
            let json_key = JSON_RULE_KEYS
                .iter()
                .find(|(k, _)| *k == arg_key)
                .map(|(_, j)| *j)?;
            read_project_rule(project_path.as_deref()?, json_key)
        })
    };

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "board": board.to_str().unwrap_or(""),
            "rules": {
                "min_clearance": rule("min_clearance", "min_clearance"),
                "min_trace_width": rule("min_track_width", "min_trace_width"),
                "min_via_drill": rule("min_via_drill", "min_via_drill"),
                "min_via_size": rule("min_via_size", "min_via_size"),
                "min_hole_to_hole": rule("min_hole_to_hole", "min_hole_to_hole")
            }
        }))
        .unwrap(),
    ))
}

#[cfg(test)]
mod design_rules_json_tests {
    use super::*;

    /// Mirrors the actual shape found in a real KiCad 10 project (ClearBell
    /// Ausseneinheit, 2026-08-15): `min_track_width` set above the "Default"
    /// class's own `track_width` — the exact contradiction that motivated
    /// this fallback in the first place.
    const SAMPLE_PROJECT: &str = r#"{
        "board": {
            "design_settings": {
                "rules": {
                    "min_clearance": 0.2,
                    "min_track_width": 0.3,
                    "min_via_diameter": 0.5,
                    "min_hole_to_hole": 0.25,
                    "min_through_hole_diameter": 0.3
                }
            }
        }
    }"#;

    fn write_sample(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("board.kicad_pro");
        std::fs::write(&path, SAMPLE_PROJECT).unwrap();
        path
    }

    #[test]
    fn read_project_rule_finds_existing_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path());
        assert_eq!(read_project_rule(&path, "min_track_width"), Some(0.3));
        assert_eq!(read_project_rule(&path, "min_via_diameter"), Some(0.5));
        assert_eq!(read_project_rule(&path, "no_such_key"), None);
    }

    #[test]
    fn write_project_rule_updates_key_and_preserves_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path());

        write_project_rule(&path, "min_track_width", 0.15).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let rules = &saved["board"]["design_settings"]["rules"];
        assert_eq!(rules["min_track_width"], 0.15, "the field we changed");
        assert_eq!(rules["min_clearance"], 0.2, "untouched sibling survives");
        assert_eq!(rules["min_hole_to_hole"], 0.25, "untouched sibling survives");
    }

    #[test]
    fn write_project_rule_errors_cleanly_without_rules_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pro");
        std::fs::write(&path, r#"{"meta": {"version": 4}}"#).unwrap();

        let err = write_project_rule(&path, "min_track_width", 0.3).unwrap_err();
        assert!(err.to_string().contains("design_settings.rules"));
    }

    #[test]
    fn sibling_project_path_none_when_no_project_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb)").unwrap();
        assert_eq!(sibling_project_path(&board), None);
    }

    #[test]
    fn sibling_project_path_some_when_project_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb)").unwrap();
        write_sample(dir.path());
        assert_eq!(sibling_project_path(&board), Some(dir.path().join("board.kicad_pro")));
    }

    /// End-to-end through the actual handlers: a board with no legacy
    /// constraint fields (the real-world case, KiCad 10) must round-trip
    /// set → get through the sibling project file, not silently no-op.
    #[tokio::test]
    async fn set_then_get_design_rules_round_trips_through_project_json() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (setup))").unwrap();
        write_sample(dir.path());

        let ctx = ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );

        let set_args = json!({
            "board": board.to_str().unwrap(),
            "min_trace_width": 0.35
        });
        handle_set_design_rules(&set_args, &ctx).await.unwrap();

        // The .kicad_pcb itself must stay untouched — no legacy field ever
        // existed there, so nothing should have been inserted into it.
        let pcb_content = std::fs::read_to_string(&board).unwrap();
        assert!(!pcb_content.contains("min_track_width"));

        let get_args = json!({ "board": board.to_str().unwrap() });
        let result = handle_get_design_rules(&get_args, &ctx).await.unwrap();
        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => {
                serde_json::from_str::<serde_json::Value>(text).unwrap()
            }
            _ => panic!("expected text content"),
        };
        assert_eq!(body["rules"]["min_trace_width"], 0.35);
        // Untouched sibling in the same JSON object still reads correctly.
        assert_eq!(body["rules"]["min_via_size"], 0.5);
    }

    /// Regression for the review finding (2026-08-15) that min_via_drill was
    /// mapped to nothing, based on a factually wrong claim that KiCad 10 has
    /// no per-board equivalent — it does (min_through_hole_diameter, merged
    /// with the old through-hole-diameter floor). Without a mapping, this
    /// key would always fall through to a dead legacy .kicad_pcb write on a
    /// modern board, exactly the silently-ineffective bug this file exists
    /// to fix.
    #[test]
    fn min_via_drill_maps_to_min_through_hole_diameter() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path());
        assert_eq!(read_project_rule(&path, "min_through_hole_diameter"), Some(0.3));

        write_project_rule(&path, "min_through_hole_diameter", 0.25).unwrap();
        assert_eq!(read_project_rule(&path, "min_through_hole_diameter"), Some(0.25));

        assert_eq!(
            JSON_RULE_KEYS
                .iter()
                .find(|(arg_key, _)| *arg_key == "min_via_drill")
                .map(|(_, json_key)| *json_key),
            Some("min_through_hole_diameter")
        );
    }

    /// Same regression, but through the actual async handlers rather than
    /// the table/helpers directly — catches a wiring bug (e.g. a typo'd
    /// arg_key in handle_set_design_rules's local `rules` table) that the
    /// table-only test above can't see.
    #[tokio::test]
    async fn set_then_get_min_via_drill_round_trips_through_project_json() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (setup))").unwrap();
        write_sample(dir.path());

        let ctx = ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );

        let set_args = json!({ "board": board.to_str().unwrap(), "min_via_drill": 0.2 });
        handle_set_design_rules(&set_args, &ctx).await.unwrap();

        let pcb_content = std::fs::read_to_string(&board).unwrap();
        assert!(
            !pcb_content.contains("min_via_drill"),
            "must land in the project JSON, not get inserted into the .kicad_pcb"
        );

        let get_args = json!({ "board": board.to_str().unwrap() });
        let result = handle_get_design_rules(&get_args, &ctx).await.unwrap();
        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => {
                serde_json::from_str::<serde_json::Value>(text).unwrap()
            }
            _ => panic!("expected text content"),
        };
        assert_eq!(body["rules"]["min_via_drill"], 0.2);
    }

    /// Regression for the alphabetical-reorder bug found in review
    /// (2026-08-15): without serde_json's preserve_order feature, writing
    /// back the whole project file resorts every key at every level. "zebra"
    /// sorts after "alpha" but appears first here — order must survive.
    #[test]
    fn write_project_rule_preserves_key_order_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pro");
        std::fs::write(
            &path,
            r#"{"board": {"design_settings": {"rules": {"zebra_field": 1, "alpha_field": 2, "min_clearance": 0.2}}}}"#,
        )
        .unwrap();

        write_project_rule(&path, "min_clearance", 0.15).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let zebra_pos = raw.find("zebra_field").unwrap();
        let alpha_pos = raw.find("alpha_field").unwrap();
        assert!(
            zebra_pos < alpha_pos,
            "original key order (zebra before alpha) must survive the round-trip, got: {raw}"
        );
    }
}

// ─── KiCAD UI management ──────────────────────────────────────────────────────

/// Check if the KiCAD GUI is running by scanning the process list.
fn is_kicad_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        // On Windows, use `tasklist` to check
        std::process::Command::new("tasklist")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("kicad.exe"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("pgrep")
            .arg("-x")
            .arg("kicad")
            .output()
            .ok()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Resolve the KiCAD binary path from config or well-known locations.
fn find_kicad_binary(config_binary: &str) -> String {
    if !config_binary.is_empty() && std::path::Path::new(config_binary).exists() {
        return config_binary.to_string();
    }
    #[cfg(target_os = "windows")]
    {
        // Scan common install roots and KiCAD version directories
        let roots = [
            r"C:\Program Files\KiCad",
            r"C:\KiCad",
            r"D:\KiCad",
            r"D:\Program Files\KiCad",
        ];
        let versions = ["10.0", "9.0", "8.0"];
        for root in &roots {
            for ver in &versions {
                let path = format!(r"{}\{}\bin\kicad.exe", root, ver);
                if std::path::Path::new(&path).exists() {
                    return path;
                }
            }
            // Also check without version subdir
            let path = format!(r"{}\bin\kicad.exe", root);
            if std::path::Path::new(&path).exists() {
                return path;
            }
        }
        "kicad".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad".to_string()
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        "kicad".to_string()
    }
}

async fn handle_check_kicad_ui(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let running = task::spawn_blocking(is_kicad_running).await?;

    if !running {
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&json!({
                "running": false,
                "ipc_responsive": false
            }))
            .unwrap(),
        ));
    }

    // Try IPC ping
    let addr = ctx.config.ipc_address.clone();
    let ipc_ok = task::spawn_blocking(move || {
        konnect_ipc::client::KiCadIpcClient::new(&addr)
            .ping()
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "running": true,
            "ipc_responsive": ipc_ok
        }))
        .unwrap(),
    ))
}

async fn handle_launch_kicad_ui(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let wait_ready = args["wait_ready"].as_bool().unwrap_or(true);
    let timeout_secs = args["timeout_seconds"].as_u64().unwrap_or(30);
    let binary = find_kicad_binary(&ctx.config.kicad_binary);

    let mut cmd = tokio::process::Command::new(&binary);
    if let Some(project) = args["project"].as_str() {
        cmd.arg(project);
    }

    // Spawn detached — we don't wait for the process to exit
    match cmd.spawn() {
        Ok(_child) => {
            if wait_ready {
                // Poll IPC until responsive or timeout
                let addr = ctx.config.ipc_address.clone();
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let addr2 = addr.clone();
                    let ok = task::spawn_blocking(move || {
                        konnect_ipc::client::KiCadIpcClient::new(&addr2)
                            .ping()
                            .unwrap_or(false)
                    })
                    .await
                    .unwrap_or(false);

                    if ok {
                        return Ok(CallToolResult::text(
                            serde_json::to_string_pretty(&json!({
                                "launched": true,
                                "ipc_ready": true
                            }))
                            .unwrap(),
                        ));
                    }
                    if std::time::Instant::now() >= deadline {
                        return Ok(CallToolResult::text(
                            serde_json::to_string_pretty(&json!({
                                "launched": true,
                                "ipc_ready": false,
                                "note": "KiCAD launched but IPC not yet responsive within timeout"
                            }))
                            .unwrap(),
                        ));
                    }
                }
            }

            Ok(CallToolResult::text(
                serde_json::to_string_pretty(&json!({
                    "launched": true,
                    "ipc_ready": null
                }))
                .unwrap(),
            ))
        }
        Err(e) => Ok(CallToolResult::error(format!(
            "Failed to launch KiCAD ({}): {}",
            binary, e
        ))),
    }
}

// ─── Copy routing pattern ─────────────────────────────────────────────────────

async fn handle_copy_routing_pattern(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let src_x1 = args["src_x1"].as_f64().unwrap_or(0.0);
    let src_y1 = args["src_y1"].as_f64().unwrap_or(0.0);
    let src_x2 = args["src_x2"].as_f64().unwrap_or(0.0);
    let src_y2 = args["src_y2"].as_f64().unwrap_or(0.0);
    let dest_x = args["dest_x"].as_f64().unwrap_or(0.0);
    let dest_y = args["dest_y"].as_f64().unwrap_or(0.0);

    let dx = dest_x - src_x1;
    let dy = dest_y - src_y1;

    let net_map: std::collections::HashMap<String, String> =
        if let Some(obj) = args["net_map"].as_object() {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    let content = tokio::fs::read_to_string(&board).await?;
    let mut new_tracks = Vec::new();

    // Find all (segment ...) and (via ...) blocks within the bounding box
    // and collect translated copies.
    for (block_start, block_end, _block_type) in find_routing_blocks(&content) {
        let block = &content[block_start..block_end];
        if let Some((bx, by)) = extract_start_xy(block) {
            if bx >= src_x1 && bx <= src_x2 && by >= src_y1 && by <= src_y2 {
                let translated = translate_block(block, dx, dy, &net_map);
                new_tracks.push(translated);
            }
        }
    }

    if new_tracks.is_empty() {
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&json!({
                "copied": 0,
                "note": "No routing elements found in the specified source region"
            }))
            .unwrap(),
        ));
    }

    // Insert all new blocks before the final `)` of the file
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let insertion = new_tracks.join("\n");
    let new_content = format!(
        "{}\n{}\n{}",
        &content[..insert_pos],
        insertion,
        &content[insert_pos..]
    );

    // Assign new UUIDs to inserted blocks (replace uuid "ORIGINAL" with new ones)
    let new_content = reassign_uuids(&new_content, insert_pos);

    write_atomic(&board, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "copied": new_tracks.len(),
            "dx": dx,
            "dy": dy
        }))
        .unwrap(),
    ))
}

/// Find all `(segment ...)` and `(via ...)` blocks in the PCB content.
/// Returns (start, end, type) tuples.
fn find_routing_blocks(content: &str) -> Vec<(usize, usize, &'static str)> {
    let mut results = Vec::new();
    for (prefix, kind) in &[("\n  (segment ", "segment"), ("\n  (via ", "via")] {
        let mut pos = 0;
        while let Some(found) = content[pos..].find(prefix) {
            let start = pos + found + 3; // skip \n
            let mut depth = 0i32;
            let mut end = start;
            for (i, ch) in content[start..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = start + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            results.push((start, end, *kind));
            pos = start + 1;
        }
    }
    results
}

/// Extract the `(start X Y)` coordinates from a routing block.
fn extract_start_xy(block: &str) -> Option<(f64, f64)> {
    let pat = "(start ";
    let pos = block.find(pat)?;
    let after = &block[pos + pat.len()..];
    let end = after.find(')')?;
    let parts: Vec<&str> = after[..end].split_whitespace().collect();
    let x = parts.first()?.parse::<f64>().ok()?;
    let y = parts.get(1)?.parse::<f64>().ok()?;
    Some((x, y))
}

/// Translate all coordinate pairs in a routing block by (dx, dy).
fn translate_block(
    block: &str,
    dx: f64,
    dy: f64,
    net_map: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = block.to_string();

    // Translate (start X Y), (end X Y), (at X Y) coordinate pairs
    for coord_key in &["start", "end", "at"] {
        let pat = format!("({} ", coord_key);
        let mut new_result = String::new();
        let mut remaining = result.as_str();
        while let Some(pos) = remaining.find(&pat) {
            new_result.push_str(&remaining[..pos]);
            new_result.push_str(&pat);
            let after = &remaining[pos + pat.len()..];
            if let Some(close) = after.find(')') {
                let coords_str = &after[..close];
                let parts: Vec<&str> = coords_str.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let (Ok(x), Ok(y)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                        new_result.push_str(&format!("{} {}", x + dx, y + dy));
                        if parts.len() > 2 {
                            new_result.push(' ');
                            new_result.push_str(&parts[2..].join(" "));
                        }
                        new_result.push(')');
                        remaining = &remaining[pos + pat.len() + close + 1..];
                        continue;
                    }
                }
                // Fall through if parsing failed
                new_result.push_str(coords_str);
                new_result.push(')');
                remaining = &remaining[pos + pat.len() + close + 1..];
            } else {
                break;
            }
        }
        new_result.push_str(remaining);
        result = new_result;
    }

    // Remap net names
    for (old_net, new_net) in net_map {
        let old_pat = format!("(net \"{}\")", old_net);
        let new_pat = format!("(net \"{}\")", new_net);
        result = result.replace(&old_pat, &new_pat);
        // Also handle numeric net references if needed (not replaced here)
    }

    result
}

/// Reassign UUIDs in all newly inserted blocks (those after `insert_boundary`).
fn reassign_uuids(content: &str, insert_boundary: usize) -> String {
    let mut result = String::with_capacity(content.len() + 64);
    result.push_str(&content[..insert_boundary]);
    let tail = &content[insert_boundary..];
    let mut remaining = tail;
    while let Some(pos) = remaining.find("(uuid \"") {
        result.push_str(&remaining[..pos]);
        result.push_str("(uuid \"");
        // Find end of UUID string
        let after = &remaining[pos + 7..];
        if let Some(end) = after.find('"') {
            let new_uuid = uuid::Uuid::new_v4().to_string();
            result.push_str(&new_uuid);
            result.push('"');
            remaining = &remaining[pos + 7 + end + 1..];
        } else {
            break;
        }
    }
    result.push_str(remaining);
    result
}

// ─── Symbol info ──────────────────────────────────────────────────────────────

// ─── Layer constraints ───────────────────────────────────────────────────────

async fn handle_set_layer_constraints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let mut content = tokio::fs::read_to_string(&board).await?;
    let mut changed = Vec::new();

    // Build a layer constraint rule block to insert into (setup ...)
    // KiCAD uses `(rule "name" (constraint ...) (condition "A.Layer == 'LAYER'"))` inside setup
    let rule_name = format!("{}_constraints", layer.replace('.', "_"));

    if let Some(clearance) = args["min_clearance"].as_f64() {
        let rule_sexp = format!(
            "\n    (rule \"{rule_name}_clearance\"\n      (constraint clearance (min {clearance}))\n      (condition \"A.Layer == '{layer}'\")\n    )"
        );
        // Insert into setup block
        if let Some(setup_pos) = content.find("(setup") {
            let mut depth = 0i32;
            let mut setup_end = setup_pos;
            for (i, ch) in content[setup_pos..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            setup_end = setup_pos + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            content = format!(
                "{}{}{}",
                &content[..setup_end],
                rule_sexp,
                &content[setup_end..]
            );
            changed.push(format!("clearance = {} on {}", clearance, layer));
        }
    }

    if let Some(trace_width) = args["min_trace_width"].as_f64() {
        let rule_sexp = format!(
            "\n    (rule \"{rule_name}_trace_width\"\n      (constraint track_width (min {trace_width}))\n      (condition \"A.Layer == '{layer}'\")\n    )"
        );
        if let Some(setup_pos) = content.find("(setup") {
            let mut depth = 0i32;
            let mut setup_end = setup_pos;
            for (i, ch) in content[setup_pos..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            setup_end = setup_pos + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            content = format!(
                "{}{}{}",
                &content[..setup_end],
                rule_sexp,
                &content[setup_end..]
            );
            changed.push(format!("min_trace_width = {} on {}", trace_width, layer));
        }
    }

    if !changed.is_empty() {
        write_atomic(&board, &content)?;
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "layer": layer,
            "changed": changed
        }))
        .unwrap(),
    ))
}

// ─── Check clearance ─────────────────────────────────────────────────────────

async fn handle_check_clearance(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    let pos1 = find_footprint_position(&tree, &ref1)?;
    let pos2 = find_footprint_position(&tree, &ref2)?;

    let dx = pos2.0 - pos1.0;
    let dy = pos2.1 - pos1.1;
    let distance = (dx * dx + dy * dy).sqrt();

    Ok(CallToolResult::json(&json!({
        "ref1": ref1,
        "ref2": ref2,
        "pos1": { "x": pos1.0, "y": pos1.1 },
        "pos2": { "x": pos2.0, "y": pos2.1 },
        "distance_mm": (distance * 1000.0).round() / 1000.0
    })))
}

/// Look up the board-space (x, y) position of a footprint by its reference designator.
fn find_footprint_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
) -> anyhow::Result<(f64, f64)> {
    let fp_node = tree
        .find_all("footprint")
        .into_iter()
        .find(|fp| {
            fp.find_all("property").iter().any(|p| {
                p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                    && p.get(2).and_then(|n| n.as_str()) == Some(reference)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found on board", reference))?;

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);

    Ok((fp_x, fp_y))
}
