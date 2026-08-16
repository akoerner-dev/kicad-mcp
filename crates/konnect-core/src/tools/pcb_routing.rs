//! `pcb_routing` toolset — traces, vias, copper pours, nets, netclasses, and diff pairs.
//!
//! Routing operations use the KiCAD IPC API; `add_net`, `create_netclass`, and
//! `add_copper_pour` use S-expression file manipulation.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::writer::{apply_edits, new_uuid, write_atomic, SexpEdit};
use serde_json::json;

use super::cli;

// ─── IPC helper ───────────────────────────────────────────────────────────────

async fn with_ipc<T, F>(addr: String, f: F) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(&KiCadIpcClient::new(&addr))).await {
        Ok(Ok(r)) => Ok(Ok(r)),
        Ok(Err(e)) => Ok(Err(e.to_string())),
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

macro_rules! ipc {
    ($ctx:expr, |$c:ident| $body:expr) => {{
        let addr = $ctx.config.ipc_address.clone();
        match with_ipc(addr, move |$c| $body).await? {
            Ok(v) => v,
            Err(msg) => {
                return Ok(CallToolResult::error(format!(
                    "KiCAD must be running with the board loaded (IPC error: {})",
                    msg
                )))
            }
        }
    }};
}

// ─── S-expression helpers ─────────────────────────────────────────────────────

fn format_zone(
    net_id: i32,
    net_name: &str,
    layer: &str,
    clearance: f64,
    min_w: f64,
    pts: &[(f64, f64)],
) -> String {
    let uuid = new_uuid();
    let pt_str: String = pts
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (zone (net {net_id}) (net_name \"{net_name}\") (layer \"{layer}\") (uuid \"{uuid}\")\n    \
         (hatch edge 0.508)\n    (connect_pads (clearance {clearance}))\n    \
         (min_thickness {min_w})\n    (fill yes)\n    \
         (polygon (pts{pt_str}\n    ))\n  )"
    )
}

fn find_net_id(content: &str, net_name: &str) -> i32 {
    let search = format!(r#" "{net_name}")"#);
    if let Some(pos) = content.find(&search) {
        let before = &content[..pos];
        let net_pos = before.rfind("(net ").unwrap_or(0);
        let num_str = &before[net_pos + 5..];
        let num_end = num_str.find(' ').unwrap_or(0);
        num_str[..num_end].parse().unwrap_or(0)
    } else {
        0
    }
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_net",
            "Add a new net entry to the PCB file (S-expression insert, no KiCAD IPC required).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" }
                },
                "required": ["board", "net_name"]
            }),
            |args, ctx| async move { handle_add_net(args, ctx).await }
        ),
        tool!(
            "route_trace",
            "Route a trace segment between two points on a copper layer via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" },
                    "layer":    { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_trace(args, ctx).await }
        ),
        tool!(
            "route_pad_to_pad",
            "Route a direct trace between two pads of named components (L-bend routing) via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "net_name":    { "type": "string" },
                    "ref1":        { "type": "string", "description": "First component reference" },
                    "pad1":        { "type": "string", "description": "First pad number" },
                    "ref2":        { "type": "string", "description": "Second component reference" },
                    "pad2":        { "type": "string", "description": "Second pad number" },
                    "layer":       { "type": "string", "default": "F.Cu" },
                    "width":       { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "ref1", "pad1", "ref2", "pad2"]
            }),
            |args, ctx| async move { handle_route_pad_to_pad(args, ctx).await }
        ),
        tool!(
            "add_via",
            "Add a through-hole via at a given position and assign it to a net via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" },
                    "drill":     { "type": "number", "description": "Drill diameter in mm", "default": 0.4 },
                    "pad_size":  { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 }
                },
                "required": ["board", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_add_via(args, ctx).await }
        ),
        tool!(
            "add_copper_pour",
            "Add a copper fill zone polygon on a layer/net via S-expression file insert.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "points": {
                        "type": "array",
                        "items": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } }
                    },
                    "clearance": { "type": "number", "default": 0.2 },
                    "min_width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "points"]
            }),
            |args, ctx| async move { handle_add_copper_pour(args, ctx).await }
        ),
        tool!(
            "delete_trace",
            "Delete a trace segment identified by its UUID via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the track segment to delete" }
                },
                "required": ["board", "uuid"]
            }),
            |args, ctx| async move { handle_delete_trace(args, ctx).await }
        ),
        tool!(
            "query_traces",
            "List trace segments on the board, optionally filtered by net and/or layer.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Filter by net (optional)" },
                    "layer":    { "type": "string", "description": "Filter by layer (optional)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_query_traces(args, ctx).await }
        ),
        tool!(
            "get_nets_list",
            "Return all nets defined on the PCB via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_nets_list(args, ctx).await }
        ),
        tool!(
            "get_unrouted_connections",
            "List missing copper connections (ratsnest) on the board: for each net, the pad \
             pairs that KiCAD's DRC connectivity check considers still unconnected. Built on \
             `pcb drc`'s `unconnected_items` section, but resolves each item to its exact \
             (reference, pad, net) by matching board-space position against the board's own \
             pads rather than parsing KiCAD's localized violation text — works regardless of \
             KiCAD's UI language. This is the pad-pair list an auto-router or a routing loop \
             needs; `run_drc` (verification toolset) only exposes the raw DRC report.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "refill_zones": {
                        "type": "boolean",
                        "description": "Refill copper zones before checking (matches the GUI's \
                            'Refill all zones before DRC'). A pad only touching an unfilled zone \
                            looks unconnected without this. Default true.",
                        "default": true
                    },
                    "net_filter": {
                        "type": "array",
                        "description": "Only return connections on these net names (optional)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_unrouted_connections(args, ctx).await }
        ),
        tool!(
            "modify_trace",
            "Modify a trace segment by deleting and re-adding it with new parameters.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "uuid":      { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width":     { "type": "number", "default": 0.25 }
                },
                "required": ["board", "uuid", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_modify_trace(args, ctx).await }
        ),
        tool!(
            "create_netclass",
            "Create or update a netclass. For modern boards (constraints in the sibling \
             .kicad_pro) this upserts an entry in the project's netclass list; for legacy \
             boards it inserts a netclass block directly into the .kicad_pcb. On update, \
             omitted fields are left at their current value — pass ONLY the fields you want \
             to change. On create (name doesn't exist yet), omitted fields fall back to \
             0.2mm/0.25mm/0.4mm/0.8mm.",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string" },
                    "name":         { "type": "string", "description": "Netclass name (e.g. 'Power')" },
                    "clearance":    { "type": "number", "description": "Clearance in mm — omit to leave unchanged on an existing class" },
                    "trace_width":  { "type": "number", "description": "Default trace width in mm — omit to leave unchanged on an existing class" },
                    "via_drill":    { "type": "number", "description": "Via drill diameter in mm — omit to leave unchanged on an existing class" },
                    "via_diameter": { "type": "number", "description": "Via pad diameter in mm — omit to leave unchanged on an existing class" }
                },
                "required": ["board", "name"]
            }),
            |args, ctx| async move { handle_create_netclass(args, ctx).await }
        ),
        tool!(
            "assign_net_to_class",
            "Assign a net to an existing netclass in the PCB file (S-expression edit).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string", "description": "Path to .kicad_pcb file" },
                    "net_name":  { "type": "string", "description": "Net name to assign" },
                    "netclass":  { "type": "string", "description": "Netclass name to assign the net to" }
                },
                "required": ["board", "net_name", "netclass"]
            }),
            |args, ctx| async move { handle_assign_net_to_class(args, ctx).await }
        ),
        tool!(
            "route_differential_pair",
            "Route a differential pair (two parallel traces with a specified gap).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_pos":  { "type": "string", "description": "Positive net name" },
                    "net_neg":  { "type": "string", "description": "Negative net name" },
                    "layer":    { "type": "string", "default": "F.Cu" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.1 },
                    "gap":   { "type": "number", "description": "Gap between pair traces in mm", "default": 0.1 }
                },
                "required": ["board", "net_pos", "net_neg", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_diff_pair(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_net(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    // Count existing nets to determine next net ID
    let net_id = content.matches("(net ").count() as i32;
    let net_sexp = format!("\n  (net {net_id} \"{net_name}\")");
    // Insert before the last closing paren
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, net_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net_id": net_id, "net_name": net_name }),
    ))
}

