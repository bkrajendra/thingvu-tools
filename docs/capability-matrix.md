# Capability matrix

What each kind of session can do, and which backend serves it. This is what `session_status` reports in `capabilities` for a live session; the table is what has been observed and tested. Backends: **ipc** is KiCad's API, **kicad_cli** is `kicad-cli`, **project_file** is the schematic file itself.

| Capability | Headless (KiCad 11 / 10.99) | GUI (KiCad 11 editor, via the plugin) | File (KiCad 10 or 11, no KiCad running) |
|---|---|---|---|
| `session.status` | ipc | ipc | project_file |
| `session.open_documents` | ipc | ipc | project_file (the session's one file) |
| `session.document_lifecycle` | ipc: create, open, save, revert, close | unavailable: the editor owns the document | unavailable: no editor document |
| `session.commits` | ipc, when the document was opened by the session (always the case here) | ipc: each commit is one undo step, and an apply is two | unavailable: changes are verified in a copy, then the file is replaced in one step |
| `sch.read_hierarchy` | ipc | ipc | unavailable: only the root schematic file is read |
| `sch.read_symbols` | ipc | ipc | project_file |
| `sch.netlist` | ipc | ipc | kicad_cli: the netlister omits power symbols and flags |
| `sch.place_symbol` | ipc | ipc | project_file: symbols from the file, the project's `sym-lib-table`, or the install's libraries |
| `sch.edit_items` | ipc | ipc | project_file |
| `sch.save` | ipc | ipc | project_file: every applied plan is written at once |
| `sch.erc` | kicad_cli, on a snapshot of the in-memory document | kicad_cli, on a snapshot | kicad_cli, on a copy of the file |
| `sch.render` | kicad_cli (PNG) | kicad_cli (PNG) | kicad_cli (PNG): needs a KiCad 11 / 10.99 install, because the 10.0 CLI cannot export PNG |
| `sch.export_netlist` | kicad_cli | kicad_cli | kicad_cli |

## Notes

- **A capability is probed, not assumed.** The API providers send a harmless request per command family when a session opens; an answer of "unhandled" or "unimplemented" turns the capability off, with the reason in `note`. The file provider's set is fixed by design, with the CLI-dependent entries turned off when no suitable `kicad-cli` is installed.
- **KiCad 10's API is not used.** KiCad 10.0.7 answers only version, open-document names, and commits; it has no schematic commands (docs/kicad-api-compatibility.md). A KiCad 10 user works through the file provider. There is no provider that attaches to a running KiCad 10 editor.
- **Which provider for which job:** the headless provider generates and checks designs without a GUI; the GUI provider co-edits the open editor with normal undo; the file provider needs no KiCad running and works with KiCad 10.
- **One provider per document.** A document open in an editor is never edited through the headless or file provider (lock-file check), and the GUI provider is the only one that edits it.
- **Version-specific `kicad-cli` differences** (PNG export, `sch diff`, `api-server`) are in docs/kicad-api-compatibility.md.

## Review, tidy, and field changes

These need no capability of their own: `parts_lookup` (read-only; it returns calls for the assistant to make on the pcbparts MCP), `sch_review`, `sch_describe_layout`, `sch_tidy_plan`, `sch_arrange_plan`, `sch_reconnect_plan`, `sch_pin_net_plan`, `sym_search`, `sym_get`, `sch_declutter_plan`, `sch_label_plan`, `sch_draw_plan`, `sch_edit_items_plan`, `bom_get`, `bom_review`, `sch_set_fields_plan`, and `sch_footprint_plan` read the sheet (`fp_search` and `fp_get` read library files and need no session) (`sch.read_symbols`, `sch.netlist`), and applying a tidy or `sch_set_field` needs `sch.place_symbol` and `sch.edit_items`.

| | Headless | GUI | File |
|---|---|---|---|
| Whole sheet read (wires, labels, junctions, notes) | ipc | ipc | project_file |
| Netlist used for a tidy | ipc (includes power symbols and flags) | ipc | kicad_cli, plus power symbols and flags attached from the drawing |
| Applying a tidy | Two commits; reloads the saved file if it fails, so unsaved changes must be saved first | Two commits (two undo steps); a failure after the first says how many undo steps to take | Verified copy, then atomic replace |
| Circuit plans with `no_connects` and `power_flags` (`sch_plan_changes`) | ipc; each flag read back | ipc; not yet tried in the editor | Verified copy; each flag read back from the file |
| Page and title block (`sch_page_plan`) | `SetPageSettings` and `SetTitleBlockInfo`, read back | same; not yet tried in the editor | `(paper …)` and `(title_block …)` rewritten in the text, loaded in `kicad-cli`, read back |
| `sch_set_field` | One commit | One commit (one undo step) | Verified copy, then atomic replace |

## Board sessions (Phase 11)

Board sessions (`pcb_open`) are separate from schematic sessions. They have no probed capability set of their own yet: what each provider can do is fixed, as for the file provider.

| | Headless | GUI | File |
|---|---|---|---|
| Needs | KiCad 11 (or the 10.99 nightly) `kicad-cli` | KiCad 11 PCB editor with the plugin button pressed | any `kicad-cli` (KiCad 10 or 11) |
| Read the board (`pcb_info`, `pcb_list_*` including `pcb_list_drawings` and `detail: full` footprints, `pcb_get_footprint`, `pcb_rules`) | ipc | ipc | project_file (`kicad-sexpr`) |
| DRC (`pcb_run_drc`) | kicad_cli on a snapshot | kicad_cli on a snapshot | kicad_cli on the file |
| Render (`pcb_render`) | kicad_cli (KiCad 11 only) | kicad_cli (KiCad 11 only) | kicad_cli (KiCad 11 only) |
| Review (`pcb_review`) | ipc + kicad_cli | ipc + kicad_cli | project_file + kicad_cli |
| Update from the schematic (`pcb_update_from_schematic_plan`, `pcb_apply_plan`) | ipc (`ImportNetlist`); the board stays unsaved until `pcb_save` | ipc; live in the editor | not available |
| Checkpoints (`checkpoint_create`, `checkpoint_list`, `checkpoint_diff`, `checkpoint_restore`) | project files copied; a live session's open documents saved to a copy first | refused to restore while the editor has the files open; create and diff work | project files copied |
| Reload and freshness (`pcb_reload`, `session_reload`, `file_changed_on_disk`) | `RevertDocument` and the file's time and size | the same; not yet tried in the editor | the file is read each time; `read_only` opens a locked file |
| Outline, moves, placement, footprint attributes, routing, pours (`pcb_outline_plan`, `pcb_move_plan`, `pcb_footprint_attrs_plan`, `pcb_place_plan`, `pcb_route_plan`, `pcb_track_edit_plan`, `pcb_rules_plan`, `pcb_zone_plan`, `pcb_apply_plan`) | ipc, one commit; unsaved until `pcb_save` | ipc, one commit (one undo step); not yet tried live | not available (D3) |
| Rollback of a board change that fails its checks | ipc (snapshot and restore, one commit) | ipc (one more undo step) | not applicable |
| Autoroute (`pcb_autoroute_plan`, optional Freerouting; `nets`, `exclude_nets`, `keep_existing`) | kicad_cli export of a snapshot, the DSN edited as text, Freerouting (its process tree ended at the time limit), ipc import | same; not yet tried live | not available |
| Manufacturing outputs (`pcb_export`, `pcb_fab_package`) | kicad_cli on a snapshot; read back with `kicad-cli gerber info` | same; not yet tried live | kicad_cli on the file (read-back of Gerbers needs a KiCad 11 `kicad-cli`; not yet tried on a KiCad 10 board) |
| Mechanical model (`pcb_mechanical_info`) | project_file (`kicad-sexpr`) on a snapshot | same | project_file |
| Save (`pcb_save`) | ipc | ipc | not applicable |

