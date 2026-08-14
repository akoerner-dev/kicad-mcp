# Konnect (KiCad MCP) — fork with autonomous net-sync & DRC tooling

**A fork of the [Konnect](#relationship--credits) KiCad MCP server that I extended so an
AI assistant can close board-vs-schematic parity without the KiCad GUI.**

Konnect is a single Rust binary that lets Claude (and other [MCP](https://modelcontextprotocol.io)
clients) drive **KiCad 10** — schematic capture, PCB layout/routing, ERC/DRC, and
manufacturing export — either by editing the project files directly or live over
KiCad's IPC API. This repository is **my fork**; I added a file-based *"Update PCB from
Schematic"* (F8) tool, pad-level net editing, and a corrected DRC pipeline, all driven
from real hardware work on a two-family smart-doorbell PCB.

> **Attribution up front.** I did not write Konnect. The base project is the work of its
> upstream authors (see [Relationship & credits](#relationship--credits)). Everything in
> **[My contributions](#my-contributions)** below is mine; the rest is upstream. Keeping
> that line clear matters more to me than a longer list.

---

## My contributions

All of the following are file-based S-expression edits on the `.kicad_pcb` — no GUI, and
(where possible) no running KiCad — with a pure, unit-tested core plus live verification
against a real board via `kicad-cli`.

| Tool / change | What it does | Why it was needed |
|---|---|---|
| **`update_pcb_from_schematic`** | File-based equivalent of KiCad's **F8** for connectivity: exports the schematic netlist, diffs it against the board, and bulk-rewrites every stale pad net and footprint value to match the schematic. Reports (never silently applies) component add/delete/refootprint. Defaults to a `dry_run`. | Design→Board sync was the single biggest gap — previously only doable in the KiCad GUI. |
| **`set_pad_net`** | Reassigns one pad's net by rewriting its `(net …)` entry, format-preserving (coded `(net 3 "GND")` vs. name-only `(net "GND")`). | The atomic building block for fixing a swapped/stale pad net without the GUI. |
| **DRC observability** | `run_drc` now surfaces the report sections the stock path dropped — `unconnected_items` and `schematic_parity` — plus each violation's item positions and machine type, and an optional zone-refill so headless DRC matches the GUI. | The unpatched tool read only `violations` and reported a broken board as "0 errors" — a silent, dangerous blind spot. |
| **`get_component_pads` fix** | Reads the pad net on name-only boards (index 1 fallback), not just coded ones. | Name-only boards previously returned empty net strings. |
| **`get_unrouted_connections`** | Ratsnest tool: lists missing copper connections per net, resolving each DRC item to an exact (reference, pad) pair by matching board-space position against the board's own pads — not by parsing KiCad's localized violation text (this install runs German KiCad). | No pad-pair-level ratsnest tool existed; it's the prerequisite input any auto-router or routing loop needs. |
| **Footprint rotation-sign fix** | `find_pad_board_position` (used by `route_pad_to_pad`) and `get_component_pads` applied a rotation transform with the wrong sign — invisible at 0°/180° (the sign only enters through `sin`), but silently returned the *other* pad's position on any 90°/270°-rotated footprint. Found while verifying the tool above against a live `kicad-cli` DRC report; fixed with a regression test encoding the real rotated footprint that exposed it. | A latent correctness bug in already-shipped tooling — `route_pad_to_pad` could have routed to the wrong pad on a rotated footprint. |
| **Starter-kit toolset tuning** | Pre-loads `pcb_routing` at server startup instead of only via runtime `load_toolset`. | Traced a real MCP client that reads `tools/list` once at connection and never revisits it — confirmed server-side `list_changed` notifications fire correctly after `load_toolset`, so this routes around a client-side gap rather than a server bug. |
| **Windows build chain** | GNU-toolchain build recipe + [`BUILD_NOTES_WINDOWS.md`](BUILD_NOTES_WINDOWS.md). | Upstream targeted Unix; getting the `nng`/protobuf stack building under MinGW took real work. |

**Full tool catalogue:** [`tool-directory.md`](tool-directory.md) — 18 on-demand toolsets, ~190 tools.

### Design notes worth reading (the interesting part)

- **Never sync `unconnected-*` nets.** KiCad gives every no-connect pad a *unique* auto-net
  so a lone pad counts as trivially routed; the exported netlist collapses them onto one
  name. A naïve sync merges those pads onto one net with no copper between them and
  manufactures phantom ratsnest. I found this by measuring DRC unconnected count
  before/after (0 → 6), and now skip that net class — which is exactly what the real F8
  does. Source + rationale: [`netlist.rs`](crates/konnect-core/src/tools/netlist.rs).
- **Format-preserving edits, not parse→serialize.** All writes are targeted string edits
  (locate a balanced `(…)` block, splice, atomic write via write→fsync→rename) so KiCad's
  exact formatting and UUIDs survive. A full round-trip through a serializer had previously
  corrupted files.
- **Verifiable by construction.** The parse/diff core is pure and unit-tested with no KiCad
  installed; live behaviour is checked with env-gated tests against a real board and a
  `kicad-cli --schematic-parity` DRC before/after. On the doorbell board the net/value sync
  closed one real parity item (parity 10 → 9) with unconnected staying 0, and is idempotent
  on a second pass.
- **Blind-tested against a real board fault.** After building `get_unrouted_connections`, I
  had someone displace a component on a real board without telling me what changed or where.
  The tool's ratsnest output alone (two dangling-track endpoints, both offset from the same
  footprint by an identical distance) was enough to localize it to one specific component,
  and a plain position check against a previously-known-good coordinate for that part
  confirmed a clean 5 mm translation — correctly separating it from an unrelated pre-existing
  routing gap on a different net that was *not* part of the injected fault.

Everything above landed as small, reviewed commits on the `drc-parity-unconnected` branch —
see `git log` for the step-by-step history.

---

## Build

Rust workspace, one binary (`konnect`). On Windows (MinGW/GNU toolchain — full notes and the
`nng`/protobuf gotchas are in [`BUILD_NOTES_WINDOWS.md`](BUILD_NOTES_WINDOWS.md)):

```bash
# needs: rustup GNU toolchain, cmake, protoc, WinLibs-MinGW on PATH
export PROTOC=$(command -v protoc)
export CFLAGS="-Wno-error=incompatible-pointer-types"
export CXXFLAGS="$CFLAGS"
cargo +stable-x86_64-pc-windows-gnu build --release -p konnect
```

The result is a single ~5 MB binary at `target/release/konnect.exe`. Run `cargo test` for the
pure test suite (no KiCad needed).

## Use it with Claude

Konnect speaks MCP over **stdio** (default) or HTTP. As a Claude Desktop extension, point the
extension's `mcp_config.command` at the built binary with a small TOML config:

```toml
transport = "stdio"
ipc_address = 'C:\Users\<you>\AppData\Local\Temp\kicad\api.sock'  # KiCad's IPC socket
kicad_cli   = 'C:\Program Files\KiCad\10.0\bin\kicad-cli.exe'
kicad_binary= 'C:\Program Files\KiCad\10.0\bin\kicad.exe'
```

**Live IPC prerequisites** (for real-time board edits): KiCad running with the API enabled
(*Preferences → Plugins → Enable KiCad API*), the **PCB editor window open**, and **no modal
dialog open** (KiCad answers `AS_BUSY` while one is). File-based tools (including
`update_pcb_from_schematic`) work without a running KiCad.

## Repository layout

```
crates/
  konnect              MCP server binary (stdio/HTTP transport, router, extension install)
  konnect-core         Tool implementations, grouped into on-demand toolsets
    src/tools/netlist.rs        ← netlist parse + board diff (my work; pure, unit-tested)
    src/tools/pcb_components.rs  ← set_pad_net, update_pcb_from_schematic (my work)
    src/tools/cli.rs, verification.rs ← patched DRC pipeline (my work)
  konnect-sexp         Format-preserving S-expression reader/writer + atomic file writes
  konnect-ipc          KiCad 10 IPC client (protobuf over NNG)
  konnect-schematic-editor   .kicad_sch editing engine
tool-directory.md      Full catalogue of every tool, per toolset
```

## Relationship & credits

- **Base project:** [Konnect](https://github.com/obhox/kicad-mcp) — the Rust KiCad MCP server
  this repo forks.
- **Original lineage:** [mixelpixx/KiCAD-MCP-Server](https://github.com/mixelpixx/KiCAD-MCP-Server),
  the Python/TypeScript predecessor Konnect grew out of.
- All upstream design, architecture, and the bulk of the tool surface are the work of those
  authors. My additions are scoped to the [contributions above](#my-contributions).

## License

**GNU AGPL-3.0-only**, inherited from upstream — see [`LICENSE`](LICENSE). This is a public
fork provided as a project record; contributions and questions are welcome via issues.
