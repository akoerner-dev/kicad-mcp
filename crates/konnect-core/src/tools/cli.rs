//! kicad-cli subprocess wrapper for KiCAD 10.
//!
//! All exports, ERC, DRC, and annotation operations shell out to kicad-cli.
//! This module provides a typed interface to those commands.
//!
//! VERIFIED against: kicad-cli from KiCAD 10.0 (C:\Program Files\KiCad\10.0\bin\kicad-cli.exe)
//! Commands validated: sch erc, sch export (bom/netlist/pdf/svg), pcb drc,
//!   pcb export (gerbers/drill/pdf/svg/step/vrml/pos/ipcd356/dxf/gencad/ipc2581/odb),
//!   pcb render

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Extended timeout for long operations (export, ERC, DRC).
const LONG_TIMEOUT: Duration = Duration::from_secs(600);

// ─── Result Types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErcViolation {
    pub severity: String,
    pub description: String,
    /// Sheet the violation was reported under, taken from the enclosing
    /// `sheets[]` entry's `path` (e.g. `"/"` for the root sheet). The violation
    /// object itself carries no sheet field.
    pub sheet: Option<String>,
    /// KiCAD's machine-readable violation type, e.g. `"lib_symbol_mismatch"`,
    /// `"pin_not_connected"`. Empty when the report omits it.
    pub rule_type: String,
    /// Convenience: position of the first referenced item that carries one
    /// (units per the report's `coordinate_units`, normally mm). `None` when no
    /// item has a position.
    pub pos: Option<ReportPos>,
    /// Every object the violation references, each with its own position — the
    /// symbol, pin or label a caller needs to go look at.
    pub items: Vec<ReportItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportPos {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrcViolation {
    pub severity: String,
    pub description: String,
    /// Which report section this came from: "rule" (geometric/clearance),
    /// "unconnected" (ratsnest / copper connectivity) or "parity"
    /// (board-vs-schematic netlist mismatch).
    pub kind: String,
    /// KiCAD's machine-readable violation type, e.g. "shorting_items",
    /// "silk_over_copper", "lib_footprint_issues". Empty when the report omits it.
    pub rule_type: String,
    /// Convenience: position of the first referenced item that carries one
    /// (units per the report's `coordinate_units`, normally mm). `None` when no
    /// item has a position.
    pub pos: Option<ReportPos>,
    /// Every object the violation references, each with its own position. A
    /// short, for example, lists the two colliding items at their two locations,
    /// which is what a caller needs to actually go fix it.
    pub items: Vec<ReportItem>,
}

/// A single object referenced by a violation — a pad, track or footprint in a
/// DRC report, a symbol, pin or label in an ERC report. Both reports use the
/// same item shape, so both parsers share this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportItem {
    pub description: String,
    pub pos: Option<ReportPos>,
    pub uuid: Option<String>,
}

// ─── KiCAD CLI Runner ─────────────────────────────────────────────────────────

/// Run a kicad-cli command with arguments and capture stdout.
async fn run_cli(cli: &str, args: &[&str], timeout_dur: Duration) -> Result<String> {
    info!("[BETA] kicad-cli {} {}", cli, args.join(" "));

    let mut cmd = Command::new(cli);
    cmd.args(args)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn kicad-cli: {}", cli))?;

    let output = timeout(timeout_dur, child.wait_with_output())
        .await
        .with_context(|| format!("kicad-cli timed out after {:?}", timeout_dur))?
        .with_context(|| "kicad-cli process failed")?;

    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            if line.contains("Error") || line.contains("error") {
                warn!("[BETA] kicad-cli: {}", line);
            } else {
                debug!("[BETA] kicad-cli stderr: {}", line);
            }
        }
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "kicad-cli exited with {}: {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ─── ERC ─────────────────────────────────────────────────────────────────────

