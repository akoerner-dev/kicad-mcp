//! `library` toolset — create and manage footprints, symbols, and KiCAD library tables.
//!
//! Operations are file-based (S-expression manipulation + directory scanning).
//! No IPC or kicad-cli is required for most tools.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use konnect_sexp::parser::{parse_sexp, SexpNode};
use konnect_sexp::writer::write_atomic;
use serde_json::json;
use std::path::{Path, PathBuf};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_footprint",
            "Create a new footprint (.kicad_mod) file from a pad layout description.",
            json!({
                "type": "object",
                "properties": {
                    "output": { "type": "string", "description": "Output .kicad_mod file path" },
                    "name": { "type": "string", "description": "Footprint name" },
                    "description": { "type": "string", "description": "Footprint description (optional)" },
                    "pads": {
                        "type": "array",
                        "description": "Pad definitions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "number": { "type": "string" },
                                "type": { "type": "string", "description": "'smd', 'thru_hole', 'np_thru_hole'" },
                                "shape": { "type": "string", "description": "'rect', 'oval', 'circle', 'roundrect'" },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "width": { "type": "number" },
                                "height": { "type": "number" },
                                "drill": { "type": "number", "description": "Drill diameter for thru-hole pads" }
                            },
                            "required": ["number", "type", "shape", "x", "y", "width", "height"]
                        }
                    },
                    "body_width": { "type": "number", "description": "Physical component body width in mm (optional; used for silk/fab outlines). Falls back to the pad envelope if omitted." },
                    "body_height": { "type": "number", "description": "Physical component body height in mm (optional)." },
                    "package_type": { "type": "string", "description": "'smd' (0.25mm courtyard), 'through_hole' (0.5mm), 'small' (0.15mm, <0603), or 'bga' (1.0mm). Sets courtyard clearance when courtyard_clearance is not given." },
                    "courtyard_clearance": { "type": "number", "description": "Explicit courtyard clearance in mm (overrides package_type / auto-detection)." },
                    "model": {
                        "type": "object",
                        "description": "Optional 3D model to associate with the footprint.",
                        "properties": {
                            "path": { "type": "string", "description": "Path to the 3D model file (.step/.wrl); absolute or a KiCAD env-var path like ${KICAD9_3DMODEL_DIR}/..." },
                            "offset": { "type": "object", "description": "{x,y,z} in mm (default 0,0,0)" },
                            "scale": { "type": "object", "description": "{x,y,z} (default 1,1,1)" },
                            "rotate": { "type": "object", "description": "{x,y,z} in degrees (default 0,0,0)" }
                        },
                        "required": ["path"]
                    }
                },
                "required": ["output", "name", "pads"]
            }),
            |args, ctx| async move { handle_create_footprint(args, ctx).await }
        ),
        tool!(
            "edit_footprint_pad",
            "Edit the size, shape, or position of a pad in an existing .kicad_mod footprint file.",
            json!({
                "type": "object",
                "properties": {
                    "footprint_path": { "type": "string", "description": "Path to .kicad_mod file" },
                    "pad_number": { "type": "string", "description": "Pad number to edit" },
                    "x": { "type": "number", "description": "New X position in mm (optional)" },
                    "y": { "type": "number", "description": "New Y position in mm (optional)" },
                    "width": { "type": "number", "description": "New pad width in mm (optional)" },
                    "height": { "type": "number", "description": "New pad height in mm (optional)" },
                    "shape": { "type": "string", "description": "New pad shape (optional)" },
                    "drill": { "type": "number", "description": "New drill diameter in mm (optional)" }
                },
                "required": ["footprint_path", "pad_number"]
            }),
            |args, ctx| async move { handle_edit_footprint_pad(args, ctx).await }
        ),
        tool!(
            "register_footprint_library",
            "Register a local footprint library directory in the KiCAD global or project library table.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .pretty directory" },
                    "nickname": { "type": "string", "description": "Library nickname" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global' or 'project'",
                        "default": "project"
                    },
                    "project": { "type": "string", "description": "Path to .kicad_pro file (required for project scope)" }
                },
                "required": ["library_path", "nickname"]
            }),
            |args, ctx| async move { handle_register_footprint_library(args, ctx).await }
        ),
        tool!(
            "list_footprint_libraries",
            "List all registered footprint libraries (global and optionally project-level).",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro to include project libraries (optional)" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global', 'project', or 'all'",
                        "default": "all"
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_list_footprint_libraries(args, ctx).await }
        ),
        tool!(
            "create_symbol",
            "Create a new KiCAD schematic symbol and append it to a .kicad_sym library file.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" },
                    "name": { "type": "string", "description": "Symbol name" },
                    "reference_prefix": { "type": "string", "description": "Default reference prefix (e.g. 'U')" },
                    "value": { "type": "string", "description": "Default value string" },
                    "pins": {
                        "type": "array",
                        "description": "Pin definitions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "number": { "type": "string" },
                                "name": { "type": "string" },
                                "type": { "type": "string", "description": "'input', 'output', 'bidirectional', 'power_in', 'power_out', 'passive'" },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "angle": { "type": "number", "default": 0 },
                                "length": { "type": "number", "default": 2.54 }
                            },
                            "required": ["number", "name", "type", "x", "y"]
                        }
                    },
                    "show_pin_names": { "type": "boolean", "description": "Show pin names on the symbol (default true).", "default": true },
                    "show_pin_numbers": { "type": "boolean", "description": "Show pin numbers on the symbol (default true).", "default": true }
                },
                "required": ["library_path", "name", "reference_prefix", "pins"]
            }),
            |args, ctx| async move { handle_create_symbol(args, ctx).await }
        ),
        tool!(
            "delete_symbol",
            "Delete a symbol definition from a .kicad_sym library file.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" },
                    "symbol_name": { "type": "string", "description": "Name of the symbol to delete" }
                },
                "required": ["library_path", "symbol_name"]
            }),
            |args, ctx| async move { handle_delete_symbol(args, ctx).await }
        ),
        tool!(
            "list_symbols_in_library",
            "List all symbol names defined in a .kicad_sym library file.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" }
                },
                "required": ["library_path"]
            }),
            |args, ctx| async move { handle_list_symbols_in_library(args, ctx).await }
        ),
        tool!(
            "register_symbol_library",
            "Register a .kicad_sym library file in the KiCAD global or project symbol table.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym file" },
                    "nickname": { "type": "string", "description": "Library nickname" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global' or 'project'",
                        "default": "project"
                    },
                    "project": { "type": "string", "description": "Path to .kicad_pro file (required for project scope)" }
                },
                "required": ["library_path", "nickname"]
            }),
            |args, ctx| async move { handle_register_symbol_library(args, ctx).await }
        ),
        tool!(
            "list_symbol_libraries",
            "List all registered symbol libraries (global and optionally project-level).",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro to include project libraries (optional)" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global', 'project', or 'all'",
                        "default": "all"
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_list_symbol_libraries(args, ctx).await }
        ),
        tool!(
            "search_symbols",
            "Search for symbols across all registered libraries by name or keyword.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search string (partial name or keyword match)" },
                    "limit": { "type": "integer", "description": "Maximum number of results to return", "default": 50 }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_symbols(args, ctx).await }
        ),
        tool!(
            "list_library_footprints",
            "List all footprints in a specific registered footprint library (.pretty directory).",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .pretty directory (or nickname to look up)" }
                },
                "required": ["library_path"]
            }),
            |args, ctx| async move { handle_list_library_footprints(args, ctx).await }
        ),
        tool!(
            "get_footprint_info",
            "Return detailed information about a footprint: full pad geometry (padstacks), courtyard bbox, attr, 3D model, description. Resolves 'Library:Footprint' via the fp-lib-table (expands ${KICAD*_FOOTPRINT_DIR} and follows nested standard-library tables).",
            json!({
                "type": "object",
                "properties": {
                    "footprint_path": { "type": "string", "description": "Path to .kicad_mod file, OR 'Library:Footprint' identifier" },
                    "project": { "type": "string", "description": "Optional path to a .kicad_pro/.kicad_pcb to also consult the project fp-lib-table when resolving a 'Library:Footprint' id" }
                },
                "required": ["footprint_path"]
            }),
            |args, ctx| async move { handle_get_footprint_info(args, ctx).await }
        ),
        tool!(
            "search_footprints",
            "Search for footprints across all registered libraries by name or keyword.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search string (partial name or keyword)" },
                    "limit": { "type": "integer", "description": "Maximum number of results to return", "default": 50 }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_footprints(args, ctx).await }
        ),
        tool!(
            "get_symbol_info",
            "Return detailed information about a schematic symbol: pins, properties, description.",
            json!({
                "type": "object",
                "properties": {
                    "lib_id": { "type": "string", "description": "Library:Symbol identifier (e.g. 'Device:R')" }
                },
                "required": ["lib_id"]
            }),
            |args, ctx| async move { handle_get_symbol_info(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

// ─── Footprint / symbol geometry (pure, unit-tested) ──────────────────────────

/// Minimal pad geometry needed to derive outlines, courtyards, and pin 1.
#[derive(Debug, Clone)]
struct PadGeom {
    number: String,
    pad_type: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Axis-aligned bounding box `(min_x, min_y, max_x, max_y)` over pad extents.
fn pads_bbox(pads: &[PadGeom]) -> (f64, f64, f64, f64) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in pads {
        min_x = min_x.min(p.x - p.w / 2.0);
        min_y = min_y.min(p.y - p.h / 2.0);
        max_x = max_x.max(p.x + p.w / 2.0);
        max_y = max_y.max(p.y + p.h / 2.0);
    }
    (min_x, min_y, max_x, max_y)
}

/// Courtyard clearance per the contributor's rule: an explicit value wins, else
/// `package_type`, else auto-detect (through-hole 0.5 mm, sub-0603 body 0.15 mm,
/// otherwise SMT 0.25 mm). BGA (1.0 mm) is opt-in via `package_type` because an
/// area array can't be reliably auto-detected from pads alone.
fn courtyard_clearance(
    explicit: Option<f64>,
    package_type: Option<&str>,
    pads: &[PadGeom],
    body: Option<(f64, f64)>,
) -> f64 {
    if let Some(c) = explicit {
        return c;
    }
    match package_type {
        Some("bga") => return 1.0,
        Some("small") => return 0.15,
        Some("through_hole") | Some("th") => return 0.5,
        Some("smd") => return 0.25,
        _ => {}
    }
    if pads.iter().any(|p| p.pad_type.contains("thru")) {
        return 0.5;
    }
    if let Some((bw, bh)) = body {
        // 0603 imperial body is 1.6 x 0.8 mm; anything shorter is "smaller".
        if bw.max(bh) < 1.6 {
            return 0.15;
        }
    }
    0.25
}

/// Index of pin 1: the pad numbered "1", else the first pad. `None` if no pads.
fn pin1_index(pads: &[PadGeom]) -> Option<usize> {
    if pads.is_empty() {
        return None;
    }
    Some(pads.iter().position(|p| p.number == "1").unwrap_or(0))
}

/// The rectangle corner (of the four) nearest point `(px, py)`.
fn nearest_corner(min_x: f64, min_y: f64, max_x: f64, max_y: f64, px: f64, py: f64) -> (f64, f64) {
    let cx = if (px - min_x).abs() <= (max_x - px).abs() {
        min_x
    } else {
        max_x
    };
    let cy = if (py - min_y).abs() <= (max_y - py).abs() {
        min_y
    } else {
        max_y
    };
    (cx, cy)
}

fn point_toward(from: (f64, f64), toward: (f64, f64), d: f64) -> (f64, f64) {
    let dx = toward.0 - from.0;
    let dy = toward.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 {
        return from;
    }
    (from.0 + dx / len * d, from.1 + dy / len * d)
}

/// Ordered vertices of a rectangle outline whose corner nearest `(px, py)` is
/// chamfered by `chamfer` mm (clamped to 40% of the shorter side) — the F.Fab
/// pin-1 marker. Clockwise, KiCAD footprint Y-down.
fn chamfered_rect_points(
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    px: f64,
    py: f64,
    chamfer: f64,
) -> Vec<(f64, f64)> {
    let ch = chamfer
        .min(0.4 * (max_x - min_x).min(max_y - min_y))
        .max(0.0);
    let corners = [
        (min_x, min_y),
        (max_x, min_y),
        (max_x, max_y),
        (min_x, max_y),
    ];
    let (tcx, tcy) = nearest_corner(min_x, min_y, max_x, max_y, px, py);
    let mut pts = Vec::new();
    for (i, &(cx, cy)) in corners.iter().enumerate() {
        if (cx - tcx).abs() < 1e-9 && (cy - tcy).abs() < 1e-9 && ch > 0.0 {
            let prev = corners[(i + 3) % 4];
            let next = corners[(i + 1) % 4];
            pts.push(point_toward((cx, cy), prev, ch));
            pts.push(point_toward((cx, cy), next, ch));
        } else {
            pts.push((cx, cy));
        }
    }
    pts
}

/// Emit the `(model ...)` block when a `model` object with a non-empty `path`
/// is present. Path is passed through verbatim (absolute or KiCAD env-var).
fn build_model_sexp(args: &serde_json::Value) -> String {
    let model = match args.get("model") {
        Some(m) if m.is_object() => m,
        _ => return String::new(),
    };
    let path = match model["path"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return String::new(),
    };
    let xyz = |key: &str, default: f64| -> (f64, f64, f64) {
        let o = &model[key];
        (
            o["x"].as_f64().unwrap_or(default),
            o["y"].as_f64().unwrap_or(default),
            o["z"].as_f64().unwrap_or(default),
        )
    };
    let (ox, oy, oz) = xyz("offset", 0.0);
    let (sx, sy, sz) = xyz("scale", 1.0);
    let (rx, ry, rz) = xyz("rotate", 0.0);
    format!(
        "\n  (model \"{}\"\n    (offset (xyz {} {} {}))\n    (scale (xyz {} {} {}))\n    (rotate (xyz {} {} {}))\n  )",
        path, ox, oy, oz, sx, sy, sz, rx, ry, rz
    )
}

/// Build the courtyard, silkscreen, fab outline, reference/value text, and the
/// pin-1 marker (silk dot + fab chamfer) for a footprint from its pad geometry.
fn build_footprint_graphics(args: &serde_json::Value, name: &str, pads: &[PadGeom]) -> String {
    let (pmin_x, pmin_y, pmax_x, pmax_y) = pads_bbox(pads);

    let body = match (args["body_width"].as_f64(), args["body_height"].as_f64()) {
        (Some(bw), Some(bh)) => Some((bw, bh)),
        _ => None,
    };
    let clearance = courtyard_clearance(
        args["courtyard_clearance"].as_f64(),
        args["package_type"].as_str(),
        pads,
        body,
    );

    // Courtyard: pad envelope + clearance.
    let (cmin_x, cmin_y, cmax_x, cmax_y) = (
        pmin_x - clearance,
        pmin_y - clearance,
        pmax_x + clearance,
        pmax_y + clearance,
    );

    // Silk: just outside the pad envelope so it clears pads (avoids the
    // silk-over-pad DRC violation) regardless of the body outline.
    let silk_margin = 0.15;
    let (smin_x, smin_y, smax_x, smax_y) = (
        pmin_x - silk_margin,
        pmin_y - silk_margin,
        pmax_x + silk_margin,
        pmax_y + silk_margin,
    );

    // Fab: the component body when given, else the pad envelope. May overlap
    // pads — fab is a documentation layer, not subject to silk-over-pad rules.
    let (fmin_x, fmin_y, fmax_x, fmax_y) = match body {
        Some((bw, bh)) => {
            let cx = (pmin_x + pmax_x) / 2.0;
            let cy = (pmin_y + pmax_y) / 2.0;
            (cx - bw / 2.0, cy - bh / 2.0, cx + bw / 2.0, cy + bh / 2.0)
        }
        None => (pmin_x, pmin_y, pmax_x, pmax_y),
    };

    let mut s = String::new();

    // Courtyard rectangle (F.CrtYd) — required for DRC.
    s.push_str(&format!(
        "\n  (fp_rect (start {:.4} {:.4}) (end {:.4} {:.4}) (stroke (width 0.05) (type solid)) (fill none) (layer \"F.CrtYd\"))",
        cmin_x, cmin_y, cmax_x, cmax_y
    ));
    // Silkscreen outline (F.SilkS).
    s.push_str(&format!(
        "\n  (fp_rect (start {:.4} {:.4}) (end {:.4} {:.4}) (stroke (width 0.12) (type solid)) (fill none) (layer \"F.SilkS\"))",
        smin_x, smin_y, smax_x, smax_y
    ));

    if let Some(i1) = pin1_index(pads) {
        let p1 = &pads[i1];

        // Fab outline with the pin-1 corner chamfered.
        let chamfer = (0.25 * (fmax_x - fmin_x).min(fmax_y - fmin_y)).clamp(0.3, 1.0);
        let pts = chamfered_rect_points(fmin_x, fmin_y, fmax_x, fmax_y, p1.x, p1.y, chamfer);
        let pts_str: String = pts
            .iter()
            .map(|(x, y)| format!("(xy {:.4} {:.4}) ", x, y))
            .collect();
        s.push_str(&format!(
            "\n  (fp_poly (pts {}) (stroke (width 0.1) (type solid)) (fill none) (layer \"F.Fab\"))",
            pts_str.trim()
        ));

        // Silk pin-1 dot just outside the silk outline, aligned with pin 1's
        // pad — NOT at the footprint corner, where a dot is ambiguous between
        // pin 1 and the last pin that shares the same corner. It sits directly
        // beside pin 1 so the mark is unmistakable.
        let bcx = (pmin_x + pmax_x) / 2.0;
        let bcy = (pmin_y + pmax_y) / 2.0;
        let (dx, dy) = if (p1.x - bcx).abs() >= (p1.y - bcy).abs() {
            // Pin 1 is on a left/right edge: dot outside that edge, at pin 1's y.
            let sign = if p1.x < bcx { -1.0 } else { 1.0 };
            let edge = if sign < 0.0 { smin_x } else { smax_x };
            (edge + sign * 0.4, p1.y)
        } else {
            // Pin 1 is on a top/bottom edge: dot outside that edge, at pin 1's x.
            let sign = if p1.y < bcy { -1.0 } else { 1.0 };
            let edge = if sign < 0.0 { smin_y } else { smax_y };
            (p1.x, edge + sign * 0.4)
        };
        s.push_str(&format!(
            "\n  (fp_circle (center {:.4} {:.4}) (end {:.4} {:.4}) (stroke (width 0.1) (type solid)) (fill solid) (layer \"F.SilkS\"))",
            dx, dy, dx + 0.15, dy
        ));
    } else {
        // No pads to mark pin 1 against — plain fab rectangle.
        s.push_str(&format!(
            "\n  (fp_rect (start {:.4} {:.4}) (end {:.4} {:.4}) (stroke (width 0.1) (type solid)) (fill none) (layer \"F.Fab\"))",
            fmin_x, fmin_y, fmax_x, fmax_y
        ));
    }

    // Reference (F.SilkS, above) and value (F.Fab, below).
    let cx = (pmin_x + pmax_x) / 2.0;
    s.push_str(&format!(
        "\n  (fp_text reference \"REF**\" (at {:.4} {:.4} 0) (layer \"F.SilkS\") (effects (font (size 1 1) (thickness 0.15))))",
        cx, cmin_y - 1.0
    ));
    s.push_str(&format!(
        "\n  (fp_text value \"{}\" (at {:.4} {:.4} 0) (layer \"F.Fab\") (effects (font (size 1 1) (thickness 0.15))))",
        name, cx, cmax_y + 1.0
    ));

    s
}

async fn handle_create_footprint(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let output = get_path(args, "output")?;
    let name = args["name"].as_str().unwrap_or("Footprint");
    let description = args["description"].as_str().unwrap_or("");

    let pads_val = args["pads"].as_array().cloned().unwrap_or_default();
    let mut pad_geoms: Vec<PadGeom> = Vec::new();
    let mut pad_sexp = String::new();
    for pad in &pads_val {
        let number = pad["number"].as_str().unwrap_or("1").to_string();
        let pad_type = pad["type"].as_str().unwrap_or("smd").to_string();
        let shape = pad["shape"].as_str().unwrap_or("rect");
        let x = pad["x"].as_f64().unwrap_or(0.0);
        let y = pad["y"].as_f64().unwrap_or(0.0);
        let w = pad["width"].as_f64().unwrap_or(1.0);
        let h = pad["height"].as_f64().unwrap_or(1.0);

        let layers = if pad_type == "smd" {
            r#"(layers "F.Cu" "F.Paste" "F.Mask")"#
        } else {
            r#"(layers "*.Cu" "*.Mask")"#
        };

        let drill_sexp = if let Some(drill) = pad["drill"].as_f64() {
            format!("(drill {})", drill)
        } else {
            String::new()
        };

        pad_sexp.push_str(&format!(
            "\n  (pad \"{}\" {} {} (at {} {}) (size {} {}) {} {})",
            number, pad_type, shape, x, y, w, h, layers, drill_sexp
        ));
        pad_geoms.push(PadGeom {
            number,
            pad_type,
            x,
            y,
            w,
            h,
        });
    }

    // Courtyard, silk, fab, text, and pin-1 marker, derived from pad geometry.
    let graphics = if pad_geoms.is_empty() {
        String::new()
    } else {
        build_footprint_graphics(args, name, &pad_geoms)
    };
    let model_sexp = build_model_sexp(args);

    let attr = if pad_geoms.iter().any(|p| p.pad_type == "smd") {
        "smd"
    } else {
        "through_hole"
    };

    let content = format!(
        "(footprint \"{}\"\n  (version 20240108)\n  (generator \"konnect\")\n  (layer \"F.Cu\")\n  (descr \"{}\")\n  (attr {}){}{}{}\n)",
        name, description, attr, pad_sexp, graphics, model_sexp
    );

    // Ensure parent directory exists
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(&output, &content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "footprint": name,
            "output": output.to_str().unwrap_or(""),
            "pad_count": pad_geoms.len(),
            "courtyard": !pad_geoms.is_empty(),
            "pin1_marked": !pad_geoms.is_empty(),
            "model": args.get("model").and_then(|m| m["path"].as_str()).unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_edit_footprint_pad(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "footprint_path")?;
    let pad_number = require_str(args, "pad_number").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let content = tokio::fs::read_to_string(&path).await?;

    // Find the pad block:  (pad "N" ... (at X Y) (size W H) ...)
    // We search for the at/size/drill atoms and replace them individually.
    let pad_pat = format!(r#"(pad "{}""#, pad_number);
    let pad_start = content
        .find(&pad_pat)
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found in footprint", pad_number))?;

    // Find the closing paren of this pad block (simple depth count)
    let pad_end = {
        let mut depth = 0i32;
        let mut end = pad_start;
        for (i, ch) in content[pad_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = pad_start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        end
    };
    let pad_block = &content[pad_start..pad_end];

    // Helper: replace or add a sub-expression within the pad block
    let mut new_pad = pad_block.to_string();

    if let Some(x) = args["x"].as_f64() {
        // Replace (at OLD_X OLD_Y [ROT]) → update X
        if let Some(at_pos) = new_pad.find("(at ") {
            let at_end = new_pad[at_pos..]
                .find(')')
                .map(|i| at_pos + i + 1)
                .unwrap_or(new_pad.len());
            let at_block = &new_pad[at_pos..at_end];
            // Parse existing values
            let parts: Vec<&str> = at_block
                .trim_start_matches("(at ")
                .trim_end_matches(')')
                .split_whitespace()
                .collect();
            let old_y = parts
                .get(1)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts.get(2).map(|s| format!(" {}", s)).unwrap_or_default();
            let new_at = format!("(at {} {}{})", x, old_y, rot);
            new_pad.replace_range(at_pos..at_end, &new_at);
        }
    }
    if let Some(y) = args["y"].as_f64() {
        if let Some(at_pos) = new_pad.find("(at ") {
            let at_end = new_pad[at_pos..]
                .find(')')
                .map(|i| at_pos + i + 1)
                .unwrap_or(new_pad.len());
            let at_block = &new_pad[at_pos..at_end];
            let parts: Vec<&str> = at_block
                .trim_start_matches("(at ")
                .trim_end_matches(')')
                .split_whitespace()
                .collect();
            let old_x = parts
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts.get(2).map(|s| format!(" {}", s)).unwrap_or_default();
            let new_at = format!("(at {} {}{})", old_x, y, rot);
            new_pad.replace_range(at_pos..at_end, &new_at);
        }
    }
    if let (Some(w), Some(h)) = (args["width"].as_f64(), args["height"].as_f64()) {
        if let Some(sz_pos) = new_pad.find("(size ") {
            let sz_end = new_pad[sz_pos..]
                .find(')')
                .map(|i| sz_pos + i + 1)
                .unwrap_or(new_pad.len());
            let new_size = format!("(size {} {})", w, h);
            new_pad.replace_range(sz_pos..sz_end, &new_size);
        }
    }
    if let Some(drill) = args["drill"].as_f64() {
        if let Some(dr_pos) = new_pad.find("(drill ") {
            let dr_end = new_pad[dr_pos..]
                .find(')')
                .map(|i| dr_pos + i + 1)
                .unwrap_or(new_pad.len());
            let new_drill = format!("(drill {})", drill);
            new_pad.replace_range(dr_pos..dr_end, &new_drill);
        } else {
            // Insert drill before closing paren of pad
            let insert_at = new_pad.rfind(')').unwrap_or(new_pad.len());
            new_pad.insert_str(insert_at, &format!(" (drill {})", drill));
        }
    }

    // Apply the pad block replacement
    let new_content = format!(
        "{}{}{}",
        &content[..pad_start],
        new_pad,
        &content[pad_end..]
    );
    write_atomic(&path, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "pad": pad_number
        }))
        .unwrap(),
    ))
}

// ─── Library table helpers ────────────────────────────────────────────────────

/// Returns the path to the global fp-lib-table file.
fn global_fp_lib_table() -> PathBuf {
    super::kicad_config_dir().join("fp-lib-table")
}

/// Returns the path to the global sym-lib-table file.
fn global_sym_lib_table() -> PathBuf {
    super::kicad_config_dir().join("sym-lib-table")
}

/// Parse a lib-table into `{nickname, uri, type, description}` objects. Goes
/// through the S-expression parser, so it is robust to the tab indentation KiCad
/// writes — a substring scan for `"\n  (lib "` (two spaces) parsed real KiCad
/// tables as empty, since KiCad indents entries with a tab.
fn parse_lib_table(content: &str) -> Vec<serde_json::Value> {
    let Ok(tree) = parse_sexp(content) else {
        return Vec::new();
    };
    tree.find_all("lib")
        .into_iter()
        .map(|lib| {
            json!({
                "nickname": lib.find_str("name").unwrap_or_default(),
                "uri": lib.find_str("uri").unwrap_or_default(),
                "type": lib.find_str("type").unwrap_or_default(),
                "description": lib.find_str("descr").unwrap_or_default(),
            })
        })
        .collect()
}

/// Extract a quoted string value from `(key "value")` within a block.
fn extract_sexp_string(block: &str, key: &str) -> Option<String> {
    let pat = format!("({} \"", key);
    let start = block.find(&pat)? + pat.len();
    let end = block[start..].find('"')? + start;
    Some(block[start..end].to_string())
}

async fn handle_register_footprint_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let nickname = require_str(args, "nickname").map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let scope = args["scope"].as_str().unwrap_or("project");

    let table_path = if scope == "global" {
        global_fp_lib_table()
    } else if let Some(proj) = args["project"].as_str() {
        PathBuf::from(proj)
            .parent()
            .unwrap_or(Path::new("."))
            .join("fp-lib-table")
    } else {
        return Ok(CallToolResult::error(
            "For project scope, provide 'project' path to .kicad_pro file",
        ));
    };

    register_in_lib_table(
        &table_path,
        nickname,
        lib_path.to_str().unwrap_or(""),
        "KiCad",
    )
    .await?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "nickname": nickname,
            "scope": scope,
            "table": table_path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_list_footprint_libraries(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let scope = args["scope"].as_str().unwrap_or("all");
    let mut all_libs = Vec::new();

    if scope == "global" || scope == "all" {
        let table = global_fp_lib_table();
        if table.exists() {
            let content = tokio::fs::read_to_string(&table).await?;
            let mut libs = parse_lib_table(&content);
            for lib in &mut libs {
                lib["scope"] = json!("global");
            }
            all_libs.extend(libs);
        }
    }

    if (scope == "project" || scope == "all") && args["project"].is_string() {
        let proj = PathBuf::from(args["project"].as_str().unwrap());
        let table = proj.parent().unwrap_or(Path::new(".")).join("fp-lib-table");
        if table.exists() {
            let content = tokio::fs::read_to_string(&table).await?;
            let mut libs = parse_lib_table(&content);
            for lib in &mut libs {
                lib["scope"] = json!("project");
            }
            all_libs.extend(libs);
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "count": all_libs.len(),
            "libraries": all_libs
        }))
        .unwrap(),
    ))
}