async fn handle_route_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.25);

    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, |c| c
        .add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2));
    Ok(CallToolResult::json(&json!({
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 }
    })))
}

async fn handle_route_pad_to_pad(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad1 = match require_str(args, "pad1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad2 = match require_str(args, "pad2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let width = args["width"].as_f64().unwrap_or(0.25);

    // Look up pad positions from the PCB S-expression file
    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    let pos1 = find_pad_board_position(&tree, &ref1, &pad1)?;
    let pos2 = find_pad_board_position(&tree, &ref2, &pad2)?;

    // Route an L-bend: horizontal first, then vertical
    let (x1, y1) = pos1;
    let (x2, y2) = pos2;
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();

    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        // Already axis-aligned: single segment
        ipc!(ctx, |c| c
            .add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2));
    } else {
        // L-bend: horizontal then vertical
        let mid_x = x2;
        let mid_y = y1;
        let net_a = net_name.clone();
        let net_b = net_name.clone();
        let layer_a = layer.clone();
        let layer_b = layer.clone();
        ipc!(ctx, |c| {
            c.add_track(&net_a, &layer_a, width, x1, y1, mid_x, mid_y)?;
            c.add_track(&net_b, &layer_b, width, mid_x, mid_y, x2, y2)?;
            Ok(())
        });
    }

    Ok(CallToolResult::json(&json!({
        "routed": true,
        "net": net_name, "layer": layer, "width": width,
        "from": { "ref": ref1, "pad": pad1, "x": x1, "y": y1 },
        "to":   { "ref": ref2, "pad": pad2, "x": x2, "y": y2 }
    })))
}

/// Look up a pad's board-space (x, y) position from the parsed PCB S-expression tree.
fn find_pad_board_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
    pad_number: &str,
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
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pad = fp_node
        .find_all("pad")
        .into_iter()
        .find(|p| p.get(1).and_then(|n| n.as_str()) == Some(pad_number))
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found on '{}'", pad_number, reference))?;

    let pad_at = pad
        .find("at")
        .ok_or_else(|| anyhow::anyhow!("Pad has no (at) node"))?;
    let local_x = pad_at.get_f64(1).unwrap_or(0.0);
    let local_y = pad_at.get_f64(2).unwrap_or(0.0);

    // Transform local pad coords to board space. KiCAD's `at` rotation is
    // clockwise-positive (Y axis points down), i.e. the mirror of the
    // standard CCW math convention — verified against a real kicad-cli DRC
    // report: a -90° footprint with pad "2" at local (0.9125, 0) reports that
    // pad at board (fp_x, fp_y + 0.9125), which only this sign combination
    // reproduces. Using the naive CCW formula silently swaps pads on any
    // 90°/270° rotated footprint (invisible at 0°/180°, where sin(rad)==0).
    let rad = fp_rot.to_radians();
    let board_x = fp_x + local_x * rad.cos() + local_y * rad.sin();
    let board_y = fp_y - local_x * rad.sin() + local_y * rad.cos();

    Ok((board_x, board_y))
}

