# KiCad API compatibility record

Observed facts per pinned KiCad build. Update this file whenever a pinned build changes; re-run the Phase 0 probes first.

## Pinned builds

| Tier | Build | Install path | `kicad-cli` | API server setting |
|---|---|---|---|---|
| 1 (KiCad 11) | `10.99.0-5896-gd033635628`, release build, 23 Oct 2026 | `D:\Program Files\KiCad\10.99` | has `api-server` | `api.enable_server = true` |
| 2 (KiCad 10) | `10.0.7`, release build, 6 Oct 2026 (`KICAD_IPC_API=ON`) | `C:\Program Files\KiCad\10.0` | no `api-server` | enabled by user, 24 Oct 2026 |

Platform: Windows 11 Pro 26H2 (build 26300), x64.

Environment: machine-level `KICAD_HOME = D:\Program Files\KiCad\10.99`; `%KICAD_HOME%\bin` on machine `Path` (REG_EXPAND_SZ). Already-running processes must be restarted to see it.

User documents (plugin directories): `C:\Users\bkraj\OneDrive\Documents\KiCad\10.0`, `...\10.99`.

## Protos

| Tier | Source | Commit | SHA-256 per file |
|---|---|---|---|
| 1 | `gitlab.com/kicad/code/kicad` `api/proto` (24 files, vendored in `crates/kicad-ipc/proto/`) | `d033635628` | `crates/kicad-ipc/proto/SHA256SUMS` (manifest SHA-256 `033e0bbc…5db8`) |

License: 20 proto files are GPL-3.0-or-later and 4 are MIT (library and variant protos); this project is GPL-3.0-or-later (decided 24 Oct 2026).

## `kicad-cli` differences observed

| Command | 10.0.7 | 10.99 nightly |
|---|---|---|
| Top level | `fp`, `jobset`, `pcb`, `sch`, `sym`, `version` | adds `api-server`, `gerber`, `import`, `mergetool`, `project` |
| `sch` | `erc`, `export`, `upgrade` | adds `diff`, `downgrade`, `import` |
| `sch export` | `bom`, `dxf`, `hpgl`, `netlist`, `pdf`, `ps`, `python-bom`, `svg` | adds `png` |
| `sch erc` | `--format json\|report`, severity flags, `--exit-code-violations` | same |
| `api-server` | — | `kicad-cli api-server [--socket SOCKET_PATH] [PROJECT_OR_FILE]` |

## Probe results

Probe tool: `scripts/probe-kicad.ps1`. Fixtures produced along the way: `tests/fixtures/v11/`.