async fn handle_register_symbol_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let nickname = require_str(args, "nickname").map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let scope = args["scope"].as_str().unwrap_or("project");

    let table_path = if scope == "global" {
        global_sym_lib_table()
    } else if let Some(proj) = args["project"].as_str() {
        PathBuf::from(proj)
            .parent()
            .unwrap_or(Path::new("."))
            .join("sym-lib-table")
    } else {
        return Ok(CallToolResult::error(
            "For project scope, provide 'project' path to .kicad_pro file",
        ));
    };

    register_in_lib_table(
        &table_path,
        nickname,
        lib_path.to_str().unwrap_or(""),
        "KiCad",
    )
    .await?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "nickname": nickname,
            "scope": scope,
            "table": table_path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_list_symbol_libraries(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let scope = args["scope"].as_str().unwrap_or("all");
    let mut all_libs = Vec::new();

    if scope == "global" || scope == "all" {
        let table = global_sym_lib_table();
        if table.exists() {
            let content = tokio::fs::read_to_string(&table).await?;
            let mut libs = parse_lib_table(&content);
            for lib in &mut libs {
                lib["scope"] = json!("global");
            }
            all_libs.extend(libs);
        }
    }

    if (scope == "project" || scope == "all") && args["project"].is_string() {
        let proj = PathBuf::from(args["project"].as_str().unwrap());
        let table = proj
            .parent()
            .unwrap_or(Path::new("."))
            .join("sym-lib-table");
        if table.exists() {
            let content = tokio::fs::read_to_string(&table).await?;
            let mut libs = parse_lib_table(&content);
            for lib in &mut libs {
                lib["scope"] = json!("project");
            }
            all_libs.extend(libs);
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "count": all_libs.len(),
            "libraries": all_libs
        }))
        .unwrap(),
    ))
}