async fn handle_add_via(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let drill = args["drill"].as_f64().unwrap_or(0.4);
    let pad_size = args["pad_size"].as_f64().unwrap_or(0.8);

    let net_ipc = net_name.clone();
    ipc!(ctx, |c| c.add_via(&net_ipc, x, y, drill, pad_size));
    Ok(CallToolResult::json(
        &json!({ "net": net_name, "x": x, "y": y, "drill": drill, "pad_size": pad_size }),
    ))
}

async fn handle_add_copper_pour(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let clearance = args["clearance"].as_f64().unwrap_or(0.2);
    let min_w = args["min_width"].as_f64().unwrap_or(0.25);
    let pts_arr = match args["points"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'points' array")),
    };

    let pts: Vec<(f64, f64)> = pts_arr
        .iter()
        .filter_map(|p| Some((p["x"].as_f64()?, p["y"].as_f64()?)))
        .collect();
    if pts.len() < 3 {
        return Ok(CallToolResult::error("Zone requires at least 3 points"));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let net_id = find_net_id(&content, &net_name);
    let zone_s = format_zone(net_id, &net_name, &layer, clearance, min_w, &pts);
    let close = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close, zone_s)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net": net_name, "layer": layer, "points": pts.len() }),
    ))
}

async fn handle_delete_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let uuid_ipc = uuid.clone();
    ipc!(ctx, |c| c.delete_track(&uuid_ipc));
    Ok(CallToolResult::json(&json!({ "deleted_uuid": uuid })))
}

async fn handle_query_traces(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net = args["net_name"].as_str().map(String::from);
    let layer = args["layer"].as_str().map(String::from);

    let tracks = ipc!(ctx, |c| { c.get_tracks(net.as_deref(), layer.as_deref()) });

    let items: Vec<serde_json::Value> = tracks
        .iter()
        .map(|t| {
            json!({
                "net": t.net_name, "layer": t.layer, "width": t.width,
                "x1": t.start.x, "y1": t.start.y,
                "x2": t.end.x,   "y2": t.end.y
            })
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "traces": items }),
    ))
}

async fn handle_get_nets_list(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let nets = ipc!(ctx, |c| c.get_nets());
    let items: Vec<serde_json::Value> = nets
        .iter()
        .map(|n| json!({ "name": n.name, "netcode": n.netcode }))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "nets": items }),
    ))
}

async fn handle_modify_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.25);

    let uuid_ipc = uuid.clone();
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, |c| {
        c.delete_track(&uuid_ipc)?;
        c.add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2)
    });
    Ok(CallToolResult::json(&json!({
        "modified_uuid": uuid,
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 }
    })))
}

/// Modern (KiCAD 7+) projects store netclasses in the sibling .kicad_pro's
/// `net_settings.classes[]` instead of the legacy per-class `(net_class Name
/// ...)` elements KiCad ≤6 wrote directly into the .kicad_pcb (singular,
/// unwrapped — per KiCad's own file-format docs — not the plural
/// `(net_classes ...)` container this comment previously and wrongly
/// assumed). Confirmed live (2026-08-15) against a real KiCad 10 board: no
/// `(net_class ...)` element anywhere in its .kicad_pcb. `board.with_extension`
/// gives the sibling path directly since both files share a stem by
/// convention.
fn sibling_project_path(board: &std::path::Path) -> Option<std::path::PathBuf> {
    let candidate = board.with_extension("kicad_pro");
    candidate.exists().then_some(candidate)
}

fn project_has_netclasses(project_path: &std::path::Path) -> bool {
    std::fs::read_to_string(project_path)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| v.pointer("/net_settings/classes").cloned())
        .is_some_and(|v| v.is_array())
}

