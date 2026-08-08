//! `pcb_components` toolset — place, move, rotate, query, and array footprints on the PCB.
//!
//! Most operations use the KiCAD IPC API so they integrate with KiCAD's undo/redo
//! system and don't require a separate file-sync step. `get_board_2d_view` uses
//! kicad-cli to render a PNG.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::netlist::{parse_board, parse_netlist, Plan};
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::writer::{apply_edits, find_balanced_block, write_atomic, SexpEdit};
use serde_json::json;

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

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "place_component",
            "Place a footprint on the PCB at the given position and layer via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":      { "type": "string" },
                    "footprint":  { "type": "string", "description": "Library:Footprint (e.g. 'Resistor_SMD:R_0402')" },
                    "reference":  { "type": "string", "description": "Reference designator" },
                    "x":          { "type": "number" },
                    "y":          { "type": "number" },
                    "rotation":   { "type": "number", "default": 0 },
                    "layer":      { "type": "string", "default": "F.Cu" }
                },
                "required": ["board", "footprint", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_place_component(args, ctx).await }
        ),
        tool!(
            "move_component",
            "Move a placed footprint to a new X/Y position via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" }
                },
                "required": ["board", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_component(args, ctx).await }
        ),
        tool!(
            "rotate_component",
            "Set the rotation angle of a placed footprint via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation":  { "type": "number", "description": "Rotation angle in degrees" }
                },
                "required": ["board", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_component(args, ctx).await }
        ),
        tool!(
            "delete_component",
            "Remove a footprint from the board via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_delete_component(args, ctx).await }
        ),
        tool!(
            "edit_component",
            "Update the value or other properties of a placed footprint via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "value":     { "type": "string", "description": "New value string (optional)" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_edit_component(args, ctx).await }
        ),
        tool!(
            "find_component",
            "Find a footprint on the board by reference designator and return its position.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_find_component(args, ctx).await }
        ),
        tool!(
            "get_component_pads",
            "Return the pad positions and net assignments for a footprint.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_get_component_pads(args, ctx).await }
        ),
        tool!(
            "get_pad_position",
            "Return the schematic-space position of a specific pad number on a footprint.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "reference":   { "type": "string" },
                    "pad_number":  { "type": "string" }
                },
                "required": ["board", "reference", "pad_number"]
            }),
            |args, ctx| async move { handle_get_pad_position(args, ctx).await }
        ),
        tool!(
            "set_pad_net",
            "Reassign the net of a single pad on a footprint by rewriting the pad's (net ...) entry directly in the .kicad_pcb (S-expression edit, no KiCAD IPC required). The target net must already exist in the board's net table. Fixes a stale/swapped pad-net assignment — the file-level effect of 'Update PCB from Schematic' for one pad — without the GUI. Does NOT move copper: traces touching the pad keep their own net, so re-run DRC afterwards. If KiCAD has the board open, revert/reload it to see the change.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator, e.g. 'U1'" },
                    "pad":       { "type": "string", "description": "Pad number/name as written in the footprint, e.g. '1'" },
                    "net_name":  { "type": "string", "description": "Target net name; must already exist in the board's net table (see get_nets_list / add_net)" }
                },
                "required": ["board", "reference", "pad", "net_name"]
            }),
            |args, ctx| async move { handle_set_pad_net(args, ctx).await }
        ),
        tool!(
            "update_pcb_from_schematic",
            "File-based 'Update PCB from Schematic' (KiCAD's F8) for connectivity: exports the schematic's netlist with kicad-cli, then rewrites every board pad's (net ...) and each footprint's Value to match the schematic — the authoritative source — directly in the .kicad_pcb (S-expression edits, no KiCAD IPC). This closes board-vs-schematic net parity in one pass (a bulk set_pad_net). It does NOT add, delete, or refootprint components (that needs library geometry / IPC) or move copper — such cases are reported as warnings, not applied. Defaults to dry_run=true: returns the full diff without touching the file. Set dry_run=false to apply. After applying, re-run DRC; if KiCAD has the board open, revert/reload it.",
            json!({
                "type": "object",
                "properties": {
                    "board":      { "type": "string", "description": "Path to .kicad_pcb file" },
                    "schematic":  { "type": "string", "description": "Path to .kicad_sch (default: sibling of the board with the same stem)" },
                    "netlist":    { "type": "string", "description": "Optional path to a pre-exported kicadsexpr netlist; skips the kicad-cli export" },
                    "dry_run":    { "type": "boolean", "description": "Report the diff without writing (default true)", "default": true }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_update_pcb_from_schematic(args, ctx).await }
        ),
        tool!(
            "get_component_list",
            "List all footprints on the board with their positions, layers, and values.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_component_list(args, ctx).await }
        ),
        tool!(
            "place_component_array",
            "Place multiple copies of a footprint in a grid or line array via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string" },
                    "footprint":    { "type": "string" },
                    "start_x":      { "type": "number" },
                    "start_y":      { "type": "number" },
                    "count_x":      { "type": "integer", "description": "Number of columns" },
                    "count_y":      { "type": "integer", "description": "Number of rows", "default": 1 },
                    "spacing_x":    { "type": "number", "description": "Column spacing in mm" },
                    "spacing_y":    { "type": "number", "description": "Row spacing in mm", "default": 0 },
                    "ref_prefix":   { "type": "string", "description": "Reference prefix (e.g. 'R')", "default": "U" },
                    "ref_start":    { "type": "integer", "description": "Starting reference number", "default": 1 }
                },
                "required": ["board", "footprint", "start_x", "start_y", "count_x", "spacing_x"]
            }),
            |args, ctx| async move { handle_place_array(args, ctx).await }
        ),
        tool!(
            "align_components",
            "Align multiple footprints along a common X or Y axis via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "references":  { "type": "array", "items": { "type": "string" } },
                    "axis":        { "type": "string", "description": "'x' or 'y'", "default": "x" },
                    "value":       { "type": "number", "description": "Target coordinate to align to" }
                },
                "required": ["board", "references", "value"]
            }),
            |args, ctx| async move { handle_align_components(args, ctx).await }
        ),
        tool!(
            "duplicate_component",
            "Duplicate an existing footprint at a new position via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":         { "type": "string" },
                    "reference":     { "type": "string", "description": "Reference to duplicate" },
                    "new_reference": { "type": "string", "description": "New reference designator" },
                    "x":             { "type": "number" },
                    "y":             { "type": "number" }
                },
                "required": ["board", "reference", "new_reference", "x", "y"]
            }),
            |args, ctx| async move { handle_duplicate_component(args, ctx).await }
        ),
        tool!(
            "get_board_2d_view",
            "Render the PCB as a 2-D image using kicad-cli and return it as a base64 PNG.",
            json!({
                "type": "object",
                "properties": {
                    "board":  { "type": "string" },
                    "layers": {
                        "type": "array",
                        "description": "Layers to include (empty = default copper + silkscreen)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_2d_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_place_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let footprint = match require_str(args, "footprint") {
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
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();

    let fp = ipc!(ctx, |c| c
        .place_footprint(&footprint, x, y, rotation, &layer));
    Ok(CallToolResult::json(&json!({
        "placed": fp.reference,
        "footprint": fp.footprint,
        "x": fp.position.x, "y": fp.position.y,
        "rotation": fp.rotation, "layer": fp.layer
    })))
}

async fn handle_move_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
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

    let ref_ipc = reference.clone();
    ipc!(ctx, |c| c.move_footprint(&ref_ipc, x, y));
    Ok(CallToolResult::json(
        &json!({ "moved": reference, "x": x, "y": y }),
    ))
}

async fn handle_rotate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    ipc!(ctx, |c| c.rotate_footprint(&ref_ipc, rotation));
    Ok(CallToolResult::json(
        &json!({ "rotated": reference, "rotation": rotation }),
    ))
}

async fn handle_delete_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    ipc!(ctx, |c| c.delete_footprint(&ref_ipc));
    Ok(CallToolResult::json(&json!({ "deleted": reference })))
}

async fn handle_edit_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // IPC doesn't have a direct "set value" command; re-get the footprint and report
    // For now this is a query + informational response. Full field edits require S-expr.
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let fp = ipc!(ctx, |c| {
        c.get_footprint(&reference)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", reference))
    });
    Ok(CallToolResult::json(&json!({
        "reference": fp.reference,
        "value": fp.value,
        "footprint": fp.footprint,
        "note": "Field edits via IPC are not yet supported. Edit in the schematic (edit_schematic_component), then open the PCB in KiCAD and run Tools > Update PCB from Schematic."
    })))
}

