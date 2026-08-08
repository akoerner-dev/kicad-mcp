//! KiCAD netlist (`kicadsexpr`) parsing and board-vs-schematic diff.
//!
//! This is the read/plan half of the file-based "Update PCB from Schematic"
//! (KiCAD's F8). `kicad-cli sch export netlist --format kicadsexpr` emits the
//! authoritative connectivity from the schematic; [`parse_netlist`] turns that
//! into a per-pad net map, [`parse_board`] models the current `.kicad_pcb`, and
//! [`plan`] diffs them into a concrete, apply-able change set plus warnings for
//! everything the file-only path can't do (add/refootprint components — those
//! need library geometry or IPC).
//!
//! All functions here are pure and operate on strings, so the whole diff is
//! unit-testable without KiCAD installed.

use konnect_sexp::parser::{parse_sexp, SexpNode};
use std::collections::BTreeSet;

// ─── Netlist model ─────────────────────────────────────────────────────────────

/// One component as declared in the schematic netlist.
#[derive(Debug, Clone, PartialEq)]
pub struct NlComp {
    pub reference: String,
    pub value: String,
    pub footprint: String,
}

/// Parsed schematic netlist: components, the desired net of every pad, and the
/// distinct net names (first-seen order).
#[derive(Debug, Clone, Default)]
pub struct Netlist {
    pub comps: Vec<NlComp>,
    /// `(reference, pad)` → desired net name.
    pub pad_nets: std::collections::HashMap<(String, String), String>,
    /// All distinct net names, in first-seen order.
    pub net_names: Vec<String>,
}

/// Parse a `kicadsexpr` netlist into [`Netlist`].
pub fn parse_netlist(content: &str) -> anyhow::Result<Netlist> {
    let tree = parse_sexp(content)?;

    let mut comps = Vec::new();
    if let Some(cn) = tree.find("components") {
        for c in cn.find_all("comp") {
            let reference = c.find_str("ref").unwrap_or("").to_string();
            if reference.is_empty() {
                continue;
            }
            comps.push(NlComp {
                reference,
                value: c.find_str("value").unwrap_or("").to_string(),
                footprint: c.find_str("footprint").unwrap_or("").to_string(),
            });
        }
    }

    let mut pad_nets = std::collections::HashMap::new();
    let mut net_names = Vec::new();
    if let Some(nets_node) = tree.find("nets") {
        for net in nets_node.find_all("net") {
            let name = match net.find_str("name") {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !net_names.contains(&name) {
                net_names.push(name.clone());
            }
            for node in net.find_all("node") {
                let r = node.find_str("ref").unwrap_or("");
                let p = node.find_str("pin").unwrap_or("");
                if r.is_empty() || p.is_empty() {
                    continue;
                }
                pad_nets.insert((r.to_string(), p.to_string()), name.clone());
            }
        }
    }

    Ok(Netlist {
        comps,
        pad_nets,
        net_names,
    })
}

// ─── Board model ───────────────────────────────────────────────────────────────

/// A pad as it currently exists on the board.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardPad {
    pub number: String,
    /// Current net name, or `None` if the pad carries no `(net ...)`.
    pub net: Option<String>,
}

/// A footprint as it currently exists on the board.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardFp {
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub pads: Vec<BoardPad>,
}

/// Modeled `.kicad_pcb`: footprints plus the top-level net table.
#[derive(Debug, Clone, Default)]
pub struct BoardModel {
    pub fps: Vec<BoardFp>,
    /// Does the file store nets with numeric codes (`(net 3 "GND")`)? Some
    /// boards store pad nets by name only (`(net "GND")`) with no top-level
    /// table — the two formats are written differently on apply.
    pub uses_codes: bool,
    /// Existing net names in the top-level net table.
    pub net_table: BTreeSet<String>,
}

/// Read the pad net from a `(net ...)` node: `(net <code> "name")` on coded
/// boards or `(net "name")` on name-only ones — name is at index 2 or, lacking
/// a code, index 1.
fn pad_net_name(pad: &SexpNode) -> Option<String> {
    let net = pad.find("net")?;
    net.get(2)
        .or_else(|| net.get(1))
        .and_then(|n| n.as_str())
        .map(String::from)
}

