# Architecture

How thingsvu-tools is built **as of version 0.1**. The design plan ([kicad-ai-mcp-design-plan.md](kicad-ai-mcp-design-plan.md)) holds the reasoning and the history; where the two differ, this page is right. [Changes from the plan](#changes-from-the-original-plan) lists what moved.

## The pieces

```mermaid
flowchart TB
    LLM["AI assistant<br/>(MCP host)"]

    subgraph MCP["kicad-ai-mcp: one process per assistant, stdio"]
        TOOLS["MCP tools<br/>session_*  sch_*<br/>plan store, idempotency keys,<br/>result envelope"]
    end

    DOMAIN["kicad-domain<br/>circuit spec, plans, layout engine,<br/>readability checks, capabilities, results"]
    ANY["kicad-session<br/>AnySession"]

    subgraph API["API sessions: KiCad 11"]
        SESS["Session<br/>headless or gui"]
        IPC["kicad-ipc<br/>protobuf over a native NNG client"]
        REG[("instance registry<br/>one JSON file per editor")]
    end

    subgraph FILE["File sessions: KiCad 10, or no KiCad running"]
        FS["FileSession<br/>verified copy, atomic replace"]
        SEXPR["kicad-sexpr<br/>parser, library reader,<br/>in-place writer"]
    end

    CLIR["kicad-cli-runner<br/>discovery, ERC, netlist, PNG"]
    CLI["kicad-cli"]
    HL["kicad-cli api-server<br/>headless, a child of the MCP process"]
    GUI["KiCad 11 editor"]
    PLUGIN["kicad-ai-plugin<br/>toolbar button: registers the editor"]
    FILES[("schematic files")]

    LLM <-->|"MCP JSON-RPC over stdio"| TOOLS
    TOOLS --> DOMAIN
    TOOLS --> ANY
    ANY --> SESS
    ANY --> FS
    SESS --> IPC
    IPC <-->|"private named pipe or socket"| HL
    IPC <-->|"socket and token from the registry"| GUI
    GUI -->|"runs it on a button press"| PLUGIN
    PLUGIN -->|"writes socket, token, version"| REG
    SESS -->|"reads"| REG
    FS --> SEXPR
    SEXPR <--> FILES
    SESS --> CLIR
    FS --> CLIR
    CLIR --> CLI
    CLI --> FILES
    HL <--> FILES
    GUI <--> FILES
```

Every provider produces the same kind of session, so the tools do not care which one they are talking to. What each can do is probed (API sessions) or fixed (file sessions) and reported in the capability matrix: [capability-matrix.md](capability-matrix.md).

## Crates

```mermaid
flowchart LR
    mcp["kicad-ai-mcp<br/>(binary)"] --> session
    mcp --> domain
    mcp --> runner
    plugin["kicad-ai-plugin<br/>(binary)"] --> session
    plugin --> ipc
    plugin --> domain
    plugin --> runner
    session["kicad-session"] --> ipc
    session --> sexpr
    session --> runner
    session --> domain
    sexpr["kicad-sexpr"] --> domain
    mcp --> sexpr
    domain --> mech["mech-model"]
    ipc["kicad-ipc"]
    runner["kicad-cli-runner"]
    domain["kicad-domain"]
    mock["kicad-ipc-mock<br/>(tests)"] -.-> ipc
    faults["kicad-fault-tests<br/>(tests)"] -.-> session
    faults -.-> mock
```

| Crate | Responsibility | Must not |
|---|---|---|
| `kicad-domain` | Circuit spec, validation, geometry, plans and their operations, the layout engine and its readability checks, the tidy planner, the arrange planner, and the router for existing designs, capability and result types | Depend on a transport, KiCad, or the file system |
| `kicad-ipc` | KiCad's protobuf API (vendored protos), the NNG `ipc` transport written in Rust, request deadlines, status mapping | Leak protobuf types upward |
| `kicad-cli-runner` | Finds KiCad installs; runs `kicad-cli` with fixed argument arrays (ERC, DRC, netlist, PNG, Gerbers, drill, positions, STEP, plots, Gerber read-back) and Freerouting; parses reports | Use a shell, or treat exit code 0 as proof of correctness |
| `kicad-sexpr` | Parses KiCad files keeping source positions; reads symbol libraries and schematics; writes a plan into a schematic by editing only the text the plan names (inserting new items, deleting old ones, changing a symbol's numbers) | Re-print a file; change its format version |
| `kicad-session` | `Session` (headless or gui, over the API), `FileSession`, and `AnySession` over both; capability probing; applying a plan and verifying it; the editor registry | Fall back to another backend after KiCad rejected a request |
| `kicad-ai-mcp` | The MCP tools, plan store, idempotency, result envelopes, logging | Write anything but MCP messages to stdout |
| `kicad-ai-plugin` | Runs when the user presses the toolbar button; registers the editor | Serve requests; stay running |
| `mech-model` | The board as a neutral mechanical model (outline, holes, keep-outs, connectors, heights), millimetres, versioned | Depend on KiCad or any other crate here |

## Sessions

| Provider | KiCad | Opened with | What it works on | Edits via |
|---|---|---|---|---|
| `headless` | 11 | `session_open` (a file, or `create: true`) | A private `kicad-cli api-server` the MCP process starts and owns | The API, in two commits |
| `gui` | 11 | The user presses **KiCad AI: connect**, then `session_open` with `provider: gui` | The user's open Schematic Editor | The API, in two commits; each is one undo step in the editor |
| `file` | 10 or 11 | `session_open` with `provider: file` | The `.kicad_sch` file, when no editor has it open | Text edits, verified in a copy first |

One provider works on a document at a time. A file that an editor has open (its `~name.kicad_sch.lck` lock file) is never edited through the headless or file provider.

### How the editor registration works

There is no protocol between the MCP server and the plugin. KiCad only passes its API socket and token to a plugin it launches, so the plugin writes them down and the MCP server picks them up.

```mermaid
sequenceDiagram
    actor User
    participant KiCad as KiCad 11 editor
    participant Plugin as kicad-ai-plugin
    participant Registry as instance registry (files)
    participant MCP as kicad-ai-mcp

    User->>KiCad: presses "KiCad AI: connect"
    KiCad->>Plugin: runs it (socket path and token in its environment)
    Plugin->>KiCad: GetVersion, GetOpenDocuments
    Plugin->>Registry: writes socket, token, version, open projects
    Plugin-->>User: "connected" dialog, then exits
    MCP->>Registry: session_open (provider gui) reads the records
    MCP->>KiCad: connects to the socket, pins the token
    Note over MCP,KiCad: records of editors that no longer answer are removed
```

## Planning and applying a change

An assistant describes **what** the circuit is (parts and which pins share a net). The tools decide **where** things go, show the plan, and only change the design after the user confirms.

```mermaid
flowchart LR
    SPEC["Circuit spec<br/>parts + nets"] --> VAL["Validate<br/>ids, pins, limits"]
    VAL --> LIB["Library check<br/>symbols exist, pins exist"]
    LIB --> ROLES["Roles<br/>power / ground / signal nets<br/>main / shunt / attached parts"]
    ROLES --> LAYOUT["Layout<br/>signal flow, power symbols,<br/>wires, hub layout for ICs"]
    LAYOUT --> READ["Readability checks<br/>shorts, overlaps,<br/>wires through parts"]
    READ --> PLAN["Operation plan<br/>shown to the user"]
```

Applying differs by provider, but both end in the same check: KiCad's own netlist must show exactly the planned nets.

```mermaid
sequenceDiagram
    participant Agent as AI assistant
    participant MCP as kicad-ai-mcp
    participant S as Session (API)
    participant K as KiCad (headless or editor)

    Agent->>MCP: sch_apply_plan (confirm: true, idempotency key)
    MCP->>S: apply_plan
    S->>K: commit 1: place symbols
    S->>K: commit 2: values, wires, labels, junctions
    S->>K: read the netlist back
    K-->>S: nets
    S-->>MCP: verified, or a mismatch report
    MCP-->>Agent: result with provenance
```

```mermaid
sequenceDiagram
    participant Agent as AI assistant
    participant MCP as kicad-ai-mcp
    participant F as FileSession
    participant CLI as kicad-cli
    participant File as schematic file

    Agent->>MCP: sch_apply_plan (confirm: true, idempotency key)
    MCP->>F: apply_plan
    F->>File: still the version the plan was made for? no editor lock file?
    F->>File: write a verification copy next to it
    F->>CLI: export the copy's netlist
    CLI-->>F: nets
    alt exactly the planned nets
        F->>File: unchanged since? then keep a backup and replace the file in one step
        F-->>MCP: verified
    else anything else (crash, hang, wrong nets, file changed)
        F->>File: delete the copy, change nothing
        F-->>MCP: error, nothing was changed
    end
```

## Tidying an existing schematic

The same engine that lays out a new circuit can lay out an existing one. The circuit is read back from the sheet (the parts, and the nets KiCad reports), laid out again, and the difference becomes a plan of **moves** (each part keeps its identity, values, footprint, and fields), **removals** (the old wires, labels, junctions, and net-naming power symbols), and **additions** (the new drawing). One plan store and one apply tool serve both new circuits and tidies.

```mermaid
flowchart LR
    SHEET["The sheet<br/>parts, wires, labels,<br/>junctions, notes"] --> CHECK{"Anything tidying<br/>cannot redraw?<br/>buses, global labels,<br/>sheets"}
    CHECK -->|yes| REFUSE["Refused, nothing changed<br/>the reason is reported"]
    CHECK -->|no| SPEC["Circuit spec<br/>parts + KiCad's nets"]
    NETS["Nets<br/>(power symbols and flags<br/>attached from the drawing)"] --> SPEC
    SPEC --> LAYOUT["Layout engine<br/>(same as for new circuits)<br/>keeps clear of text notes"]
    LAYOUT --> DIFF["Plan<br/>moves, removals, additions<br/>identical items cancel out"]
    DIFF --> REVIEW["User reviews it<br/>before and after readability"]
    REVIEW --> APPLY["sch_apply_plan<br/>nets must be identical,<br/>ERC no worse"]
```

### Placing parts by hand, then reconnecting

Moving parts and drawing the connections are separate steps, so an assistant can place everything where it belongs and wire once. `sch_arrange_plan` changes only the named parts (wires stay and break); the server remembers the nets KiCad reported just before the first such change; `sch_reconnect_plan` then removes the old wiring and draws it again around the parts where they are, with a router that never crosses a part or touches another net (and falls back to labels). The same router draws the wires when `sch_tidy_plan` is given layout hints.

```mermaid
flowchart LR
    DESCRIBE["sch_describe_layout<br/>positions, bodies, pins, nets"] --> ARRANGE["sch_arrange_plan<br/>move, turn, mirror, delete<br/>(wires stay and break)"]
    ARRANGE -->|"apply: nets remembered<br/>from just before"| BROKEN["Parts in place,<br/>connections broken"]
    BROKEN --> RECONNECT["sch_reconnect_plan<br/>router: stubs, power symbols,<br/>wires around parts, labels as fallback"]
    RECONNECT --> VERIFY["sch_apply_plan<br/>nets must match the remembered circuit"]
    HINTS["sch_tidy_plan hints<br/>keep, place, near"] --> ROUTER["Layout engine places the rest,<br/>router draws the wires"]
    ROUTER --> VERIFY
```

### Text, labels, and drawings (Phase 9)

KiCad decides where a part's reference and value text lands, and its API cannot report how far text extends (`GetBoundingBox` is not handled for schematics). The server therefore reads where the text is (position, angle, size, justification), estimates the area it covers, and checks it against other text, wires, and other parts' bodies. `sch_declutter_plan` moves the text that runs into something; the label, drawing, and item planners are ordinary plans, so they use the same confirm, apply, and verify flow. After applying, every position, turn, and text move is read back and compared with the plan.

```mermaid
flowchart LR
    READ["Read the sheet<br/>parts + field text, labels,<br/>notes, drawings (file or API)"] --> BOXES["Text boxes<br/>estimated from string, size,<br/>angle, justification"]
    BOXES --> CHECK["TEXT_OVERLAP<br/>in sch_review and plans"]
    BOXES --> DECLUTTER["sch_declutter_plan<br/>move field text, turn labels,<br/>hide PWR_FLAG text"]
    READ --> LABELS["sch_label_plan<br/>add, rename, turn, delete<br/>refuses a second name or a join"]
    READ --> DRAW["sch_draw_plan<br/>shapes, notes, titled box"]
    READ --> ITEMS["sch_edit_items_plan<br/>delete or move by id"]
    DECLUTTER --> APPLY["sch_apply_plan"]
    LABELS --> APPLY
    DRAW --> APPLY
    ITEMS --> APPLY
    APPLY --> VERIFY["Nets checked against KiCad,<br/>positions and text read back"]
```

### Parts, footprints, and BOM (Phase 10)

The server reads every field of every part (standard and custom) and the do-not-populate and exclude flags, from the API or the file. The BOM is built from that; footprints are read from the library files on disk. Part data that does not exist yet (manufacturer, part numbers) comes from outside: the review hands the assistant a list of lookups, `parts_lookup` (`kicad-domain::parts`) turns each into the calls to make on the pcbparts MCP, which is a separate server the installer offers to add, and the answers are written back as hidden fields by an ordinary plan. The server itself makes no network call. A `PartsSource` says where answers come from, so a source built into the server can take over the same tool without a change for the assistant.

```mermaid
flowchart LR
    SHEET["The sheet<br/>all fields, DNP and<br/>exclude flags"] --> BOM["bom_get<br/>grouped lines"]
    SHEET --> REVIEW["bom_review<br/>what is missing,<br/>lookups to do"]
    LIBS["Footprint libraries<br/>install + project<br/>(.kicad_mod files)"] --> FP["fp_search, fp_get<br/>sch_footprint_plan<br/>pads vs pins, package"]
    REVIEW -->|"lookup list"| PL["parts_lookup (stub)<br/>calls to make, fields<br/>to record; no network"]
    PL -->|"source: pcbparts MCP<br/>(later: built in)"| ASSISTANT["Assistant calls the<br/>pcbparts MCP<br/>(outside the server)"]
    ASSISTANT -->|"part numbers"| FIELDS["sch_set_fields_plan<br/>hidden fields, flags"]
    FP --> APPLY["sch_apply_plan<br/>nets unchanged,<br/>every field read back"]
    FIELDS --> APPLY
    APPLY --> EXPORT["bom_export<br/>CSV via kicad-cli<br/>generic or JLCPCB"]
```

### Boards (Phases 11 to 13)

A board is a second kind of session, with its own tools (`pcb_*`) and its own model. One board model (footprints with placed pads, tracks, vias, zones, outline, nets) is filled from a live KiCad (the headless server or the PCB editor, through the API) or from the `.kicad_pcb` file (KiCad 10 or 11). DRC and the 3D render run through `kicad-cli` on a snapshot of the live board. The review compares the board with the schematic next to it. Updating the board uses KiCad's own netlist import: a dry run is the plan, and applying it is read back and compared with the netlist.

Board edits (Phase 12: outline, mounting holes, moves) are planned in `kicad-domain::board_edit` from the board model: the plan lists the operations and the board it should leave, measured (overlaps, parts outside the outline) before and after. `pcb_apply_plan` applies a plan in one API commit (dropped on any failure, so nothing stays half done) and `verify_edit` compares the board read back with the plan, down to every pad's place and net. Because KiCad moves only a footprint's anchor when its position changes, the session turns and shifts every item inside the footprint itself (`kicad-session::board::edit`). The placement engine (`kicad-domain::placement`) produces the same kind of plan: a constructive pass and a deterministic improvement loop over one cost (wire length, overlaps, outline, decoupling, connector edges, hints), with blocks taken from rectangles drawn on the schematic. Routing (`kicad-domain::routing`) checks each planned track and via against every other net's copper (pads by their turned copper box and layers, tracks, vias, holes) and the board edge, with clearances and sizes read from the board's net classes and minimums (`BoardSession::routing_rules`); applying a route plan runs DRC before and after. Copper pours (`kicad-domain::zones`) are created in the same commit with every setting given, refilled right after it (a refill inside the commit does not see the new zone), and judged by DRC the same way. The optional autorouter (`kicad-cli-runner::freerouting`) runs Freerouting headless in its own process on a DSN exported by `kicad-cli` from a snapshot of the live board (KiCad's own export job reads the file on disk); `pcb_apply_plan` imports the session through the API and checks that no part moved and DRC is no worse.

Every board change is recorded before it is applied (`BoardSession::snapshot`: footprints, tracks, vias, zones, drawings as KiCad sends them). The KiCad API has no undo, so a change that fails its read-back or makes DRC worse is put back by a change of its own (`restore`: delete what was added, recreate what was removed, flip and re-send what moved), and the board is read again to confirm it is as it was.

```mermaid
flowchart LR
    LIVE["Live board<br/>headless server or<br/>PCB editor (API)"] --> MODEL["Board model<br/>footprints + pads,<br/>tracks, vias, zones, outline"]
    FILE[".kicad_pcb file<br/>KiCad 10 or 11<br/>(kicad-sexpr)"] --> MODEL
    MODEL --> READ["pcb_info, pcb_list_*,<br/>pcb_get_footprint"]
    MODEL --> REVIEW["pcb_review<br/>overlaps, outline, edge and<br/>decoupling rules, vs schematic"]
    LIVE -->|"snapshot"| CLI["kicad-cli<br/>pcb drc, pcb render"]
    FILE --> CLI
    CLI --> REVIEW
    SCH["Schematic file<br/>next to the board"] -->|"netlist export"| PLAN["pcb_update_from_schematic_plan<br/>ImportNetlist dry run"]
    PLAN --> APPLY["pcb_apply_plan<br/>real import, read back,<br/>every pad net checked"]
    APPLY --> LIVE
    MODEL --> EDIT["pcb_outline_plan, pcb_move_plan,<br/>pcb_footprint_attrs_plan,<br/>pcb_track_edit_plan<br/>pcb_rules_plan (rules.rs)<br/>board_edit: operations +<br/>expected board, measured"]
    MODEL --> PLACE["pcb_place_plan<br/>placement: constructive pass,<br/>improvement loop, one cost"]
    SCH -->|"boxes drawn<br/>around parts"| PLACE
    PLACE -->|"move plan"| APPLYE
    MODEL --> ROUTE["pcb_route_plan<br/>routing: paths, clearance<br/>vs other nets and edge"]
    RULES["Net classes and minimums<br/>(GetNetClassForNets,<br/>GetBoardDesignRules)"] --> ROUTE
    ROUTE -->|"tracks + vias"| APPLYE
    MODEL --> ZONE["pcb_zone_plan<br/>zones: net, layers, area,<br/>clearance, thermals"]
    RULES --> ZONE
    ZONE -->|"zones; refill after<br/>the commit"| APPLYE
    LIVE -->|"snapshot"| DSN["kicad-cli pcb export<br/>specctra"]
    DSN -->|"nets to route, rip-up:<br/>text edits (dsn.rs)"| FR["Freerouting (optional,<br/>own process tree, no network)"]
    FR -->|"session (.ses)"| AUTO["pcb_autoroute_plan<br/>summary per net,<br/>what really changes"]
    AUTO -->|"ImportSpecctraSession"| LIVE
    APPLYE -->|"every plan but a move"| DRCBA["DRC before and after<br/>unrouted count, no new errors"]
    DRCBA -->|"problems"| ROLL["Rollback<br/>snapshot restored,<br/>board read again"]
    EDIT --> APPLYE["pcb_apply_plan<br/>one commit: shapes, holes,<br/>flips, footprints moved<br/>with every item"]
    APPLYE --> LIVE
    APPLYE -->|"read back"| VERIFY["verify_edit<br/>outline, places, turns,<br/>sides, locks, pads, nets"]
```

### Working on a board you do not own alone (Phase 14)

An editor, another program, or a version control checkout can change the files while a session is open, and a plan can delete things. Four pieces keep that safe, each reached by a tool, so no project file is edited by hand.

- **Freshness.** Every session keeps the modified time and size of its file as it last read or saved it (`kicad-session::freshness`). `pcb_info` and `session_status` report `unsaved_changes` (the editor's own flag for a live session), `file_changed_on_disk`, and `read_only`. `pcb_reload` and `session_reload` read the file again (`RevertDocument` for a live session, which does read a file changed on disk; the file provider reads it at every call), and refuse while there are unsaved changes unless told to drop them, listing what would be lost.
- **Checkpoints.** `checkpoint_create` copies the project's `.kicad_pcb`, `.kicad_sch`, and `.kicad_pro` (a live session's open documents are saved to a copy first) into `.thingsvu/checkpoints/<time>/` with a manifest of hashes. `checkpoint_diff` compares two of them, or one with the project now, in terms of parts, nets, tracks, zones, and rules (`kicad-domain::changes`), and `checkpoint_restore` writes them back through temporary files, after taking a checkpoint of the present, and reloads the sessions on the project. `pcb_apply_plan` can take one itself (`checkpoint`).
- **Track and rule edits.** `pcb_track_edit_plan` (`kicad-domain::track_edit`) selects copper by net, layer, area, or id and deletes it, widens it, or locks it; locked copper is skipped and listed. `pcb_rules_plan` (`kicad-domain::rules`) changes net classes, assignments, and minimums through the API in a live session and through the `.kicad_pro` JSON, checked with `kicad-cli` DRC, in a file session. Both go through `pcb_apply_plan`, so they are read back and rolled back like any board change (the rollback also puts back tracks that were changed in place).
- **Autorouting under control.** `pcb_autoroute_plan` edits the Specctra file as text (`kicad-sexpr::dsn`): a net given no pins is left alone, and with `keep_existing: false` the unlocked tracks of the routed nets are taken out of the wiring. The time limit ends the router and every process it started (`kicad-cli-runner`).

```mermaid
flowchart LR
    SESSION["Board or schematic<br/>session"] --> FRESH["Freshness stamp<br/>file time + size,<br/>unsaved flag"]
    FRESH -->|"file_changed_on_disk"| RELOAD["pcb_reload, session_reload<br/>RevertDocument or reread;<br/>unsaved work listed first"]
    SESSION --> CKPT["checkpoint_create<br/>.thingsvu/checkpoints/<br/>files + hashes"]
    CKPT --> CDIFF["checkpoint_diff<br/>parts, nets, tracks,<br/>zones, rules"]
    CKPT --> CREST["checkpoint_restore<br/>checkpoint of now first,<br/>then write and reload"]
    CREST --> RELOAD
    PLAN["pcb_track_edit_plan<br/>pcb_rules_plan"] -->|"pcb_apply_plan<br/>checkpoint: label"| CKPT
    PLAN --> SESSION
```

### Manufacturing outputs (Phase 13)

`pcb_export` and `pcb_fab_package` work on one snapshot of the board (the live board saved as a copy, or the file itself), so the checks, the outputs, and their read-back all see the same board. What the board model leaves out (the `Edge.Cuts` shapes, silkscreen extents, drilled holes, rule areas, footprint attributes, fields, and 3D models, the board thickness) is read from that copy by `kicad-sexpr::fab`. The checks (`kicad-domain::fab`) come first; a failed one stops the export unless the user accepts it, and the acceptance is written to the checks report. The outputs are made by `kicad-cli` into a scratch folder, read back (the Gerber job file for the layer count and outline, `kicad-cli gerber info --strict` for every Gerber and drill file, the drill hits against the board's holes, the placement rows against the parts, the zips reopened) and only then copied into the project. `pcb_mechanical_info` turns the same facts into the neutral `mech-model`, which has no KiCad types, for the enclosure tools to come.

```mermaid
flowchart LR
    BOARD["Board session<br/>live or file"] -->|"snapshot"| COPY[".kicad_pcb copy"]
    COPY --> FACTS["kicad-sexpr::fab<br/>edges, silkscreen, holes,<br/>rule areas, attributes"]
    COPY -->|"kicad-cli pcb drc"| DRC["DRC summary"]
    SCH["Schematic file"] -->|"BOM"| CHECKS
    FACTS --> CHECKS["Pre-export checks<br/>outline closed, DRC,<br/>BOM lines, silkscreen"]
    DRC --> CHECKS
    CHECKS -->|"a failure, not accepted"| STOP["Nothing written;<br/>failures returned"]
    CHECKS -->|"pass, or accepted"| OUT["kicad-cli pcb export<br/>gerbers, drill, pos, step,<br/>pdf, svg; sch export bom"]
    OUT --> BACK["Read back<br/>job file, gerber info --strict,<br/>holes, parts, zips"]
    BACK -->|"all as expected"| PROJ[("fab folder<br/>files, zips,<br/>checks report")]
    FACTS --> MECH["kicad-domain::mechanical"]
    MECH --> MODEL["mech-model<br/>outline, holes, keep-outs,<br/>connectors, heights"]
```

How the three providers apply it:

| Provider | Applying | If it fails |
|---|---|---|
| `gui` | Two commits through the API (two undo steps); each part's text is moved with it, because the API leaves it behind | Before the first commit: nothing changed. After it: the error says how many times to press Ctrl+Z |
| `headless` | The same two commits; refuses to start with unsaved changes | The saved file is reloaded, undoing everything |
| `file` | The text edits go into a verified copy, which replaces the file in one step | The file is untouched |

## Releases

```mermaid
flowchart LR
    BUMP["Version changed in Cargo.toml<br/>pushed to main"] --> CHECK{"Tag v#lt;version#gt;<br/>exists?"}
    CHECK -->|yes| STOP["nothing to do"]
    CHECK -->|no| TEST["tests + release build<br/>(Windows, Linux)"]
    TEST --> PKG["package.ps1<br/>zip, PCM zip, installer"]
    PKG --> SMOKE["installer smoke test<br/>in a sandbox"]
    SMOKE --> PUB["tag + GitHub release<br/>with files and SHA256SUMS"]
```

See [releasing.md](releasing.md).

## Changes from the original plan

The plan was written before any code existed. These are the places where building it led somewhere else.

| Plan | What was built | Why |
|---|---|---|
| A `KiCadBackend` trait with IPC, CLI, and file backends chosen per operation | `AnySession`, an enum over `Session` (API) and `FileSession`; the CLI is a helper both use | Only two real kinds of session exist; a trait added nothing, and each operation's backend is reported in the capability matrix anyway |
| A plugin that is a bridge with its own protocol (`kicad-ai-bridge/v1`) to the MCP server | A **registrar**: the plugin writes the editor's socket and token to a file and exits; the MCP server connects to KiCad directly | KiCad gives the plugin its connection details but the plugin does not need to stay alive to serve requests; this removes a protocol, a process, and a failure mode |
| A `DirectSocketProvider` to attach to a running KiCad 10 | Not built | KiCad 10's API answers almost nothing and only the first KiCad on Windows gets an endpoint; the lock file is a better guard against an open editor |
| A lossless syntax tree in `kicad-sexpr` | Source-position parser plus a writer that edits text in place | Nothing is re-printed, so everything a plan does not name is untouched by construction; Phase 8 added deleting items and changing a symbol's position and its fields' positions |
| Packaging as a PCM package and a PowerShell installer | A release zip with `install.ps1`, a PCM zip (experimental), and release automation driven by the Cargo version | PCM cannot install the MCP server, and the plugin alone is not useful; the installer sets up both |
| Layout was not in the plan | `kicad-domain::layout` and `::readability`, with golden circuits | Generated schematics that were correct but unreadable were not acceptable |
| Plan operations: place symbols, add wires and labels | Also power symbols, junctions, mirrored placement, and (Phase 8) moving and removing existing items | Needed for a schematic that reads like a person drew it, and for tidying one that exists |