/// Create or update `name` in the modern project's `net_settings.classes[]`.
/// Returns `true` if an existing class was updated, `false` if a new one was
/// appended. Only fields actually present in `args` are applied to an
/// EXISTING class — the tool's documented defaults (0.2/0.25/0.4/0.8) are
/// only used to fill in a brand-NEW class, so calling this to nudge one
/// field on an already-tuned class (e.g. just `trace_width`) never silently
/// clobbers its other fields (clearance, via sizing, …) back to generic
/// defaults.
/// Returns `(updated, final_class)` — `updated` is `true` if an existing
/// class was patched in place, `false` if a new one was appended;
/// `final_class` is the class object as it now stands on disk, so callers
/// can report what's actually there instead of re-deriving it from `args`
/// (which would wrongly show this tool's generic defaults for fields an
/// update left untouched).
fn upsert_project_netclass(
    project_path: &std::path::Path,
    name: &str,
    args: &serde_json::Value,
) -> anyhow::Result<(bool, serde_json::Value)> {
    let content = std::fs::read_to_string(project_path)?;
    let mut root: serde_json::Value = serde_json::from_str(&content)?;
    let classes = root
        .pointer_mut("/net_settings/classes")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project file has no net_settings.classes array: {}",
                project_path.display()
            )
        })?;

    let existing = classes
        .iter_mut()
        .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(name));

    let (updated, final_class) = if let Some(class) = existing {
        for (arg_key, json_key) in [
            ("clearance", "clearance"),
            ("trace_width", "track_width"),
            ("via_drill", "via_drill"),
            ("via_diameter", "via_diameter"),
        ] {
            if let Some(val) = args[arg_key].as_f64() {
                class[json_key] = json!(val);
            }
        }
        (true, class.clone())
    } else {
        // Field set mirrors what KiCad itself writes for a class with no
        // special tuning — same shape as the project's existing "Default"
        // class, just with this tool's values for the 4 fields it exposes.
        let new_class = json!({
            "name": name,
            "clearance": args["clearance"].as_f64().unwrap_or(0.2),
            "track_width": args["trace_width"].as_f64().unwrap_or(0.25),
            "via_drill": args["via_drill"].as_f64().unwrap_or(0.4),
            "via_diameter": args["via_diameter"].as_f64().unwrap_or(0.8),
            "bus_width": 12,
            "diff_pair_gap": 0.25,
            "diff_pair_via_gap": 0.25,
            "diff_pair_width": 0.2,
            "line_style": 0,
            "microvia_diameter": 0.3,
            "microvia_drill": 0.1,
            "pcb_color": "rgba(0, 0, 0, 0.000)",
            "priority": 2147483647i64,
            "schematic_color": "rgba(0, 0, 0, 0.000)",
            "tuning_profile": "",
            "wire_width": 6
        });
        classes.push(new_class.clone());
        (false, new_class)
    };

    // Trailing newline matches what KiCad's own writer produces — keeps a
    // one-field edit from also flipping the file's EOF-newline status.
    write_atomic(
        project_path,
        &format!("{}\n", serde_json::to_string_pretty(&root)?),
    )?;
    Ok((updated, final_class))
}

async fn handle_create_netclass(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let name = match require_str(args, "name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let clearance = args["clearance"].as_f64().unwrap_or(0.2);
    let trace_width = args["trace_width"].as_f64().unwrap_or(0.25);
    let via_drill = args["via_drill"].as_f64().unwrap_or(0.4);
    let via_dia = args["via_diameter"].as_f64().unwrap_or(0.8);

    // Singular, unwrapped element per KiCad's own file-format docs — see the
    // doc comment on sibling_project_path above for why this isn't
    // "(net_classes" (a container that doesn't exist in real KiCad files).
    let has_legacy_block = std::fs::read_to_string(&board_path)?.contains("(net_class ");

    // Prefer the modern project file whenever this board isn't actively
    // using the legacy (net_class ...) block itself — see doc comments on
    // sibling_project_path/upsert_project_netclass above.
    if !has_legacy_block {
        if let Some(project_path) = sibling_project_path(&board_path) {
            if project_has_netclasses(&project_path) {
                let (updated, class) = upsert_project_netclass(&project_path, &name, args)?;
                return Ok(CallToolResult::json(&json!({
                    "project_netclass": name,
                    "updated_existing": updated,
                    "clearance": class["clearance"],
                    "trace_width": class["track_width"],
                    "via_drill": class["via_drill"],
                    "via_diameter": class["via_diameter"]
                })));
            }
        }
    }

    let netclass_sexp = format!(
        "\n      (netclass \"{name}\"\n        (clearance {clearance})\n        \
         (trace_width {trace_width})\n        (via_drill {via_drill})\n        \
         (via_diameter {via_dia})\n      )"
    );

    let content = std::fs::read_to_string(&board_path)?;
    // Find (net_classes block or (net_settings block to insert into
    let insert_pos = if let Some(nc_pos) = content.find("(net_classes") {
        // Find closing paren of (net_classes ...)
        let block = &content[nc_pos..];
        nc_pos
            + block
                .find("\n    )")
                .unwrap_or(block.find(')').unwrap_or(block.len() - 1))
    } else {
        // No net_classes block; insert before last )
        content.rfind(')').unwrap_or(content.len())
    };

    let new_content = apply_edits(content, vec![SexpEdit::insert(insert_pos, netclass_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "created_netclass": name,
        "clearance": clearance, "trace_width": trace_width,
        "via_drill": via_drill, "via_diameter": via_dia
    })))
}

async fn handle_assign_net_to_class(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let netclass = match require_str(args, "netclass") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;

    // Find the netclass block: (netclass "NAME" ...)
    let nc_pat = format!("(netclass \"{}\"", netclass);
    let nc_pos = match content.find(&nc_pat) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Netclass '{}' not found in board file",
                netclass
            )))
        }
    };

    // Find the closing paren of the netclass block
    let mut depth = 0i32;
    let mut nc_end = nc_pos;
    for (i, ch) in content[nc_pos..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    nc_end = nc_pos + i;
                    break;
                }
            }
            _ => {}
        }
    }

    // Check if net is already assigned
    let nc_block = &content[nc_pos..nc_end];
    let net_check = format!("(net \"{}\")", net_name);
    if nc_block.contains(&net_check) {
        return Ok(CallToolResult::json(&json!({
            "already_assigned": true,
            "net_name": net_name,
            "netclass": netclass
        })));
    }

    // Insert the net assignment before the closing paren of the netclass block
    let net_entry = format!("\n        (net \"{}\")", net_name);
    let new_content = apply_edits(content, vec![SexpEdit::insert(nc_end, net_entry)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "assigned": true,
        "net_name": net_name,
        "netclass": netclass
    })))
}