async fn handle_find_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let fp = ipc!(ctx, |c| {
        c.get_footprint(&reference)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", reference))
    });
    Ok(CallToolResult::json(&json!({
        "reference": fp.reference,
        "value": fp.value,
        "footprint": fp.footprint,
        "x": fp.position.x, "y": fp.position.y,
        "rotation": fp.rotation, "layer": fp.layer
    })))
}

async fn handle_get_component_pads(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    // Find the footprint with matching reference
    let fp_node = tree.find_all("footprint").into_iter().find(|fp| {
        fp.find_all("property").iter().any(|p| {
            p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                && p.get(2).and_then(|n| n.as_str()) == Some(reference.as_str())
        })
    });

    let fp_node = match fp_node {
        Some(n) => n,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pads: Vec<serde_json::Value> = fp_node
        .find_all("pad")
        .iter()
        .filter_map(|pad| {
            let number = pad.get(1)?.as_str()?.to_string();
            let pad_at = pad.find("at")?;
            let local_x = pad_at.get_f64(1)?;
            let local_y = pad_at.get_f64(2)?;
            // Transform local pad coords to board space (simplified: only rotation)
            let rad = fp_rot.to_radians();
            let board_x = fp_x + local_x * rad.cos() - local_y * rad.sin();
            let board_y = fp_y + local_x * rad.sin() + local_y * rad.cos();
            // Pad net is `(net <code> "name")` on coded boards or `(net "name")`
            // on name-only boards — the name is at index 2 or, lacking a code,
            // index 1. Reading only index 2 returned "" for name-only files.
            let net = pad
                .find("net")
                .and_then(|n| n.get(2).or_else(|| n.get(1)))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            Some(json!({ "number": number, "x": board_x, "y": board_y, "net": net }))
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "reference": reference, "pad_count": pads.len(), "pads": pads }),
    ))
}

