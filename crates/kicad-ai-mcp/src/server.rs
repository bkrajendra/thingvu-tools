// SPDX-License-Identifier: GPL-3.0-or-later
//! MCP tools: sessions, schematic reads, and planned schematic changes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use kicad_cli_runner::KicadInstall;
use kicad_domain::{plan_circuit, CircuitSpec, ErrorCode, OperationPlan, Origin, Provenance, ResultState, ToolError};
use kicad_session::registry::{self, Registration};
use kicad_session::{AnySession, FileSession, OpenTarget, Provider, Session, SessionConfig};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell};

use crate::envelope;
use crate::plans::{plan_view, PlanStore, StoredPlan};

#[path = "checkpoint_tools.rs"]
mod checkpoint_tools;
#[path = "fab_tools.rs"]
mod fab_tools;
#[path = "pcb_tools.rs"]
mod pcb_tools;

const INSTRUCTIONS: &str =
    "KiCad schematic and board tools. Start with session_status to see the KiCad installs and what each \
session can do, then session_open (a .kicad_sch) or pcb_open (a .kicad_pcb), or create: true for a \
new schematic. Use the headless or gui provider with KiCad 11; with KiCad 10 (or a file no editor \
has open) use the file provider, which edits the file itself and checks the result with kicad-cli \
before replacing it. Every result has ok, result_state, and provenance; a check that could not run \
is reported as not_run, never as passing. Every change is a plan: a *_plan tool describes it, you \
show it to the user, then sch_apply_plan or pcb_apply_plan (confirm: true, a fresh idempotency_key) \
applies it in one step, reads the result back from KiCad, and reports any mismatch; pcb_apply_plan \
can take a checkpoint first (checkpoint: label). Do not edit .kicad_* files by hand: there is a tool \
for each step below. Schematic: sch_plan_changes (circuit as components and nets, with no_connects \
and power_flags), sch_pin_net_plan (move or swap a pin's net), sch_page_plan (page size and title block), sch_reconnect_plan, sch_label_plan, \
sch_draw_plan, sch_edit_items_plan, sch_tidy_plan, sch_arrange_plan, sch_declutter_plan; \
sch_set_fields_plan sets Value, Footprint, MPN and other fields on many parts at once (sch_set_field \
for one); sch_footprint_plan assigns footprints; sym_search and sym_get find symbols and their pins, \
fp_search and fp_get find footprints; sch_run_erc and sch_render check the result. Board: \
pcb_update_from_schematic_plan brings the schematic in; pcb_outline_plan, pcb_place_plan, \
pcb_move_plan, pcb_footprint_attrs_plan (DNP, BOM, position-file flags), pcb_footprint_edit_plan (pad hole, size, \
zone connection; reference and value text), pcb_zone_plan; pcb_rules \
and pcb_rules_plan read and change net classes, assignments, and minimums; pcb_route_plan draws the \
tracks you name, pcb_autoroute_plan routes with Freerouting (whole board, chosen nets, or all but \
some); pcb_track_edit_plan rips up, re-widths, locks, and unlocks tracks and vias that exist; \
pcb_list_tracks, pcb_list_footprints (detail: full), pcb_list_drawings, and pcb_list_nets read the \
board; pcb_run_drc, pcb_review, and pcb_render check it. Sessions and safety: pcb_info and \
session_status say when the file changed on disk or the editor has unsaved changes; pcb_reload and \
session_reload read the file again; checkpoint_create, checkpoint_diff, and checkpoint_restore keep \
and restore copies of the project's files. To order a board: bom_review lists what the parts need \
(footprints, values, MPN, supplier numbers), parts_lookup turns that into the calls to make on the \
pcbparts MCP (a separate server the assistant has; the installer offers to add it), \
sch_set_fields_plan records the answers, bom_export writes the BOM, and pcb_fab_package makes the Gerber, drill, BOM, and placement files for a board \
house (pcb_export for a single kind of file). Save with sch_save or pcb_save (a file session is \
saved whenever a plan is applied). Close sessions with session_close or pcb_close when done.";

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Explicitly configured kicad-cli paths, tried before automatic discovery.
    pub configured_cli: Vec<PathBuf>,
    pub session: SessionConfig,
    /// Reject every tool that changes a schematic or writes files into a project.
    pub read_only: bool,
    /// The optional external autorouter; none unless configured.
    pub freerouting: Option<kicad_cli_runner::freerouting::Freerouting>,
}

struct State {
    options: Options,
    installs: OnceCell<Vec<KicadInstall>>,
    sessions: Mutex<BTreeMap<String, Arc<AnySession>>>,
    plans: Mutex<PlanStore>,
    /// The nets a session's circuit is meant to have, kept from just before its first arrange
    /// (moving or deleting parts leaves wires behind, so KiCad no longer reports them). Used by
    /// sch_reconnect_plan and dropped once that plan is applied.
    intended: Mutex<BTreeMap<String, Vec<kicad_domain::NetInfo>>>,
    /// Open board sessions (the pcb_* tools).
    boards: Mutex<BTreeMap<String, Arc<kicad_session::BoardSession>>>,
    /// Updates of a board from its schematic that were planned and wait to be applied.
    pending_updates: Mutex<BTreeMap<String, pcb_tools::PendingUpdate>>,
}