async fn handle_route_diff_pair(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net_pos = match require_str(args, "net_pos") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_neg = match require_str(args, "net_neg") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.1);
    let gap = args["gap"].as_f64().unwrap_or(0.1);
    let offset = (gap + width) / 2.0;

    // Route two parallel traces offset perpendicular to the direction
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    let perp_x = -dy / len * offset;
    let perp_y = dx / len * offset;

    let np_ipc = net_pos.clone();
    let nn_ipc = net_neg.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, |c| {
        c.add_track(
            &np_ipc,
            &layer_ipc,
            width,
            x1 + perp_x,
            y1 + perp_y,
            x2 + perp_x,
            y2 + perp_y,
        )?;
        c.add_track(
            &nn_ipc,
            &layer_ipc,
            width,
            x1 - perp_x,
            y1 - perp_y,
            x2 - perp_x,
            y2 - perp_y,
        )
    });

    Ok(CallToolResult::json(&json!({
        "net_pos": net_pos, "net_neg": net_neg,
        "layer": layer, "width": width, "gap": gap
    })))
}

// ─── Ratsnest / unrouted connections ───────────────────────────────────────────

/// One pad's board-space position and net, resolved from the S-expression tree.
struct PadRecord {
    reference: String,
    pad_number: String,
    net: String,
    x: f64,
    y: f64,
    through_hole: bool,
}

/// Enumerate every pad on the board with its board-space position and net.
/// Reuses the same rotation transform and coded-vs-name-only net fallback as
/// `find_pad_board_position` / `get_component_pads`, just applied across every
/// footprint in one pass instead of one reference at a time.
fn all_pads(tree: &konnect_sexp::parser::SexpNode) -> Vec<PadRecord> {
    let mut out = Vec::new();
    for fp in tree.find_all("footprint") {
        let reference = fp
            .find_all("property")
            .iter()
            .find(|p| p.get(1).and_then(|n| n.as_str()) == Some("Reference"))
            .and_then(|p| p.get(2))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        if reference.is_empty() {
            continue;
        }

        let fp_at = fp.find("at");
        let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
        let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
        let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);
        let rad = fp_rot.to_radians();

        for pad in fp.find_all("pad") {
            let Some(pad_number) = pad.get(1).and_then(|n| n.as_str()) else {
                continue;
            };
            let Some(pad_at) = pad.find("at") else {
                continue;
            };
            let local_x = pad_at.get_f64(1).unwrap_or(0.0);
            let local_y = pad_at.get_f64(2).unwrap_or(0.0);
            // Same clockwise-positive rotation as find_pad_board_position above.
            let x = fp_x + local_x * rad.cos() + local_y * rad.sin();
            let y = fp_y - local_x * rad.sin() + local_y * rad.cos();

            // Pad net is `(net <code> "name")` on coded boards or `(net "name")`
            // on name-only boards — same fallback as get_component_pads.
            let net = pad
                .find("net")
                .and_then(|n| n.get(2).or_else(|| n.get(1)))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();

            let pad_type = pad.get(2).and_then(|n| n.as_str()).unwrap_or("");
            let through_hole = pad_type == "thru_hole" || pad_type == "np_thru_hole";

            out.push(PadRecord {
                reference: reference.clone(),
                pad_number: pad_number.to_string(),
                net,
                x,
                y,
                through_hole,
            });
        }
    }
    out
}

/// kicad-cli reports DRC item coordinates in mm at ~1e-6 precision — the same
/// numbers our own rotation transform produces for the same pad. 0.001mm safely
/// absorbs float rounding without risking a false match between distinct pads.
const POS_EPSILON_MM: f64 = 0.001;

/// Find the pad at board position (x, y) within `POS_EPSILON_MM`, closest first.
fn resolve_pad_at(pads: &[PadRecord], x: f64, y: f64) -> Option<&PadRecord> {
    pads.iter()
        .filter(|p| (p.x - x).abs() < POS_EPSILON_MM && (p.y - y).abs() < POS_EPSILON_MM)
        .min_by(|a, b| {
            let da = (a.x - x).powi(2) + (a.y - y).powi(2);
            let db = (b.x - x).powi(2) + (b.y - y).powi(2);
            da.partial_cmp(&db).unwrap()
        })
}

async fn handle_get_unrouted_connections(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let refill_zones = args["refill_zones"].as_bool().unwrap_or(true);
    let net_filter: Option<Vec<String>> = args["net_filter"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let violations = cli::run_drc(&ctx.config.kicad_cli, &board_path, refill_zones, false).await?;

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let pads = all_pads(&tree);

    let mut connections = Vec::new();
    for v in violations.iter().filter(|v| v.kind == "unconnected") {
        let endpoints: Vec<serde_json::Value> = v
            .items
            .iter()
            .map(|item| {
                match item
                    .pos
                    .as_ref()
                    .and_then(|p| resolve_pad_at(&pads, p.x, p.y))
                {
                    Some(pad) => json!({
                        "reference": pad.reference,
                        "pad": pad.pad_number,
                        "net": pad.net,
                        "x": pad.x,
                        "y": pad.y,
                        "through_hole": pad.through_hole,
                        "resolved": true
                    }),
                    None => json!({
                        "description": item.description,
                        "pos": item.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y })),
                        "resolved": false
                    }),
                }
            })
            .collect();

        let net = endpoints
            .iter()
            .find_map(|e| e["net"].as_str())
            .unwrap_or("")
            .to_string();

        if let Some(filter) = &net_filter {
            if !net.is_empty() && !filter.iter().any(|f| f == &net) {
                continue;
            }
        }

        let distance_mm = if endpoints.len() == 2 {
            match (
                endpoints[0]["x"].as_f64(),
                endpoints[0]["y"].as_f64(),
                endpoints[1]["x"].as_f64(),
                endpoints[1]["y"].as_f64(),
            ) {
                (Some(x1), Some(y1), Some(x2), Some(y2)) => {
                    Some(((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt())
                }
                _ => None,
            }
        } else {
            None
        };

        connections.push(json!({
            "net": net,
            "distance_mm": distance_mm,
            "endpoints": endpoints
        }));
    }

    let mut nets_affected: Vec<String> = connections
        .iter()
        .filter_map(|c| c["net"].as_str().map(String::from))
        .filter(|n| !n.is_empty())
        .collect();
    nets_affected.sort();
    nets_affected.dedup();

    Ok(CallToolResult::json(&json!({
        "board": board_path.to_str().unwrap_or(""),
        "refill_zones": refill_zones,
        "unrouted_count": connections.len(),
        "nets_affected": nets_affected,
        "connections": connections
    })))
}