async fn handle_get_pad_position(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let pad_number = match require_str(args, "pad_number") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pads_result = handle_get_component_pads(args, ctx).await?;
    // Parse the result and filter for the specific pad number
    if let Some(crate::mcp::protocol::ToolContent::Text { text }) = pads_result.content.first() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(pads) = parsed["pads"].as_array() {
                if let Some(pad) = pads
                    .iter()
                    .find(|p| p["number"].as_str() == Some(&pad_number))
                {
                    return Ok(CallToolResult::json(pad));
                }
            }
        }
    }
    Ok(CallToolResult::error(format!(
        "Pad '{}' not found",
        pad_number
    )))
}

async fn handle_set_pad_net(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad_number = match require_str(args, "pad") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;

    // Resolve the target net's numeric code from the board's top-level net table,
    // if the file uses coded nets at all. `find_all` is non-recursive, so this
    // only sees the top-level `(net code "name")` declarations, not pad nets.
    // Some boards store nets by name only — `(net "name")` with no code — in which
    // case this stays None and we mirror that name-only format below.
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let top_nets = tree.find_all("net");
    let file_uses_codes = top_nets.iter().any(|n| n.get(2).is_some());
    let net_code = top_nets
        .iter()
        .find(|n| n.get(2).and_then(|x| x.as_str()) == Some(net_name.as_str()))
        .and_then(|n| n.get(1))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    // 2. Locate the footprint block via its Reference property, then the
    //    enclosing `(footprint ...)`. The trailing quote in the pattern keeps
    //    'U1' from matching 'U10'.
    let ref_pat = format!("(property \"Reference\" \"{}\"", reference);
    let ref_pos = match content.find(&ref_pat) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };
    let fp_open = match content[..ref_pos].rfind("(footprint ") {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Could not locate the footprint block enclosing '{}'",
                reference
            )))
        }
    };
    let (fp_start, fp_end) = match find_balanced_block(&content, fp_open) {
        Some(r) => r,
        None => {
            return Ok(CallToolResult::error(
                "Unbalanced footprint block".to_string(),
            ))
        }
    };

    // 3. Locate the target pad within that footprint. Trailing space keeps
    //    '1' from matching '10'.
    let pad_pat = format!("(pad \"{}\" ", pad_number);
    let pad_pos = match content[fp_start..fp_end].find(&pad_pat) {
        Some(p) => fp_start + p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Pad '{}' not found on footprint '{}'",
                pad_number, reference
            )))
        }
    };
    let (pad_start, pad_end) = match find_balanced_block(&content, pad_pos) {
        Some(r) => r,
        None => return Ok(CallToolResult::error("Unbalanced pad block".to_string())),
    };

    // 4. Replace the pad's existing `(net ...)` node (or insert one), mirroring
    //    the file's net format: coded `(net <code> "name")` or name-only
    //    `(net "name")`.
    let existing = match content[pad_start..pad_end].find("(net ") {
        Some(net_rel) => {
            let net_abs = pad_start + net_rel;
            match find_balanced_block(&content, net_abs) {
                Some((ns, ne)) => Some((ns, ne, content[ns..ne].to_string())),
                None => {
                    return Ok(CallToolResult::error(
                        "Unbalanced (net ...) block on pad".to_string(),
                    ))
                }
            }
        }
        None => None,
    };

    // Does this pad's net (or, absent one, the file) carry a numeric code?
    let coded = match &existing {
        Some((_, _, old)) => old
            .trim_start()
            .trim_start_matches("(net")
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit()),
        None => file_uses_codes,
    };

    let new_net = if coded {
        match &net_code {
            Some(code) => format!("(net {code} \"{net_name}\")"),
            None => {
                return Ok(CallToolResult::error(format!(
                    "Net '{net_name}' is not in the board's net table, so it has no net code. Add it first (add_net) or check the name."
                )))
            }
        }
    } else {
        format!("(net \"{net_name}\")")
    };

    let (edit, old_net) = match existing {
        Some((net_start, net_end, old)) => {
            if old == new_net {
                return Ok(CallToolResult::json(&json!({
                    "reference": reference,
                    "pad": pad_number,
                    "net_name": net_name,
                    "unchanged": true,
                    "note": "Pad already assigned to this net."
                })));
            }
            (
                SexpEdit::replace(net_start, net_end, new_net.clone()),
                Some(old),
            )
        }
        // Pad currently has no net: insert one just before its closing paren.
        None => (SexpEdit::insert(pad_end - 1, format!(" {new_net}")), None),
    };

    let new_content = apply_edits(content, vec![edit]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "pad": pad_number,
        "net_name": net_name,
        "net_code": net_code,
        "old_net": old_net,
        "note": "Pad net rewritten in the .kicad_pcb. Copper was not moved — re-run DRC to verify. If KiCAD has the board open, revert/reload it to see the change."
    })))
}