#[derive(Clone)]
pub struct KicadAiServer {
    state: Arc<State>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusArgs {
    /// Report one session. Omit to list every session and the KiCad installs found.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderArg {
    /// A KiCad API server started and owned by this MCP server (KiCad 11 / 10.99).
    #[default]
    Headless,
    /// The KiCad 11 editor the user has open, after they pressed the "KiCad AI" toolbar button
    /// (kicad-ai-plugin). Changes appear live in the editor and can be undone there.
    Gui,
    /// The schematic file itself, for KiCad 10 or any file no editor has open. Needs no running
    /// KiCad; every applied plan is verified with kicad-cli and then written to the file.
    File,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenArgs {
    /// Absolute path of the .kicad_sch file. Required for headless and file; for gui, it selects
    /// the project (root schematic) when the editor has more than one open.
    #[serde(default)]
    pub schematic_path: Option<String>,
    /// Headless and file only: create a new schematic at schematic_path instead of opening an
    /// existing one (a file session writes the empty file immediately).
    #[serde(default)]
    pub create: bool,
    #[serde(default)]
    pub provider: ProviderArg,
    /// File provider only: open a file that a KiCad editor has open (its lock file is present), for
    /// reading only. Every tool that would change the file is refused for this session.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseArgs {
    pub session_id: String,
    /// Close even if the document has unsaved changes (they are lost).
    #[serde(default)]
    pub discard_unsaved: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PagePlanArgs {
    pub session_id: String,
    /// A standard size: A5, A4, A3, A2, A1, A0, A, B, C, D, E (ANSI), USLetter, USLegal, USLedger, GERBER.
    #[serde(default)]
    pub size: Option<String>,
    /// A custom page: both sides in mm (50 to 1219.2). Not with `size`.
    #[serde(default)]
    pub width_mm: Option<f64>,
    #[serde(default)]
    pub height_mm: Option<f64>,
    /// `landscape` or `portrait` (standard sizes only).
    #[serde(default)]
    pub orientation: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    /// Comment lines by number, "1" to "9"; an empty text clears one.
    #[serde(default)]
    pub comments: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PartsLookupArgs {
    pub session_id: String,
    /// Only the BOM lines that contain one of these references. Default: every line that lacks
    /// a part number or a supplier part number.
    #[serde(default)]
    pub references: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MirrorArg {
    None,
    /// Flipped top to bottom.
    X,
    /// Flipped left to right.
    Y,
}

impl From<MirrorArg> for kicad_domain::Mirror {
    fn from(m: MirrorArg) -> Self {
        match m {
            MirrorArg::None => kicad_domain::Mirror::None,
            MirrorArg::X => kicad_domain::Mirror::X,
            MirrorArg::Y => kicad_domain::Mirror::Y,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PartEditArgs {
    /// The part's reference, e.g. R1 (from sch_describe_layout).
    pub reference: String,
    /// Move to this place on the sheet, in mm (give both x_mm and y_mm; y grows downward).
    #[serde(default)]
    pub x_mm: Option<f64>,
    #[serde(default)]
    pub y_mm: Option<f64>,
    /// Or move by this much from where it is now.
    #[serde(default)]
    pub dx_mm: Option<f64>,
    #[serde(default)]
    pub dy_mm: Option<f64>,
    /// Turn counter-clockwise (as seen on screen) by 90, 180, or 270 degrees from where it is now.
    #[serde(default)]
    pub rotate_deg: Option<u16>,
    /// Set the mirroring.
    #[serde(default)]
    pub mirror: Option<MirrorArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArrangeArgs {
    pub session_id: String,
    /// Parts to move, turn, or mirror.
    #[serde(default)]
    pub edits: Vec<PartEditArgs>,
    /// References of parts to delete. Their wires stay until sch_reconnect_plan.
    #[serde(default)]
    pub delete: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaceHintArgs {
    pub reference: String,
    pub x_mm: f64,
    pub y_mm: f64,
    /// Turn counter-clockwise by 90, 180, or 270 degrees from where the part is now.
    #[serde(default)]
    pub rotate_deg: Option<u16>,
    #[serde(default)]
    pub mirror: Option<MirrorArg>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SideArg {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NearHintArgs {
    pub reference: String,
    /// The part it goes next to.
    pub to: String,
    pub side: SideArg,
    /// Clear space between the two, in mm (default 10).
    #[serde(default)]
    pub gap_mm: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TidyArgs {
    pub session_id: String,
    /// Parts that stay exactly where they are (e.g. the connector the user wants on the left).
    #[serde(default)]
    pub keep: Vec<String>,
    /// Parts that go to a given place.
    #[serde(default)]
    pub place: Vec<PlaceHintArgs>,
    /// Parts that go next to another part (e.g. the two input capacitors together).
    #[serde(default)]
    pub near: Vec<NearHintArgs>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirArg {
    Left,
    Right,
    Up,
    Down,
}

impl From<DirArg> for kicad_domain::Dir {
    fn from(d: DirArg) -> Self {
        match d {
            DirArg::Left => kicad_domain::Dir::Left,
            DirArg::Right => kicad_domain::Dir::Right,
            DirArg::Up => kicad_domain::Dir::Up,
            DirArg::Down => kicad_domain::Dir::Down,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelAddArgs {
    /// The net name the label gives (up to 128 characters).
    pub text: String,
    /// Put it on a part's pin: `R1.2` (reference, a dot, the pin number).
    #[serde(default)]
    pub pin: Option<String>,
    /// Or on a wire or pin at this place, in mm (give both).
    #[serde(default)]
    pub x_mm: Option<f64>,
    #[serde(default)]
    pub y_mm: Option<f64>,
    /// The way the text runs from its anchor; chosen to keep clear of other text when omitted.
    #[serde(default)]
    pub direction: Option<DirArg>,
    /// Allow the label to join this net with an existing net of the same name.
    #[serde(default)]
    pub allow_join: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelRenameArgs {
    /// The label's id (from sch_describe_layout).
    pub id: String,
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelTurnArgs {
    pub id: String,
    pub direction: DirArg,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelPlanArgs {
    pub session_id: String,
    /// Labels to add.
    #[serde(default)]
    pub add: Vec<LabelAddArgs>,
    /// Labels to give another text (the net takes the new name).
    #[serde(default)]
    pub rename: Vec<LabelRenameArgs>,
    /// Labels to turn to run another way.
    #[serde(default)]
    pub turn: Vec<LabelTurnArgs>,
    /// Ids of labels to delete.
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlignArg {
    Left,
    Center,
    Right,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DrawItemArgs {
    /// A rectangle from its top left corner.
    Rectangle {
        x_mm: f64,
        y_mm: f64,
        width_mm: f64,
        height_mm: f64,
        #[serde(default)]
        stroke_mm: Option<f64>,
        #[serde(default)]
        filled: bool,
    },
    Circle {
        x_mm: f64,
        y_mm: f64,
        radius_mm: f64,
        #[serde(default)]
        stroke_mm: Option<f64>,
        #[serde(default)]
        filled: bool,
    },
    Line {
        x1_mm: f64,
        y1_mm: f64,
        x2_mm: f64,
        y2_mm: f64,
        #[serde(default)]
        stroke_mm: Option<f64>,
    },
    /// A text note anchored at its left end (or as `align` says), sitting on its baseline.
    Text {
        x_mm: f64,
        y_mm: f64,
        text: String,
        #[serde(default)]
        size_mm: Option<f64>,
        #[serde(default)]
        rotate_deg: Option<u16>,
        #[serde(default)]
        align: Option<AlignArg>,
    },
    /// A rectangle around the named parts (bodies, pins, and their text) with an optional title
    /// inside at the top left, for marking a block such as "Power supply".
    BoxAround {
        parts: Vec<String>,
        #[serde(default)]
        margin_mm: Option<f64>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        stroke_mm: Option<f64>,
    },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DrawArgs {
    pub session_id: String,
    pub items: Vec<DrawItemArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemMoveArgs {
    /// Id from sch_describe_layout.
    pub id: String,
    pub dx_mm: f64,
    pub dy_mm: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditItemsArgs {
    pub session_id: String,
    /// Ids of wires, labels, junctions, notes, and drawings to delete.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Labels, notes, and drawings to move by an offset.
    #[serde(default)]
    pub moves: Vec<ItemMoveArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BomGetArgs {
    pub session_id: String,
    /// Fields that must match for parts to share a line. Default: value, footprint, and every
    /// custom field (manufacturer, part numbers, ...).
    #[serde(default)]
    pub group_by: Vec<String>,
    /// Leave out parts marked do-not-populate.
    #[serde(default)]
    pub exclude_dnp: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BomPreset {
    /// Reference, value, footprint, datasheet, every custom field, quantity, and DNP.
    #[default]
    Generic,
    /// JLCPCB's columns: Comment, Designator, Footprint, LCSC Part #. DNP parts are left out.
    Jlcpcb,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BomExportArgs {
    pub session_id: String,
    /// Absolute path of the .csv file to write, inside the schematic's folder.
    pub output_path: String,
    #[serde(default)]
    pub preset: BomPreset,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldsEditArgs {
    /// The part's reference, e.g. R1.
    pub reference: String,
    /// Field name to text. Any name except Reference: Value, Footprint, Datasheet, Description, or
    /// a custom field such as MPN, Manufacturer, LCSC (created hidden when missing). The flags
    /// `DNP`, `Exclude from BOM`, and `Exclude from board` take `yes` or `no`.
    pub fields: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetFieldsPlanArgs {
    pub session_id: String,
    pub edits: Vec<FieldsEditArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymSearchArgs {
    /// Words to find in a symbol's name, library, description, or keywords, e.g. `ldo 3.3v` or
    /// `usb c receptacle`.
    pub query: String,
    /// Only this library, e.g. Regulator_Linear.
    #[serde(default)]
    pub library: Option<String>,
    /// How many to return (default 15, at most 50).
    #[serde(default)]
    pub limit: Option<usize>,
    /// A session, to include the project's own symbol libraries.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymGetArgs {
    /// `Library:Name`, like Device:R.
    pub id: String,
    /// A session, to include the project's own symbol libraries.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MovePinArg {
    /// The pin, `REF.PIN` like U1.3.
    pub pin: String,
    /// The net it joins (an existing net, or a new name).
    pub to_net: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PinNetArgs {
    pub session_id: String,
    /// Move one pin to another net: {"pin": "U1.3", "to_net": "GND"}. Give this or swap.
    #[serde(default, rename = "move")]
    pub move_pin: Option<MovePinArg>,
    /// Swap the nets of two pins: ["U1.3", "U1.4"].
    #[serde(default)]
    pub swap: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FpSearchArgs {
    /// Words the footprint's `Library:Name` must contain, e.g. `r 0603` or `sot-23-5`.
    pub query: String,
    /// Only this library, e.g. Resistor_SMD.
    #[serde(default)]
    pub library: Option<String>,
    /// Only footprints with this many pads.
    #[serde(default)]
    pub pad_count: Option<usize>,
    /// How many to return (default 15, at most 50).
    #[serde(default)]
    pub limit: Option<usize>,
    /// A session, to include the project's own footprint libraries.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FpGetArgs {
    /// `Library:Name`, e.g. `Resistor_SMD:R_0603_1608Metric`.
    pub id: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FootprintAssignArgs {
    pub reference: String,
    /// `Library:Name`; an empty text clears the footprint.
    pub footprint: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FootprintPlanArgs {
    pub session_id: String,
    /// Footprints to assign. Leave empty to only check the footprints parts already have.
    #[serde(default)]
    pub assign: Vec<FootprintAssignArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconnectArgs {
    pub session_id: String,
    /// Wire what KiCad reads from the drawing now instead of the circuit as it was before the first
    /// move (use it if the circuit itself was edited in between).
    #[serde(default)]
    pub use_current_nets: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolPinsArgs {
    pub session_id: String,
    /// Symbol id (UUID from sch_list_symbols), or a reference such as R1 that matches exactly
    /// one symbol. Prefer the id: references can change when a schematic is re-annotated.
    pub symbol: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanArgs {
    pub session_id: String,
    /// Electrical intent: {"schema_version": 1, "name": "...", "components": [{"id", "library_id"
    /// (e.g. "Device:R"), "value", optional "reference_preference", "footprint", "fields"}], "nets":
    /// [{"name", "pins": [{"component_id", "pin_number"}]}], optional "no_connects": [{"component_id",
    /// "pin_number"}] (pins left open, each gets a no-connect flag; not in a net) and "power_flags":
    /// ["+5V"] (nets that get a PWR_FLAG part)}. Coordinates are chosen by the layout.
    pub circuit: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyArgs {
    pub session_id: String,
    pub plan_id: String,
    /// Must be true: the user has reviewed the plan.
    pub confirm: bool,
    /// A new value per intended apply (8–128 characters). Repeating a key returns the earlier
    /// result instead of applying twice.
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldArg {
    Value,
    Footprint,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetFieldArgs {
    pub session_id: String,
    /// Symbol id (UUID from sch_list_symbols), or a reference such as R1 that matches exactly one symbol.
    pub symbol: String,
    pub field: FieldArg,
    /// The new text, up to 256 characters, no control characters.
    pub text: String,
    /// Must be true: the user asked for this change.
    pub confirm: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportArgs {
    pub session_id: String,
    /// Absolute path of the .net file to write, inside the schematic's folder.
    pub output_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderArgs {
    pub session_id: String,
    /// Resolution, 72–600 (default 150).
    #[serde(default)]
    pub dpi: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoArgs {
    pub session_id: String,
    /// Apply the plan as well. Without it, only the plan is returned.
    #[serde(default)]
    pub apply: bool,
    /// Required with apply: true.
    #[serde(default)]
    pub confirm: bool,
    /// Required with apply: true.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// The circuit `sch_create_demo_circuit` builds.
const DEMO_CIRCUIT: &str = include_str!("../../../docs/examples/two-resistor-series-chain.json");

#[derive(Serialize)]
struct InstallInfo<'a> {
    cli: String,
    version: &'a str,
    series: u32,
    development_build: bool,
    found_via: kicad_cli_runner::InstallSource,
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Headless => "headless",
        Provider::Gui => "gui",
        Provider::File => "file",
    }
}

fn origin_of(provider: Provider) -> Origin {
    match provider {
        Provider::File => Origin::ProjectFile,
        Provider::Headless | Provider::Gui => Origin::KicadIpc,
    }
}

fn provenance(status: &kicad_session::SessionStatus) -> Provenance {
    Provenance {
        origin: origin_of(status.provider),
        kicad_version: Some(status.kicad_version.clone()),
        provider: Some(provider_name(status.provider).into()),
        document: status.document.as_ref().map(|d| d.display().to_string()),
        revision: None,
    }
}

/// Provenance for data read from a session's KiCad (or, for a file session, from its file).
fn live(session: &AnySession) -> Vec<Provenance> {
    vec![Provenance {
        origin: origin_of(session.provider()),
        kicad_version: Some(session.kicad_version().to_owned()),
        provider: Some(provider_name(session.provider()).into()),
        document: Some(session.document_path().display().to_string()),
        revision: None,
    }]
}

/// Provenance for data that `kicad-cli` produced from a file session's document.
fn from_cli(session: &AnySession) -> Vec<Provenance> {
    let mut prov = live(session);
    if session.provider() == Provider::File {
        prov[0].origin = Origin::KicadCli;
    }
    prov
}

impl KicadAiServer {
    pub fn new(options: Options) -> Self {
        Self {
            state: Arc::new(State {
                options,
                installs: OnceCell::new(),
                sessions: Mutex::new(BTreeMap::new()),
                plans: Mutex::new(PlanStore::default()),
                intended: Mutex::new(BTreeMap::new()),
                boards: Mutex::new(BTreeMap::new()),
                pending_updates: Mutex::new(BTreeMap::new()),
            }),
            tool_router: Self::tool_router() + Self::pcb_router() + Self::fab_router() + Self::checkpoint_router(),
        }
    }

    async fn installs(&self) -> &[KicadInstall] {
        self.state
            .installs
            .get_or_init(|| async {
                let found = kicad_cli_runner::discover(&self.state.options.configured_cli).await;
                tracing::info!(count = found.len(), "KiCad installs discovered");
                found
            })
            .await
    }

    async fn session(&self, id: &str) -> Result<Arc<AnySession>, ToolError> {
        self.state.sessions.lock().await.get(id).cloned().ok_or_else(|| {
            ToolError::new(
                ErrorCode::NoSession,
                format!("no session '{id}'; call session_status to list sessions"),
            )
        })
    }

    /// Registered KiCad GUIs that still answer. Records of instances that are gone are removed.
    async fn live_registrations(&self) -> Vec<Registration> {
        let dir = registry::registry_dir();
        let mut live = Vec::new();
        for r in registry::list(&dir) {
            if registry::is_reachable(&r, std::time::Duration::from_secs(3)).await {
                live.push(r);
            } else {
                let _ = registry::remove(&dir, &r.socket_path);
            }
        }
        live
    }

    /// The registered GUI to use: the one with `schematic`'s project open, or the only one.
    async fn pick_registration(&self, schematic: Option<&std::path::Path>) -> Result<Registration, ToolError> {
        let live = self.live_registrations().await;
        if live.is_empty() {
            return Err(ToolError::new(
                ErrorCode::NoSession,
                "no KiCad editor is registered: open the project in KiCad 11 and press the \"KiCad AI\" toolbar button",
            ));
        }
        if let Some(schematic) = schematic {
            let folder = schematic.parent().map(|p| p.to_string_lossy().to_lowercase());
            let stem = schematic.file_stem().map(|s| s.to_string_lossy().to_lowercase());
            let matches = |r: &&Registration| {
                r.documents.iter().any(|d| {
                    d.project.as_ref().map(|p| p.to_lowercase()) == stem
                        && d.project_path
                            .as_ref()
                            .map(|p| p.trim_end_matches(['\\', '/']).to_lowercase())
                            == folder
                })
            };
            if let Some(r) = live.iter().find(matches) {
                return Ok(r.clone());
            }
        }
        match live.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "several KiCad editors are registered; pass schematic_path to choose one",
            )),
        }
    }

    fn refuse_if_read_only(&self) -> Result<(), ToolError> {
        if self.state.options.read_only {
            Err(ToolError::new(
                ErrorCode::ReadOnlyMode,
                "the server runs in read-only mode",
            ))
        } else {
            Ok(())
        }
    }

    /// A private scratch folder for one tool call (ERC copies, renders).
    fn scratch_dir(&self, session_id: &str, purpose: &str, cid: &str) -> Result<PathBuf, ToolError> {
        let dir = self
            .state
            .options
            .session
            .runtime_dir
            .join(session_id)
            .join(format!("{purpose}-{cid}"));
        std::fs::create_dir_all(&dir)
            .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot create {}: {e}", dir.display())))?;
        Ok(dir)
    }

    /// Validates, lays out, and stores a plan; returns its client view.
    async fn make_plan(&self, session: &Arc<AnySession>, spec: CircuitSpec) -> Result<serde_json::Value, ToolError> {
        let ids = kicad_domain::library_ids_for(&spec);
        let library = session.library_symbols(&ids).await?;
        let existing = session.existing_document().await?;
        let plan = plan_circuit(&spec, &library, &existing);
        self.store_plan(session, plan).await
    }

    /// Stores a plan against the document's current fingerprint and returns its client view.
    async fn store_plan(&self, session: &Arc<AnySession>, plan: OperationPlan) -> Result<serde_json::Value, ToolError> {
        let fingerprint = session.fingerprint().await?;
        let mut plans = self.state.plans.lock().await;
        let stored = StoredPlan {
            session_id: session.id().to_owned(),
            plan,
            fingerprint,
            applied_with: None,
        };
        let view_plan = stored.plan.clone();
        let id = plans.insert(stored);
        Ok(plan_view(&id, &view_plan))
    }

    /// The library symbols a redraw of `design` needs: its own parts, and the power symbols for
    /// the net names once the circuit can be read at all.
    async fn redraw_library(
        &self,
        session: &Arc<AnySession>,
        design: &kicad_domain::SchematicDesign,
        nets: &[kicad_domain::NetInfo],
    ) -> Result<BTreeMap<kicad_domain::LibraryId, kicad_domain::plan::LibrarySymbol>, ToolError> {
        let mut ids = kicad_domain::design_library_ids(design);
        let mut library = session.library_symbols(&ids).await?;
        if let Ok(spec) = kicad_domain::tidy_spec(design, nets) {
            ids.extend(kicad_domain::library_ids_for(&spec));
            ids.sort();
            ids.dedup();
            library = session.library_symbols(&ids).await?;
        }
        Ok(library)
    }

    /// Reads the sheet and plans how to lay it out again. Nothing is changed.
    async fn tidy_plan_for(
        &self,
        session: &Arc<AnySession>,
        hints: &kicad_domain::LayoutHints,
    ) -> Result<(kicad_domain::SchematicDesign, OperationPlan), ToolError> {
        let design = session.design().await?;
        let nets = session.netlist().await?;
        let library = self.redraw_library(session, &design, &nets).await?;
        let plan = kicad_domain::plan_tidy_with_hints(&design, &nets, &library, hints);
        Ok((design, plan))
    }

    /// What the planners that edit labels, drawings, and text need: the sheet, its nets, and the
    /// library symbols of its parts.
    async fn edit_context(
        &self,
        session: &Arc<AnySession>,
    ) -> Result<
        (
            kicad_domain::SchematicDesign,
            Vec<kicad_domain::NetInfo>,
            BTreeMap<kicad_domain::LibraryId, kicad_domain::plan::LibrarySymbol>,
        ),
        ToolError,
    > {
        let design = session.design().await?;
        let nets = session.netlist().await?;
        let library = session
            .library_symbols(&kicad_domain::design_library_ids(&design))
            .await?;
        Ok((design, nets, library))
    }

    /// The footprint libraries on this machine, with the session's project when one is given.
    async fn footprint_libraries(
        &self,
        session_id: Option<&str>,
    ) -> Result<kicad_session::footprints::FootprintLibraries, ToolError> {
        let project = match session_id {
            Some(id) => self
                .session(id)
                .await?
                .document_path()
                .parent()
                .map(std::path::Path::to_path_buf),
            None => None,
        };
        Ok(kicad_session::footprints::FootprintLibraries::discover(
            self.installs().await,
            project.as_deref(),
        ))
    }

    /// The symbol libraries on this machine, with the project's own when a session is given.
    async fn symbol_libraries(
        &self,
        session_id: Option<&str>,
    ) -> Result<kicad_session::symbols::SymbolLibraries, ToolError> {
        let project = match session_id {
            Some(id) => Some(match self.session(id).await {
                Ok(s) => s.document_path().parent().map(std::path::Path::to_path_buf),
                Err(_) => self
                    .board(id)
                    .await?
                    .document_path()
                    .parent()
                    .map(std::path::Path::to_path_buf),
            }),
            None => None,
        };
        Ok(kicad_session::symbols::SymbolLibraries::discover(
            self.installs().await,
            project.flatten().as_deref(),
        ))
    }

    /// The footprint libraries on this machine, with a project folder's own libraries.
    async fn footprint_libraries_in(
        &self,
        project: Option<&std::path::Path>,
    ) -> kicad_session::footprints::FootprintLibraries {
        kicad_session::footprints::FootprintLibraries::discover(self.installs().await, project)
    }

    /// Stores a planner's result and answers with the plan.
    async fn store_and_reply(
        &self,
        cid: &str,
        session: &Arc<AnySession>,
        plan: Result<OperationPlan, ToolError>,
    ) -> CallToolResult {
        match plan {
            Ok(plan) => match self.store_plan(session, plan).await {
                Ok(view) => envelope::ok(cid, view, live(session), vec![]),
                Err(e) => envelope::error(cid, &e),
            },
            Err(e) => envelope::error(cid, &e),
        }
    }

    async fn describe_layout(&self, session: &Arc<AnySession>) -> Result<serde_json::Value, ToolError> {
        let design = session.design().await?;
        let nets = session.netlist().await?;
        let library = session
            .library_symbols(&kicad_domain::design_library_ids(&design))
            .await?;
        let net_of: BTreeMap<String, String> = nets
            .iter()
            .flat_map(|n| {
                n.pins.iter().map(move |p| {
                    (
                        format!("{}.{}", p.reference, p.pin),
                        n.name.trim_start_matches('/').to_owned(),
                    )
                })
            })
            .collect();
        let mm = |p: kicad_domain::Point| serde_json::json!({ "x": round2(p.x.to_mm()), "y": round2(p.y.to_mm()) });
        let facing = |d: kicad_domain::Dir| match d {
            kicad_domain::Dir::Left => "left",
            kicad_domain::Dir::Right => "right",
            kicad_domain::Dir::Up => "up",
            kicad_domain::Dir::Down => "down",
        };
        let mut parts = Vec::new();
        for s in design.symbols.iter().filter(|s| !s.reference.starts_with("#PWR")) {
            let def = kicad_domain::LibraryId::parse(&s.library_id)
                .ok()
                .and_then(|id| library.get(&id));
            let pins: Vec<_> = def
                .map(|d| {
                    d.pins
                        .iter()
                        .filter(|p| p.unit == 0 || p.unit == s.unit)
                        .filter_map(|p| {
                            let at = s.position + s.orientation.apply(p.local)?;
                            let mut pin = serde_json::json!({
                                "number": p.number, "name": p.name,
                                "x": round2(at.x.to_mm()), "y": round2(at.y.to_mm()),
                                "faces": facing(p.outward.transformed(s.orientation)),
                            });
                            if let Some(net) = net_of.get(&format!("{}.{}", s.reference, p.number)) {
                                pin["net"] = net.clone().into();
                            }
                            Some(pin)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let body = def
                .and_then(|d| d.body)
                .map(|b| b.transformed(s.orientation).translate(s.position));
            parts.push(serde_json::json!({
                "reference": s.reference, "value": s.value, "library_id": s.library_id,
                "at": mm(s.position),
                "rotation_deg": s.orientation.rotation_deg,
                "mirror": match (s.orientation.mirror_x, s.orientation.mirror_y) {
                    (false, false) => "none", (true, false) => "x", (false, true) => "y", (true, true) => "both" },
                "body": body.map(|b| serde_json::json!({ "min": mm(b.min), "max": mm(b.max) })),
                "text": s.fields.iter().map(|f| serde_json::json!({
                    "field": f.name, "text": f.text, "at": mm(f.position), "hidden": f.hidden,
                    "area": f.bounds().map(|b| serde_json::json!({ "min": mm(b.min), "max": mm(b.max) })) })).collect::<Vec<_>>(),
                "pins": pins,
            }));
        }
        let dir = |d: kicad_domain::Dir| facing(d);
        Ok(serde_json::json!({
            "units": "mm; x grows right, y grows down; the grid is 1.27 mm",
            "parts": parts,
            "power_symbols": design.symbols.iter().filter(|s| s.reference.starts_with("#PWR")).count(),
            "wires": design.wires.iter().map(|w| serde_json::json!({ "id": w.id, "from": mm(w.start), "to": mm(w.end) })).collect::<Vec<_>>(),
            "labels": design.labels.iter().map(|l| serde_json::json!({
                "id": l.id, "text": l.text, "at": mm(l.position), "direction": dir(l.direction) })).collect::<Vec<_>>(),
            "junctions": design.junctions.iter().map(|j| serde_json::json!({ "id": j.id, "at": mm(j.position) })).collect::<Vec<_>>(),
            "notes": design.texts.iter().map(|t| serde_json::json!({
                "id": t.id, "text": t.text, "at": mm(t.position), "size": round2(t.size.to_mm()),
                "area": { "min": mm(t.bounds().min), "max": mm(t.bounds().max) } })).collect::<Vec<_>>(),
            "drawings": design.shapes.iter().map(|s| serde_json::json!({
                "id": s.id, "shape": s.kind, "filled": s.filled })).collect::<Vec<_>>(),
        }))
    }

    /// Applies a stored plan once per idempotency key.
    async fn apply_planned(&self, cid: &str, session: &Arc<AnySession>, plan_id: &str, key: &str) -> CallToolResult {
        if !(8..=128).contains(&key.len()) {
            return envelope::error(
                cid,
                &ToolError::new(ErrorCode::InvalidArgument, "idempotency_key must be 8–128 characters"),
            );
        }
        let (plan, fingerprint) = {
            let plans = self.state.plans.lock().await;
            if let Some((previous_plan, envelope)) = plans.previous_apply(key) {
                if previous_plan == plan_id {
                    let mut replay = envelope.clone();
                    if let Some(w) = replay.get_mut("warnings").and_then(|w| w.as_array_mut()) {
                        w.push("replayed: this idempotency key was already applied; nothing was changed".into());
                    }
                    return CallToolResult::structured(replay);
                }
                return envelope::error(
                    cid,
                    &ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("idempotency_key was already used for {previous_plan}"),
                    ),
                );
            }
            let Some(stored) = plans.get(plan_id) else {
                return envelope::error(
                    cid,
                    &ToolError::new(ErrorCode::StalePlan, format!("no plan '{plan_id}'; make a new plan")),
                );
            };
            if stored.session_id != session.id() {
                return envelope::error(
                    cid,
                    &ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("{plan_id} belongs to another session"),
                    ),
                );
            }
            if let Some(used) = &stored.applied_with {
                return envelope::error(
                    cid,
                    &ToolError::new(
                        ErrorCode::StalePlan,
                        format!("{plan_id} was already applied (key {used}); make a new plan"),
                    ),
                );
            }
            (stored.plan.clone(), stored.fingerprint.clone())
        };

        // Moving or deleting parts leaves their wires behind, so KiCad stops reporting the circuit
        // the person meant. Keep what it reported just before the first such change.
        if plan.circuit == "arrange" {
            if let Ok(nets) = session.netlist().await {
                self.state
                    .intended
                    .lock()
                    .await
                    .entry(session.id().to_owned())
                    .or_insert(nets);
            }
        }
        let result = session.apply_plan(&plan, &fingerprint).await;
        if plan.circuit == "reconnect" && result.as_ref().is_ok_and(|r| r.verified) {
            self.state.intended.lock().await.remove(session.id());
        }
        let envelope = match &result {
            Ok(report) => {
                let (state, warnings) = if report.verified {
                    (ResultState::Done, vec![])
                } else {
                    (
                        ResultState::Partial,
                        vec!["applied, but KiCad's connectivity differs from the plan; see verification".to_owned()],
                    )
                };
                envelope::ok_with_state(cid, state, report, live(session), warnings)
            }
            Err(e) => envelope::error(cid, e),
        };
        // A refusal before anything changed may be retried with the same key; anything else is final.
        let final_outcome = match &result {
            Ok(_) => true,
            Err(e) => e.mutation_state != kicad_domain::MutationState::NotStarted,
        };
        if final_outcome {
            let mut plans = self.state.plans.lock().await;
            if let Some(stored) = plans.get_mut(plan_id) {
                stored.applied_with = Some(key.to_owned());
            }
            if let Some(value) = envelope::structured(&envelope) {
                plans.record_apply(key.to_owned(), plan_id.to_owned(), value.clone());
            }
        }
        envelope
    }
}

#[tool_router(router = tool_router)]
impl KicadAiServer {
    #[tool(
        name = "session_status",
        description = "List KiCad installs and open sessions, or report one session: KiCad version, document, \
liveness, unsaved changes, whether the file on disk changed since the session read it (file_changed_on_disk, with a note on what to do), and the capability matrix (which features are available and through which backend)."
    )]
    pub async fn session_status(&self, Parameters(args): Parameters<StatusArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Some(id) = args.session_id {
            return match self.session(&id).await {
                Ok(session) => {
                    let status = session.status().await;
                    let prov = provenance(&status);
                    envelope::ok(&cid, status, vec![prov], vec![])
                }
                Err(e) => envelope::error(&cid, &e),
            };
        }

        let installs: Vec<_> = self
            .installs()
            .await
            .iter()
            .map(|i| InstallInfo {
                cli: i.cli.display().to_string(),
                version: &i.version.full,
                series: i.version.series(),
                development_build: i.version.is_development_build(),
                found_via: i.source,
            })
            .collect();
        let sessions: Vec<_> = self.state.sessions.lock().await.values().cloned().collect();
        let mut statuses = Vec::new();
        for s in sessions {
            statuses.push(s.status().await);
        }
        let warnings = if installs.iter().any(|i| i.series >= 11) {
            vec![]
        } else {
            vec!["no KiCad 11 (or 10.99 nightly) install found: only file sessions are available".to_owned()]
        };
        // Registered KiCad editors (the token is never shown).
        let gui_editors: Vec<_> = self
            .live_registrations()
            .await
            .iter()
            .map(|r| {
                serde_json::json!({
                    "kicad_version": r.kicad_version,
                    "projects": r.documents.iter().filter(|d| d.kind == "schematic").map(|d| serde_json::json!({ "project": d.project, "folder": d.project_path })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut boards = Vec::new();
        for b in self.state.boards.lock().await.values().cloned().collect::<Vec<_>>() {
            boards.push(b.status().await);
        }
        envelope::ok(
            &cid,
            serde_json::json!({
                "boards": boards,
                "server": { "name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION") },
                "kicad_installs": installs,
                "autorouter": {
                    "configured": self.state.options.freerouting.is_some(),
                    "program": self.state.options.freerouting.as_ref().map(|f| f.program.display().to_string()),
                    "tool": "pcb_autoroute_plan",
                    "setup": "install.ps1 -FreeroutingOnly, then restart the server (see the README)",
                },
                "parts_lookup": {
                    "tool": "parts_lookup",
                    "source": kicad_domain::parts::active_source().describe(),
                },
                "gui_editors": gui_editors,
                "sessions": statuses,
            }),
            vec![],
            warnings,
        )
    }

    #[tool(
        name = "session_open",
        description = "Open a .kicad_sch file (or create a new one with create: true) in a new session. The \
headless provider starts a private KiCad 11 API server owned by this MCP server; the file provider works on \
the file directly (KiCad 10 or 11, no KiCad running) and checks every change with kicad-cli before it \
replaces the file; the gui provider drives the editor the user has open. Refuses a file that a KiCad editor \
has open, unless read_only is true (file provider: the file is then read, never written). Returns the session status, including its capability matrix."
    )]
    pub async fn session_open(&self, Parameters(args): Parameters<OpenArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let path = args.schematic_path.as_ref().map(PathBuf::from);
        if path.as_ref().is_some_and(|p| !p.is_absolute()) {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::InvalidArgument, "schematic_path must be an absolute path"),
            );
        }
        let installs = self.installs().await.to_vec();
        let opened: Result<AnySession, ToolError> = match args.provider {
            ProviderArg::Headless | ProviderArg::File => {
                let Some(path) = path else {
                    return envelope::error(
                        &cid,
                        &ToolError::new(
                            ErrorCode::InvalidArgument,
                            "schematic_path is required for headless and file sessions",
                        ),
                    );
                };
                let target = if args.create {
                    OpenTarget::New(path)
                } else {
                    OpenTarget::Existing(path)
                };
                if args.read_only && !matches!(args.provider, ProviderArg::File) {
                    return envelope::error(
                        &cid,
                        &ToolError::new(
                            ErrorCode::InvalidArgument,
                            "read_only is for the file provider (the headless and gui providers have no lock to get around)",
                        ),
                    );
                }
                if matches!(args.provider, ProviderArg::File) {
                    // A file session writes the new file straight away.
                    if args.create {
                        if let Err(e) = self.refuse_if_read_only() {
                            return envelope::error(&cid, &e);
                        }
                    }
                    FileSession::open_with(&self.state.options.session, &installs, target, args.read_only)
                        .await
                        .map(AnySession::from)
                } else {
                    Session::open_headless(&self.state.options.session, &installs, target)
                        .await
                        .map(AnySession::from)
                }
            }
            ProviderArg::Gui => {
                if args.create {
                    return envelope::error(
                        &cid,
                        &ToolError::new(
                            ErrorCode::InvalidArgument,
                            "create is only for headless sessions; create the project in KiCad",
                        ),
                    );
                }
                match self.pick_registration(path.as_deref()).await {
                    Ok(registration) => {
                        Session::open_gui(&self.state.options.session, &installs, &registration, path.as_deref())
                            .await
                            .map(AnySession::from)
                    }
                    Err(e) => Err(e),
                }
            }
        };
        match opened {
            Ok(session) => {
                let session = Arc::new(session);
                let status = session.status().await;
                self.state
                    .sessions
                    .lock()
                    .await
                    .insert(session.id().to_owned(), session);
                let prov = provenance(&status);
                envelope::ok(&cid, status, vec![prov], vec![])
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "session_close",
        description = "Close a session and stop its KiCad server. Refuses while the document has unsaved \
changes unless discard_unsaved is true."
    )]
    pub async fn session_close(&self, Parameters(args): Parameters<CloseArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        match session.close(args.discard_unsaved).await {
            Ok(discarded) => {
                self.state.sessions.lock().await.remove(&args.session_id);
                self.state.plans.lock().await.forget_session(&args.session_id);
                envelope::ok(
                    &cid,
                    serde_json::json!({ "session_id": args.session_id, "closed": true, "discarded_unsaved_changes": discarded }),
                    vec![],
                    vec![],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "session_reload",
        description = "Read the schematic of the session from its file again: after another program or a version \\ncontrol checkout changed the file, or to drop changes made in the open document. With unsaved changes it \\nrefuses and lists what would be lost; pass discard_unsaved: true to go ahead. Plans made before the \\nreload are dropped. A file session reads the file each time, so there is nothing to drop."
    )]
    pub async fn session_reload(&self, Parameters(args): Parameters<CloseArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        match session.reload(args.discard_unsaved).await {
            Ok(report) => {
                self.state.plans.lock().await.forget_session(&args.session_id);
                envelope::ok(&cid, report, live(&session), vec![])
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "session_list_documents",
        description = "Documents open in the session's KiCad instance (schematics and boards), as KiCad reports them."
    )]
    pub async fn session_list_documents(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.session(&args.session_id).await {
            Ok(session) => match session.open_documents().await {
                Ok(docs) => envelope::ok(&cid, serde_json::json!({ "documents": docs }), live(&session), vec![]),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_list_symbols",
        description = "Every placed symbol in the session's schematic: id (stable UUID), reference, value, \
library id, footprint, position in mm, orientation, unit, and sheet path. Read live from KiCad."
    )]
    pub async fn sch_list_symbols(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.session(&args.session_id).await {
            Ok(session) => match session.list_symbols().await {
                Ok(symbols) => envelope::ok(
                    &cid,
                    serde_json::json!({ "count": symbols.len(), "symbols": symbols }),
                    live(&session),
                    vec![],
                ),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_get_symbol_pins",
        description = "Pins of one symbol: number, name, electrical type, and connection point on the sheet \
in mm (after the symbol's rotation and mirroring). Wires must end exactly on these points to connect."
    )]
    pub async fn sch_get_symbol_pins(&self, Parameters(args): Parameters<SymbolPinsArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.session(&args.session_id).await {
            Ok(session) => match session.symbol_pins(&args.symbol).await {
                Ok(pins) => envelope::ok(&cid, pins, live(&session), vec![]),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_get_netlist",
        description = "Connectivity as KiCad's netlister sees it: each net with its symbol pins (reference, \
pin number, symbol id) and a count of other connected items such as wires and labels. Use it to verify that \
intended connections really exist."
    )]
    pub async fn sch_get_netlist(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.session(&args.session_id).await {
            Ok(session) => match session.netlist().await {
                Ok(nets) => {
                    let unconnected = nets.iter().filter(|n| n.name.starts_with("unconnected-")).count();
                    envelope::ok(
                        &cid,
                        serde_json::json!({ "count": nets.len(), "unconnected_pins": unconnected, "nets": nets }),
                        from_cli(&session),
                        vec![],
                    )
                }
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_plan_changes",
        description = "Turn a circuit (components and the nets connecting their pins) into an exact plan: \
which symbols are placed where, which references they get, and which wires and labels connect them. \
Nothing is changed. Returns a plan_id for sch_apply_plan, the operations in mm, the nets that must exist \
afterwards, and findings (errors block applying)."
    )]
    pub async fn sch_plan_changes(&self, Parameters(args): Parameters<PlanArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let spec: CircuitSpec = match serde_json::from_value(args.circuit) {
            Ok(spec) => spec,
            Err(e) => {
                return envelope::error(
                    &cid,
                    &ToolError::new(ErrorCode::InvalidArgument, format!("circuit: {e}")),
                )
            }
        };
        match self.make_plan(&session, spec).await {
            Ok(view) => envelope::ok(&cid, view, live(&session), vec![]),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_review",
        description = "Look at the whole drawing of an existing schematic and say what is untidy: symbols \
that overlap, wires that run through a part, power nets named by text instead of a power symbol. Also \
reports whether sch_tidy_plan can redraw it (it handles one sheet of parts joined by wires, local labels, \
and power symbols, with no-connect flags kept on their pins; buses, global labels, and hierarchical sheets are refused) and how \
much it would change. Read-only. Use sch_render to see it."
    )]
    pub async fn sch_review(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let (design, plan) = match self.tidy_plan_for(&session, &Default::default()).await {
            Ok(v) => v,
            Err(e) => return envelope::error(&cid, &e),
        };
        let power = design
            .symbols
            .iter()
            .filter(|s| s.reference.starts_with("#PWR"))
            .count();
        let problems: Vec<String> = match &plan.readability_before {
            Some(r) => r
                .overlapping_symbols
                .iter()
                .map(|(a, b)| format!("{a} and {b} overlap"))
                .chain(
                    r.wires_through_symbols
                        .iter()
                        .map(|(wire, part)| format!("{wire} passes through {part}")),
                )
                .chain(
                    r.labelled_power_nets
                        .iter()
                        .map(|n| format!("{n} is named by a label; a power symbol would be clearer")),
                )
                .chain(
                    r.text_overlaps
                        .iter()
                        .map(|(a, b)| format!("the text of {a} runs into {b}")),
                )
                .collect(),
            None => vec![],
        };
        let blockers: Vec<_> = plan
            .findings
            .iter()
            .filter(|f| f.severity == kicad_domain::Severity::Error)
            .collect();
        let counts = plan.counts();
        envelope::ok(
            &cid,
            serde_json::json!({
                "parts": design.symbols.len() - power,
                "power_symbols": power,
                "wires": design.wires.len(),
                "labels": design.labels.len(),
                "junctions": design.junctions.len(),
                "notes_and_graphics": design.annotations,
                "unsupported_items": design.unsupported,
                "problems": problems,
                "readability": plan.readability_before,
                "tidy": {
                    "possible": blockers.is_empty(),
                    "blockers": blockers,
                    "would_move": counts.moves,
                    "would_remove": counts.removals,
                    "would_remove_items": plan.removals.iter().map(|r| r.description.clone()).collect::<Vec<_>>(),
                    "would_add": plan.operations.len(),
                    "already_tidy": plan.findings.iter().any(|f| f.code == "NOTHING_TO_CHANGE"),
                },
            }),
            live(&session),
            vec![],
        )
    }

    #[tool(
        name = "sch_tidy_plan",
        description = "Plan a cleaner layout for an existing schematic: the parts are laid out again with \
signal flow left to right, power and ground as symbols, and wires instead of labels. Every part keeps its \
identity, value, footprint, and fields; the old wires, labels, junctions, and net-naming power symbols are \
replaced. Optional hints steer it: keep (parts that stay where they are), place (parts to put at x_mm, \
y_mm, optionally turned), near (a part next to another, on a side, with a gap). With hints the parts are \
placed as asked and wired by the router, which may join a net with labels where a wire would cross a part. \
Returns a plan_id for sch_apply_plan, with the moves, removals, and additions, the drawing's readability \
before and after, and the nets that must be identical afterwards. Nothing is changed until the plan is \
applied with confirm: true; show it to the user first."
    )]
    pub async fn sch_tidy_plan(&self, Parameters(args): Parameters<TidyArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let hints = match tidy_hints(&args) {
            Ok(h) => h,
            Err(e) => return envelope::error(&cid, &e),
        };
        let plan = match self.tidy_plan_for(&session, &hints).await {
            Ok((_, plan)) => plan,
            Err(e) => return envelope::error(&cid, &e),
        };
        match self.store_plan(&session, plan).await {
            Ok(view) => envelope::ok(&cid, view, live(&session), vec![]),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_describe_layout",
        description = "Where everything is, for planning moves: each part's reference, value, library id, \
position (mm, y grows downward), rotation, mirroring, body rectangle, and every pin with its position, \
the direction it faces, and its net; plus the text notes' areas and counts of wires and power symbols. \
Read-only. Use it before sch_arrange_plan or the hints of sch_tidy_plan."
    )]
    pub async fn sch_describe_layout(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        match self.describe_layout(&session).await {
            Ok(v) => envelope::ok(&cid, v, live(&session), vec![]),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_arrange_plan",
        description = "Plan bare edits of existing parts: move (to x_mm/y_mm or by dx_mm/dy_mm), turn \
(rotate_deg, counter-clockwise from now), mirror, or delete. Only the named parts change; wires, labels, \
and junctions stay where they are, so connections to moved parts BREAK (the plan says how many wires are \
left behind). Place every part where it belongs first, then call sch_reconnect_plan to draw the \
connections again: the circuit as it was before the first move is remembered for that. Suited to small \
circuits and the start of a design; for a whole sheet use sch_tidy_plan. Returns a plan_id for \
sch_apply_plan (confirm: true after the user has seen it)."
    )]
    pub async fn sch_arrange_plan(&self, Parameters(args): Parameters<ArrangeArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let edits = match arrange_edits(&args) {
            Ok(e) => e,
            Err(e) => return envelope::error(&cid, &e),
        };
        let plan = async {
            let design = session.design().await?;
            let library = session
                .library_symbols(&kicad_domain::design_library_ids(&design))
                .await?;
            Ok::<_, ToolError>(kicad_domain::plan_arrange(&design, &library, &edits, &args.delete))
        }
        .await;
        match plan {
            Ok(plan) => match self.store_plan(&session, plan).await {
                Ok(view) => envelope::ok(&cid, view, live(&session), vec![]),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_reconnect_plan",
        description = "Plan the wiring again with every part where it is now: all old wires, labels, \
junctions, and net-naming power symbols are replaced by new ones that join exactly the pins of the circuit \
as it was before the first sch_arrange_plan (or as KiCad reads it now, with use_current_nets). Wires go \
around parts and other nets; a net that cannot be wired without crossing is joined by labels instead and \
reported. No part moves. After sch_apply_plan the nets are verified against KiCad. Returns a plan_id."
    )]
    pub async fn sch_reconnect_plan(&self, Parameters(args): Parameters<ReconnectArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let plan = async {
            let design = session.design().await?;
            let remembered = self.state.intended.lock().await.get(session.id()).cloned();
            let (nets, source) = match remembered {
                Some(nets) if !args.use_current_nets => (nets, "the circuit as it was before the parts were moved"),
                _ => (session.netlist().await?, "the circuit as KiCad reads it now"),
            };
            let library = self.redraw_library(&session, &design, &nets).await?;
            let mut plan = kicad_domain::plan_reconnect(&design, &nets, &library);
            plan.circuit = "reconnect".into();
            plan.findings.push(kicad_domain::Finding {
                severity: kicad_domain::Severity::Warning,
                code: "RECONNECT_NETS_FROM",
                message: format!("the connections are drawn from {source}"),
                objects: vec![],
            });
            Ok::<_, ToolError>(plan)
        }
        .await;
        match plan {
            Ok(plan) => match self.store_plan(&session, plan).await {
                Ok(view) => envelope::ok(&cid, view, live(&session), vec![]),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_declutter_plan",
        description = "Find text that runs into other text, a wire, or a part (part references and values, \
labels, notes) and plan moving it to the nearest free place beside its part, or turning a label to face \
another way. The text of a PWR_FLAG that cannot be placed anywhere is hidden. Only text changes, so every \
net stays as it is. Run it after sch_tidy_plan or sch_reconnect_plan, whose parts KiCad labels where it \
likes. The extent of text is estimated, so check the result with sch_render. Returns a plan_id."
    )]
    pub async fn sch_declutter_plan(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let plan = async {
            let (design, nets, library) = self.edit_context(&session).await?;
            Ok::<_, ToolError>(kicad_domain::plan_declutter(&design, &nets, &library))
        }
        .await;
        self.store_and_reply(&cid, &session, plan).await
    }

    #[tool(
        name = "sch_label_plan",
        description = "Add, rename, turn, or delete net labels. A label names the net it sits on (on a pin \
`R1.2`, or on a wire at x_mm/y_mm). Refused when it would give a net a second name, or join two nets by \
accident (pass allow_join to join on purpose). Ids come from sch_describe_layout. Every net is checked \
against KiCad after applying. Returns a plan_id."
    )]
    pub async fn sch_label_plan(&self, Parameters(args): Parameters<LabelPlanArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let edits = match label_edits(&args) {
            Ok(e) => e,
            Err(e) => return envelope::error(&cid, &e),
        };
        let plan = async {
            let (design, nets, library) = self.edit_context(&session).await?;
            Ok::<_, ToolError>(kicad_domain::plan_labels(&design, &nets, &library, &edits))
        }
        .await;
        self.store_and_reply(&cid, &session, plan).await
    }

    #[tool(
        name = "sch_draw_plan",
        description = "Draw on the sheet: rectangles, circles, lines, text notes, and a titled box around \
named parts (kind box_around, e.g. title \"Power supply\"). Drawings carry no electrical meaning, so \
every net is checked unchanged after applying. Positions in mm (y grows downward) snap to the 1.27 mm \
grid. Returns a plan_id."
    )]
    pub async fn sch_draw_plan(&self, Parameters(args): Parameters<DrawArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let items = match draw_items(&args) {
            Ok(i) => i,
            Err(e) => return envelope::error(&cid, &e),
        };
        let plan = async {
            let (design, nets, library) = self.edit_context(&session).await?;
            Ok::<_, ToolError>(kicad_domain::plan_draw(&design, &nets, &library, &items))
        }
        .await;
        self.store_and_reply(&cid, &session, plan).await
    }

    #[tool(
        name = "sch_edit_items_plan",
        description = "Delete wires, labels, junctions, notes, and drawings by id, or move labels, notes, \
and drawings by an offset (a label may only move along its own wire). Wires and junctions cannot be moved: \
move parts with sch_arrange_plan and redraw with sch_reconnect_plan. Deleting a wire, label, or junction \
can split or rename nets, which the plan warns about. Ids come from sch_describe_layout. Returns a plan_id."
    )]
    pub async fn sch_edit_items_plan(&self, Parameters(args): Parameters<EditItemsArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let moves: Result<Vec<(String, kicad_domain::Point)>, ToolError> = args
            .moves
            .iter()
            .map(|m| Ok((m.id.clone(), mm_point(m.dx_mm, m.dy_mm)?)))
            .collect();
        let moves = match moves {
            Ok(m) => m,
            Err(e) => return envelope::error(&cid, &e),
        };
        let plan = async {
            let (design, nets, library) = self.edit_context(&session).await?;
            Ok::<_, ToolError>(kicad_domain::plan_edit_items(
                &design,
                &nets,
                &library,
                &args.remove,
                &moves,
            ))
        }
        .await;
        self.store_and_reply(&cid, &session, plan).await
    }

    #[tool(
        name = "bom_get",
        description = "The bill of materials of the sheet as KiCad groups it: one line per set of parts \
with the same value, footprint, and custom fields (do-not-populate parts apart), with quantity, \
references, datasheet, description, every custom field (manufacturer, part numbers), and the parts left \
out by their own flag. Read-only."
    )]
    pub async fn bom_get(&self, Parameters(args): Parameters<BomGetArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        match session.design().await {
            Ok(design) => {
                let bom = kicad_domain::bom::build(
                    &design,
                    &kicad_domain::bom::BomOptions {
                        group_by: args.group_by.clone(),
                        exclude_dnp: args.exclude_dnp,
                    },
                );
                envelope::ok(&cid, bom, live(&session), vec![])
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "bom_review",
        description = "Review the BOM for what an order needs: parts with no footprint, placeholder values, \
the same value written two ways (10k and 10K), parts left out of the BOM that go on the board, parts drawn \
on top of each other, and parts with no manufacturer part number (field MPN), manufacturer, or supplier \
part number (field LCSC, Mouser, DigiKey). Returns the issues and a list of lookups to do, each with a \
search hint. The server looks nothing up itself: parts_lookup turns the lookups into the calls to make on \
the pcbparts MCP, then record the answers with sch_set_fields_plan. Read-only."
    )]
    pub async fn bom_review(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        match session.design().await {
            Ok(design) => envelope::ok(&cid, kicad_domain::bom::review(&design), live(&session), vec![]),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_page_plan",
        description = "Plan the schematic's page and title block: size (A5 to A0, ANSI A to E, US Letter, Legal, \
Ledger, or a custom width_mm x height_mm), landscape or portrait, and the title block's title, date, revision, \
company, and comments 1 to 9 (an empty text clears one). Anything left out stays as it is. Nothing else on the \
sheet changes: a page that gets smaller does not move parts, so the plan warns when a part would end up off the \
page. Works on every provider; the result is read back from KiCad or the file. Returns a plan_id; apply it with \
sch_apply_plan, then sch_save (a file session is saved when applied)."
    )]
    pub async fn sch_page_plan(&self, Parameters(args): Parameters<PagePlanArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let mut comments = std::collections::BTreeMap::new();
        for (key, text) in &args.comments {
            match key.trim().parse::<u8>() {
                Ok(n) => {
                    comments.insert(n, text.clone());
                }
                Err(_) => {
                    return envelope::error(
                        &cid,
                        &ToolError::new(
                            ErrorCode::InvalidArgument,
                            format!("comments are keyed \"1\" to \"9\", not \"{key}\""),
                        ),
                    )
                }
            }
        }
        let request = kicad_domain::sheet::SheetRequest {
            size: args.size.clone(),
            width_mm: args.width_mm,
            height_mm: args.height_mm,
            orientation: args.orientation.clone(),
            title: args.title.clone(),
            date: args.date.clone(),
            revision: args.revision.clone(),
            company: args.company.clone(),
            comments,
        };
        let plan = async {
            let current = session.sheet().await?;
            let mut plan = kicad_domain::sheet::plan_sheet(&current, &request);
            // A smaller page leaves parts off the sheet; say so rather than move anything.
            if let Some(change) = &plan.sheet {
                if let Some((w, h)) = kicad_domain::sheet::size_mm(&change.to.page) {
                    let design = session.design().await?;
                    let off = kicad_domain::sheet::parts_off_page(&design, w, h);
                    if !off.is_empty() {
                        plan.findings.push(kicad_domain::validation::Finding {
                            severity: kicad_domain::validation::Severity::Warning,
                            code: "PARTS_OFF_PAGE",
                            message: format!(
                                "{} part(s) lie outside the new {w:.0} x {h:.0} mm page; move them with sch_arrange_plan or choose a larger page",
                                off.len()
                            ),
                            objects: off,
                        });
                    }
                }
            }
            Ok::<_, ToolError>(plan)
        }
        .await;
        self.store_and_reply(&cid, &session, plan).await
    }

    #[tool(
        name = "parts_lookup",
        description = "Turn the BOM's missing part data into the exact calls to make on the parts service, and the fields to record afterwards. For each BOM line that lacks a manufacturer part number or a supplier part number (the same lines as bom_review) it returns steps: the tool to call (jlc_search with a query and package from the value and footprint, or jlc_get_part for a known MPN, both on the pcbparts MCP), and record: which fields (MPN, Manufacturer, LCSC) take which answer. This server makes no network call itself; the pcbparts MCP (https://pcbparts.dev/mcp, no key) is a separate server the assistant calls, and a part's value and package go to it. When the assistant has no pcbparts tools, source.if_missing says what to ask the user to add. Then record the answers for every line in one sch_set_fields_plan. If this server gets its own parts data, this tool answers itself (status answered) and nothing else changes. Optional references limits it to some parts. Read-only."
    )]
    pub async fn parts_lookup(&self, Parameters(args): Parameters<PartsLookupArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        match session.design().await {
            Ok(design) => {
                let review = kicad_domain::bom::review(&design);
                let lookup =
                    kicad_domain::parts::plan_lookups(kicad_domain::parts::active_source(), &review, &args.references);
                envelope::ok(&cid, lookup, live(&session), vec![])
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_set_fields_plan",
        description = "Plan setting fields on parts, many at once: Value, Footprint, Datasheet, Description, \
custom fields such as MPN, Manufacturer, LCSC (added hidden, so the drawing does not change), and the flags \
DNP, Exclude from BOM, and Exclude from board (yes or no). The reference cannot be set here. Nothing \
else changes and every net is checked unchanged after applying; each field is read back. Use it for the BOM's missing data after bom_review (MPN, Manufacturer, LCSC), for footprints on many parts (sch_footprint_plan suggests them), and for DNP flags; sch_set_field is for a single field. Returns a plan_id."
    )]
    pub async fn sch_set_fields_plan(&self, Parameters(args): Parameters<SetFieldsPlanArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let edits: Vec<kicad_domain::bom::FieldEdit> = args
            .edits
            .iter()
            .map(|e| kicad_domain::bom::FieldEdit {
                reference: e.reference.clone(),
                fields: e.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            })
            .collect();
        let plan = async {
            let design = session.design().await?;
            let nets = session.netlist().await?;
            Ok::<_, ToolError>(kicad_domain::bom::plan_set_fields(&design, &nets, &edits))
        }
        .await;
        self.store_and_reply(&cid, &session, plan).await
    }

    #[tool(
        name = "sym_search",
        description = "Search the KiCad symbol libraries (the install's, and the project's when a session_id is given) \
for symbols whose name, description, or keywords contain every word of the query, e.g. `ldo 3.3v` or \
`usb c receptacle`, best matches first (an exact name, then names, then keywords, then descriptions). \
Returns the id (Library:Name), description, keywords, the default footprint and its filters, the \
datasheet, the number of pins and units, and whether it is a power symbol, without placing anything. \
The first search reads the libraries and takes a few seconds. A user's global library table is not \
read. Read-only."
    )]
    pub async fn sym_search(&self, Parameters(args): Parameters<SymSearchArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let libs = match self.symbol_libraries(args.session_id.as_deref()).await {
            Ok(l) => l,
            Err(e) => return envelope::error(&cid, &e),
        };
        let limit = args.limit.unwrap_or(15).clamp(1, 50);
        let query = args.query.clone();
        let library = args.library.clone();
        // Reading the libraries is blocking work.
        let hits = match tokio::task::spawn_blocking(move || libs.search(&query, library.as_deref(), limit)).await {
            Ok(h) => h,
            Err(e) => return envelope::error(&cid, &ToolError::new(ErrorCode::KicadError, e.to_string())),
        };
        envelope::ok(
            &cid,
            serde_json::json!({ "count": hits.len(), "symbols": hits.iter().map(|e| serde_json::json!({
                "id": e.id(), "description": e.description, "keywords": e.keywords,
                "footprint": e.footprint, "footprint_filters": e.footprint_filters, "datasheet": e.datasheet,
                "pins": e.pins, "units": e.units, "power": e.power, "derived_from": e.extends,
            })).collect::<Vec<_>>() }),
            vec![],
            if hits.is_empty() {
                vec!["nothing matched; try fewer or other words".to_owned()]
            } else {
                vec![]
            },
        )
    }

    #[tool(
        name = "sym_get",
        description = "One symbol from the libraries without placing it: its fields (description, keywords, default \
footprint, footprint filters, datasheet), reference prefix, units, and every pin (number, name, \
electrical type, unit, place in mm, and the way it points). Use it to learn a part's pin numbers before \
writing a circuit. Read-only."
    )]
    pub async fn sym_get(&self, Parameters(args): Parameters<SymGetArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let libs = match self.symbol_libraries(args.session_id.as_deref()).await {
            Ok(l) => l,
            Err(e) => return envelope::error(&cid, &e),
        };
        let id = args.id.clone();
        match tokio::task::spawn_blocking(move || libs.get(&id)).await {
            Ok(Ok(d)) => envelope::ok(
                &cid,
                serde_json::json!({
                    "id": d.id, "description": d.entry.description, "keywords": d.entry.keywords,
                    "footprint": d.entry.footprint, "footprint_filters": d.entry.footprint_filters,
                    "datasheet": d.entry.datasheet, "reference_prefix": d.reference_prefix,
                    "units": d.entry.units, "power": d.entry.power, "derived_from": d.entry.extends,
                    "pin_count": d.pins.len(), "pins": d.pins,
                    "units_note": "unit 0 pins belong to every unit",
                }),
                vec![],
                vec![],
            ),
            Ok(Err(e)) => envelope::error(&cid, &e),
            Err(e) => envelope::error(&cid, &ToolError::new(ErrorCode::KicadError, e.to_string())),
        }
    }

    #[tool(
        name = "sch_pin_net_plan",
        description = "Plan moving one pin to another net (move: {pin: U1.3, to_net: GND}), or swapping the nets of \
two pins (swap: [U1.3, U1.4]), changing only those pins' wires, labels, and power symbols: the pin's own \
wire and the name at its end go, and a short stub that names the new net (a power symbol, or a label) \
is drawn. Where the pin is wired into the middle of other wires, the wiring of the nets involved is \
drawn again and the rest of the sheet is left alone. Refused when the new wiring would cross \
something that is drawn. Every net is read back from KiCad after sch_apply_plan. Returns a plan_id."
    )]
    pub async fn sch_pin_net_plan(&self, Parameters(args): Parameters<PinNetArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let plan = async {
            let request = match (&args.move_pin, &args.swap) {
                (Some(m), None) => kicad_domain::pin_net::PinNetRequest::Move {
                    pin: m.pin.clone(),
                    to_net: m.to_net.clone(),
                },
                (None, Some(s)) if s.len() == 2 => kicad_domain::pin_net::PinNetRequest::Swap {
                    a: s[0].clone(),
                    b: s[1].clone(),
                },
                _ => {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        "give move: {pin, to_net} or swap: [pin, pin], one of them",
                    ))
                }
            };
            let design = session.design().await?;
            let nets = session.netlist().await?;
            let library = self.redraw_library(&session, &design, &nets).await?;
            kicad_domain::pin_net::plan_pin_net(&design, &nets, &library, &request)
        }
        .await;
        self.store_and_reply(&cid, &session, plan).await
    }

    #[tool(
        name = "fp_search",
        description = "Search the KiCad footprint libraries (the install's, and the project's when a \
session_id is given) for names containing every word of the query, e.g. `r 0603` or `sot-23-5`, \
optionally in one library or with a given pad count. Returns the id, pad count, and description. A \
user's global library table is not read. Read-only."
    )]
    pub async fn fp_search(&self, Parameters(args): Parameters<FpSearchArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let libs = match self.footprint_libraries(args.session_id.as_deref()).await {
            Ok(l) => l,
            Err(e) => return envelope::error(&cid, &e),
        };
        let limit = args.limit.unwrap_or(15).clamp(1, 50);
        let hits = libs.search(&args.query, args.library.as_deref(), args.pad_count, limit);
        let warnings = if libs.library_names().is_empty() {
            vec!["no footprint libraries were found: is KiCad installed?".to_owned()]
        } else {
            vec![]
        };
        envelope::ok(
            &cid,
            serde_json::json!({ "count": hits.len(), "footprints": hits }),
            vec![],
            warnings,
        )
    }

    #[tool(
        name = "fp_get",
        description = "One footprint: its pads (number, kind, position, size in mm), courtyard size, \
description, and tags. Read-only."
    )]
    pub async fn fp_get(&self, Parameters(args): Parameters<FpGetArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let libs = match self.footprint_libraries(args.session_id.as_deref()).await {
            Ok(l) => l,
            Err(e) => return envelope::error(&cid, &e),
        };
        match libs.get(&args.id) {
            Some(fp) => {
                let mm =
                    |p: kicad_domain::Point| serde_json::json!({ "x": round2(p.x.to_mm()), "y": round2(p.y.to_mm()) });
                envelope::ok(
                    &cid,
                    serde_json::json!({
                        "id": fp.id, "description": fp.description, "tags": fp.tags,
                        "pad_count": fp.pad_numbers().len(),
                        "pads": fp.pads.iter().map(|p| serde_json::json!({
                            "number": p.number, "kind": p.kind, "at": mm(p.at),
                            "size": { "x": round2(p.size.0.to_mm()), "y": round2(p.size.1.to_mm()) } })).collect::<Vec<_>>(),
                        "courtyard": fp.courtyard.map(|c| serde_json::json!({
                            "width": round2(c.width().to_mm()), "height": round2(c.height().to_mm()) })),
                    }),
                    vec![],
                    vec![],
                )
            }
            None => envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::SymbolNotFound,
                    format!("no footprint '{}' in the KiCad libraries (use fp_search)", args.id),
                ),
            ),
        }
    }

    #[tool(
        name = "sch_footprint_plan",
        description = "Check every part's footprint, and plan assigning footprints. Reports parts with no \
footprint (with a suggestion for plain resistors, capacitors, inductors, and LEDs), footprints that are not \
in the libraries, footprints with no pad for one of the part's pins (an error), extra pads, and a package \
named in the value or description (0603, SOT-23-5) that the footprint is not. Choose footprints with \
fp_search first. With no assign list it is a check only. Returns a plan_id."
    )]
    pub async fn sch_footprint_plan(&self, Parameters(args): Parameters<FootprintPlanArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let assign: Vec<(String, String)> = args
            .assign
            .iter()
            .map(|a| (a.reference.clone(), a.footprint.clone()))
            .collect();
        let plan = async {
            let (design, nets, library) = self.edit_context(&session).await?;
            let libs = self.footprint_libraries(Some(session.id())).await?;
            let mut ids: std::collections::BTreeSet<String> = design
                .symbols
                .iter()
                .filter(|s| !s.footprint.is_empty())
                .map(|s| s.footprint.clone())
                .collect();
            ids.extend(assign.iter().map(|(_, f)| f.clone()).filter(|f| !f.is_empty()));
            let footprints: BTreeMap<String, kicad_domain::FootprintInfo> = ids
                .into_iter()
                .filter_map(|id| Some((id.clone(), libs.get(&id)?)))
                .collect();
            Ok::<_, ToolError>(kicad_domain::plan_footprints(
                &design,
                &nets,
                &library,
                &footprints,
                &assign,
            ))
        }
        .await;
        self.store_and_reply(&cid, &session, plan).await
    }

    #[tool(
        name = "bom_export",
        description = "Write the BOM as a CSV file inside the schematic's folder, through kicad-cli. Preset \
generic: reference, value, footprint, datasheet, every custom field, quantity, DNP. Preset jlcpcb: Comment, \
Designator, Footprint, LCSC Part # (from the field LCSC), do-not-populate parts left out. Nothing is \
uploaded. Returns the path and the number of lines."
    )]
    pub async fn bom_export(&self, Parameters(args): Parameters<BomExportArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let output = PathBuf::from(&args.output_path);
        let folder = session.document_path().parent().map(PathBuf::from).unwrap_or_default();
        let inside = output.is_absolute() && std::path::absolute(&output).is_ok_and(|p| p.starts_with(&folder));
        if !inside || output.extension().and_then(|e| e.to_str()) != Some("csv") {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    format!("output_path must be an absolute .csv path inside {}", folder.display()),
                ),
            );
        }
        let Some(cli) = session.cli().cloned() else {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::FeatureUnavailable, "no matching kicad-cli install"),
            );
        };
        let dir = match self.scratch_dir(session.id(), "bom", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            let design = session.design().await?;
            let bom = kicad_domain::bom::build(&design, &Default::default());
            let settings = bom_settings(args.preset, &bom, &design);
            let copy = session.save_copy(&dir).await?;
            kicad_cli_runner::export::export_bom(&cli, &copy, &output, &settings, std::time::Duration::from_secs(180))
                .await
                .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
            let lines = std::fs::read_to_string(&output)
                .map(|t| t.lines().count().saturating_sub(1))
                .unwrap_or(0);
            Ok::<_, ToolError>((lines, bom.rows.len()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok((lines, rows)) => envelope::ok(
                &cid,
                serde_json::json!({ "path": output.display().to_string(), "lines": lines, "bom_lines": rows }),
                from_cli(&session),
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_apply_plan",
        description = "Apply a plan from sch_plan_changes or sch_tidy_plan after the user has reviewed it \
(confirm: true). Refused if the schematic changed since planning. Places or moves symbols, sets values, \
adds and removes wires, labels, and junctions, then reads connectivity back from KiCad and compares it \
with the plan's expected nets. On failure the changes are undone where KiCad allows it; in a live editor a \
partly applied plan is reported with how many undo steps to take. Repeating the same idempotency_key \
returns the earlier result."
    )]
    pub async fn sch_apply_plan(&self, Parameters(args): Parameters<ApplyArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        if !args.confirm {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    "confirm must be true; show the plan to the user first",
                ),
            );
        }
        match self.session(&args.session_id).await {
            Ok(session) => {
                self.apply_planned(&cid, &session, &args.plan_id, &args.idempotency_key)
                    .await
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(name = "sch_save", description = "Save the session's schematic to its file.")]
    pub async fn sch_save(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        match self.session(&args.session_id).await {
            Ok(session) => match session.save().await {
                Ok(()) => envelope::ok(
                    &cid,
                    serde_json::json!({ "saved": session.document_path().display().to_string() }),
                    live(&session),
                    vec![],
                ),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_set_field",
        description = "Change the value (e.g. 10k to 4k7) or the footprint of one part, and nothing else: \
the part keeps its place and connections. Needs confirm: true. In a live editor it is one undo step; in a \
file session the new file must load in kicad-cli before it replaces the old one. Returns the part as it is \
now. For anything bigger (moving parts, redrawing), use a plan."
    )]
    pub async fn sch_set_field(&self, Parameters(args): Parameters<SetFieldArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        if !args.confirm {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    "confirm must be true; the user asked for this change",
                ),
            );
        }
        if args.text.chars().count() > 256 || args.text.chars().any(char::is_control) {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    "text must be at most 256 characters with no control characters",
                ),
            );
        }
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let field = match args.field {
            FieldArg::Value => kicad_session::SymbolField::Value,
            FieldArg::Footprint => kicad_session::SymbolField::Footprint,
        };
        match session.set_field(&args.symbol, field, &args.text).await {
            Ok(info) => envelope::ok(&cid, info, live(&session), vec![]),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_run_erc",
        description = "Run KiCad's electrical rules check on the schematic as it is now (unsaved changes \
included). Returns error and warning counts and every violation with its position in mm. Zero errors does \
not prove the circuit works."
    )]
    pub async fn sch_run_erc(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let Some(cli) = session.cli().cloned() else {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::ErcUnavailable, "no matching kicad-cli install"),
            );
        };
        let dir = match self.scratch_dir(session.id(), "erc", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        // ERC runs on a snapshot of the in-memory document, so the user's file is not touched.
        let result = async {
            let copy = session.save_copy(&dir).await?;
            kicad_cli_runner::erc::run_erc(&cli, &copy, &dir.join("erc.json"), std::time::Duration::from_secs(180))
                .await
                .map_err(|e| ToolError::new(ErrorCode::ErcUnavailable, e.to_string()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(report) => {
                let violations: Vec<_> = report
                    .violations()
                    .map(|(sheet, v)| {
                        serde_json::json!({
                            "sheet": sheet.path,
                            "severity": v.severity,
                            "type": v.kind,
                            "description": v.description,
                            "items": v.items.iter().map(|i| serde_json::json!({ "description": i.description, "position_mm": i.pos })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                let provenance = vec![Provenance {
                    origin: Origin::KicadCli,
                    kicad_version: Some(cli.version.full.clone()),
                    provider: None,
                    document: Some(session.document_path().display().to_string()),
                    revision: None,
                }];
                envelope::ok(
                    &cid,
                    serde_json::json!({
                        "errors": report.count("error"),
                        "warnings": report.count("warning"),
                        "violations": violations,
                    }),
                    provenance,
                    vec![match session.provider() {
                        Provider::File => "ERC ran on a copy of the schematic file".to_owned(),
                        _ => "ERC ran on a snapshot of the in-memory schematic".to_owned(),
                    }],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_export_netlist",
        description = "Write a KiCad netlist (.net, s-expression format) of the schematic as it is now to \
output_path, which must be inside the schematic's folder. Returns the net and component counts."
    )]
    pub async fn sch_export_netlist(&self, Parameters(args): Parameters<ExportArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let output = PathBuf::from(&args.output_path);
        let folder = session.document_path().parent().map(PathBuf::from).unwrap_or_default();
        let inside = output.is_absolute()
            && std::path::absolute(&output)
                .is_ok_and(|p| p.parent() == Some(folder.as_path()) || p.starts_with(&folder));
        if !inside || output.extension().and_then(|e| e.to_str()) != Some("net") {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    format!("output_path must be an absolute .net path inside {}", folder.display()),
                ),
            );
        }
        let Some(cli) = session.cli().cloned() else {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::FeatureUnavailable, "no matching kicad-cli install"),
            );
        };
        let dir = match self.scratch_dir(session.id(), "netlist", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        // KiCad's IPC export jobs read the saved file, so export from a snapshot of the in-memory
        // document instead (docs/tasks.md, 3.7).
        let result = async {
            let copy = session.save_copy(&dir).await?;
            kicad_cli_runner::netlist::export_netlist(&cli, &copy, &output, std::time::Duration::from_secs(180))
                .await
                .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(netlist) => envelope::ok(
                &cid,
                serde_json::json!({
                    "path": output.display().to_string(),
                    "components": netlist.components.len(),
                    "nets": netlist.nets.len(),
                }),
                from_cli(&session),
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_render",
        description = "Render the schematic as it is now to a PNG image and return it, so the layout can be \
checked visually."
    )]
    pub async fn sch_render(&self, Parameters(args): Parameters<RenderArgs>) -> CallToolResult {
        use base64::Engine;
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let dpi = args.dpi.unwrap_or(150);
        if !(72..=600).contains(&dpi) {
            return envelope::error(&cid, &ToolError::new(ErrorCode::InvalidArgument, "dpi must be 72–600"));
        }
        let dir = match self.scratch_dir(session.id(), "render", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        let Some(cli) = session.render_cli().cloned() else {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    "no kicad-cli that can export PNG (KiCad 10's cannot); install KiCad 11 or the 10.99 nightly",
                ),
            );
        };
        // Render a snapshot of the in-memory document (IPC plot jobs read the saved file).
        let result = async {
            let snapshot = dir.join("snapshot");
            let images = dir.join("png");
            for d in [&snapshot, &images] {
                std::fs::create_dir_all(d).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
            }
            let copy = session.save_copy(&snapshot).await?;
            let pngs =
                kicad_cli_runner::export::export_png(&cli, &copy, &images, dpi, std::time::Duration::from_secs(180))
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
            std::fs::read(&pngs[0]).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(bytes) => {
                let mut r = envelope::ok(
                    &cid,
                    serde_json::json!({ "format": "png", "dpi": dpi, "bytes": bytes.len() }),
                    from_cli(&session),
                    vec![],
                );
                r.content.push(rmcp::model::ContentBlock::image(
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "image/png",
                ));
                r
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_create_demo_circuit",
        description = "Plan (and with apply: true, confirm: true, and an idempotency_key, also apply) the \
two-resistor series chain INPUT - R1 10k - MID - R2 10k - OUTPUT. A quick end-to-end check of planning, \
applying, and verification."
    )]
    pub async fn sch_create_demo_circuit(&self, Parameters(args): Parameters<DemoArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let spec = CircuitSpec::from_json(DEMO_CIRCUIT).expect("bundled demo circuit is valid");
        let view = match self.make_plan(&session, spec).await {
            Ok(v) => v,
            Err(e) => return envelope::error(&cid, &e),
        };
        if !args.apply {
            return envelope::ok(
                &cid,
                view,
                live(&session),
                vec!["plan only; pass apply: true to build it".into()],
            );
        }
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let (true, Some(key)) = (args.confirm, args.idempotency_key.as_deref()) else {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    "apply: true needs confirm: true and an idempotency_key",
                ),
            );
        };
        let plan_id = view["plan_id"].as_str().unwrap_or_default().to_owned();
        self.apply_planned(&cid, &session, &plan_id, key).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KicadAiServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn mm_point(x: f64, y: f64) -> Result<kicad_domain::Point, ToolError> {
    kicad_domain::Point::from_mm(x, y)
        .map_err(|e| ToolError::new(ErrorCode::InvalidArgument, format!("({x}, {y}) mm: {e}")))
}

fn tidy_hints(args: &TidyArgs) -> Result<kicad_domain::LayoutHints, ToolError> {
    let mut hints = kicad_domain::LayoutHints {
        keep: args.keep.clone(),
        ..Default::default()
    };
    for p in &args.place {
        hints.place.push(kicad_domain::PlaceHint {
            reference: p.reference.clone(),
            position: mm_point(p.x_mm, p.y_mm)?,
            rotate_deg: p.rotate_deg,
            mirror: p.mirror.map(Into::into),
        });
    }
    for n in &args.near {
        let gap = kicad_domain::Nm::from_mm(n.gap_mm.unwrap_or(10.0))
            .map_err(|e| ToolError::new(ErrorCode::InvalidArgument, format!("gap_mm: {e}")))?;
        hints.near.push(kicad_domain::NearHint {
            reference: n.reference.clone(),
            to: n.to.clone(),
            side: match n.side {
                SideArg::Left => kicad_domain::Dir::Left,
                SideArg::Right => kicad_domain::Dir::Right,
                SideArg::Above => kicad_domain::Dir::Up,
                SideArg::Below => kicad_domain::Dir::Down,
            },
            gap,
        });
    }
    Ok(hints)
}

fn arrange_edits(args: &ArrangeArgs) -> Result<Vec<kicad_domain::PartEdit>, ToolError> {
    let bad = |m: String| ToolError::new(ErrorCode::InvalidArgument, m);
    args.edits
        .iter()
        .map(|e| {
            let at = match (e.x_mm, e.y_mm) {
                (Some(x), Some(y)) => Some(mm_point(x, y)?),
                (None, None) => None,
                _ => return Err(bad(format!("{}: give both x_mm and y_mm", e.reference))),
            };
            let by = match (e.dx_mm, e.dy_mm) {
                (None, None) => None,
                (dx, dy) => Some(mm_point(dx.unwrap_or(0.0), dy.unwrap_or(0.0))?),
            };
            Ok(kicad_domain::PartEdit {
                reference: e.reference.clone(),
                at,
                by,
                rotate_deg: e.rotate_deg,
                mirror: e.mirror.map(Into::into),
            })
        })
        .collect()
}

fn label_edits(args: &LabelPlanArgs) -> Result<kicad_domain::LabelEdits, ToolError> {
    let bad = |m: String| ToolError::new(ErrorCode::InvalidArgument, m);
    let mut edits = kicad_domain::LabelEdits::default();
    for a in &args.add {
        let at = match (&a.pin, a.x_mm, a.y_mm) {
            (Some(pin), None, None) => {
                let (reference, number) = pin
                    .split_once('.')
                    .ok_or_else(|| bad(format!("pin '{pin}': write it as REF.PIN, e.g. R1.2")))?;
                kicad_domain::LabelAt::Pin {
                    reference: reference.to_owned(),
                    pin: number.to_owned(),
                }
            }
            (None, Some(x), Some(y)) => kicad_domain::LabelAt::Point(mm_point(x, y)?),
            _ => {
                return Err(bad(format!(
                    "label '{}': give either pin, or both x_mm and y_mm",
                    a.text
                )))
            }
        };
        edits.add.push(kicad_domain::LabelAdd {
            text: a.text.clone(),
            at,
            direction: a.direction.map(Into::into),
            allow_join: a.allow_join,
        });
    }
    edits.rename = args.rename.iter().map(|r| (r.id.clone(), r.text.clone())).collect();
    edits.turn = args.turn.iter().map(|t| (t.id.clone(), t.direction.into())).collect();
    edits.remove = args.remove.clone();
    Ok(edits)
}

fn draw_items(args: &DrawArgs) -> Result<Vec<kicad_domain::DrawItem>, ToolError> {
    let nm = |mm: f64| {
        kicad_domain::Nm::from_mm(mm).map_err(|e| ToolError::new(ErrorCode::InvalidArgument, format!("{mm} mm: {e}")))
    };
    let stroke = |s: Option<f64>| match s {
        Some(v) if v >= 0.0 => nm(v),
        Some(v) => Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("stroke_mm {v} is negative"),
        )),
        None => Ok(kicad_domain::edits::DEFAULT_STROKE),
    };
    args.items
        .iter()
        .map(|item| {
            Ok(match item {
                DrawItemArgs::Rectangle {
                    x_mm,
                    y_mm,
                    width_mm,
                    height_mm,
                    stroke_mm,
                    filled,
                } => kicad_domain::DrawItem::Rectangle {
                    min: mm_point(*x_mm, *y_mm)?,
                    max: mm_point(x_mm + width_mm, y_mm + height_mm)?,
                    stroke: stroke(*stroke_mm)?,
                    filled: *filled,
                },
                DrawItemArgs::Circle {
                    x_mm,
                    y_mm,
                    radius_mm,
                    stroke_mm,
                    filled,
                } => kicad_domain::DrawItem::Circle {
                    center: mm_point(*x_mm, *y_mm)?,
                    radius: nm(*radius_mm)?,
                    stroke: stroke(*stroke_mm)?,
                    filled: *filled,
                },
                DrawItemArgs::Line {
                    x1_mm,
                    y1_mm,
                    x2_mm,
                    y2_mm,
                    stroke_mm,
                } => kicad_domain::DrawItem::Line {
                    start: mm_point(*x1_mm, *y1_mm)?,
                    end: mm_point(*x2_mm, *y2_mm)?,
                    stroke: stroke(*stroke_mm)?,
                },
                DrawItemArgs::Text {
                    x_mm,
                    y_mm,
                    text,
                    size_mm,
                    rotate_deg,
                    align,
                } => kicad_domain::DrawItem::Text {
                    text: text.clone(),
                    position: mm_point(*x_mm, *y_mm)?,
                    size: match size_mm {
                        Some(v) if *v > 0.0 => nm(*v)?,
                        Some(v) => {
                            return Err(ToolError::new(
                                ErrorCode::InvalidArgument,
                                format!("size_mm {v} must be positive"),
                            ))
                        }
                        None => kicad_domain::text::DEFAULT_SIZE,
                    },
                    angle_deg: rotate_deg.unwrap_or(0),
                    h: match align {
                        Some(AlignArg::Center) => kicad_domain::text::HAlign::Center,
                        Some(AlignArg::Right) => kicad_domain::text::HAlign::Right,
                        _ => kicad_domain::text::HAlign::Left,
                    },
                    v: kicad_domain::text::VAlign::Bottom,
                },
                DrawItemArgs::BoxAround {
                    parts,
                    margin_mm,
                    title,
                    stroke_mm,
                } => kicad_domain::DrawItem::BoxAround {
                    parts: parts.clone(),
                    margin: nm(margin_mm.unwrap_or(5.08).max(0.0))?,
                    title: title.clone(),
                    stroke: stroke(*stroke_mm)?,
                },
            })
        })
        .collect()
}

/// The columns of a BOM export.
fn bom_settings(
    preset: BomPreset,
    bom: &kicad_domain::bom::Bom,
    design: &kicad_domain::SchematicDesign,
) -> kicad_cli_runner::export::BomSettings {
    let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    match preset {
        BomPreset::Generic => {
            let mut fields = names(&["Reference", "Value", "Footprint", "Datasheet"]);
            fields.extend(bom.custom_fields.iter().cloned());
            fields.extend(names(&["${QUANTITY}", "${DNP}"]));
            let mut labels = names(&["Refs", "Value", "Footprint", "Datasheet"]);
            labels.extend(bom.custom_fields.iter().cloned());
            labels.extend(names(&["Qty", "DNP"]));
            let mut group_by = names(&["Value", "Footprint"]);
            group_by.extend(bom.custom_fields.iter().cloned());
            kicad_cli_runner::export::BomSettings {
                fields,
                labels,
                group_by,
                exclude_dnp: false,
            }
        }
        BomPreset::Jlcpcb => {
            // The field the part numbers are kept in; LCSC unless the parts use another spelling.
            let field = ["LCSC", "LCSC Part", "LCSC Part #", "JLCPCB Part #"]
                .into_iter()
                .find(|f| {
                    design
                        .symbols
                        .iter()
                        .any(|s| s.properties.get(*f).is_some_and(|v| !v.is_empty()))
                })
                .unwrap_or("LCSC");
            kicad_cli_runner::export::BomSettings {
                fields: names(&["Value", "Reference", "Footprint"])
                    .into_iter()
                    .chain([field.to_owned()])
                    .collect(),
                labels: names(&["Comment", "Designator", "Footprint", "LCSC Part #"]),
                group_by: names(&["Value", "Footprint"])
                    .into_iter()
                    .chain([field.to_owned()])
                    .collect(),
                exclude_dnp: true,
            }
        }
    }
}