/// Insert a new `(lib ...)` entry into a lib-table file (fp-lib-table or sym-lib-table).
/// Creates the file with minimal scaffolding if it doesn't exist.
async fn register_in_lib_table(
    table_path: &Path,
    nickname: &str,
    uri: &str,
    lib_type: &str,
) -> anyhow::Result<()> {
    let content = if table_path.exists() {
        tokio::fs::read_to_string(table_path).await?
    } else {
        "(fp_lib_table\n  (version 7)\n)\n".to_string()
    };

    // Check if nickname already registered
    if content.contains(&format!("(name \"{}\")", nickname)) {
        return Ok(()); // already registered, idempotent
    }

    // Find closing paren of the root expression
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let entry = format!(
        "\n  (lib (name \"{}\") (type \"{}\") (uri \"{}\") (options \"\") (descr \"\"))",
        nickname, lib_type, uri
    );

    let new_content = format!("{}{}\n)", &content[..insert_pos], entry);

    if let Some(parent) = table_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(table_path, &new_content)?;
    Ok(())
}

// ─── Symbol library tools ─────────────────────────────────────────────────────

/// Minimal pin geometry for deriving the symbol body.
#[derive(Debug, Clone, Copy)]
struct PinGeom {
    x: f64,
    y: f64,
    angle: f64,
    length: f64,
}