/// Render a [`Plan`] as the JSON report both dry-run and apply return.
fn plan_report(plan: &Plan) -> serde_json::Value {
    json!({
        "pad_net_changes": plan.pad_net_changes.iter().map(|c| json!({
            "reference": c.reference, "pad": c.pad, "from": c.old, "to": c.new
        })).collect::<Vec<_>>(),
        "value_changes": plan.value_changes.iter().map(|c| json!({
            "reference": c.reference, "from": c.old, "to": c.new
        })).collect::<Vec<_>>(),
        "nets_to_add": plan.nets_to_add,
        "warnings": {
            "missing_on_board": plan.missing_on_board,
            "extra_on_board": plan.extra_on_board,
            "footprint_mismatches": plan.footprint_mismatches.iter().map(|(r, b, s)| json!({
                "reference": r, "board": b, "schematic": s
            })).collect::<Vec<_>>(),
            "unmatched_board_pads": plan.unmatched_board_pads.iter().map(|(r, p)| json!({
                "reference": r, "pad": p
            })).collect::<Vec<_>>(),
        }
    })
}

/// Locate the `(footprint ...)` block enclosing the given reference designator.
/// The trailing quote in the search pattern keeps 'U1' from matching 'U10'.
fn locate_fp_block(content: &str, reference: &str) -> Option<(usize, usize)> {
    let ref_pat = format!("(property \"Reference\" \"{}\"", reference);
    let ref_pos = content.find(&ref_pat)?;
    let fp_open = content[..ref_pos].rfind("(footprint ")?;
    find_balanced_block(content, fp_open)
}