/// Parse a `.kicad_pcb` into a [`BoardModel`] (footprints + net table).
pub fn parse_board(content: &str) -> anyhow::Result<BoardModel> {
    let tree = parse_sexp(content)?;

    // Top-level net table: `(net <code> "name")`. `find_all` is non-recursive,
    // so this only sees the table, not pad nets.
    let mut net_table = BTreeSet::new();
    let mut uses_codes = false;
    for n in tree.find_all("net") {
        if n.get(2).is_some() {
            uses_codes = true;
            if let Some(name) = n.get(2).and_then(|x| x.as_str()) {
                net_table.insert(name.to_string());
            }
        }
    }

    let mut fps = Vec::new();
    for fp in tree.find_all("footprint") {
        let footprint = fp.get(1).and_then(|n| n.as_str()).unwrap_or("").to_string();
        let mut reference = String::new();
        let mut value = String::new();
        for p in fp.find_all("property") {
            match p.get(1).and_then(|n| n.as_str()) {
                Some("Reference") => {
                    reference = p.get(2).and_then(|n| n.as_str()).unwrap_or("").to_string()
                }
                Some("Value") => {
                    value = p.get(2).and_then(|n| n.as_str()).unwrap_or("").to_string()
                }
                _ => {}
            }
        }
        if reference.is_empty() {
            continue;
        }
        let pads = fp
            .find_all("pad")
            .iter()
            .filter_map(|pad| {
                let number = pad.get(1)?.as_str()?.to_string();
                Some(BoardPad {
                    number,
                    net: pad_net_name(pad),
                })
            })
            .collect();
        fps.push(BoardFp {
            reference,
            value,
            footprint,
            pads,
        });
    }

    Ok(BoardModel {
        fps,
        uses_codes,
        net_table,
    })
}

// ─── Diff / plan ───────────────────────────────────────────────────────────────

/// A single pad whose net must change to match the schematic.
#[derive(Debug, Clone, PartialEq)]
pub struct PadNetChange {
    pub reference: String,
    pub pad: String,
    pub old: Option<String>,
    pub new: String,
}

/// A component whose value on the board differs from the schematic.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueChange {
    pub reference: String,
    pub old: String,
    pub new: String,
}

/// The full board-vs-schematic diff: actionable changes plus warnings for
/// everything the file-only path deliberately does not touch.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Pad net reassignments to apply.
    pub pad_net_changes: Vec<PadNetChange>,
    /// Component value updates to apply.
    pub value_changes: Vec<ValueChange>,
    /// Net names referenced by a change but missing from a coded board's net
    /// table (empty for name-only boards, which need no table). First-seen order.
    pub nets_to_add: Vec<String>,
    /// Components in the schematic but not on the board — need placement
    /// (footprint geometry / IPC), which the file path can't do. Warning only.
    pub missing_on_board: Vec<String>,
    /// Components on the board but not in the schematic — deletion candidates.
    /// Not removed automatically. Warning only.
    pub extra_on_board: Vec<String>,
    /// `(reference, board_footprint, schematic_footprint)` where the footprint
    /// id differs — refootprinting needs new geometry. Warning only.
    pub footprint_mismatches: Vec<(String, String, String)>,
    /// `(reference, pad)` pads that carry a net on the board but have no node
    /// in the schematic netlist. Left untouched. Informational.
    pub unmatched_board_pads: Vec<(String, String)>,
}

impl Plan {
    /// Are there any changes to apply?
    pub fn is_empty(&self) -> bool {
        self.pad_net_changes.is_empty() && self.value_changes.is_empty()
    }
}