/// The point where a pin meets the symbol body. In KiCAD symbols the pin's
/// connection endpoint (the "bulb", where wires attach) is at `(x, y)` and the
/// pin extends by `length` in its orientation to reach the body outline. Angles
/// are 0=E, 90=N, 180=W, 270=S with Y up, so the body-attach point (root) is
/// `(x + length*cos, y + length*sin)` — on the far side of the bulb.
fn pin_root(x: f64, y: f64, angle_deg: f64, length: f64) -> (f64, f64) {
    let a = angle_deg.to_radians();
    (x + length * a.cos(), y + length * a.sin())
}

/// Body rectangle `(min_x, min_y, max_x, max_y)` for a symbol: edges that pins
/// attach to pass through those pins' roots (so each pin's far end touches the
/// border and its connection bulb sits outside), and edges with no pins are
/// pushed out by a margin so there is clear spacing beyond the outermost pins.
/// `None` when there are no pins.
fn symbol_body_rect(pins: &[PinGeom]) -> Option<(f64, f64, f64, f64)> {
    if pins.is_empty() {
        return None;
    }
    let roots: Vec<(f64, f64)> = pins
        .iter()
        .map(|p| pin_root(p.x, p.y, p.angle, p.length))
        .collect();
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for &(x, y) in &roots {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }

    // Which edges have pins attaching, by orientation (Y up): a pin pointing
    // right (0) sits on the left edge, left (180) on the right edge, up (90) on
    // the bottom edge, down (270) on the top edge.
    let norm = |a: f64| ((a % 360.0) + 360.0) % 360.0;
    let near = |a: f64, t: f64| {
        let d = (norm(a) - t).abs();
        !(1.0..=359.0).contains(&d)
    };
    let (mut has_left, mut has_right, mut has_bottom, mut has_top) = (false, false, false, false);
    for p in pins {
        if near(p.angle, 0.0) {
            has_left = true;
        } else if near(p.angle, 180.0) {
            has_right = true;
        } else if near(p.angle, 90.0) {
            has_bottom = true;
        } else if near(p.angle, 270.0) {
            has_top = true;
        }
    }

    // Spacing beyond the last pin on any edge without attachments (~1 grid).
    let margin = 2.54;
    if !has_left {
        min_x -= margin;
    }
    if !has_right {
        max_x += margin;
    }
    if !has_bottom {
        min_y -= margin;
    }
    if !has_top {
        max_y += margin;
    }

    // Minimum visible body.
    let min_size = 2.54;
    if max_x - min_x < min_size {
        let c = (min_x + max_x) / 2.0;
        min_x = c - min_size / 2.0;
        max_x = c + min_size / 2.0;
    }
    if max_y - min_y < min_size {
        let c = (min_y + max_y) / 2.0;
        min_y = c - min_size / 2.0;
        max_y = c + min_size / 2.0;
    }
    Some((min_x, min_y, max_x, max_y))
}