fn escape_sexp_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn handle_update_pcb_from_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);

    // 1. Obtain the schematic netlist: use a pre-exported one if given, else
    //    export from the schematic (sibling of the board by default) via kicad-cli.
    let netlist_content = if let Some(nl) = args["netlist"].as_str() {
        std::fs::read_to_string(nl)?
    } else {
        let schematic = match args["schematic"].as_str() {
            Some(s) => std::path::PathBuf::from(s),
            None => board_path.with_extension("kicad_sch"),
        };
        if !schematic.exists() {
            return Ok(CallToolResult::error(format!(
                "Schematic not found at '{}'. Pass 'schematic' or 'netlist' explicitly.",
                schematic.display()
            )));
        }
        let out = board_path.with_extension("sync.net");
        if let Err(e) =
            super::cli::export_netlist(&ctx.config.kicad_cli, &schematic, &out, "kicadsexpr").await
        {
            return Ok(CallToolResult::error(format!(
                "kicad-cli netlist export failed: {e}"
            )));
        }
        let content = std::fs::read_to_string(&out)?;
        let _ = std::fs::remove_file(&out);
        content
    };

    // 2. Parse both sides and diff.
    let nl = parse_netlist(&netlist_content)?;
    let content = std::fs::read_to_string(&board_path)?;
    let board = parse_board(&content)?;
    let plan = crate::tools::netlist::plan(&nl, &board);
    let report = plan_report(&plan);

    if dry_run {
        return Ok(CallToolResult::json(&json!({
            "board": board_path.display().to_string(),
            "dry_run": true,
            "would_change": {
                "pad_nets": plan.pad_net_changes.len(),
                "values": plan.value_changes.len(),
                "nets_added": plan.nets_to_add.len(),
            },
            "plan": report,
            "note": "Dry run — nothing written. Re-run with dry_run=false to apply. Warnings (missing/extra/refootprint) are not applied automatically."
        })));
    }

    if plan.is_empty() {
        return Ok(CallToolResult::json(&json!({
            "board": board_path.display().to_string(),
            "dry_run": false,
            "applied": { "pad_nets": 0, "values": 0, "nets_added": 0 },
            "plan": report,
            "note": "Board already matches the schematic for pad nets and values. No changes written."
        })));
    }

    // 3. Build the net name→code map and append any nets a coded board is
    //    missing, assigning fresh codes after the current maximum.
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let mut name_to_code: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut max_code = 0i64;
    for n in tree.find_all("net") {
        if let (Some(code), Some(name)) = (
            n.get(1)
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<i64>().ok()),
            n.get(2).and_then(|x| x.as_str()),
        ) {
            name_to_code.insert(name.to_string(), code);
            max_code = max_code.max(code);
        }
    }
    let mut edits: Vec<SexpEdit> = Vec::new();
    if !plan.nets_to_add.is_empty() {
        let mut table_additions = String::new();
        for name in &plan.nets_to_add {
            max_code += 1;
            name_to_code.insert(name.clone(), max_code);
            table_additions.push_str(&format!(
                "\n  (net {} \"{}\")",
                max_code,
                escape_sexp_str(name)
            ));
        }
        let close_pos = content.rfind(')').unwrap_or(content.len());
        edits.push(SexpEdit::insert(close_pos, table_additions));
    }

    // 4. Pad net rewrites — one edit per changed pad, format-preserving
    //    (coded `(net <code> "name")` vs name-only `(net "name")`).
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    for ch in &plan.pad_net_changes {
        let Some((fp_start, fp_end)) = locate_fp_block(&content, &ch.reference) else {
            skipped.push(json!({ "reference": ch.reference, "pad": ch.pad, "reason": "footprint block not found" }));
            continue;
        };
        let pad_pat = format!("(pad \"{}\" ", ch.pad);
        let Some(pad_rel) = content[fp_start..fp_end].find(&pad_pat) else {
            skipped.push(
                json!({ "reference": ch.reference, "pad": ch.pad, "reason": "pad not found" }),
            );
            continue;
        };
        let pad_pos = fp_start + pad_rel;
        let Some((pad_start, pad_end)) = find_balanced_block(&content, pad_pos) else {
            skipped.push(json!({ "reference": ch.reference, "pad": ch.pad, "reason": "unbalanced pad block" }));
            continue;
        };

        // Existing (net ...) on this pad, if any, and whether it's coded.
        let existing = content[pad_start..pad_end].find("(net ").and_then(|rel| {
            let abs = pad_start + rel;
            find_balanced_block(&content, abs).map(|(s, e)| (s, e))
        });
        let coded = match existing {
            Some((s, e)) => content[s..e]
                .trim_start()
                .trim_start_matches("(net")
                .trim_start()
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit()),
            None => board.uses_codes,
        };

        let new_net = if coded {
            match name_to_code.get(&ch.new) {
                Some(code) => format!("(net {} \"{}\")", code, escape_sexp_str(&ch.new)),
                None => {
                    skipped.push(json!({ "reference": ch.reference, "pad": ch.pad, "reason": format!("net '{}' has no code", ch.new) }));
                    continue;
                }
            }
        } else {
            format!("(net \"{}\")", escape_sexp_str(&ch.new))
        };

        match existing {
            Some((s, e)) => edits.push(SexpEdit::replace(s, e, new_net)),
            None => edits.push(SexpEdit::insert(pad_end - 1, format!(" {new_net}"))),
        }
    }

    // 5. Value rewrites — replace the quoted string in the footprint's
    //    (property "Value" "...") node.
    for ch in &plan.value_changes {
        let Some((fp_start, fp_end)) = locate_fp_block(&content, &ch.reference) else {
            skipped.push(json!({ "reference": ch.reference, "field": "Value", "reason": "footprint block not found" }));
            continue;
        };
        let val_pat = "(property \"Value\" \"";
        let Some(rel) = content[fp_start..fp_end].find(val_pat) else {
            skipped.push(json!({ "reference": ch.reference, "field": "Value", "reason": "Value property not found" }));
            continue;
        };
        let val_open = fp_start + rel + val_pat.len();
        // Old value runs to the next unescaped quote.
        let Some(close_rel) = content[val_open..fp_end].find('"') else {
            skipped.push(json!({ "reference": ch.reference, "field": "Value", "reason": "unterminated Value string" }));
            continue;
        };
        let val_close = val_open + close_rel;
        edits.push(SexpEdit::replace(
            val_open,
            val_close,
            escape_sexp_str(&ch.new),
        ));
    }

    let applied_pad_nets =
        plan.pad_net_changes.len() - skipped.iter().filter(|s| s["pad"].is_string()).count();
    let applied_values =
        plan.value_changes.len() - skipped.iter().filter(|s| s["field"].is_string()).count();

    let new_content = apply_edits(content, edits);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "board": board_path.display().to_string(),
        "dry_run": false,
        "applied": {
            "pad_nets": applied_pad_nets,
            "values": applied_values,
            "nets_added": plan.nets_to_add.len(),
        },
        "skipped": skipped,
        "plan": report,
        "note": "Applied to the .kicad_pcb. Copper was not moved — re-run DRC to verify. Warnings (missing/extra/refootprint components) were not applied. If KiCAD has the board open, revert/reload it."
    })))
}