| Probe | Nightly headless | Nightly GUI | 10.0.7 GUI |
|---|---|---|---|
| `api-server --socket <path>` | ✅ honored: pipe `\\.\pipe\<path>` (verified with `…\run\k.sock`). One earlier attempt with `…\run\api.sock` never became reachable; cause not yet known. With stdout redirected the server prints nothing (no "listening" line), so readiness must be detected by polling | — | — |
| `api-server` with a hand-written minimal `.kicad_sch` | `Failed to load schematic` — use KiCad-authored fixtures | — | — |
| Default endpoint (no `--socket`) | `ipc://C:\Users\<user>\AppData\Local\Temp\kicad\api.sock` → named pipe `\\.\pipe\C:\Users\<user>\AppData\Local\Temp\kicad\api.sock` | ✅ same default endpoint | ✅ same default endpoint (when it is the only KiCad running) |
| Two GUI instances at once (nightly started 15:17:13, 10.0.7 at 15:17:29, both with the API server enabled) | — | The first instance (nightly) owns `api.sock`; 10 of 10 connections reached it | **No reachable endpoint** for the second instance: not `api.sock`, not `api-<pid>.sock`. Multi-instance discovery cannot rely on per-instance socket names on Windows |
| Endpoint visible in `\\.\pipe\` listing | Not listed by `Directory.GetFiles('\\.\pipe\')` — the name contains backslashes. Discovery must derive the name from the socket path, not enumerate pipes | same | same |
| NNG handshake (SP header, REQ0 ↔ REP0) | ✅ peer replies `00 53 50 00 00 31 00 00` (REP0) | ✅ | ✅ |
| NNG ipc framing | ✅ `0x01` + u64 big-endian length + body; REQ body prefixed with 4-byte request id (high bit set), echoed in reply | ✅ | ✅ |
| `GetVersion` | ✅ `major=10 minor=99 patch=0`, `full_version="10.99.0-5896-gd033635628"` | ✅ same | ✅ `major=10 minor=0`, `full_version="10.0.7"`, decoded with the KiCad 11 protos |
| `Ping` | ✅ reply `google.protobuf.Empty` | ✅ | _not run_ |
| Token: empty token | Accepted; reply header carries the instance token (UUID form, stable across calls and connections) | Accepted | Accepted; reply header carries the instance token |
| Token: mismatch | _not tested_ | ✅ wrong pinned token → `AS_TOKEN_MISMATCH` "the provided kicad_token did not match this KiCad instance's token" | _not run_ |
| Instance token per instance | Differs per server instance (`2757…` → `2e39…` after restart): usable to pin a session | — | — |
| `GetOpenDocuments` (SCHEMATIC / PCB / PROJECT) | Without a loaded file: `AS_UNHANDLED` for all types. With `api-server <file>.kicad_sch`: SCHEMATIC ✅ `DocumentSpecifier{type=SCHEMATIC, project{name, path}}` (no sheet path); PCB `AS_UNHANDLED` | SCHEMATIC ✅ (schematic editor open) | SCHEMATIC ✅ but identifies the document **only by filename** (field 4, e.g. `kicad-demo10.1.kicad_sch`), with **no project block**; PCB `AS_UNHANDLED` |
| `GetSchematicHierarchy` | ✅ root `SheetInstance{path=[root KIID], "/", name, filename, page "1"}`. Root KIID equals the file's top-level `(uuid …)` — file UUIDs and IPC KIIDs are the same identity | _not run_ | `AS_UNHANDLED` |
| `GetSchematicNetlist` | ✅ nets list **pin KIIDs** per sheet (map to REF.PIN via each instance's pins); unconnected pins appear as `unconnected-(R1-Pad1)` | ✅ | `AS_UNHANDLED` |
| `LoadAllLibraries{LT_SYMBOL}` | ✅ `LCS_OK`, async; 223 symbol libraries. `GetLibraryStatuses` reported `Device` as status 1 (`LLS_INVALID`) yet placement succeeded 1 s later — re-check | ❌ `AS_UNIMPLEMENTED` "LoadAllLibraries is not available in GUI mode" (the proto comment says no-op; send it only in headless sessions) | `AS_UNHANDLED` |
| `PlaceSymbolFromLibrary` before library load | `AS_BAD_REQUEST` "symbol 'Device:R' not found" | — (GUI preloads; placement immediate) | — |
| `PlaceSymbolFromLibrary` | ✅ full `SchematicSymbolInstance`: new KIID, sheet path, position (nm), library symbol with per-instance pin KIIDs (pins 1/2 at local y = ∓3.81 mm), fields (Reference as requested, Value = "R") | ✅ | `AS_UNHANDLED` |
| `UpdateItems` (instance with edited `value_field.text`) | ✅ `ISC_OK`; Value = "10k". Full-object replace, no field mask | ✅ | _not run_ (no schematic item commands) |
| `CreateItems` (SchematicLine wire, 3× LocalLabel) | ✅ `ISC_OK` per item, new KIIDs, default stroke/text attributes | ✅ | _not run_ (KiCad 10 has no `SchematicLine` type) |
| `BeginCommit` / `EndCommit` | **Depends on how the document was opened.** Server started *with* a file argument: `BeginCommit` → `AS_UNHANDLED`. Document opened with `OpenDocument`/`CreateDocument` (what sessions do): commits ✅. **Inside an open commit a newly placed symbol is not yet in the document**: `UpdateItems` on it fails "an item with UUID … does not exist". Place in one commit, then update in a second | ✅ commit opened and closed around place + update + create. **Commits are owned by `client_name`**: another client gets `AS_BAD_REQUEST` "the client … does not has a commit in progress". A commit left open by a crashed client did not block a new client's commit. **One commit = one undo step**: a single Ctrl+Z removed the whole demo (user-verified) | `BeginCommit`/`EndCommit(CMA_DROP)` ✅ answered, but no schematic item commands exist to use inside a commit |
| Connectivity read-back after edits | ✅ `/INPUT` = {R1.1}, `/MID` = {R1.2, R2.1}, `/OUTPUT` = {R2.2}; labels exactly on pin endpoints connect | ✅ same | — |
| `SaveCopyOfDocument` (`include_project=true`) | ✅ writes `.kicad_sch`, `.kicad_pro`, `.kicad_prl`; loaded document unchanged. Target directory must exist | ✅; open document and file on disk unchanged | `AS_UNHANDLED` |
| `RunSchematicJobExportNetlist` (KiCad s-expr) | ❌ **reads the saved file, not the in-memory document**: on a never-saved document the job fails "Failed to load schematic" (exit 3) | ✅ `JS_SUCCESS`, netlist identical to the read-back (the document had been saved) | — |
| `RunSchematicJobExportSvg` | ❌ same as netlist job (PNG job tested): reads the saved file | ✅ `JS_SUCCESS`, output dir → `<project>.svg` | — |
| ERC over IPC | No ERC job exists in `schematic_jobs.proto` (exports only: SVG, DXF, PDF, PS, PNG, netlist, BOM). ERC stays on `kicad-cli` | same | — |
| `SaveDocument` (in `project_commands.proto` in KiCad 11) | ✅ writes the document to its path; modified state → `DMS_UNMODIFIED` | _not run_ | _not run_ |
| `RevertDocument` | ✅ discards unsaved edits (R2 gone, R1 kept); state → `DMS_UNMODIFIED` | _not run_ | _not run_ |
| Saved copy verified by `kicad-cli` 10.99 | ✅ netlist matches intent; ERC 2 warnings (`isolated_pin_label`, expected) | — | — |
| KiCad 11 file read by 10.0.7 `kicad-cli` | ❌ "Failed to load schematic" (exit 3), as expected from format `20261015` | — | — |
| Native Rust transport + client (`crates/kicad-ipc`, `examples/probe.rs`) | ✅ full demo from a fresh empty fixture | ✅ full `gui` scenario | ✅ `basic` |
| Unknown command type | `AS_UNHANDLED` "no handler available for request of type …" | — | `AS_UNHANDLED`, same message format |
| Robustness | Repeated connect/handshake/close and bare pipe connect/close left the server responsive | Interrupted client with an open commit did not affect later clients | — |
| v11 generated protos decode v10 replies | — | — | ✅ envelope, status, `GetVersion`, `GetOpenDocuments`, commit replies |
| Exec plugin lifetime, schematic scope | — | _deferred to Phase 4_ (the headless path does not depend on it) | — |
| Headless document lifecycle (server started without a file) | ✅ `CreateDocument` (schematic, in memory; no file until saved) → `GetDocumentModifiedState` `DMS_UNMODIFIED` → edit → `DMS_MODIFIED` → `SaveDocument` → `RevertDocument` → `CloseDocument` (document commands then `AS_UNHANDLED` again) | — | — |
| Generic editor commands in the schematic editor | — | — | `GetItems`, `GetSelection`, `SaveCopyOfDocument` → `AS_UNHANDLED`. Live KiCad 10 schematic access is limited to version, open-document filename, and commits |
| `GetItemsFromLibrary` (symbols) | ✅ returns `SchematicSymbol` per id with reference prefix, `unit_count`, and pins (number, name, local position in the placed y-down axes, unit). Unknown entries are omitted | _not run_ | — |
| `UpdateItems` of one field with `ItemHeader.field_mask` and no embedded `definition` | ✅ value changes, no duplicate library symbol (one or two instances) | ⚠ the earlier full-object update inside a commit replied `10k` but did not persist; the masked form is untested in the GUI | — |
| `DeleteItems` (cleanup of placed symbols) | ✅ used to undo a failed apply; document returned to its starting fingerprint | _not run_ | — |

## File provider observations (KiCad 10.0.7, Phase 6)

Observed 25 Oct 2026 while building the file provider (`kicad-sexpr`, `FileSession`).

| Topic | Observation |
|---|---|
| Schematic format | 10.0.7 writes `(version 20260306)`, `(generator_version "10.0")`. The 10.99 nightly writes `20261015`. The 10.0 CLI refuses `20261015` files ("Failed to load schematic"); the 10.99 CLI reads both. |
| Symbol library format | 10.0.7 libraries are `(version 20251024)`, `(generator_version "10.0")`; same structure as the nightly's for everything the layout reads. |
| `kicad-cli sch upgrade --force <file>` | 10.0.7 rewrites an older schematic (e.g. a bundled demo, format `20250114`) in the current format. Used to make KiCad-authored fixtures. |
| Instance syntax | Identical in 20260306 and 20261015: `lib_id`, `at`, `(mirror x\|y)`, `unit`, `body_style`, flags, `uuid`, `property`×5, `pin`, `instances`. Wires use `(stroke (width 0) (type default))`; labels use `(justify left\|right bottom)` with angle 0/90/180/270; junctions `(diameter 0) (color 0 0 0 0)`. |
| Field text | A property's `(at x y angle)` is the library's offset moved by the symbol's transform; the **angle is stored relative to the symbol** (a fuse rotated 90° keeps the library's 90 for its reference and is drawn horizontal). |
| Pin geometry | Library y-up coordinates map to the API's y-down ones by negating y; the library's pin angle points from the connection point toward the body. The geometry read from 10.0.7's library files equals the 10.99 API's for every symbol of the golden circuits. |
| `extends` | A derived symbol (`NE555P` extends `NE555D`) is embedded in schematics as one flattened symbol; its sub-symbols take the derived name. |
| Netlist | `kicad-cli sch export netlist` omits power symbols and flags (`#PWR…`, `#FLG…`), unlike the API's `GetSchematicNetlist`. Net names are still set by power symbols (`+5V`, `GND`). |
| Lock file | An editor with a schematic open leaves `~<name>.kicad_sch.lck` beside it (seen beside the demo project while the nightly had it open). |
| PNG export | `kicad-cli sch export png` exists only in the 10.99 CLI; 10.0.7 has `svg`, `pdf`, `ps`, `dxf`, `hpgl`. |
| 10.99 CLI on 10.0 files | Reads a 20260306 file and exports PNG of it; used for rendering in file sessions. |
| Library lookup | Global libraries live in `<install>/share/kicad/symbols/<nickname>.kicad_sym` (10.0 and 10.99 on Windows). The project's own `sym-lib-table` can name others; the user's global table (custom libraries) is not read. |
| KiCad 10 GUI loading of a generated file | **Not yet checked by the author**: loaded by `kicad-cli` 10.0.7 (netlist, ERC) only. See docs/tasks.md, Phase 6 notes. |

## Rules, tracks, editor state (10.99, Phase 14.0 probe)

Probe: `crates/kicad-session/tests/live_probe14.rs`, headless, on the regulator board. "Handled" means the call works and a read back (or the saved file) shows the result.

| Command | Result |
|---|---|
| `GetNetClasses` / `SetNetClasses` (project) | **Handled.** `MMM_MERGE` adds a class (`Power`: 0.5 mm track, 0.25 mm clearance) and leaves `Default`. The saved `.kicad_pro` contains it. Needs the document's `ProjectSpecifier` |
| `SetNetClassAssignments` (project) | **Handled.** A per-net assignment (`GND` -> `Power`) shows up in `GetNetClassForNets` as an effective class (0.5 mm track, 0.25 mm clearance) and in the saved `.kicad_pro` (`netclass_assignments`) |
| `GetBoardDesignRules` / `SetBoardDesignRules` | **Handled.** Changing `min_track_width` to 0.15 mm reads back and is saved as `"min_track_width": 0.15`. The whole `BoardDesignRules` is sent back, so read first and change one field |
| `GetCustomDesignRules` / `SetCustomDesignRules` | **Handled** with an empty rule list (status `CRS_NONE`). A non-empty list was not tried |
| Rules through the tools (14.3) | `SetNetClasses` writes whole classes by name, so the tool reads the class, changes single fields, and sends it back (a via size goes in the class's `via_stack` as the first copper layer's size and the drill diameter, as `GetNetClassForNets` reports it). `SetNetClassAssignments` in merge mode replaces a net's classes and an empty list removes them; a pattern with an empty class is removed. `SetBoardDesignRules` takes the whole message, so the tool reads it first. Solder mask and paste margins are in `BoardDesignRules` (live) and in the board file, not the project file. `.kicad_pro` keys: `net_settings.classes[]` (`clearance`, `track_width`, `via_diameter`, `via_drill`, `diff_pair_width`, `diff_pair_gap`, `priority`), `netclass_assignments` (net to a list of classes in format version 5), `netclass_patterns[]` (`pattern`, `netclass`), `board.design_settings.rules.min_*`; all in mm |
| `GetBoardStackup` | **Handled** (finish, edge, every layer with thickness and type). `UpdateBoardStackup` is documented "not yet implemented", so the stackup is read-only |
| `GetDocumentModifiedState` | **Handled.** `DMS_MODIFIED` after an edit, `DMS_UNMODIFIED` after a revert |
| `RevertDocument` | **Handled** on a headless board: unsaved tracks are gone and the state returns to unmodified. **It also reads a file that changed on disk** after the session opened (a line added to the file appeared after the revert; `live_probe14_reload`), which makes it the reload. Not tried in the editor |
| `RefreshEditor` | **Unhandled** on a headless server ("no handler available"). Not tried on a GUI editor |
| `GetConnectedItems` | **Handled.** For one track it returned both tracks and the via of that connected run |
| Full geometry (14.4) | A footprint from `GetItems` carries its pads (`Pad` with a `PadStack`: shape, size, rounding ratio, and drill on the stack), its drawings (`BoardGraphicShape`, courtyard on `F.CrtYd`/`B.CrtYd`), extra fields (`Field`) and free text (`BoardText`) in `definition.items`, and the four standard fields on the instance. Positions are on the board. File and API give the same values for the regulator board (pad places, drills, reference text with the same id) |
| `GetTextExtents` | **Handled.** Returns a box (position and size) for a temporary `Text`: "R1 probe" at 1 mm came back 7.08 x 1.61 mm. Replaces the Phase 13 estimate in 15.4 |
| `GetPadShapeAsPolygon` | **Handled.** One outline per pad, in board coordinates, for a given layer |
| `GetTitleBlockInfo` / `SetTitleBlockInfo` | **Handled.** Title and revision read back. The message has nine comment fields |
| `GetPageSettings` / `SetPageSettings` | **Handled**, on a board and on a schematic. A change of size and of orientation takes and reads back (15.1, `live_probe15_page`). A custom size is kept in thousandths of an inch: 200 x 150 mm reads back as 199.9996 x 150.0124 and the saved file says so. `SetPageSettings` replies with the settings; `SetTitleBlockInfo` replies with nothing |
| Tracks and vias: `CreateItems`, `UpdateItems`, `DeleteItems` | **Handled.** `UpdateItems` re-widens a track (0.25 -> 0.4 mm) and locks it (`LS_LOCKED` = 2, unlocked = 1) in place. **`DeleteItems` deletes a locked track without complaint** (status OK), so the tools must refuse locked items themselves unless the item is named |

## Re-pinning to a new KiCad build

Do this when KiCad 11.0 is released (the nightly `10.99` stops being the target), and whenever the pinned nightly is replaced.

1. Install the new build. Update the *Pinned builds* table above.
2. Run `scripts\probe-kicad.ps1` against the new build and compare with the probe table. Any difference is a finding to record here.
3. If the API protos changed, vendor them again from the new KiCad commit into `crates/kicad-ipc/proto/`, regenerate `SHA256SUMS`, and update the commit in this file. Rebuild; fix what the new definitions break.
4. Run the whole live suite: `$env:KICAD_LIVE = '1'; cargo test --workspace -- --test-threads=1`. The `library_geometry_matches_kicads_own` and `both_backends_build_the_same_circuit` tests in `kicad-session` are the ones that notice a format or geometry change.
5. If the schematic format version changed, extend the supported range (`MIN_VERSION` / `MAX_VERSION` in `crates/kicad-sexpr/src/sch.rs`) only after the live tests pass on files of the new format, and add its series to `format_of` in `crates/kicad-session/src/file.rs`.
6. Update `docs/capability-matrix.md`, the README's supported-version table, and the badges.

## Reading and editing a whole sheet over the API (10.99, Phase 8)

Observed 25 Oct 2026.

| Topic | Observation |
|---|---|
| `GetItems` with many types | One request with the types for lines, junctions, local/global/hierarchical/directive labels, no-connect markers, bus entries, sheets, text, text boxes, shapes, images, tables, groups, and rule areas answers in one reply on the headless server. A `SchematicLine` has a type: wire, bus, or graphic |
| `DeleteItems` | Deletes wires, labels, junctions, and placed symbols (including power symbols) by KIID; each item has its own status (`IDS_OK`, `IDS_NONEXISTENT`, …) |
| `UpdateItems` with `field_mask` `position` and `transform` | Moves and turns a symbol. **The symbol's reference, value, and other field text stay where they were**: they are not carried along. Fields must be updated separately (`reference_field`, `value_field`, …, each carrying its own `text.position`) |
| Field text angle | `text.attributes.angle` is the same value the file stores: relative to the symbol. It does not change when the symbol is turned, and leaving it alone gives the same drawing as KiCad's own move in the editor |
| Wires after `CreateItems` | KiCad joins wires that meet end to end into one, so a set of wires read back may be cut differently from the set that was created; compare wires as runs along a line |
| `kicad-cli` netlist vs API netlist | The CLI netlister omits power symbols and flags; the API netlist includes them. A flag that is connected to the rest of a net only through the name of a power symbol on its wire has to be attached by name |
| ERC `power_pin_not_driven` | One violation per net. A net named by a **power symbol** whose pins nothing drives (no `PWR_FLAG`, no output pin) gets it; the same net named by a **label** does not. A tidy must therefore not turn a label-named net into a power-symbol net |

## Text, labels, and drawings over the API and in the file (10.99, Phase 9 probe)

Observed 25 Oct 2026 with `crates/kicad-session/tests/live_probe9.rs` (`KICAD_LIVE=1 cargo test -p kicad-session --test live_probe9 -- --nocapture`), on the headless server and in the saved file.

| Topic | Observation |
|---|---|
| `GetBoundingBox` | **Not handled for schematics** (`AS_UNHANDLED: no handler available`), with either mode. The extent of text cannot be asked of KiCad, so it is estimated from the string, size, angle, and justification (`kicad-domain::text`; about 0.9 of the text height per character, 1.2 high per line). The estimate errs on the generous side |
| SVG export | Carries `textLength` on a hidden `<text>` for each string, which can be used to calibrate the estimate offline. Digits and capitals measure about 1.1 × the text height per character, mixed text about 0.76–0.9 |
| `CreateItems` of `SchematicText`, `SchematicGraphicShape` (rectangle, circle), `SchematicLine` with type `SLT_GRAPHIC`, `SchematicTextBox`, `LocalLabel` | All accepted (`ISC_OK`) on the headless server and read back by `GetItems`. In the file they are `text`, `rectangle`, `circle`, `polyline` (two points), `text_box`, and `label` |
| Field text, API vs file | `reference_field` and `value_field` carry `text.position`, `attributes.angle`, `size`, `horizontal_alignment`, `vertical_alignment`, and `visible`; for parts with no rotation, position, size, justification, and angle match the file's `property` exactly |
| Label direction | A new `LocalLabel` with no spin style is saved as angle 180, `justify right bottom` (text running left). The reader maps spin styles and file angles (0, 90, 180, 270) to the way the text runs |
| Text size in a plan | KiCad's text height is 1.27 mm by default; `size` on text attributes is read as the larger of the two sides |

## Parts, fields, BOM, and footprints (10.99 and 10.0.7, Phase 10 probe)

Observed 25 Oct 2026 with `crates/kicad-session/tests/live_probe10.rs` (`KICAD_LIVE=1 cargo test -p kicad-session --test live_probe10 -- --nocapture`) and `kicad-cli sch export bom --help`.

| Topic | Observation |
|---|---|
| Custom fields over the API | `SchematicSymbolInstance.user_fields` is a list of `SchematicField`. An update with `field_mask` `user_fields` that carries the **whole list** (the old fields plus a new one with `visible: false`) is accepted (`ISC_OK`) and read back; the file gets `(property "MPN" "…" (at x y 0) (hide yes) (show_name no) (do_not_autoplace no) (effects …))` at the symbol's own position. Sending only the new field would replace the others, so a symbol's changes are sent in one update |
| Flags | `attributes` (`do_not_populate`, `exclude_from_bill_of_materials`, `exclude_from_board`, …) updates with field mask `attributes`; the file stores `(dnp yes)`, `(in_bom yes)`, `(on_board yes)` on the symbol (note the inversion for the exclude flags) |
| Standard fields | `datasheet_field`, `description_field`, `footprint_field`, `value_field` update by their own field-mask paths |
| Hidden property style | In the 10.99 files the hide flag sits on the property (`(hide yes)`); older files put `(hide yes)` inside `effects`. The writer copies the style of the symbol's own properties |
| Simulator fields | KiCad's simulator keeps `Sim.Device`, `Sim.Pins`, … as custom fields; they come back from `user_fields` and are ignored for the BOM |
| `kicad-cli sch export bom` | On 10.0.7 and 10.99: `--fields`, `--labels`, `--group-by`, `--sort-field`, `--exclude-dnp`, field and string delimiters, `--preset` / `--format-preset` (named presets saved in the editor). `${QUANTITY}` and `${DNP}` are generated fields. 10.99 adds `--variant` |
| Footprint libraries | `share/kicad/footprints/<Library>.pretty/<Name>.kicad_mod` under the install (155 libraries on 10.99); read as files, not through the API. A project's `fp-lib-table` may add libraries (`${KIPRJMOD}` expands to the project folder). The user's global table is not read |

## Boards (10.99, Phase 11 probe)

Observed 25 Oct 2026 with `crates/kicad-session/tests/live_probe11.rs` (`KICAD_LIVE=1 cargo test -p kicad-session --test live_probe11 -- --nocapture`; add `THINGSVU_WRITE_FIXTURE=1` to rewrite `tests/fixtures/v11/regulator-board/`).

| Topic | Observation |
|---|---|
| Opening a board headless | `kicad-cli api-server` takes `OpenDocument` with the PCB document type; `LoadAllLibraries` for footprints and symbols is accepted. A freshly opened board reports `DMS_UNMODIFIED` (note the enum: `DMS_UNMODIFIED = 1`, `DMS_MODIFIED = 2`) |
| `ImportNetlist` | Works headless, with `dry_run` and without. Matching by reference. The dry run's report names every footprint it would add and the real run's names every field and net it changed (`Added R3 …`, `Changed R3 value from R_0603_1608Metric to 1k`, `Connected R3 pin 1 to +3V3`). It returns error, warning, and new-footprint counts. New footprints are placed in a cluster near the origin. After the import the document is modified until saved |
| `GetItems` for a board | `KOT_PCB_FOOTPRINT` returns `FootprintInstance` with the reference and value fields, library id (`definition.id`), position, orientation (degrees), layer, lock state (`LS_UNLOCKED = 1`, `LS_LOCKED = 2`), attributes (`do_not_populate`), and the pads in `definition.items` |
| Pad positions | **Already placed on the board**: a pad at footprint offset (-0.775, 0) of a footprint at (11.355, 3.265) mm is reported at (10.58, 3.265) mm, although the proto comment says "relative to the parent footprint's origin". Nets come with the pad (`pad.net.name`), as `/VBUS` for a net named on a sheet |
| Other board commands | `GetNets` (names, including the empty net), `GetBoardEnabledLayers` (copper layer count and layer ids), `GetBoardStackup`, `GetBoardDesignRules`, and `GetBoardBoundingBox` (`BBBM_BOARD_EDGES_ONLY` gives a zero-size box for a board with no outline) are all handled |
| File layout | 10.99 (`version 20261018`): `(footprint "Lib:Name" (layer …) (transform (translate x y) (rotate r) (scale 1 1)) …)`, pads `(net "name")`, no net table, `(sheetname)`/`(sheetfile)` on footprints. KiCad 10.0 and earlier: `(at x y rot)`, `(net N "name")` pads, a `(net N "name")` table at the top. **The 10.0 CLI cannot open a 10.99 board** ("created with a more recent version") |
| `kicad-cli pcb drc` | `--format json --severity-all --units mm` writes `violations` (type, description, severity, items with a description and place), `unconnected_items`, `schematic_parity` (with `--schematic-parity`), and `ignored_checks`. An empty regulator board gives one error (`invalid_outline`: no edges on Edge.Cuts), silkscreen warnings, and 18 unconnected items (nothing is routed). Exit code is 0 unless `--exit-code-violations` |
| `kicad-cli pcb render` | 3D render to PNG, 0.2 s for the small board; options for side, size, background, quality; 10.99 only (KiCad 10.0's `kicad-cli` has no `pcb render`) |

| Library loading and slow answers | `LoadAllLibraries` returns at once, but the headless server keeps loading in the background and answers later requests slowly until it is done: with several servers running in parallel, `SaveCopyOfDocument` on a board timed out twice at 30 s. A board session therefore asks for the footprint libraries only when an import needs them, not when it is opened, and a copy that times out is asked for again once |

## Board editing (10.99, Phase 12 probe)

Observed 25 Oct 2026 with `crates/kicad-session/tests/live_probe12.rs` (`KICAD_LIVE=1 cargo test -p kicad-session --test live_probe12 -- --nocapture`), on a copy of the regulator board.

| Topic | Observation |
|---|---|
| Commits | `BeginCommit` / `EndCommit` work on a headless board. `CMA_DROP` undoes everything the commit did (checked: a failed outline apply left the board unchanged) |
| Outline | `CreateItems` with a `BoardGraphicShape` rectangle on `BL_Edge_Cuts` works, with `corner_radius` (saved as `(gr_rect … (radius 2))`). `GetBoardBoundingBox` with `BBBM_BOARD_EDGES_ONLY` then includes half the line width on every side (a 50 x 40 mm rectangle drawn 0.05 mm wide is reported 50.05 x 40.05 mm). `DeleteItems` removes a shape by id |
| `PlaceFootprintFromLibrary` | Works, but answers "not found" while a headless server is still loading the libraries (up to about 30 s after `LoadAllLibraries`). The footprint arrives named `REF**`. **Inside an open commit it cannot be updated** ("an item with UUID … does not exist") until the commit ends, so it cannot be renamed in the same undo step |
| A footprint from the library, created in a commit | `GetItemsFromLibrary` (footprint type) returns the `Footprint` definition: pads and shapes at the footprint's own coordinates, **no reference or value field**. A `FootprintInstance` built from it, with fields written by the client and every item moved to the place, is accepted by `CreateItems` in a commit, keeps the library id, and is saved with the given reference |
| **Moving a footprint with `UpdateItems`** | Changing `position` or `orientation` moves **only the anchor**: the pads, drawings, and text in the definition are sent back with their old places on the board, and KiCad keeps them, so the saved footprint has pads tens of millimetres from its origin. The field mask is ignored. A footprint sent with only id, position, orientation, and layer **loses its pads and fields** (reference becomes empty). What works: every item of the definition (pads with their pad-stack angle, shapes, texts, fields, zones) turned and shifted by the client, then the whole instance sent. `Footprint3DModel` items need no change |
| Lock | `locked: LS_LOCKED` through `UpdateItems` works. **The API moves a locked footprint anyway**, so the tools refuse it themselves |
| Flip | `FlipItems` flips a footprint with its pads (`BFD_LEFT_RIGHT` mirrors x and reports 180°; `BFD_TOP_BOTTOM` mirrors y). Setting `layer` to `B.Cu` through `UpdateItems` changes the layer but does not mirror the pads |
| Net classes and rules (12.5) | `GetNetClassForNets` answers with each net's class: clearance, track width, and the via's pad stack (drill and copper size); the regulator board has only `Default` (0.2 mm clearance and track, 0.6/0.3 mm via). `GetBoardDesignRules` gives the minimums (track width 0.2, copper to edge 0.5, minimum clearance 0 on that board). A via created with that pad stack keeps its sizes. Pads report their copper size, shape, turn, and layers in `pad_stack` (an SMD pad: `F.Cu`, `F.Mask`, `F.Paste`) |
| Zones (12.6) | A `Zone` with `CopperZoneSettings` (net, clearance, `min_thickness`, connection style and thermal spoke width and gap, island mode, fill mode) and a priority is created as given and saved with `(connect_pads (clearance …))`, `(min_thickness …)`, `thermal_gap`, `thermal_bridge_width`. Sent without settings, clearance and minimum width are **zero**. `RefillZones` inside the commit that creates a zone answers OK but leaves the new zone unfilled; after `EndCommit` it fills it. The document is then modified |
| Undo (12.8) | **The API has no undo.** `RevertDocument` reloads the document, losing every unsaved change; `RunAction` runs editor actions but is documented as not an API. A rollback is therefore a change of its own: the items recorded before (`GetItems`) are recreated with `CreateItems` (with new ids), added ones deleted, and moved footprints flipped back (`FlipItems`) and re-sent whole (`UpdateItems`), which puts every pad and drawing back where it was |
| Footprint attributes from a library (13.1) | `GetItemsFromLibrary` returns the footprint definition without its attributes (`exclude_from_pos_files`, `exclude_from_bom`, `board_only` live on the instance), so a footprint created from it has none. A mounting hole added by `pcb_outline_plan` before v0.8.0 was therefore in the position file; the instance now sets them as the library does |
| Footprint attributes by update (13b.2) | `UpdateItems` on a footprint **does** write its `FootprintAttributes` (`exclude_from_position_files`, `exclude_from_bill_of_materials`, `not_in_schematic`, `do_not_populate`) in place, in a commit, and a read back shows them (checked on the regulator board's J1: all four set, the other footprints untouched). So a hole from v0.7.0 is repaired by an update, not recreated. `not_in_schematic` is the file's `board_only` |
| Field text angle (13b.1) | The angle is stored relative to the symbol; KiCad draws it swapped between horizontal and vertical when the symbol is turned a quarter turn (`SCH_FIELD::GetDrawRotation`), and half turns and mirrors leave it. The readers now report the angle as drawn, so text boxes, the declutter, and the readability check see the text where it is |
| Manufacturing exports (13.1) | Not the API: `kicad-cli pcb export gerbers/drill/pos/step/pdf/svg` on a snapshot, the same flags in KiCad 10.0 and 10.99. `kicad-cli gerber info --strict` (10.99 only) reads a Gerber or Excellon file with the Gerber viewer's reader and fails on any parse warning. `--check-zones` refills zones in the copy before plotting. Without `--layers`, `pcb export gerbers` plots every enabled layer (24 on a two-layer board), so the layer list is always given |
| Freerouting 2.5.0 native build | Not KiCad, but recorded here: the GraalVM native Windows CLI (`freerouting-cli-2.5.0-windows-x64.zip`) fails in its routing thread on any KiCad DSN with component outlines (`UnsupportedFeatureError … ComponentOutline`) and hangs without exiting; the jar works |
| DSN wiring and the router (14.8) | `kicad-cli pcb export specctra` writes every existing track and via into `(wiring)` as `(wire (path LAYER WIDTH x y x y)(net NAME)(type protect))` and `(via "name" x y (net NAME)(type protect))`, locked or not, in the file's units (`(resolution um 10)`: tenths of a micrometre, y up). Freerouting keeps a protected wire and writes it again in its session. A net with an empty `(pins)` list in `(network)` is not routed and its pads stay obstacles. Net classes are written as `(class NAME nets… (rule (width …) (clearance …)))` and used for widths |
| Specctra round trip (12.7) | `RunBoardJobExportSpecctraDsn` (in `kiapi.board.jobs`) writes the DSN **from the board file on disk, not the live board**: after an unsaved placement it exports the old places. `kicad-cli pcb export specctra` on a saved copy gives the live state. The DSN carries the net classes (`(class kicad_default … (rule (width 200) (clearance 200)))`) and the placement. `ImportSpecctraSession` takes a `.ses` path, answers with a skipped count and a report, deletes the unlocked tracks and vias, adds the session's, and **places the footprints where the session says**; 41 tracks in 70 ms on the regulator board |
| Tracks, vias, zones (for 12.5 and 12.6) | `CreateItems` accepts a `Track` (width, layer, net by name), a `Via` (net; diameter and drill left at zero come back as zero and must be set), and a copper `Zone` (layers, outline, net). `RefillZones` with no ids fills every zone; the zone then reports `filled` with one polygon |