async fn handle_create_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let name = args["name"].as_str().unwrap_or("Symbol");
    let ref_prefix = args["reference_prefix"].as_str().unwrap_or("U");
    let value_str = args["value"].as_str().unwrap_or(name);
    let pins_val = args["pins"].as_array().cloned().unwrap_or_default();
    let show_names = args["show_pin_names"].as_bool().unwrap_or(true);
    let show_numbers = args["show_pin_numbers"].as_bool().unwrap_or(true);

    // Build pin S-expressions and collect pin geometry for the body rectangle.
    let mut pins_sexp = String::new();
    let mut pin_geoms: Vec<PinGeom> = Vec::new();
    for pin in &pins_val {
        let number = pin["number"].as_str().unwrap_or("1");
        let pin_name = pin["name"].as_str().unwrap_or("~");
        let pin_type = pin["type"].as_str().unwrap_or("passive");
        let x = pin["x"].as_f64().unwrap_or(0.0);
        let y = pin["y"].as_f64().unwrap_or(0.0);
        let angle = pin["angle"].as_f64().unwrap_or(0.0);
        let length = pin["length"].as_f64().unwrap_or(2.54);

        pin_geoms.push(PinGeom {
            x,
            y,
            angle,
            length,
        });
        pins_sexp.push_str(&format!(
            "\n    (pin {} line (at {} {} {})\n      (length {})\n      (name \"{}\" (effects (font (size 1.27 1.27))))\n      (number \"{}\" (effects (font (size 1.27 1.27))))\n    )",
            pin_type, x, y, angle, length, pin_name, number
        ));
    }

    // Body rectangle enclosing the pin roots, plus reference/value placement
    // above/below it (symbol coordinates are Y-up).
    let body = symbol_body_rect(&pin_geoms);
    let body_sexp = match body {
        Some((min_x, min_y, max_x, max_y)) => format!(
            "\n      (rectangle (start {:.4} {:.4}) (end {:.4} {:.4})\n        (stroke (width 0.254) (type default))\n        (fill (type background))\n      )",
            min_x, min_y, max_x, max_y
        ),
        None => String::new(),
    };
    let (ref_y, value_y) = match body {
        Some((_, min_y, _, max_y)) => (max_y + 2.54, min_y - 2.54),
        None => (2.54, -2.54),
    };

    let numbers_vis = if show_numbers { "" } else { " hide" };
    let names_vis = if show_names { "" } else { " hide" };

    let symbol_sexp = format!(
        "\n  (symbol \"{}\"\n    (pin_numbers{})\n    (pin_names (offset 1.016){})\n    (in_bom yes)\n    (on_board yes)\n    (property \"Reference\" \"{}\" (at 0 {:.4} 0) (effects (font (size 1.27 1.27))))\n    (property \"Value\" \"{}\" (at 0 {:.4} 0) (effects (font (size 1.27 1.27))))\n    (property \"Footprint\" \"\" (at 0 0 0) (effects (font (size 1.27 1.27)) hide))\n    (property \"Datasheet\" \"~\" (at 0 0 0) (effects (font (size 1.27 1.27)) hide))\n    (symbol \"{}_0_1\"{}{}\n    )\n  )",
        name, numbers_vis, names_vis, ref_prefix, ref_y, value_str, value_y, name, body_sexp, pins_sexp
    );

    // If file doesn't exist, create scaffold
    let content = if lib_path.exists() {
        tokio::fs::read_to_string(&lib_path).await?
    } else {
        "(kicad_symbol_lib\n  (version 20240108)\n  (generator \"konnect\")\n)\n".to_string()
    };

    // Insert before closing paren of root expression
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = format!("{}{}\n)", &content[..insert_pos], symbol_sexp);

    if let Some(parent) = lib_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(&lib_path, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "symbol": name,
            "library": lib_path.to_str().unwrap_or(""),
            "pin_count": pins_val.len(),
            "body": body.is_some()
        }))
        .unwrap(),
    ))
}

async fn handle_delete_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let symbol_name = require_str(args, "symbol_name").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let content = tokio::fs::read_to_string(&lib_path).await?;

    // Find `  (symbol "NAME"` block
    let pat = format!(r#"  (symbol "{}""#, symbol_name);
    let start = content
        .find(&pat)
        .ok_or_else(|| anyhow::anyhow!("Symbol '{}' not found in library", symbol_name))?;

    // Walk back to find preceding newline
    let block_start = content[..start].rfind('\n').map(|i| i + 1).unwrap_or(start);

    // Walk forward to find end of block (depth count)
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
    // Skip trailing newline
    let end = if content[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };

    let new_content = format!("{}{}", &content[..block_start], &content[end..]);
    write_atomic(&lib_path, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "deleted": symbol_name
        }))
        .unwrap(),
    ))
}

async fn handle_list_symbols_in_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let content = tokio::fs::read_to_string(&lib_path).await?;

    // Match all top-level symbol names: `  (symbol "NAME"` at depth 1
    let mut symbols = Vec::new();
    let mut search = content.as_str();
    while let Some(pos) = search.find("\n  (symbol \"") {
        let after = &search[pos + 13..]; // skip `\n  (symbol "`
        if let Some(end) = after.find('"') {
            let sym_name = &after[..end];
            // Exclude sub-units like "NAME_0_1"
            if !sym_name.contains('_') || {
                // Allow symbols whose name contains underscores but are NOT sub-unit patterns
                let parts: Vec<&str> = sym_name.rsplitn(3, '_').collect();
                parts.len() < 3 || parts[0].parse::<u32>().is_err()
            } {
                symbols.push(sym_name.to_string());
            }
            search = &search[pos + 1..];
        } else {
            break;
        }
    }
    symbols.sort();
    symbols.dedup();

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "library": lib_path.to_str().unwrap_or(""),
            "count": symbols.len(),
            "symbols": symbols
        }))
        .unwrap(),
    ))
}

async fn handle_search_symbols(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    // Walk all global symbol libraries
    let table = global_sym_lib_table();
    let mut results = Vec::new();

    if table.exists() {
        let table_content = tokio::fs::read_to_string(&table).await?;
        let libs = parse_lib_table(&table_content);

        'outer: for lib in &libs {
            let uri = lib["uri"].as_str().unwrap_or("");
            // Expand KiCAD env vars ${KICAD8_SYMBOL_DIR} etc. — skip if unresolvable
            if uri.starts_with("${") {
                continue;
            }
            let lib_path = PathBuf::from(uri);
            if !lib_path.exists() {
                continue;
            }
            let lib_content = match tokio::fs::read_to_string(&lib_path).await {
                Ok(c) => c,
                Err(_) => continue,
            };

            let nickname = lib["nickname"].as_str().unwrap_or("");
            let mut search = lib_content.as_str();
            while let Some(pos) = search.find("\n  (symbol \"") {
                let after = &search[pos + 13..];
                if let Some(end) = after.find('"') {
                    let sym_name = &after[..end];
                    if sym_name.to_lowercase().contains(&query) && !sym_name.contains('_') {
                        results.push(json!({
                            "library": nickname,
                            "name": sym_name,
                            "id": format!("{}:{}", nickname, sym_name)
                        }));
                        if results.len() >= limit {
                            break 'outer;
                        }
                    }
                    search = &search[pos + 1..];
                } else {
                    break;
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "query": query,
            "count": results.len(),
            "results": results
        }))
        .unwrap(),
    ))
}

async fn handle_list_library_footprints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let library_path_str =
        require_str(args, "library_path").map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let lib_dir = PathBuf::from(library_path_str);

    if !lib_dir.is_dir() {
        return Ok(CallToolResult::error(format!(
            "Not a directory: {}",
            library_path_str
        )));
    }

    let mut footprints = Vec::new();
    let mut rd = tokio::fs::read_dir(&lib_dir).await?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".kicad_mod") {
            footprints.push(name_str.trim_end_matches(".kicad_mod").to_string());
        }
    }
    footprints.sort();

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "library": library_path_str,
            "count": footprints.len(),
            "footprints": footprints
        }))
        .unwrap(),
    ))
}

// ─── Footprint geometry reader (parse an existing .kicad_mod) ─────────────────

/// A pad read from a `.kicad_mod`, carrying the full padstack needed to place it
/// on a board.
#[derive(Debug, Clone)]
pub struct FpPad {
    pub number: String,
    pub pad_type: String,
    pub shape: String,
    pub x: f64,
    pub y: f64,
    pub rot: Option<f64>,
    pub w: f64,
    pub h: f64,
    pub layers: Vec<String>,
    pub drill: Option<f64>,
    pub roundrect_rratio: Option<f64>,
}

/// A footprint parsed from a library `.kicad_mod` file — the geometry needed to
/// display it and (later) place it on a board.
#[derive(Debug, Clone)]
pub struct LibFootprint {
    pub name: String,
    pub descr: String,
    pub attr: String,
    pub pads: Vec<FpPad>,
    /// Courtyard bbox `(min_x, min_y, max_x, max_y)`, unioned over F/B.CrtYd graphics.
    pub courtyard: Option<(f64, f64, f64, f64)>,
    pub model_path: Option<String>,
}