/// Diff a parsed schematic netlist against a board model.
pub fn plan(nl: &Netlist, board: &BoardModel) -> Plan {
    let mut out = Plan::default();

    let board_refs: BTreeSet<&str> = board.fps.iter().map(|f| f.reference.as_str()).collect();
    let comp_by_ref: std::collections::HashMap<&str, &NlComp> =
        nl.comps.iter().map(|c| (c.reference.as_str(), c)).collect();

    // Track nets we will write that aren't already in a coded net table, in
    // first-seen order, deduplicated.
    let mut nets_seen: BTreeSet<String> = BTreeSet::new();

    for fp in &board.fps {
        let Some(comp) = comp_by_ref.get(fp.reference.as_str()) else {
            out.extra_on_board.push(fp.reference.clone());
            continue;
        };

        if !comp.value.is_empty() && comp.value != fp.value {
            out.value_changes.push(ValueChange {
                reference: fp.reference.clone(),
                old: fp.value.clone(),
                new: comp.value.clone(),
            });
        }

        if !comp.footprint.is_empty() && comp.footprint != fp.footprint {
            out.footprint_mismatches.push((
                fp.reference.clone(),
                fp.footprint.clone(),
                comp.footprint.clone(),
            ));
        }

        for pad in &fp.pads {
            let key = (fp.reference.clone(), pad.number.clone());
            match nl.pad_nets.get(&key) {
                // KiCad auto-generates a UNIQUE `unconnected-(...)` net for every
                // no-connect pad so a lone pad counts as trivially routed. The
                // exported netlist collapses these onto one shared name; forcing
                // the board to match would merge several no-connect pads onto one
                // net with no copper between them, manufacturing phantom ratsnest
                // (unconnected) items. KiCad manages these itself — never sync to
                // an `unconnected-*` net.
                Some(desired) if desired.starts_with("unconnected-") => {}
                Some(desired) => {
                    if pad.net.as_deref() != Some(desired.as_str()) {
                        out.pad_net_changes.push(PadNetChange {
                            reference: fp.reference.clone(),
                            pad: pad.number.clone(),
                            old: pad.net.clone(),
                            new: desired.clone(),
                        });
                        if board.uses_codes
                            && !board.net_table.contains(desired)
                            && nets_seen.insert(desired.clone())
                        {
                            out.nets_to_add.push(desired.clone());
                        }
                    }
                }
                None => {
                    // Pad with copper net on the board but no schematic node —
                    // leave it, just report.
                    if pad.net.as_deref().is_some_and(|n| !n.is_empty()) {
                        out.unmatched_board_pads
                            .push((fp.reference.clone(), pad.number.clone()));
                    }
                }
            }
        }
    }

    for comp in &nl.comps {
        if !board_refs.contains(comp.reference.as_str()) {
            out.missing_on_board.push(comp.reference.clone());
        }
    }

    out
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const NETLIST: &str = r#"(export (version "E")
      (components
        (comp (ref "R1") (value "10k") (footprint "Resistor_SMD:R_0402"))
        (comp (ref "C1") (value "100n") (footprint "Capacitor_SMD:C_0402"))
        (comp (ref "U9") (value "NEW") (footprint "Package:SOT-23")))
      (nets
        (net (code "1") (name "GND")
          (node (ref "R1") (pin "2"))
          (node (ref "C1") (pin "2")))
        (net (code "2") (name "VCC")
          (node (ref "R1") (pin "1"))
          (node (ref "C1") (pin "1")))))"#;

    // Name-only board: R1 pad2 net is stale (VCC, should be GND); C1 value is
    // stale (1u vs 100n). X1 is on the board but not in the schematic.
    const BOARD: &str = r#"(kicad_pcb
      (footprint "Resistor_SMD:R_0402"
        (property "Reference" "R1")
        (property "Value" "10k")
        (pad "1" smd roundrect (at 0 0) (net "VCC"))
        (pad "2" smd roundrect (at 1 0) (net "VCC")))
      (footprint "Capacitor_SMD:C_0402"
        (property "Reference" "C1")
        (property "Value" "1u")
        (pad "1" smd roundrect (at 0 0) (net "VCC"))
        (pad "2" smd roundrect (at 1 0) (net "GND")))
      (footprint "Connector:Screw"
        (property "Reference" "X1")
        (property "Value" "CONN")
        (pad "1" thru_hole circle (at 0 0) (net "GND"))))"#;

    #[test]
    fn parse_netlist_extracts_comps_and_pad_nets() {
        let nl = parse_netlist(NETLIST).unwrap();
        assert_eq!(nl.comps.len(), 3);
        assert_eq!(
            nl.pad_nets
                .get(&("R1".into(), "2".into()))
                .map(String::as_str),
            Some("GND")
        );
        assert_eq!(
            nl.pad_nets
                .get(&("C1".into(), "1".into()))
                .map(String::as_str),
            Some("VCC")
        );
        assert_eq!(nl.net_names, vec!["GND", "VCC"]);
    }

    #[test]
    fn parse_board_reads_name_only_pad_nets() {
        let b = parse_board(BOARD).unwrap();
        assert!(!b.uses_codes);
        assert!(b.net_table.is_empty());
        let r1 = b.fps.iter().find(|f| f.reference == "R1").unwrap();
        assert_eq!(r1.value, "10k");
        let p2 = r1.pads.iter().find(|p| p.number == "2").unwrap();
        assert_eq!(p2.net.as_deref(), Some("VCC"));
    }

    #[test]
    fn plan_finds_stale_pad_net_and_value() {
        let nl = parse_netlist(NETLIST).unwrap();
        let b = parse_board(BOARD).unwrap();
        let p = plan(&nl, &b);

        // R1/pad2 must move VCC → GND. C1 pads already correct.
        assert_eq!(p.pad_net_changes.len(), 1);
        let ch = &p.pad_net_changes[0];
        assert_eq!((ch.reference.as_str(), ch.pad.as_str()), ("R1", "2"));
        assert_eq!(ch.old.as_deref(), Some("VCC"));
        assert_eq!(ch.new, "GND");

        // C1 value 1u → 100n.
        assert_eq!(p.value_changes.len(), 1);
        assert_eq!(p.value_changes[0].reference, "C1");
        assert_eq!(p.value_changes[0].new, "100n");

        // U9 is only in the schematic; X1 only on the board.
        assert_eq!(p.missing_on_board, vec!["U9"]);
        assert_eq!(p.extra_on_board, vec!["X1"]);

        // Name-only board needs no net-table additions.
        assert!(p.nets_to_add.is_empty());
    }

    #[test]
    fn plan_reports_footprint_mismatch() {
        let nl = parse_netlist(
            r#"(export (components (comp (ref "R1") (value "10k") (footprint "Resistor_SMD:R_0603")))
               (nets))"#,
        )
        .unwrap();
        let b = parse_board(
            r#"(kicad_pcb (footprint "Resistor_SMD:R_0402"
                 (property "Reference" "R1") (property "Value" "10k")
                 (pad "1" smd (at 0 0) (net "N"))))"#,
        )
        .unwrap();
        let p = plan(&nl, &b);
        assert_eq!(p.footprint_mismatches.len(), 1);
        assert_eq!(p.footprint_mismatches[0].1, "Resistor_SMD:R_0402");
        assert_eq!(p.footprint_mismatches[0].2, "Resistor_SMD:R_0603");
    }

    #[test]
    fn plan_never_syncs_to_unconnected_nets() {
        // Netlist collapses two no-connect pads onto one `unconnected-*` name;
        // the board has KiCad's unique per-pad names. plan() must leave both.
        let nl = parse_netlist(
            r#"(export (components (comp (ref "U1") (value "IC") (footprint "F")))
               (nets (net (code "1") (name "unconnected-(U1-NC-Pad3)")
                        (node (ref "U1") (pin "3"))
                        (node (ref "U1") (pin "4")))))"#,
        )
        .unwrap();
        let b = parse_board(
            r#"(kicad_pcb (footprint "F"
                 (property "Reference" "U1") (property "Value" "IC")
                 (pad "3" smd (at 0 0) (net "unconnected-(U1-NC-Pad3)_1"))
                 (pad "4" smd (at 1 0) (net "unconnected-(U1-NC-Pad3)_2"))))"#,
        )
        .unwrap();
        let p = plan(&nl, &b);
        assert!(
            p.pad_net_changes.is_empty(),
            "must not rewrite pads onto a collapsed unconnected-* net"
        );
    }

    #[test]
    fn plan_adds_missing_net_on_coded_board() {
        let nl = parse_netlist(
            r#"(export (components (comp (ref "R1") (value "10k") (footprint "F")))
               (nets (net (code "1") (name "NEWNET") (node (ref "R1") (pin "1")))))"#,
        )
        .unwrap();
        // Coded board: has a net table (net 1 "GND"), pad currently on GND.
        let b = parse_board(
            r#"(kicad_pcb (net 0 "") (net 1 "GND")
                 (footprint "F" (property "Reference" "R1") (property "Value" "10k")
                   (pad "1" smd (at 0 0) (net 1 "GND"))))"#,
        )
        .unwrap();
        assert!(b.uses_codes);
        let p = plan(&nl, &b);
        assert_eq!(p.pad_net_changes.len(), 1);
        assert_eq!(p.nets_to_add, vec!["NEWNET"]);
    }
}
