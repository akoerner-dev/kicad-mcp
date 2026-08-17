# Roadmap

The near-term direction for Konnect. No dates — items ship when they're solid.
Opening an issue is the best way to influence priority.

## Platform

- **Linux and macOS builds.** The code already carries `#[cfg]` branches and Unix
  paths for both platforms, and CI checks all three OSes — what remains is release
  packaging, per-platform QA against a running KiCAD, and macOS code signing /
  notarization.
- **KiCAD PCM publication** — submit the plugin to the official KiCAD addon
  repository once the first tagged release is out.

## Tools

- **Library resolution** — `search_symbols` / `search_footprints` currently miss
  KiCAD's own ~220 bundled libraries: KiCAD 10 registers them through a single
  nested `type "Table"` entry pointing at a second `sym-lib-table`, and entries
  whose URI is a `${…}` path variable are skipped outright. `add_schematic_component`
  bypasses the library table entirely and resolves only against KiCAD's install
  directory. Fixing the resolver once would cover search, placement, and
  user-registered libraries together.
- **Placing components that aren't on the board yet** — `place_component` writes a
  footprint reference but no pad geometry, so the result has no pads, value, or
  nets. Reading the real `.kicad_mod` and emitting complete geometry is what would
  turn `update_pcb_from_schematic` into a full **F8** (it deliberately reports, but
  never applies, component add/delete/refootprint today). Depends on the resolver above.
- **Richer part authoring** — `create_symbol` / `create_footprint` cover simple
  parts only: one unit per symbol, no footprint/datasheet/description/keyword
  fields, axis-aligned rectangular pads, a single round drill, and no
  mask/paste margins or split thermal-pad paste apertures.
- **Bottom-side placement** — no flip/mirror tool; bottom-layer assembly is
  GUI-only.
- **Eagle project import** — migrate legacy Eagle designs.

## Known gaps

Verified against the source on 2026-08-16. Listed because these tools' descriptions
currently promise more than their handlers deliver.

- **`set_layer_constraints`** is now guarded: it used to splice `(rule …)` blocks
  into the `.kicad_pcb` `(setup …)` section — invalid there, which corrupted the
  board — so the handler now refuses with an error instead of writing. A real
  implementation must target the sibling `<project>.kicad_dru` file; per-board
  minimums already work via `set_design_rules`.
- **`assign_net_to_class`** was missed by the `.kicad_pro` migration and still
  reads/writes only the legacy `(net_class …)` block, so it is inert on KiCAD 7+ boards.
- **`move_connected`** delegates to the plain move; connected wires do not stretch.
- **`annotate_schematic`** rewrites `?` only in the instance path, not in the
  visible `(property "Reference" …)`, so other tools still find the part as `R?`.
- **`edit_schematic_component`** advertises a `fields` parameter its handler never
  reads; use `add_component_annotation` to create custom fields.
- **`get_schematic_view`** renders an SVG into a temp dir, deletes it, and returns
  only a byte count; use `export_schematic_svg`/`_pdf` with an explicit path.
- **`check_clearance`** measures the distance between footprint *origins* — not
  pads, outlines, or courtyards — so overlapping parts can report ample clearance.