/// The `(x, y)` of a coordinate node like `(start X Y)` or `(xy X Y)`.
fn xy_of(n: &SexpNode) -> Option<(f64, f64)> {
    let x = n.get(1)?.as_str()?.parse().ok()?;
    let y = n.get(2)?.as_str()?.parse().ok()?;
    Some((x, y))
}

fn is_courtyard_layer(layer: &str) -> bool {
    layer == "F.CrtYd" || layer == "B.CrtYd"
}

/// Fold a graphics node's start/end/center points and any `(pts (xy ..) ..)`
/// into the running bbox `bb = (min_x, min_y, max_x, max_y)`.
fn accumulate_bbox(node: &SexpNode, bb: &mut (f64, f64, f64, f64)) {
    let mut push = |x: f64, y: f64| {
        bb.0 = bb.0.min(x);
        bb.1 = bb.1.min(y);
        bb.2 = bb.2.max(x);
        bb.3 = bb.3.max(y);
    };
    for tag in ["start", "end", "center"] {
        if let Some((x, y)) = node.find(tag).and_then(xy_of) {
            push(x, y);
        }
    }
    if let Some(pts) = node.find("pts") {
        for xy in pts.find_all("xy") {
            if let Some((x, y)) = xy_of(xy) {
                push(x, y);
            }
        }
    }
}

/// Courtyard bounding box unioned over all F/B.CrtYd graphics, or `None`.
fn courtyard_bbox(fp: &SexpNode) -> Option<(f64, f64, f64, f64)> {
    let mut bb = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for tag in ["fp_rect", "fp_line", "fp_poly", "fp_circle"] {
        for g in fp.find_all(tag) {
            if g.find_str("layer").is_some_and(is_courtyard_layer) {
                accumulate_bbox(g, &mut bb);
            }
        }
    }
    (bb.0.is_finite() && bb.2 > bb.0 && bb.3 > bb.1).then_some(bb)
}