#[cfg(test)]
mod ratsnest_tests {
    use super::*;

    const SAMPLE: &str = r#"
    (kicad_pcb
      (footprint "R" (layer "F.Cu") (uuid "f1")
        (at 10 20 0)
        (property "Reference" "R1" (at 0 0 0))
        (pad "1" smd rect (at -1 0) (size 1 1) (layers "F.Cu") (net "GND"))
        (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net "VCC"))
      )
      (footprint "C" (layer "F.Cu") (uuid "f2")
        (at 15 20 90)
        (property "Reference" "C1" (at 0 0 0))
        (pad "1" thru_hole circle (at 0 -1) (size 1 1) (drill 0.5) (layers "*.Cu") (net "GND"))
      )
    )
    "#;

    #[test]
    fn all_pads_resolves_reference_net_type_and_rotation() {
        let tree = konnect_sexp::parser::parse_sexp(SAMPLE).unwrap();
        let pads = all_pads(&tree);
        assert_eq!(pads.len(), 3);

        let r1_pad1 = pads
            .iter()
            .find(|p| p.reference == "R1" && p.pad_number == "1")
            .unwrap();
        assert_eq!(r1_pad1.net, "GND");
        assert!(!r1_pad1.through_hole);
        assert!((r1_pad1.x - 9.0).abs() < 1e-9); // 10 + (-1) at 0deg rotation
        assert!((r1_pad1.y - 20.0).abs() < 1e-9);

        let c1_pad1 = pads.iter().find(|p| p.reference == "C1").unwrap();
        assert!(c1_pad1.through_hole);
        assert_eq!(c1_pad1.net, "GND");
        // 90deg footprint rotation (clockwise-positive) of local (0, -1) around (15, 20):
        // x = 15 + 0*cos90 + (-1)*sin90 = 14, y = 20 - 0*sin90 + (-1)*cos90 = 20
        assert!((c1_pad1.x - 14.0).abs() < 1e-9);
        assert!((c1_pad1.y - 20.0).abs() < 1e-9);
    }

    #[test]
    fn resolve_pad_at_matches_within_epsilon_and_rejects_far_points() {
        let tree = konnect_sexp::parser::parse_sexp(SAMPLE).unwrap();
        let pads = all_pads(&tree);
        let r1_pad2 = pads
            .iter()
            .find(|p| p.reference == "R1" && p.pad_number == "2")
            .unwrap();

        let hit = resolve_pad_at(&pads, r1_pad2.x + 0.0002, r1_pad2.y - 0.0002);
        assert_eq!(
            hit.map(|p| (p.reference.as_str(), p.pad_number.as_str())),
            Some(("R1", "2"))
        );

        assert!(resolve_pad_at(&pads, 999.0, 999.0).is_none());
    }

    #[test]
    fn resolve_pad_at_picks_nearest_on_near_coincident_pads() {
        // Two pads 0.0005mm apart (below epsilon from either), a probe closer
        // to one than the other must resolve to the nearer one, not the first.
        let sample = r#"
        (kicad_pcb
          (footprint "A" (layer "F.Cu") (uuid "f1")
            (at 0 0 0)
            (property "Reference" "A1" (at 0 0 0))
            (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "N1"))
          )
          (footprint "B" (layer "F.Cu") (uuid "f2")
            (at 0.0005 0 0)
            (property "Reference" "B1" (at 0 0 0))
            (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "N1"))
          )
        )
        "#;
        let tree = konnect_sexp::parser::parse_sexp(sample).unwrap();
        let pads = all_pads(&tree);
        // 0.0001mm from A1 (at x=0), 0.0004mm from B1 (at x=0.0005) — nearer to A1.
        let hit = resolve_pad_at(&pads, 0.0001, 0.0);
        assert_eq!(hit.map(|p| p.reference.as_str()), Some("A1"));
    }

    /// Regression for the rotation-sign bug found while building this tool:
    /// a real -90°-rotated two-pad resistor footprint (from
    /// ClearBell_Inneneinheit's R_TOUCH1) whose pad 2 board position was
    /// independently confirmed via a live kicad-cli DRC report
    /// (148.35, 116.0125). The pre-fix formula placed pad "2" at pad "1"'s
    /// spot instead — silently wrong for any 90°/270° rotated footprint.
    #[test]
    fn all_pads_matches_live_drc_confirmed_rotated_footprint() {
        let sample = r#"
        (kicad_pcb
          (footprint "Resistor_SMD:R_0603" (layer "F.Cu") (uuid "f1")
            (at 148.35 115.1 -90)
            (property "Reference" "R_TOUCH1" (at 0 0 0))
            (pad "1" smd roundrect (at -0.9125 0 270) (size 0.975 0.95)
              (layers "F.Cu") (net "Net-(U2-IO32)"))
            (pad "2" smd roundrect (at 0.9125 0 270) (size 0.975 0.95)
              (layers "F.Cu") (net "Net-(J_TOUCH1-Pin_1)"))
          )
        )
        "#;
        let tree = konnect_sexp::parser::parse_sexp(sample).unwrap();
        let pads = all_pads(&tree);

        let pad2 = pads
            .iter()
            .find(|p| p.reference == "R_TOUCH1" && p.pad_number == "2")
            .unwrap();
        assert!((pad2.x - 148.35).abs() < 1e-9);
        assert!((pad2.y - 116.0125).abs() < 1e-9);
        assert_eq!(pad2.net, "Net-(J_TOUCH1-Pin_1)");

        let pad1 = pads
            .iter()
            .find(|p| p.reference == "R_TOUCH1" && p.pad_number == "1")
            .unwrap();
        assert!((pad1.x - 148.35).abs() < 1e-9);
        assert!((pad1.y - 114.1875).abs() < 1e-9);
    }
}