/// Run ERC on a schematic and return parsed violations.
/// KiCAD 10: `sch erc --output <path> --format json <input>`
pub async fn run_erc(cli: &str, schematic: &Path) -> Result<Vec<ErcViolation>> {
    let out_path = schematic.with_extension("erc.json");
    let args = [
        "sch",
        "erc",
        "--output",
        out_path.to_str().unwrap(),
        "--format",
        "json",
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;

    let json_str = tokio::fs::read_to_string(&out_path)
        .await
        .context("ERC output file not found")?;
    let raw: serde_json::Value = serde_json::from_str(&json_str)?;

    let violations = parse_erc_json(&raw);
    let _ = tokio::fs::remove_file(&out_path).await;
    Ok(violations)
}

/// Parse the kicad-cli ERC JSON report.
///
/// KiCAD nests ERC violations one level deeper than DRC violations: the report
/// carries **no top-level `violations` array**. Every violation sits under
/// `sheets[].violations`, and the enclosing sheet's `path` is the only place the
/// sheet name appears. Reading the top level — which is where the *DRC* report
/// keeps its array — matches nothing and reports every schematic as clean.
///
/// A top-level `violations` array is still folded in when present, so a
/// flattened or hand-assembled report keeps working.
fn parse_erc_json(raw: &serde_json::Value) -> Vec<ErcViolation> {
    let mut out = Vec::new();

    if let Some(arr) = raw.get("violations").and_then(|v| v.as_array()) {
        out.extend(arr.iter().map(|v| parse_erc_violation(v, None)));
    }

    if let Some(sheets) = raw.get("sheets").and_then(|v| v.as_array()) {
        for sheet in sheets {
            let sheet_path = sheet["path"].as_str();
            if let Some(arr) = sheet.get("violations").and_then(|v| v.as_array()) {
                out.extend(arr.iter().map(|v| parse_erc_violation(v, sheet_path)));
            }
        }
    }

    out
}

/// Build one `ErcViolation`. `sheet_path` is the `path` of the enclosing
/// `sheets[]` entry, used only when the violation carries no `sheet` of its own.
fn parse_erc_violation(v: &serde_json::Value, sheet_path: Option<&str>) -> ErcViolation {
    let (items, pos) = parse_report_items(v);
    ErcViolation {
        severity: v["severity"].as_str().unwrap_or("error").to_string(),
        description: v["description"].as_str().unwrap_or("").to_string(),
        sheet: v["sheet"].as_str().or(sheet_path).map(String::from),
        rule_type: v["type"].as_str().unwrap_or("").to_string(),
        pos,
        items,
    }
}

/// Collect the objects a violation references, plus the position that best
/// represents the violation as a whole.
///
/// Both the ERC and the DRC report nest coordinates inside each referenced item
/// rather than on the violation itself, so the headline position is the first
/// item that has one; a violation-level `pos` is honoured as a fallback.
fn parse_report_items(v: &serde_json::Value) -> (Vec<ReportItem>, Option<ReportPos>) {
    let read_pos = |value: &serde_json::Value| -> Option<ReportPos> {
        let p = value.get("pos")?;
        Some(ReportPos {
            x: p["x"].as_f64()?,
            y: p["y"].as_f64()?,
        })
    };

    let items: Vec<ReportItem> = v
        .get("items")
        .and_then(|i| i.as_array())
        .map(|arr| {
            arr.iter()
                .map(|it| ReportItem {
                    description: it["description"].as_str().unwrap_or("").to_string(),
                    pos: read_pos(it),
                    uuid: it["uuid"].as_str().map(String::from),
                })
                .collect()
        })
        .unwrap_or_default();

    let pos = items
        .iter()
        .find_map(|it| it.pos.clone())
        .or_else(|| read_pos(v));
    (items, pos)
}

// ─── DRC ─────────────────────────────────────────────────────────────────────

/// Run DRC on a PCB and return parsed violations from ALL report sections.
///
/// KiCAD 10 emits three arrays in the JSON report: `violations` (geometric /
/// clearance rules), `unconnected_items` (ratsnest / copper connectivity) and
/// `schematic_parity` (board-vs-schematic netlist mismatch). The stock tool
/// only read `violations`; we fold in the other two so callers can assert
/// "0 unconnected, parity clean" (the MVP acceptance criterion).
///
/// `unconnected_items` is always present in the report; `schematic_parity` is
/// only populated when `--schematic-parity` is passed, which requires the
/// sibling `.kicad_sch` next to the board.
///
/// `pcb drc --output <path> --format json [--schematic-parity] [--refill-zones] <input>`
pub async fn run_drc(
    cli: &str,
    pcb: &Path,
    refill_zones: bool,
    schematic_parity: bool,
) -> Result<Vec<DrcViolation>> {
    let out_path = pcb.with_extension("drc.json");
    let mut args = vec![
        "pcb",
        "drc",
        "--output",
        out_path.to_str().unwrap(),
        "--format",
        "json",
    ];
    if schematic_parity {
        args.push("--schematic-parity");
    }
    if refill_zones {
        args.push("--refill-zones");
    }
    args.push(pcb.to_str().unwrap());
    run_cli(cli, &args, LONG_TIMEOUT).await?;

    let json_str = tokio::fs::read_to_string(&out_path)
        .await
        .context("DRC output file not found")?;
    let raw: serde_json::Value = serde_json::from_str(&json_str)?;
    let _ = tokio::fs::remove_file(&out_path).await;

    let mut out = Vec::new();
    // Geometric / clearance rules — severity is carried in the report.
    parse_drc_section(&raw, "violations", "rule", "error", &mut out);
    // Connectivity — unconnected ratsnest ends; KiCAD treats these as errors.
    parse_drc_section(&raw, "unconnected_items", "unconnected", "error", &mut out);
    // Board-vs-schematic parity — only present when --schematic-parity was set.
    parse_drc_section(&raw, "schematic_parity", "parity", "error", &mut out);
    Ok(out)
}

/// Fold one section of the kicad-cli DRC JSON report into `out`, tagging each
/// entry with `kind` and falling back to `default_severity` when the report
/// omits a severity (unconnected/parity entries sometimes do).
fn parse_drc_section(
    raw: &serde_json::Value,
    section: &str,
    kind: &str,
    default_severity: &str,
    out: &mut Vec<DrcViolation>,
) {
    let Some(arr) = raw.get(section).and_then(|v| v.as_array()) else {
        return;
    };
    for v in arr {
        let (items, pos) = parse_report_items(v);
        out.push(DrcViolation {
            severity: v["severity"]
                .as_str()
                .unwrap_or(default_severity)
                .to_string(),
            description: v["description"].as_str().unwrap_or("").to_string(),
            kind: kind.to_string(),
            rule_type: v["type"].as_str().unwrap_or("").to_string(),
            pos,
            items,
        });
    }
}

// ─── Annotation ───────────────────────────────────────────────────────────────

/// KiCAD 10: `sch annotate` is NOT in the CLI.
/// We implement annotation ourselves by parsing the schematic and assigning
/// sequential reference designators to unannotated symbols (those with "?" suffix).
pub async fn annotate_schematic(_cli: &str, schematic: &Path) -> Result<()> {
    use std::collections::HashMap;

    let content = tokio::fs::read_to_string(schematic).await?;
    let mut new_content = content.clone();
    let mut counters: HashMap<String, usize> = HashMap::new();

    // First pass: find all existing numbered references to avoid conflicts
    let mut pos = 0;
    while let Some(ref_pos) = new_content[pos..].find("(reference \"") {
        let abs = pos + ref_pos + 12;
        if let Some(end) = new_content[abs..].find('"') {
            let reference = &new_content[abs..abs + end];
            // Extract prefix and number: "R1" → ("R", 1)
            let prefix: String = reference
                .chars()
                .take_while(|c| c.is_alphabetic() || *c == '#')
                .collect();
            let num_str: String = reference.chars().skip(prefix.len()).collect();
            if let Ok(num) = num_str.parse::<usize>() {
                let counter = counters.entry(prefix).or_insert(0);
                if num >= *counter {
                    *counter = num + 1;
                }
            }
        }
        pos = abs + 1;
    }

    // Second pass: replace "?" references with sequential numbers
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    pos = 0;
    while let Some(ref_pos) = new_content[pos..].find("(reference \"") {
        let abs = pos + ref_pos + 12;
        if let Some(end) = new_content[abs..].find('"') {
            let reference = &new_content[abs..abs + end];
            if reference.ends_with('?') {
                let prefix = reference.trim_end_matches('?').to_string();
                let counter = counters.entry(prefix.clone()).or_insert(1);
                let new_ref = format!("{}{}", prefix, counter);
                *counter += 1;
                replacements.push((abs, abs + end, new_ref));
            }
        }
        pos = abs + 1;
    }

    // Apply replacements in reverse order to preserve offsets
    for (start, end, new_ref) in replacements.into_iter().rev() {
        new_content.replace_range(start..end, &new_ref);
    }

    if new_content != content {
        tokio::fs::write(schematic, &new_content).await?;
    }

    Ok(())
}

// ─── Schematic Export ────────────────────────────────────────────────────────

/// KiCAD 10: `sch export svg --output <dir> <input>`
pub async fn export_schematic_svg(
    cli: &str,
    schematic: &Path,
    output_dir: &Path,
) -> Result<PathBuf> {
    let args = [
        "sch",
        "export",
        "svg",
        "--output",
        output_dir.to_str().unwrap(),
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    let stem = schematic.file_stem().unwrap_or_default().to_string_lossy();
    Ok(output_dir.join(format!("{}.svg", stem)))
}

/// KiCAD 10: `sch export pdf --output <path> <input>`
pub async fn export_schematic_pdf(cli: &str, schematic: &Path, output: &Path) -> Result<()> {
    let args = [
        "sch",
        "export",
        "pdf",
        "--output",
        output.to_str().unwrap(),
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `sch export bom --output <path> <input>`
/// Note: v10 BOM does NOT use --format. It uses --fields, --labels, --field-delimiter.
/// Default output is CSV-like with Reference,Value,Footprint,Qty,DNP fields.
pub async fn export_bom(cli: &str, schematic: &Path, output: &Path, _format: &str) -> Result<()> {
    let args = [
        "sch",
        "export",
        "bom",
        "--output",
        output.to_str().unwrap(),
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `sch export netlist --output <path> --format <fmt> <input>`
/// Valid formats: kicadsexpr, kicadxml, cadstar, orcadpcb2, spice, spicemodel, pads, allegro
pub async fn export_netlist(
    cli: &str,
    schematic: &Path,
    output: &Path,
    format: &str,
) -> Result<()> {
    // Map friendly names to v10 format values
    let lower = format.to_lowercase();
    let v10_format = match lower.as_str() {
        "kicad" | "kicadsexpr" | "sexp" => "kicadsexpr",
        "xml" | "kicadxml" => "kicadxml",
        "spice" => "spice",
        "cadstar" => "cadstar",
        "orcad" | "orcadpcb2" => "orcadpcb2",
        "pads" => "pads",
        "allegro" => "allegro",
        _ => &lower,
    };
    let args = [
        "sch",
        "export",
        "netlist",
        "--output",
        output.to_str().unwrap(),
        "--format",
        v10_format,
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

// ─── PCB Export ──────────────────────────────────────────────────────────────

/// KiCAD 10: `pcb export gerbers --output <dir> <input>` (PLURAL!)
pub async fn export_gerber(cli: &str, pcb: &Path, output_dir: &Path) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "gerbers",
        "--output",
        output_dir.to_str().unwrap(),
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export drill --output <dir> <input>`
pub async fn export_drill(cli: &str, pcb: &Path, output: &Path) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "drill",
        "--output",
        output.to_str().unwrap(),
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export pdf --output <path> [--layers <layer>]... <input>`
pub async fn export_pdf(cli: &str, pcb: &Path, output: &Path, layers: &[&str]) -> Result<()> {
    let mut args = vec!["pcb", "export", "pdf", "--output", output.to_str().unwrap()];
    for layer in layers {
        args.push("--layers");
        args.push(layer);
    }
    args.push(pcb.to_str().unwrap());
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export svg --output <path> [--layers <layer>]... <input>`
pub async fn export_svg_pcb(cli: &str, pcb: &Path, output: &Path, layers: &[&str]) -> Result<()> {
    let mut args = vec!["pcb", "export", "svg", "--output", output.to_str().unwrap()];
    for layer in layers {
        args.push("--layers");
        args.push(layer);
    }
    args.push(pcb.to_str().unwrap());
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export <format> --output <path> <input>`
/// Supported 3D formats: step, vrml, glb, brep, stl, ply, stpz, u3d, xao, 3dpdf
pub async fn export_3d(cli: &str, pcb: &Path, output: &Path, format: &str) -> Result<()> {
    let subcommand = match format.to_lowercase().as_str() {
        "step" | "stp" => "step",
        "vrml" | "wrl" => "vrml",
        "glb" | "gltf" => "glb",
        "brep" => "brep",
        "stl" => "stl",
        "ply" => "ply",
        "stpz" => "stpz",
        "u3d" => "u3d",
        "xao" => "xao",
        "3dpdf" | "pdf3d" => "3dpdf",
        other => anyhow::bail!(
            "Unsupported 3D format: '{}'. Supported: step, vrml, glb, brep, stl, ply, stpz, u3d, xao, 3dpdf",
            other
        ),
    };
    let args = vec![
        "pcb",
        "export",
        subcommand,
        "--output",
        output.to_str().unwrap(),
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export pos --output <path> --format <fmt> <input>`
/// Formats: ascii (default), csv, gerber
pub async fn export_position_file(
    cli: &str,
    pcb: &Path,
    output: &Path,
    format: &str,
) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "pos",
        "--output",
        output.to_str().unwrap(),
        "--format",
        format,
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export ipcd356 --output <path> <input>`
pub async fn export_ipcd356(cli: &str, pcb: &Path, output: &Path) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "ipcd356",
        "--output",
        output.to_str().unwrap(),
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export dxf --output <dir> [--layers <csv>] --mode-multi <input>`
///
/// Unlike `pdf`/`svg`, DXF's `--layers` takes a single comma-separated value
/// rather than a repeatable flag, and one file per requested layer is written
/// into `output_dir` (verified against KiCAD 10.0).
pub async fn export_dxf(cli: &str, pcb: &Path, output_dir: &Path, layers: &[&str]) -> Result<()> {
    let output_str = output_dir.to_str().unwrap();
    let pcb_str = pcb.to_str().unwrap();
    let layers_csv = layers.join(",");

    let mut args: Vec<&str> = vec!["pcb", "export", "dxf", "--output", output_str];
    if !layers_csv.is_empty() {
        args.push("--layers");
        args.push(&layers_csv);
    }
    args.push("--mode-multi");
    args.push(pcb_str);

    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export gencad --output <path> <input>`
pub async fn export_gencad(cli: &str, pcb: &Path, output: &Path) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "gencad",
        "--output",
        output.to_str().unwrap(),
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export ipc2581 --output <path> --units <mm|in> [--compress] <input>`
pub async fn export_ipc2581(
    cli: &str,
    pcb: &Path,
    output: &Path,
    units: &str,
    compress: bool,
) -> Result<()> {
    let output_str = output.to_str().unwrap();
    let pcb_str = pcb.to_str().unwrap();

    let mut args: Vec<&str> = vec![
        "pcb", "export", "ipc2581", "--output", output_str, "--units", units,
    ];
    if compress {
        args.push("--compress");
    }
    args.push(pcb_str);

    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export odb --output <path> --units <mm|in> --compression <mode> <input>`
/// Compression modes (verified against KiCAD 10.0): `zip`, `none`, `tgz`.
pub async fn export_odb(
    cli: &str,
    pcb: &Path,
    output: &Path,
    units: &str,
    compression: &str,
) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "odb",
        "--output",
        output.to_str().unwrap(),
        "--units",
        units,
        "--compression",
        compression,
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

// ─── Render to image ─────────────────────────────────────────────────────────

/// Render schematic to SVG (no bitmap export in KiCAD 10 CLI).
/// KiCAD 10: `sch export svg --output <dir> <input>`
pub async fn render_schematic_svg(cli: &str, schematic: &Path, output: &Path) -> Result<PathBuf> {
    let output_dir = output.parent().unwrap_or(Path::new("."));
    export_schematic_svg(cli, schematic, output_dir).await
}

/// KiCAD 10: `pcb render --output <path> [--layers <layer>]... <input>`
pub async fn render_pcb_png(cli: &str, pcb: &Path, output: &Path, layers: &[&str]) -> Result<()> {
    let mut args = vec!["pcb", "render", "--output", output.to_str().unwrap()];
    for layer in layers {
        args.push("--layers");
        args.push(layer);
    }
    args.push(pcb.to_str().unwrap());
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

#[cfg(test)]
mod erc_report_tests {
    use super::*;
    use serde_json::json;

    /// Verbatim shape of a `kicad-cli sch erc --format json` report (KiCAD
    /// 10.0.1), reduced to one sheet and one violation. Deliberately carries **no**
    /// top-level `violations` key — that is exactly what the report lacks and what
    /// the old parser was looking for.
    fn real_report() -> serde_json::Value {
        json!({
            "$schema": "https://schemas.kicad.org/erc.v1.json",
            "coordinate_units": "mm",
            "date": "2026-08-16T07:41:00+0200",
            "ignored_checks": [],
            "included_severities": ["error", "warning"],
            "kicad_version": "10.0.1",
            "source": "ClearBell_Ausseneinheit.kicad_sch",
            "sheets": [{
                "path": "/",
                "uuid_path": "/a0793de3-2761-4557-8d55-8bdf4486f7f5",
                "violations": [{
                    "description": "Symbol \"LMR33630ADDA\" doesn't match copy in library",
                    "items": [{
                        "description": "Symbol U1 [LMR33630ADDA]",
                        "pos": { "x": 1.3589, "y": 0.9906 },
                        "uuid": "f9f33a6f-a582-4d28-b159-56d4293a0695"
                    }],
                    "severity": "warning",
                    "type": "lib_symbol_mismatch"
                }]
            }]
        })
    }

    #[test]
    fn parse_erc_json_reads_violations_nested_under_sheets() {
        let raw = real_report();

        // Guard the guard: if a future edit adds a top-level `violations` array to
        // this fixture, the regression below would pass for the wrong reason — the
        // exact trap a previous fixture fell into.
        assert!(
            raw.get("violations").is_none(),
            "fixture must not carry a top-level `violations` array"
        );

        let v = parse_erc_json(&raw);
        assert_eq!(v.len(), 1, "the sheets[] violation must be picked up");

        let first = &v[0];
        assert_eq!(first.severity, "warning");
        assert_eq!(first.rule_type, "lib_symbol_mismatch");
        assert!(first.description.contains("LMR33630ADDA"));
        // The sheet name exists only on the enclosing sheets[] entry.
        assert_eq!(first.sheet.as_deref(), Some("/"));
        // Coordinates live in items[], never on the violation itself.
        let pos = first.pos.as_ref().expect("headline pos from the first item");
        assert_eq!((pos.x, pos.y), (1.3589, 0.9906));
        assert_eq!(first.items.len(), 1);
        assert_eq!(
            first.items[0].uuid.as_deref(),
            Some("f9f33a6f-a582-4d28-b159-56d4293a0695")
        );
    }

    #[test]
    fn parse_erc_json_attributes_each_violation_to_its_own_sheet() {
        let raw = json!({
            "sheets": [
                { "path": "/", "violations": [{ "severity": "error", "description": "root" }] },
                { "path": "/Power/", "violations": [{ "severity": "warning", "description": "sub" }] }
            ]
        });

        let v = parse_erc_json(&raw);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].sheet.as_deref(), Some("/"));
        assert_eq!(v[1].sheet.as_deref(), Some("/Power/"));
        // No items[] at all must not panic and must leave pos empty.
        assert!(v[0].pos.is_none() && v[0].items.is_empty());
    }

    #[test]
    fn parse_erc_json_reports_a_clean_schematic_as_empty() {
        let raw = json!({ "sheets": [{ "path": "/", "violations": [] }] });
        assert!(parse_erc_json(&raw).is_empty());
    }

    #[test]
    fn parse_erc_json_still_accepts_a_flat_violations_array() {
        let raw = json!({
            "violations": [{
                "severity": "error",
                "description": "flat",
                "type": "pin_not_connected",
                "pos": { "x": 5.0, "y": 6.0 }
            }]
        });

        let v = parse_erc_json(&raw);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule_type, "pin_not_connected");
        // Falls back to the violation-level pos when there are no items.
        let pos = v[0].pos.as_ref().expect("violation-level pos fallback");
        assert_eq!((pos.x, pos.y), (5.0, 6.0));
    }

    /// The DRC path shares `parse_report_items` with ERC since this fix — pin its
    /// behaviour so the shared helper can't regress one report while fixing the other.
    #[test]
    fn parse_drc_section_still_gathers_items_and_headline_pos() {
        let raw = json!({
            "violations": [{
                "severity": "error",
                "description": "Clearance violation",
                "type": "clearance",
                "items": [
                    { "description": "Track [GND]", "uuid": "aaa" },
                    { "description": "Pad 2 [VCC]", "pos": { "x": 12.5, "y": 34.0 }, "uuid": "bbb" }
                ]
            }]
        });

        let mut out = Vec::new();
        parse_drc_section(&raw, "violations", "rule", "error", &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "rule");
        assert_eq!(out[0].rule_type, "clearance");
        assert_eq!(out[0].items.len(), 2);
        assert!(out[0].items[0].pos.is_none());
        // Headline pos skips the item without coordinates.
        let pos = out[0].pos.as_ref().expect("first item that has a position");
        assert_eq!((pos.x, pos.y), (12.5, 34.0));
    }
}

#[cfg(test)]
mod live_drc_tests {
    use super::*;

    /// Live end-to-end check of the patched DRC path against a real board.
    /// Skipped unless TEST_PCB (and optionally KICAD_CLI) env vars are set, so
    /// the normal `cargo test` run stays green without KiCAD installed.
    #[tokio::test]
    async fn live_drc_captures_unconnected_and_parity() {
        let Ok(pcb) = std::env::var("TEST_PCB") else {
            eprintln!("SKIP: set TEST_PCB to run the live DRC test");
            return;
        };
        let cli = std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string());
        let board = std::path::PathBuf::from(pcb);

        let all = run_drc(&cli, &board, false, true)
            .await
            .expect("run_drc with schematic_parity should succeed");

        let count = |k: &str| all.iter().filter(|v| v.kind == k).count();
        let (rule, unconn, parity) = (count("rule"), count("unconnected"), count("parity"));
        eprintln!(
            "LIVE DRC: total={} | rule={} unconnected={} parity={}",
            all.len(),
            rule,
            unconn,
            parity
        );
        for v in &all {
            eprintln!("  [{}/{}] {}", v.kind, v.severity, v.description);
        }

        // The whole point of the patch: the two sections the stock tool dropped
        // must now be present. (This board is known to have both.)
        assert!(unconn > 0, "expected unconnected_items to be captured");
        assert!(parity > 0, "expected schematic_parity to be captured");
        assert_eq!(all.len(), rule + unconn + parity, "every entry must be tagged");
    }

    /// Live end-to-end check of the ERC path against a real schematic that is
    /// known to have violations. Skipped unless TEST_SCH is set.
    ///
    /// Before the sheets[] fix this returned an empty vec for every schematic, so
    /// "found something at all" is the assertion that matters.
    #[tokio::test]
    async fn live_erc_captures_violations_nested_under_sheets() {
        let Ok(sch) = std::env::var("TEST_SCH") else {
            eprintln!("SKIP: set TEST_SCH to run the live ERC test");
            return;
        };
        let cli = std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string());
        let schematic = std::path::PathBuf::from(sch);

        let all = run_erc(&cli, &schematic).await.expect("run_erc should succeed");

        eprintln!("LIVE ERC: total={}", all.len());
        for v in &all {
            eprintln!(
                "  [{}/{}] {} (sheet {:?})",
                v.severity, v.rule_type, v.description, v.sheet
            );
        }

        assert!(
            !all.is_empty(),
            "expected violations from a schematic known to have them — an empty \
             result is the exact symptom of reading the wrong report level"
        );
        assert!(
            all.iter().all(|v| v.sheet.is_some()),
            "every violation must be attributed to its sheet"
        );
    }
}