/// Parse a `.kicad_mod` (or a board-embedded `(footprint ...)`) into geometry.
pub fn parse_lib_footprint(content: &str) -> anyhow::Result<LibFootprint> {
    let tree = parse_sexp(content)?;
    let fp = if tree.head() == Some("footprint") {
        &tree
    } else {
        tree.find("footprint")
            .ok_or_else(|| anyhow::anyhow!("no (footprint ...) node found"))?
    };

    let name = fp.get(1).and_then(|n| n.as_str()).unwrap_or("").to_string();
    let descr = fp.find_str("descr").unwrap_or("").to_string();
    let attr = fp.find_str("attr").unwrap_or("").to_string();
    let model_path = fp
        .find("model")
        .and_then(|m| m.get(1))
        .and_then(|n| n.as_str())
        .map(String::from);

    let mut pads = Vec::new();
    for p in fp.find_all("pad") {
        let Some(number) = p.get(1).and_then(|n| n.as_str()).map(String::from) else {
            continue;
        };
        let pad_type = p.get(2).and_then(|n| n.as_str()).unwrap_or("").to_string();
        let shape = p.get(3).and_then(|n| n.as_str()).unwrap_or("").to_string();
        let at = p.find("at");
        let x = at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
        let y = at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
        let rot = at.and_then(|a| a.get_f64(3));
        let size = p.find("size");
        let w = size.and_then(|s| s.get_f64(1)).unwrap_or(0.0);
        let h = size.and_then(|s| s.get_f64(2)).unwrap_or(0.0);
        let layers = p
            .find("layers")
            .and_then(|l| l.children())
            .map(|c| {
                c.iter()
                    .skip(1)
                    .filter_map(|n| n.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        // `(drill D)` or `(drill oval DX DY)` — first numeric child.
        let drill = p.find("drill").and_then(|d| {
            d.children()?
                .iter()
                .skip(1)
                .find_map(|n| n.as_str()?.parse::<f64>().ok())
        });
        let roundrect_rratio = p.find_f64("roundrect_rratio");
        pads.push(FpPad {
            number,
            pad_type,
            shape,
            x,
            y,
            rot,
            w,
            h,
            layers,
            drill,
            roundrect_rratio,
        });
    }

    Ok(LibFootprint {
        name,
        descr,
        attr,
        pads,
        courtyard: courtyard_bbox(fp),
        model_path,
    })
}

async fn handle_get_footprint_info(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let fp_path_str =
        require_str(args, "footprint_path").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    // Resolve "Library:Footprint" through the fp-lib-table (env-var expansion +
    // nested `(type "Table")` includes); anything with a path separator is taken
    // as a direct file path.
    let path = if fp_path_str.contains(':')
        && !fp_path_str.contains('/')
        && !fp_path_str.contains('\\')
    {
        let project = args["project"].as_str().map(Path::new);
        match super::resolve_lib_footprint(fp_path_str, project) {
            Some(p) => p,
            None => {
                return Ok(CallToolResult::error(format!(
                    "Footprint '{}' not resolvable via any fp-lib-table",
                    fp_path_str
                )))
            }
        }
    } else {
        PathBuf::from(fp_path_str)
    };

    let content = tokio::fs::read_to_string(&path).await?;
    let fp = match parse_lib_footprint(&content) {
        Ok(f) => f,
        Err(e) => return Ok(CallToolResult::error(format!("Failed to parse footprint: {e}"))),
    };

    let pads: Vec<_> = fp
        .pads
        .iter()
        .map(|p| {
            json!({
                "number": p.number,
                "type": p.pad_type,
                "shape": p.shape,
                "x": p.x,
                "y": p.y,
                "rot": p.rot,
                "width": p.w,
                "height": p.h,
                "layers": p.layers,
                "drill": p.drill,
                "roundrect_rratio": p.roundrect_rratio,
            })
        })
        .collect();
    let courtyard = fp.courtyard.map(|(min_x, min_y, max_x, max_y)| {
        json!({
            "min_x": min_x, "min_y": min_y, "max_x": max_x, "max_y": max_y,
            "width": max_x - min_x, "height": max_y - min_y
        })
    });

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "name": fp.name,
            "description": fp.descr,
            "attr": fp.attr,
            "pad_count": fp.pads.len(),
            "pads": pads,
            "courtyard": courtyard,
            "has_courtyard": fp.courtyard.is_some(),
            "has_3d_model": fp.model_path.is_some(),
            "model": fp.model_path,
            "path": path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

// ─── search_footprints (moved from verification toolset) ─────────────────────

async fn handle_search_footprints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    // Walk the global fp-lib-table (tab-robust via parse_lib_table), expanding
    // ${KICAD*_DIR} URIs so classic env-var library entries are searchable.
    let fp_lib_table_path = super::kicad_config_dir().join("fp-lib-table");

    let mut results = Vec::new();

    if fp_lib_table_path.exists() {
        let tc = tokio::fs::read_to_string(&fp_lib_table_path).await?;
        'outer: for lib in parse_lib_table(&tc) {
            let nickname = lib["nickname"].as_str().unwrap_or("");
            let uri = super::expand_kicad_env_vars(lib["uri"].as_str().unwrap_or(""));
            if uri.contains("${") {
                continue; // still-unresolved variable
            }
            let dir = PathBuf::from(&uri);
            if !dir.is_dir() {
                continue; // (type "Table") includes point at a file, not a dir
            }
            if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let fname = entry.file_name();
                    let fname_str = fname.to_string_lossy();
                    if let Some(fp_name) = fname_str.strip_suffix(".kicad_mod") {
                        if fp_name.to_lowercase().contains(&query) {
                            results.push(json!({
                                "library": nickname,
                                "name": fp_name,
                                "id": format!("{}:{}", nickname, fp_name)
                            }));
                            if results.len() >= limit {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "query": args["query"].as_str().unwrap_or(""),
            "count": results.len(),
            "results": results
        }))
        .unwrap(),
    ))
}

// ─── get_symbol_info (moved from verification toolset) ───────────────────────

async fn handle_get_symbol_info(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_id = require_str(args, "lib_id").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Ok(CallToolResult::error(
            "lib_id must be in 'Library:Symbol' format (e.g. 'Device:R')",
        ));
    }
    let (lib_nick, sym_name) = (parts[0], parts[1]);

    // Look up library path from global sym-lib-table
    let sym_lib_table_path = super::kicad_config_dir().join("sym-lib-table");

    let lib_path = if sym_lib_table_path.exists() {
        let tc = tokio::fs::read_to_string(&sym_lib_table_path).await?;
        // Parse lib entries: (lib (name "NICK") ... (uri "PATH") ...)
        let pat = format!(r#"(name "{}")"#, lib_nick);
        if let Some(block_start) = tc.find(&pat) {
            let block_end = tc[block_start..]
                .find(")\n")
                .map(|i| block_start + i + 2)
                .unwrap_or(tc.len());
            let block = &tc[block_start..block_end];
            if let Some(uri_pos) = block.find("(uri \"") {
                let after = &block[uri_pos + 6..];
                after.find('"').map(|end| PathBuf::from(&after[..end]))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let lib_path = match lib_path {
        Some(p) if !p.to_str().unwrap_or("").starts_with("${") => p,
        _ => {
            return Ok(CallToolResult::error(format!(
                "Library '{}' not found or path uses unresolved env var",
                lib_nick
            )));
        }
    };

    let content = tokio::fs::read_to_string(&lib_path).await?;

    // Find symbol block
    let sym_pat = format!(r#"  (symbol "{}""#, sym_name);
    let sym_start = content.find(&sym_pat).ok_or_else(|| {
        anyhow::anyhow!("Symbol '{}' not found in library '{}'", sym_name, lib_nick)
    })?;

    let sym_end = {
        let mut depth = 0i32;
        let mut end = sym_start;
        for (i, ch) in content[sym_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = sym_start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        end
    };
    let sym_block = &content[sym_start..sym_end];

    // Extract pins
    let mut pins = Vec::new();
    let mut search = sym_block;
    while let Some(pos) = search.find("\n    (pin ") {
        let inner = &search[pos + 10..];
        let pin_type = inner.split_whitespace().next().unwrap_or("").to_string();
        let pin_name = extract_sexp_string(inner, "name").unwrap_or_default();
        let pin_num = extract_sexp_string(inner, "number").unwrap_or_default();
        let (px, py) = extract_at_xy(inner).unwrap_or((0.0, 0.0));
        pins.push(json!({
            "number": pin_num,
            "name": pin_name,
            "type": pin_type,
            "x": px,
            "y": py
        }));
        search = &search[pos + 1..];
    }

    // Extract properties
    let mut properties = serde_json::Map::new();
    let mut search = sym_block;
    while let Some(pos) = search.find("\n    (property \"") {
        let inner = &search[pos + 16..];
        if let Some(key_end) = inner.find('"') {
            let key = &inner[..key_end];
            let val_start = inner[key_end + 1..]
                .find('"')
                .map(|i| key_end + 1 + i + 1)
                .unwrap_or(0);
            let val_end = inner[val_start..]
                .find('"')
                .map(|i| val_start + i)
                .unwrap_or(0);
            if val_end > val_start {
                properties.insert(
                    key.to_string(),
                    json!(inner[val_start..val_end].to_string()),
                );
            }
        }
        search = &search[pos + 1..];
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "lib_id": lib_id,
            "name": sym_name,
            "library": lib_nick,
            "pin_count": pins.len(),
            "pins": pins,
            "properties": properties
        }))
        .unwrap(),
    ))
}

fn extract_at_xy(block: &str) -> Option<(f64, f64)> {
    let pos = block.find("(at ")?;
    let after = &block[pos + 4..];
    let end = after.find(')')?;
    let parts: Vec<&str> = after[..end].split_whitespace().collect();
    let x = parts.first()?.parse::<f64>().ok()?;
    let y = parts.get(1)?.parse::<f64>().ok()?;
    Some((x, y))
}

#[cfg(test)]
mod tests {
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

    fn pad(number: &str, t: &str, x: f64, y: f64, w: f64, h: f64) -> PadGeom {
        PadGeom {
            number: number.into(),
            pad_type: t.into(),
            x,
            y,
            w,
            h,
        }
    }

    #[test]
    fn pads_bbox_covers_pad_extents() {
        let pads = vec![
            pad("1", "smd", -1.0, 0.0, 0.4, 0.6),
            pad("2", "smd", 1.0, 0.0, 0.4, 0.6),
        ];
        let (min_x, min_y, max_x, max_y) = pads_bbox(&pads);
        assert!((min_x - -1.2).abs() < 1e-9); // -1.0 - 0.4/2
        assert!((max_x - 1.2).abs() < 1e-9);
        assert!((min_y - -0.3).abs() < 1e-9);
        assert!((max_y - 0.3).abs() < 1e-9);
    }

    #[test]
    fn courtyard_clearance_follows_the_rule() {
        let smd = vec![pad("1", "smd", 0.0, 0.0, 0.4, 0.6)];
        let th = vec![pad("1", "thru_hole", 0.0, 0.0, 1.5, 1.5)];
        // Explicit wins over everything.
        assert_eq!(
            courtyard_clearance(Some(0.42), Some("bga"), &smd, None),
            0.42
        );
        // package_type mapping.
        assert_eq!(courtyard_clearance(None, Some("bga"), &smd, None), 1.0);
        assert_eq!(courtyard_clearance(None, Some("small"), &smd, None), 0.15);
        assert_eq!(
            courtyard_clearance(None, Some("through_hole"), &smd, None),
            0.5
        );
        assert_eq!(courtyard_clearance(None, Some("smd"), &smd, None), 0.25);
        // Auto: through-hole pad present.
        assert_eq!(courtyard_clearance(None, None, &th, None), 0.5);
        // Auto: sub-0603 body (1.0 x 0.5 mm).
        assert_eq!(
            courtyard_clearance(None, None, &smd, Some((1.0, 0.5))),
            0.15
        );
        // Auto: 0603 itself and larger stay at the SMT default.
        assert_eq!(
            courtyard_clearance(None, None, &smd, Some((1.6, 0.8))),
            0.25
        );
        assert_eq!(courtyard_clearance(None, None, &smd, None), 0.25);
    }

    #[test]
    fn pin1_index_prefers_pad_numbered_one() {
        let pads = vec![
            pad("2", "smd", 0.0, 0.0, 1.0, 1.0),
            pad("1", "smd", 2.0, 0.0, 1.0, 1.0),
        ];
        assert_eq!(pin1_index(&pads), Some(1));
        // No pad numbered "1" falls back to the first pad.
        let pads2 = vec![pad("A1", "smd", 0.0, 0.0, 1.0, 1.0)];
        assert_eq!(pin1_index(&pads2), Some(0));
        assert_eq!(pin1_index(&[]), None);
    }

    #[test]
    fn chamfered_rect_cuts_the_pin1_corner() {
        // Rectangle (0,0)-(10,10), pin 1 nearest the top-left corner.
        let pts = chamfered_rect_points(0.0, 0.0, 10.0, 10.0, 0.0, 0.0, 1.0);
        assert_eq!(pts.len(), 5, "one corner chamfered adds a vertex: {pts:?}");
        // The sharp corner is gone, replaced by two edge points.
        assert!(!pts.iter().any(|&(x, y)| x.abs() < 1e-9 && y.abs() < 1e-9));
        assert!(pts
            .iter()
            .any(|&(x, y)| (x - 0.0).abs() < 1e-9 && (y - 1.0).abs() < 1e-9));
        assert!(pts
            .iter()
            .any(|&(x, y)| (x - 1.0).abs() < 1e-9 && (y - 0.0).abs() < 1e-9));
    }

    #[test]
    fn pin_root_is_on_the_body_side_of_the_connection() {
        // Left pin (points right): bulb on the left, root to its right (body).
        let (lx, ly) = pin_root(-10.16, 0.0, 0.0, 2.54);
        assert!(
            (lx - -7.62).abs() < 1e-9 && ly.abs() < 1e-9,
            "left {lx},{ly}"
        );
        // Right pin (points left): root to the left of the bulb.
        let (rx, ry) = pin_root(10.16, 0.0, 180.0, 2.54);
        assert!(
            (rx - 7.62).abs() < 1e-9 && ry.abs() < 1e-9,
            "right {rx},{ry}"
        );
        // Up pin (points up, Y-up): root above the bulb.
        let (ux, uy) = pin_root(0.0, -5.0, 90.0, 2.54);
        assert!(ux.abs() < 1e-9 && (uy - -2.46).abs() < 1e-9, "up {ux},{uy}");
    }

    #[test]
    fn symbol_body_rect_touches_side_pins_and_spaces_the_ends() {
        // Three pins on the left (point right), two on the right (point left).
        let pins = vec![
            PinGeom {
                x: -10.16,
                y: 2.54,
                angle: 0.0,
                length: 2.54,
            },
            PinGeom {
                x: -10.16,
                y: 0.0,
                angle: 0.0,
                length: 2.54,
            },
            PinGeom {
                x: -10.16,
                y: -2.54,
                angle: 0.0,
                length: 2.54,
            },
            PinGeom {
                x: 10.16,
                y: 2.54,
                angle: 180.0,
                length: 2.54,
            },
            PinGeom {
                x: 10.16,
                y: -2.54,
                angle: 180.0,
                length: 2.54,
            },
        ];
        let (min_x, min_y, max_x, max_y) = symbol_body_rect(&pins).unwrap();
        // Left/right edges pass through the pin roots (pins touch the border).
        assert!((min_x - -7.62).abs() < 1e-9, "left edge {min_x}");
        assert!((max_x - 7.62).abs() < 1e-9, "right edge {max_x}");
        // Connection bulbs at x = ±10.16 stay outside the body.
        assert!(min_x > -10.16 && max_x < 10.16);
        // Top/bottom edges have no pins → spacing beyond the outermost pins.
        assert!(max_y >= 2.54 + 2.5, "top spacing {max_y}");
        assert!(min_y <= -2.54 - 2.5, "bottom spacing {min_y}");
        assert!(symbol_body_rect(&[]).is_none());
    }

    #[test]
    fn model_sexp_only_with_path() {
        assert_eq!(build_model_sexp(&json!({})), "");
        assert_eq!(build_model_sexp(&json!({ "model": {} })), "");
        let s = build_model_sexp(&json!({ "model": { "path": "x.wrl", "rotate": { "z": 90.0 } } }));
        assert!(s.contains("(model \"x.wrl\""));
        assert!(s.contains("(rotate (xyz 0 0 90)"));
        assert!(s.contains("(scale (xyz 1 1 1)"));
    }

    #[tokio::test]
    async fn create_footprint_emits_courtyard_pin1_and_model() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("TEST.kicad_mod");
        let args = json!({
            "output": out.to_string_lossy(),
            "name": "TEST_QFN",
            "pads": [
                {"number":"1","type":"smd","shape":"roundrect","x":-1.0,"y":-1.0,"width":0.3,"height":0.6},
                {"number":"2","type":"smd","shape":"roundrect","x":-1.0,"y":1.0,"width":0.3,"height":0.6},
                {"number":"3","type":"smd","shape":"roundrect","x":1.0,"y":0.0,"width":0.3,"height":0.6}
            ],
            "body_width": 2.0, "body_height": 2.0,
            "model": { "path": "${KICAD9_3DMODEL_DIR}/Package.3dshapes/TEST_QFN.wrl" }
        });
        let res = handle_create_footprint(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error);
        let c = std::fs::read_to_string(&out).unwrap();
        assert!(c.contains("F.CrtYd"), "missing courtyard:\n{c}");
        assert!(c.contains("F.SilkS"));
        assert!(c.contains("(fp_poly"), "missing fab chamfer outline");
        assert!(c.contains("(fp_circle"), "missing pin-1 silk dot");
        assert!(c.contains("(fp_text reference \"REF**\""));
        assert!(c.contains("(fp_text value \"TEST_QFN\""));
        assert!(c.contains("(model \"${KICAD9_3DMODEL_DIR}/Package.3dshapes/TEST_QFN.wrl\""));
        // Round-trips through the S-expression parser.
        assert!(
            konnect_sexp::parser::parse_sexp(&c).is_ok(),
            "generated footprint doesn't parse"
        );
    }

    #[tokio::test]
    async fn create_symbol_emits_body_and_shows_pins() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("test.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "TEST_IC",
            "reference_prefix": "U",
            "pins": [
                {"number":"1","name":"IN","type":"input","x":-7.62,"y":2.54,"angle":0,"length":2.54},
                {"number":"2","name":"GND","type":"power_in","x":-7.62,"y":-2.54,"angle":0,"length":2.54},
                {"number":"3","name":"OUT","type":"output","x":7.62,"y":0.0,"angle":180,"length":2.54}
            ]
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error);
        let c = std::fs::read_to_string(&lib).unwrap();
        assert!(
            c.contains("(rectangle"),
            "missing symbol body rectangle:\n{c}"
        );
        assert!(
            c.contains("(generator \"konnect\")"),
            "stale generator string"
        );
        assert!(c.contains("(pin_numbers)"), "pin numbers should be shown");
        assert!(!c.contains("(pin_numbers hide)"));
        assert!(
            konnect_sexp::parser::parse_sexp(&c).is_ok(),
            "generated symbol doesn't parse"
        );
    }

    // A faithful (trimmed) KiCad 10 stock footprint, tab-indented as on disk.
    const R_0402: &str = "(footprint \"R_0402_1005Metric\"\n\t(version 20260206)\n\t(generator \"kicad-footprint-generator\")\n\t(layer \"F.Cu\")\n\t(descr \"Resistor SMD 0402 (1005 Metric)\")\n\t(attr smd)\n\t(fp_rect\n\t\t(start -0.93 -0.47)\n\t\t(end 0.93 0.47)\n\t\t(stroke (width 0.05) (type solid))\n\t\t(fill no)\n\t\t(layer \"F.CrtYd\")\n\t)\n\t(pad \"1\" smd roundrect\n\t\t(at -0.51 0)\n\t\t(size 0.54 0.64)\n\t\t(layers \"F.Cu\" \"F.Mask\" \"F.Paste\")\n\t\t(roundrect_rratio 0.25)\n\t)\n\t(pad \"2\" smd roundrect\n\t\t(at 0.51 0)\n\t\t(size 0.54 0.64)\n\t\t(layers \"F.Cu\" \"F.Mask\" \"F.Paste\")\n\t\t(roundrect_rratio 0.25)\n\t)\n\t(model \"${KICAD10_3DMODEL_DIR}/Resistor_SMD.3dshapes/R_0402_1005Metric.step\"\n\t\t(offset (xyz 0 0 0))\n\t\t(scale (xyz 1 1 1))\n\t\t(rotate (xyz 0 0 0))\n\t)\n)";

    #[test]
    fn parse_lib_footprint_reads_full_padstack() {
        let fp = parse_lib_footprint(R_0402).expect("parses");
        assert_eq!(fp.name, "R_0402_1005Metric");
        assert_eq!(fp.attr, "smd");
        assert!(fp.descr.contains("Resistor"));
        assert_eq!(fp.pads.len(), 2);

        let p1 = &fp.pads[0];
        assert_eq!(p1.number, "1");
        assert_eq!(p1.pad_type, "smd");
        assert_eq!(p1.shape, "roundrect");
        assert!((p1.x - -0.51).abs() < 1e-9 && p1.y.abs() < 1e-9);
        assert!((p1.w - 0.54).abs() < 1e-9 && (p1.h - 0.64).abs() < 1e-9);
        assert_eq!(p1.layers, ["F.Cu", "F.Mask", "F.Paste"]);
        assert_eq!(p1.roundrect_rratio, Some(0.25));
        assert_eq!(p1.drill, None);

        // Courtyard bbox unions the F.CrtYd rectangle.
        let (a, b, c, d) = fp.courtyard.expect("has courtyard");
        assert!((a - -0.93).abs() < 1e-9 && (b - -0.47).abs() < 1e-9);
        assert!((c - 0.93).abs() < 1e-9 && (d - 0.47).abs() < 1e-9);

        assert_eq!(
            fp.model_path.as_deref(),
            Some("${KICAD10_3DMODEL_DIR}/Resistor_SMD.3dshapes/R_0402_1005Metric.step")
        );
    }

    #[test]
    fn parse_lib_footprint_reads_through_hole_drill() {
        let round = "(footprint \"C1\" (attr through_hole) (pad \"1\" thru_hole circle (at 0 0) (size 1.6 1.6) (drill 0.8) (layers \"*.Cu\" \"*.Mask\")))";
        let fp = parse_lib_footprint(round).expect("parses");
        assert_eq!(fp.pads[0].pad_type, "thru_hole");
        assert_eq!(fp.pads[0].drill, Some(0.8));
        assert_eq!(fp.pads[0].layers, ["*.Cu", "*.Mask"]);
        // Oval drill: first numeric child is the drill size.
        let oval = "(footprint \"C2\" (pad \"1\" thru_hole oval (at 0 0) (size 2 3) (drill oval 0.8 1.0) (layers \"*.Cu\")))";
        let fp2 = parse_lib_footprint(oval).expect("parses");
        assert_eq!(fp2.pads[0].drill, Some(0.8));
        // No courtyard graphics → None.
        assert!(fp2.courtyard.is_none());
    }

    #[test]
    fn parse_lib_table_reads_tab_indented_table() {
        // KiCad writes lib-tables tab-indented; the old two-space substring scan
        // parsed these as empty. The parser-based version must find both entries.
        let table = "(fp_lib_table\n\t(version 7)\n\t(lib (name \"Device\") (type \"KiCad\") (uri \"${KICAD10_FOOTPRINT_DIR}/Device.pretty\") (options \"\") (descr \"std\"))\n\t(lib (name \"Custom\") (type \"KiCad\") (uri \"C:/x/Custom.pretty\") (options \"\") (descr \"\"))\n)";
        let libs = parse_lib_table(table);
        assert_eq!(libs.len(), 2, "tab-indented table must parse both libs");
        assert_eq!(libs[0]["nickname"], "Device");
        assert_eq!(libs[0]["uri"], "${KICAD10_FOOTPRINT_DIR}/Device.pretty");
        assert_eq!(libs[0]["type"], "KiCad");
        assert_eq!(libs[1]["nickname"], "Custom");
        // An empty / malformed table yields no entries rather than panicking.
        assert!(parse_lib_table("").is_empty());
    }
}