- **DFM checks** (`validate_for_manufacturing`, `audit_manufacturing`, and
  `design_review`'s rule lookup) search for constraint values in the `.kicad_pcb`,
  where KiCAD 7+ boards no longer keep them.
- **Drill export** passes a file path where `kicad-cli pcb export drill` expects a
  directory, and the result is discarded rather than checked.
- **BOM/position export** emits raw `kicad-cli` formats only — no custom fields
  (LCSC/MPN), and `exclude_dnp`/`format` are accepted but unused. `fab_house`
  affects thresholds and cost math, not the generated files.
- **JLCPCB tooling** depends on a hard-coded database URL that now returns 404, and
  the datasheet lookups read `result.dataManualUrl`, a field LCSC no longer returns
  (it is `result.pdfUrl` today).
- **`estimate_cost`** returns hard-coded formulas, not quotes from any price source.

## Infrastructure

- **Deeper end-to-end tests** — tool-handler tests against a mocked IPC endpoint.

## Done

- ~~ERC reporting~~ — `run_erc` used to report every schematic clean:
  `parse_erc_json` read a top-level `violations` array, but the ERC report has
  none — violations are nested under `sheets[]`, and only the enclosing sheet
  carries the sheet name (top-level `violations` is the *DRC* report's shape).
  Violations now also surface their machine-readable `type` and their `items[]`
  with per-item positions and uuids, matching `run_drc`'s output so one caller
  can handle both. Verified live on a schematic where `kicad-cli` reports 7
  violations: 0 → 7, each attributed to its sheet.
- ~~Auto-routing~~ — `autoroute` runs a full Freerouting pipeline: Specctra DSN
  export via KiCAD's bundled `pcbnew` Python module (`kicad-cli` dropped DSN/SES
  in KiCAD 10), Freerouting headless, then SES import back into the board. Forced
  `-mt 1` (Freerouting's multi-threaded optimizer is documented to generate
  clearance violations) and `-Djava.awt.headless=true` (it otherwise draws real
  dialogs even in batch mode); non-zero exits surface in a `warning` field.
  Verified from scratch on a 194-pad board: 112 unconnected → 0.
- ~~Design→Board sync (partial)~~ — `update_pcb_from_schematic` is a file-based
  **F8** for connectivity: exports the netlist, diffs it against the board, and
  bulk-rewrites stale pad nets and footprint values. Never syncs `unconnected-*`
  nets (KiCAD's per-pad auto-names collapse on export and would fuse unconnected
  pads). Adding/removing/refootprinting components is still reported-only — see Tools above.
- ~~Ratsnest access~~ — `get_unrouted_connections` lists missing copper per net,
  resolving each DRC item to an exact (reference, pad) pair by board-space position
  rather than by parsing KiCAD's localized violation text.
- ~~DRC observability~~ — `run_drc` now reads all three report sections
  (`violations`, `unconnected_items`, `schematic_parity`), returns per-violation
  item positions and machine-readable types, and can refill zones so headless DRC
  matches the GUI.
- ~~Design rules on KiCAD 7+~~ — `set_design_rules`/`get_design_rules`/`create_netclass`
  fall back to the sibling `.kicad_pro` JSON when a board has no legacy
  `(net_class …)` block, preserving key order so one-field edits stay one-line diffs.
- ~~Teardrop control~~ — `set_pad_teardrop` suppresses regeneration at a pad and
  `delete_teardrop_zone` removes an existing teardrop zone (they have no `uuid`, so
  `delete_trace` cannot target them).
- ~~HTTP transport~~ — Streamable HTTP (MCP spec 2025-06-18) available via
  `transport = "http"` (or `"both"`): POST + GET (SSE) on a single `/mcp`
  endpoint, Origin validation, and a `/health` probe.
- ~~Additional export formats~~ — IPC-2581, ODB++, GenCAD, and DXF are now
  available via `export_ipc2581`, `export_odb`, `export_gencad`, and
  `export_dxf` in the `pcb_export` toolset (all backed by native `kicad-cli`
  subcommands, verified against KiCAD 10.0).
- ~~Retry/backoff for external services~~ — the JLCPCB database download and
  both LCSC datasheet lookups now retry transient failures (network errors,
  429, 5xx) with exponential backoff via `get_with_backoff` in
  `crates/konnect-core/src/tools/integration.rs`.
- ~~Component search caching~~ — `search_jlcpcb_parts`, `get_jlcpcb_part`, and
  `suggest_jlcpcb_alternatives` now cache results for 5 minutes via a shared
  `QueryCache` on `ToolContext`; responses carry a `"cached"` field.
- ~~Hierarchical sheets~~ — create and manage multi-sheet schematics via the
  new `sch_hierarchy` toolset: sheet lifecycle (add/edit/move/delete/duplicate,
  recursive hierarchy and page-numbering queries) plus sheet pin lifecycle
  (import from hierarchical labels, add/edit/delete pins, pin/label sync
  validation).
- ~~`import_svg_logo`~~ — import an SVG file as filled silkscreen/copper
  artwork via the new `import_svg_logo` tool in the `pcb_board` toolset.
  Curved paths (quadratic/cubic Bezier) are flattened into polygon outlines
  since KiCAD's board format doesn't support curves in filled shapes. Tries
  the IPC API first, falls back to a direct file edit if KiCAD isn't running.
- ~~Multi-sheet schematic viewer~~ — point the viewer at the root schematic of a
  hierarchical design and it walks every reachable sheet, renders each via
  `kicad-cli`, and offers a depth-indented sheet selector. Edits saved from KiCAD
  re-render only the changed sheets and refresh live; rendering runs against
  temp-folder snapshots so the viewer never blocks KiCAD from saving.