async fn handle_get_component_list(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let fps = ipc!(ctx, |c| c.list_footprints());
    let items: Vec<serde_json::Value> = fps
        .iter()
        .map(|fp| {
            json!({
                "reference": fp.reference,
                "value": fp.value,
                "footprint": fp.footprint,
                "x": fp.position.x, "y": fp.position.y,
                "rotation": fp.rotation, "layer": fp.layer
            })
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "components": items }),
    ))
}

async fn handle_place_array(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let start_x = match require_f64(args, "start_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let start_y = match require_f64(args, "start_y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let count_x = args["count_x"].as_u64().unwrap_or(1) as usize;
    let count_y = args["count_y"].as_u64().unwrap_or(1) as usize;
    let spacing_x = match require_f64(args, "spacing_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let spacing_y = args["spacing_y"].as_f64().unwrap_or(spacing_x);
    let prefix = args["ref_prefix"].as_str().unwrap_or("U").to_string();
    let ref_start = args["ref_start"].as_u64().unwrap_or(1) as usize;

    let mut placed = Vec::new();
    let mut n = ref_start;
    for row in 0..count_y {
        for col in 0..count_x {
            let x = start_x + col as f64 * spacing_x;
            let y = start_y + row as f64 * spacing_y;
            let reference = format!("{prefix}{n}");
            let fp_id = footprint.clone();
            let ref2 = reference.clone();
            match with_ipc(ctx.config.ipc_address.clone(), move |c| {
                c.place_footprint(&fp_id, x, y, 0.0, "F.Cu")
            })
            .await?
            {
                Ok(fp) => placed
                    .push(json!({ "reference": ref2, "x": fp.position.x, "y": fp.position.y })),
                Err(e) => {
                    return Ok(CallToolResult::error(format!(
                        "IPC error placing {}: {}",
                        reference, e
                    )))
                }
            }
            n += 1;
        }
    }
    Ok(CallToolResult::json(
        &json!({ "placed_count": placed.len(), "components": placed }),
    ))
}

async fn handle_align_components(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let refs = args["references"].as_array().cloned().unwrap_or_default();
    let axis = args["axis"].as_str().unwrap_or("x").to_string();
    let value = match require_f64(args, "value") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut aligned = Vec::new();
    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r.to_string(),
            None => continue,
        };
        let ref2 = reference.clone();
        let axis_clone = axis.clone();
        let res = with_ipc(ctx.config.ipc_address.clone(), move |c| {
            let fp = c
                .get_footprint(&ref2)?
                .ok_or_else(|| anyhow::anyhow!("not found"))?;
            let (nx, ny) = if axis_clone == "y" {
                (fp.position.x, value)
            } else {
                (value, fp.position.y)
            };
            c.move_footprint(&ref2, nx, ny)?;
            Ok((nx, ny))
        })
        .await?;
        match res {
            Ok((nx, ny)) => aligned.push(json!({ "reference": reference, "x": nx, "y": ny })),
            Err(e) => return Ok(CallToolResult::error(format!("IPC error: {}", e))),
        }
    }
    Ok(CallToolResult::json(
        &json!({ "aligned_count": aligned.len(), "components": aligned }),
    ))
}