#[cfg(test)]
mod live_ratsnest_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    /// Live end-to-end check against a real board with a known missing
    /// connection. Skipped unless TEST_PCB is set, so `cargo test` stays
    /// green without KiCAD installed.
    #[tokio::test]
    async fn live_get_unrouted_connections_resolves_real_pads() {
        let Ok(pcb) = std::env::var("TEST_PCB") else {
            eprintln!("SKIP: set TEST_PCB to run the live ratsnest test");
            return;
        };
        let cli_path = std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string());

        let ctx = ToolContext::new(
            ServerConfig {
                kicad_cli: cli_path,
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
            },
            Arc::new(ToolRouter::new()),
        );

        let args = json!({ "board": pcb });
        let result = handle_get_unrouted_connections(&args, &ctx)
            .await
            .expect("get_unrouted_connections should succeed");

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => {
                serde_json::from_str::<serde_json::Value>(text).unwrap()
            }
            _ => panic!("expected text content"),
        };

        eprintln!(
            "LIVE ratsnest: {}",
            serde_json::to_string_pretty(&body).unwrap()
        );

        let count = body["unrouted_count"].as_u64().unwrap();
        assert!(
            count > 0,
            "expected at least one unrouted connection on the probe board"
        );

        let connections = body["connections"].as_array().unwrap();
        let resolved_pairs: usize = connections
            .iter()
            .flat_map(|c| c["endpoints"].as_array().unwrap())
            .filter(|e| e["resolved"] == true)
            .count();
        assert!(
            resolved_pairs > 0,
            "expected at least one endpoint resolved to a real pad, not just raw DRC text"
        );
    }
}

#[cfg(test)]
mod netclass_json_tests {
    use super::*;

    /// Mirrors the actual shape found in a real KiCad 10 project (ClearBell
    /// Ausseneinheit, 2026-08-15): a single "Default" class plus the
    /// surrounding net_settings keys real projects always carry alongside it.
    const SAMPLE_PROJECT: &str = r#"{
        "net_settings": {
            "classes": [
                {
                    "name": "Default",
                    "clearance": 0.2,
                    "track_width": 0.2,
                    "via_diameter": 0.6,
                    "via_drill": 0.3,
                    "priority": 2147483647
                }
            ],
            "meta": { "version": 4 },
            "net_colors": null,
            "netclass_assignments": null,
            "netclass_patterns": []
        },
        "board": {
            "design_settings": {
                "rules": {
                    "min_clearance": 0.2,
                    "min_track_width": 0.3
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
    fn upsert_updates_existing_class_without_touching_untouched_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path());

        let args = json!({ "trace_width": 0.3 });
        let (updated, class) = upsert_project_netclass(&path, "Default", &args).unwrap();
        assert!(updated, "Default already exists, should update not append");
        assert_eq!(class["track_width"], 0.3, "returned class reflects the write");

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let classes = saved["net_settings"]["classes"].as_array().unwrap();
        assert_eq!(classes.len(), 1, "must not duplicate the class");
        let default = &classes[0];
        assert_eq!(default["track_width"], 0.3, "the field we asked to change");
        // Fields NOT passed in `args` must survive untouched — this is the
        // whole point of upsert vs. blindly re-writing tool defaults.
        assert_eq!(default["clearance"], 0.2);
        assert_eq!(default["via_diameter"], 0.6);
        assert_eq!(default["via_drill"], 0.3);
        assert_eq!(default["priority"], 2147483647i64);
    }

    #[test]
    fn upsert_appends_new_class_with_full_field_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path());

