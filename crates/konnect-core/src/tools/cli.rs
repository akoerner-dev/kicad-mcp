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
    pub sheet: Option<String>,
    pub pos: Option<ErcPos>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErcPos {
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
    pub pos: Option<ErcPos>,
}

// ─── KiCAD CLI Runner ─────────────────────────────────────────────────────────

/// Run a kicad-cli command with arguments and capture stdout.
async fn run_cli(cli: &str, args: &[&str], timeout_dur: Duration) -> Result<String> {
    info!("[BETA] kicad-cli {} {}", cli, args.join(" "));

    let mut cmd = Command::new(cli);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

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

fn parse_erc_json(raw: &serde_json::Value) -> Vec<ErcViolation> {
    let arr = match raw.get("violations").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };

    arr.iter()
        .map(|v| ErcViolation {
            severity: v["severity"].as_str().unwrap_or("error").to_string(),
            description: v["description"].as_str().unwrap_or("").to_string(),
            sheet: v["sheet"].as_str().map(String::from),
            pos: v.get("pos").and_then(|p| {
                Some(ErcPos {
                    x: p["x"].as_f64()?,
                    y: p["y"].as_f64()?,
                })
            }),
        })
        .collect()
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
        out.push(DrcViolation {
            severity: v["severity"]
                .as_str()
                .unwrap_or(default_severity)
                .to_string(),
            description: v["description"].as_str().unwrap_or("").to_string(),
            kind: kind.to_string(),
            pos: v.get("pos").and_then(|p| {
                Some(ErcPos {
                    x: p["x"].as_f64()?,
                    y: p["y"].as_f64()?,
                })
            }),
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
}