async fn handle_duplicate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let _new_reference = match require_str(args, "new_reference") {
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

    // Get the source footprint's footprint ID and rotation
    let ref_ipc = reference.clone();
    let src = ipc!(ctx, |c| {
        c.get_footprint(&ref_ipc)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc))
    });

    let fp = ipc!(ctx, |c| c.place_footprint(
        &src.footprint,
        x,
        y,
        src.rotation,
        &src.layer
    ));
    Ok(CallToolResult::json(&json!({
        "duplicated_from": reference,
        "new_reference": fp.reference,
        "x": fp.position.x, "y": fp.position.y
    })))
}

async fn handle_get_board_2d_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    use base64::Engine;
    let board_path = get_path(args, "board")?;
    let layers: Vec<String> = args["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "F.Cu".into(),
                "B.Cu".into(),
                "F.SilkS".into(),
                "B.SilkS".into(),
                "Edge.Cuts".into(),
            ]
        });

    let tmp = board_path.with_extension("render.png");
    let layer_refs: Vec<&str> = layers.iter().map(String::as_str).collect();
    super::cli::render_pcb_png(&ctx.config.kicad_cli, &board_path, &tmp, &layer_refs).await?;
    let bytes = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(CallToolResult::image(b64, "image/png"))
}

#[cfg(test)]
mod update_from_schematic_tests {
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