        let args = json!({ "clearance": 0.25, "trace_width": 0.4 });
        let (updated, _class) = upsert_project_netclass(&path, "Power", &args).unwrap();
        assert!(!updated, "Power doesn't exist yet, should append not update");

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let classes = saved["net_settings"]["classes"].as_array().unwrap();
        assert_eq!(classes.len(), 2, "Default must survive alongside the new class");
        let power = classes.iter().find(|c| c["name"] == "Power").unwrap();
        assert_eq!(power["clearance"], 0.25);
        assert_eq!(power["track_width"], 0.4);
        // Omitted fields fall back to this tool's documented defaults.
        assert_eq!(power["via_drill"], 0.4);
        assert_eq!(power["via_diameter"], 0.8);
    }

    #[test]
    fn upsert_errors_cleanly_on_project_without_classes_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pro");
        std::fs::write(&path, r#"{"meta": {"version": 4}}"#).unwrap();

        let err = upsert_project_netclass(&path, "Default", &json!({})).unwrap_err();
        assert!(err.to_string().contains("net_settings.classes"));
    }

    #[test]
    fn project_has_netclasses_is_false_for_missing_or_malformed_project() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.kicad_pro");
        assert!(!project_has_netclasses(&missing));

        let malformed = dir.path().join("malformed.kicad_pro");
        std::fs::write(&malformed, r#"{"net_settings": {}}"#).unwrap();
        assert!(!project_has_netclasses(&malformed));
    }

    /// Regression for the has_legacy_block detection bug found in review
    /// (2026-08-15): it originally searched for a plural "(net_classes"
    /// container that real KiCad never writes, so it could never detect a
    /// genuinely legacy board — meaning an old-format board's actual
    /// `(net_class ...)` data could get silently bypassed in favor of a
    /// sibling project's JSON classes. This uses the real, singular,
    /// unwrapped token from KiCad's own file-format docs.
    #[tokio::test]
    async fn create_netclass_prefers_legacy_block_when_board_actively_uses_one() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        // Real KiCad shape: (net_class ...) as an unwrapped sibling element,
        // NOT wrapped in a "(net_classes ...)" container — deliberately no
        // such wrapper here, so this fixture only matches the corrected
        // has_legacy_block check and would NOT have passed against the
        // original buggy `contains("(net_classes")` search.
        std::fs::write(
            &board,
            "(kicad_pcb (net_class Default \"\" (clearance 0.2)(trace_width 0.2)(via_dia 0.6)(via_drill 0.3)))",
        )
        .unwrap();
        // A sibling project ALSO has a populated classes array — the legacy
        // block must still win, since that's what a legacy-format board's
        // own KiCad instance actually reads.
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

        let args = json!({
            "board": board.to_str().unwrap(),
            "name": "Power",
            "trace_width": 0.4
        });
        handle_create_netclass(&args, &ctx).await.unwrap();

        // The legacy path was taken: the .kicad_pcb gained the new class...
        let pcb_content = std::fs::read_to_string(&board).unwrap();
        assert!(pcb_content.contains("\"Power\""), "legacy insert should have run");

        // ...and the sibling project's classes array was left untouched (no
        // JSON-path write happened).
        let saved: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("board.kicad_pro")).unwrap(),
        )
        .unwrap();
        let classes = saved["net_settings"]["classes"].as_array().unwrap();
        assert_eq!(classes.len(), 1, "only the original Default, no Power appended to JSON");
    }

    #[test]
    fn upsert_project_netclass_preserves_key_order_on_write() {
        // Regression for the alphabetical-reorder bug found in review
        // (2026-08-15): without serde_json's preserve_order feature, writing
        // back the whole file resorts every key at every level, turning a
        // one-field edit into a full-file reformat. "zebra" sorts after
        // "alpha" alphabetically but appears first here — order must survive.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pro");
        std::fs::write(
            &path,
            r#"{"net_settings": {"classes": [{"name": "Default", "zebra_field": 1, "alpha_field": 2}]}}"#,
        )
        .unwrap();

        upsert_project_netclass(&path, "Default", &json!({ "trace_width": 0.3 })).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let zebra_pos = raw.find("zebra_field").unwrap();
        let alpha_pos = raw.find("alpha_field").unwrap();
        assert!(
            zebra_pos < alpha_pos,
            "original key order (zebra before alpha) must survive the round-trip, got: {raw}"
        );
    }

    /// Regression for the response-echoes-defaults-not-reality bug found in
    /// review (2026-08-15), through the actual handler (not just
    /// upsert_project_netclass directly) — a partial update to an
    /// already-tuned class must report the class's REAL current field
    /// values, not this tool's generic hard-coded defaults for whatever
    /// fields the caller didn't touch.
    #[tokio::test]
    async fn create_netclass_response_reflects_actual_values_not_tool_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (setup))").unwrap();
        // Default class already tuned away from this tool's generic
        // defaults (0.2/0.25/0.4/0.8) on every field but trace_width.
        std::fs::write(
            dir.path().join("board.kicad_pro"),
            r#"{"net_settings": {"classes": [
                {"name": "Default", "clearance": 0.15, "track_width": 0.2, "via_drill": 0.3, "via_diameter": 0.6}
            ]}}"#,
        )
        .unwrap();

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

        let args = json!({
            "board": board.to_str().unwrap(),
            "name": "Default",
            "trace_width": 0.3
        });
        let result = handle_create_netclass(&args, &ctx).await.unwrap();
        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => {
                serde_json::from_str::<serde_json::Value>(text).unwrap()
            }
            _ => panic!("expected text content"),
        };

        assert_eq!(body["trace_width"], 0.3, "the field we asked to change");
        // Untouched fields must report the REAL value (0.15/0.3/0.6), not
        // this tool's generic defaults (0.2/0.4/0.8) — that mismatch was
        // exactly the bug.
        assert_eq!(body["clearance"], 0.15);
        assert_eq!(body["via_drill"], 0.3);
        assert_eq!(body["via_diameter"], 0.6);
    }
}