    fn body(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        }
    }

    /// End-to-end apply on a synthetic name-only board: a dry run reports the
    /// stale pad and value, then dry_run=false rewrites both in the file.
    #[tokio::test]
    async fn dry_run_then_apply_fixes_pad_net_and_value() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("b.kicad_pcb");
        let netlist = dir.path().join("b.net");
        std::fs::write(
            &board,
            r#"(kicad_pcb
  (footprint "R_0402" (layer "F.Cu")
    (property "Reference" "R1" (at 0 0))
    (property "Value" "1k" (at 0 0))
    (pad "1" smd roundrect (at 0 0) (net "OLD"))
    (pad "2" smd roundrect (at 1 0) (net "GND")))
)
"#,
        )
        .unwrap();
        std::fs::write(
            &netlist,
            r#"(export (components
                 (comp (ref "R1") (value "10k") (footprint "R_0402")))
               (nets
                 (net (code "1") (name "VCC") (node (ref "R1") (pin "1")))
                 (net (code "2") (name "GND") (node (ref "R1") (pin "2")))))"#,
        )
        .unwrap();

        let ctx = test_ctx();

        // Dry run: reports 1 pad net + 1 value change, writes nothing.
        let dry = handle_update_pcb_from_schematic(
            &json!({ "board": board.to_str().unwrap(), "netlist": netlist.to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        let d = body(&dry);
        assert_eq!(d["dry_run"], json!(true));
        assert_eq!(d["would_change"]["pad_nets"], json!(1));
        assert_eq!(d["would_change"]["values"], json!(1));
        assert!(std::fs::read_to_string(&board)
            .unwrap()
            .contains("(net \"OLD\")"));

        // Apply.
        let applied = handle_update_pcb_from_schematic(
            &json!({ "board": board.to_str().unwrap(), "netlist": netlist.to_str().unwrap(), "dry_run": false }),
            &ctx,
        )
        .await
        .unwrap();
        let a = body(&applied);
        assert_eq!(a["applied"]["pad_nets"], json!(1));
        assert_eq!(a["applied"]["values"], json!(1));

        let updated = std::fs::read_to_string(&board).unwrap();
        assert!(
            updated.contains("(net \"VCC\")"),
            "pad1 should be on VCC now"
        );
        assert!(!updated.contains("(net \"OLD\")"), "stale OLD net gone");
        assert!(
            updated.contains("\"Value\" \"10k\""),
            "value updated to 10k"
        );
        // Name-only board: no numeric net table introduced.
        assert!(!updated.contains("(net 1 "), "must stay name-only");
    }

    /// Diagnostic: print the dry-run diff for a real board + netlist without
    /// writing anything. Skipped unless TEST_PCB and TEST_NETLIST are set.
    #[tokio::test]
    async fn live_dry_run_report() {
        let (Ok(pcb), Ok(nl)) = (std::env::var("TEST_PCB"), std::env::var("TEST_NETLIST")) else {
            eprintln!("SKIP: set TEST_PCB and TEST_NETLIST to run the live report");
            return;
        };
        let ctx = test_ctx();
        let r = body(
            &handle_update_pcb_from_schematic(&json!({ "board": pcb, "netlist": nl }), &ctx)
                .await
                .unwrap(),
        );
        eprintln!("LIVE dry run: {}", serde_json::to_string_pretty(&r).unwrap());
    }

    /// Live check against a real board + pre-exported netlist. Skipped unless
    /// TEST_PCB and TEST_NETLIST are set; applies to a temp copy and asserts the
    /// board carries no more pad-net changes on a second pass (idempotent).
    #[tokio::test]
    async fn live_apply_is_idempotent() {
        let (Ok(pcb), Ok(nl)) = (std::env::var("TEST_PCB"), std::env::var("TEST_NETLIST")) else {
            eprintln!("SKIP: set TEST_PCB and TEST_NETLIST to run the live sync test");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // Persist the applied board to TEST_OUT if set (so an external DRC parity
        // check can inspect it); otherwise keep it in the temp dir.
        let board = match std::env::var("TEST_OUT") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => dir.path().join("live.kicad_pcb"),
        };
        std::fs::copy(&pcb, &board).unwrap();
        let ctx = test_ctx();

        let first = body(
            &handle_update_pcb_from_schematic(
                &json!({ "board": board.to_str().unwrap(), "netlist": nl, "dry_run": false }),
                &ctx,
            )
            .await
            .unwrap(),
        );
        eprintln!(
            "LIVE first apply: {}",
            serde_json::to_string_pretty(&first).unwrap()
        );

        let second = body(
            &handle_update_pcb_from_schematic(
                &json!({ "board": board.to_str().unwrap(), "netlist": nl }),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            second["would_change"]["pad_nets"],
            json!(0),
            "after applying, a dry run must find no more pad-net changes"
        );
        assert_eq!(second["would_change"]["values"], json!(0));
    }
}
