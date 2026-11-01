// SPDX-License-Identifier: GPL-3.0-or-later
//! The `pcb_*` tools: board sessions, reading, DRC, a render, a review, and updating the board from
//! the schematic (Phase 11).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kicad_domain::board::{place_rect, Board};
use kicad_domain::board_edit::{self, BoardEditPlan, BoardOp, HolePlacement, MoveRequest, OutlineRequest};
use kicad_domain::placement::{self, NearHint, PinnedPlace, PlaceHints, PlaceOptions};
use kicad_domain::{footprint_edit, routing, rules, track_edit, zones};
use kicad_domain::{ErrorCode, Origin, Point, Provenance, Rect, ToolError};
use kicad_session::{BoardSession, Provider};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{envelope, provider_name, round2, KicadAiServer, ProviderArg};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbOpenArgs {
    /// Absolute path of the .kicad_pcb file. Required for headless and file; for gui it selects
    /// the board when the PCB editor has more than one open.
    #[serde(default)]
    pub pcb_path: Option<String>,
    /// headless: a private KiCad 11 server opens the board (reading, DRC, updating from the
    /// schematic). file: the file itself is read, KiCad 10 or 11 (reading, DRC, render, exports).
    /// gui: the PCB editor the user has open, after pressing "KiCad AI: connect" there.
    #[serde(default)]
    pub provider: ProviderArg,
    /// File provider only: the board's editor may have it open; nothing is written to the board or
    /// its project by this session.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbSessionArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbCloseArgs {
    pub session_id: String,
    /// Close even if the board has unsaved changes (they are lost).
    #[serde(default)]
    pub discard_unsaved: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbFootprintsArgs {
    pub session_id: String,
    /// Include every pad (number, kind, place, net) of each footprint.
    #[serde(default)]
    pub include_pads: bool,
    /// `summary` (default) or `full`: every pad's shape, size, drill, and turn, the courtyard
    /// outline, and the footprint's text and fields with layer, size, place, and visibility.
    #[serde(default)]
    pub detail: Option<String>,
    /// Only these references (a full listing of a whole board is long).
    #[serde(default)]
    pub references: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbFootprintArgs {
    pub session_id: String,
    /// The footprint's reference, e.g. U1.
    pub reference: String,
    /// `summary` (default) or `full`, as in pcb_list_footprints.
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbDrawingsArgs {
    pub session_id: String,
    /// Only these layers, e.g. Edge.Cuts or F.SilkS.
    #[serde(default)]
    pub layers: Vec<String>,
    /// Only these kinds: line, rect, circle, arc, polygon, bezier, or text.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// How many items to return (default 200, at most 1000).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbTracksArgs {
    pub session_id: String,
    /// Only items on this net.
    #[serde(default)]
    pub net: Option<String>,
    /// How many tracks to return (default 200, at most 1000).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbDrcArgs {
    pub session_id: String,
    /// Also compare the board with its schematic (parts, footprints, and nets that differ).
    #[serde(default)]
    pub schematic_parity: bool,
    /// Counts only (by type and severity), no violation list.
    #[serde(default)]
    pub summary_only: bool,
    /// List only this severity: error, warning, or exclusion.
    #[serde(default)]
    pub severity: Option<String>,
    /// List only these violation types, like `clearance` or `shorting_items`.
    #[serde(default)]
    pub types: Option<Vec<String>>,
    /// Most violations to list (default 100, at most 1000).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Refill the zones first, in the copy that is checked (default true), so stale fills do not
    /// show as clearance errors. The board in the editor is not changed.
    #[serde(default)]
    pub refill_zones: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbRenderArgs {
    pub session_id: String,
    /// top (default), bottom, left, right, front, or back.
    #[serde(default)]
    pub side: Option<String>,
    /// Image size in pixels (default 1600 x 900, at most 3200 x 2400).
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbReviewArgs {
    pub session_id: String,
    /// The board's schematic, to compare the two. Defaults to the .kicad_sch with the board's name
    /// next to it.
    #[serde(default)]
    pub schematic_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbUpdateArgs {
    pub session_id: String,
    /// The schematic to update from. Defaults to the .kicad_sch with the board's name next to it.
    /// The file on disk is used, so save the schematic first.
    #[serde(default)]
    pub schematic_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbApplyArgs {
    pub session_id: String,
    pub plan_id: String,
    /// Must be true: the user has reviewed the plan.
    pub confirm: bool,
    /// A new value per intended apply (8-128 characters).
    pub idempotency_key: String,
    /// Keep a board change that fails its read-back or makes DRC worse, to look at it. By default
    /// such a change is rolled back. (Not used for an update from the schematic.)
    #[serde(default)]
    pub keep_if_unverified: bool,
    /// Take a checkpoint of the project first, with this label (see checkpoint_create), so the
    /// change can be taken back with checkpoint_restore. Use it for anything that deletes.
    #[serde(default)]
    pub checkpoint: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaceArg {
    pub reference: String,
    pub x_mm: f64,
    pub y_mm: f64,
    /// Keeps its turn when left out.
    #[serde(default)]
    pub rotation_deg: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NearArg {
    pub reference: String,
    /// The footprint it should be near.
    pub target: String,
    /// Distance between the two anchors that is close enough, mm (default 5).
    #[serde(default)]
    pub max_mm: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EdgeArg {
    pub reference: String,
    /// left, right, top, or bottom.
    pub edge: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SideArg {
    pub reference: String,
    /// front or back.
    pub side: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbPlaceArgs {
    pub session_id: String,
    /// Footprints that stay where they are (locked footprints and mounting holes always do).
    #[serde(default)]
    pub keep: Vec<String>,
    /// Footprints put exactly here (snapped to the grid), then left alone.
    #[serde(default)]
    pub place: Vec<PlaceArg>,
    /// Footprints that belong close to another one.
    #[serde(default)]
    pub near: Vec<NearArg>,
    /// Footprints that belong on one edge (connectors go to the nearest edge without a hint).
    #[serde(default)]
    pub edge: Vec<EdgeArg>,
    /// Footprints kept together, e.g. [["U2", "C7", "L1"]].
    #[serde(default)]
    pub groups: Vec<Vec<String>>,
    /// Footprints placed on the back (or front).
    #[serde(default)]
    pub side: Vec<SideArg>,
    /// Place only these; every other footprint stays where it is.
    #[serde(default)]
    pub only: Option<Vec<String>>,
    /// Also keep together the parts inside each rectangle drawn on the schematic (default true).
    #[serde(default)]
    pub use_schematic_blocks: Option<bool>,
    /// The board's schematic, for the blocks. Defaults to the .kicad_sch next to the board.
    #[serde(default)]
    pub schematic_path: Option<String>,
    /// Grid the places snap to, mm (default 0.5).
    #[serde(default)]
    pub grid_mm: Option<f64>,
    /// Space between a courtyard and the outline, mm (default 0.25; connectors may reach it).
    #[serde(default)]
    pub edge_clearance_mm: Option<f64>,
    /// Space kept between courtyards, mm (default 1), for reference text and routing.
    #[serde(default)]
    pub spacing_mm: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutePointArg {
    /// A pad, REF.PAD (e.g. U1.2). Or give x_mm and y_mm.
    #[serde(default)]
    pub pad: Option<String>,
    #[serde(default)]
    pub x_mm: Option<f64>,
    #[serde(default)]
    pub y_mm: Option<f64>,
    /// The layer of the segment that leaves this point (F.Cu, B.Cu, ...). A change of layer puts a
    /// via here (none at a through-hole pad).
    #[serde(default)]
    pub layer: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteArg {
    /// The net; taken from the pads when left out.
    #[serde(default)]
    pub net: Option<String>,
    /// Two or more points. With two and no layers on them, a path is found: straight, a 45°
    /// dogleg, or an L, on the asked layer or either outer layer.
    pub points: Vec<RoutePointArg>,
    /// The layer to route on (default: the pads' common layer, front first).
    #[serde(default)]
    pub layer: Option<String>,
    /// Track width, mm (default: the net class's).
    #[serde(default)]
    pub width_mm: Option<f64>,
    /// Lock the tracks and vias this route makes, so a later rip-up (pcb_track_edit_plan) or
    /// autoroute keeps them.
    #[serde(default)]
    pub locked: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbRouteArgs {
    pub session_id: String,
    pub routes: Vec<RouteArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RectMm {
    pub min: XyMm,
    pub max: XyMm,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ZoneArg {
    /// The net to pour, e.g. GND.
    pub net: String,
    /// Copper layers (default: every copper layer of the board).
    #[serde(default)]
    pub layers: Option<Vec<String>>,
    /// An area other than the whole board: a rectangle in mm.
    #[serde(default)]
    pub rect: Option<RectMm>,
    /// Or a polygon, three or more corners in mm.
    #[serde(default)]
    pub polygon: Option<Vec<XyMm>>,
    /// Clearance to other nets, mm (default: the net class's).
    #[serde(default)]
    pub clearance_mm: Option<f64>,
    /// Thinnest copper the fill keeps, mm (default 0.25).
    #[serde(default)]
    pub min_width_mm: Option<f64>,
    /// How pads of the net connect: thermal (default), solid, or none.
    #[serde(default)]
    pub pad_connection: Option<String>,
    /// Thermal relief gap and spoke width, mm (default 0.5 each).
    #[serde(default)]
    pub thermal_gap_mm: Option<f64>,
    #[serde(default)]
    pub spoke_width_mm: Option<f64>,
    /// Higher fills first where zones of different nets overlap (default 0).
    #[serde(default)]
    pub priority: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    /// Replace the zones this net already has on these layers.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbZoneArgs {
    pub session_id: String,
    pub zones: Vec<ZoneArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbAutorouteArgs {
    pub session_id: String,
    /// Most routing passes (default 20, 1 to 200).
    #[serde(default)]
    pub passes: Option<u32>,
    /// Stop the router after this many seconds (default 120, 10 to 900). Its processes are all
    /// ended when the time is up.
    #[serde(default)]
    pub time_limit_s: Option<u64>,
    /// Route only these nets; every other net is left as it is (its tracks stay, its pads are
    /// obstacles). Default: every net.
    #[serde(default)]
    pub nets: Vec<String>,
    /// Do not route these nets (they stay as they are).
    #[serde(default)]
    pub exclude_nets: Vec<String>,
    /// Tracks and vias already on the board stay as they are and the router routes what is left
    /// (default true). With false the router starts the nets it routes from nothing: their
    /// unlocked tracks and vias are ripped up and drawn again (locked ones always stay).
    #[serde(default)]
    pub keep_existing: Option<bool>,
}

/// A routed session waiting to be imported.
pub struct PendingSession {
    pub ses: PathBuf,
    pub dir: PathBuf,
    /// Nets the session routes.
    pub nets: Vec<String>,
}

/// What a pending board plan does.
pub enum PendingWork {
    /// An update of the board from the schematic.
    Update { netlist: PathBuf, schematic: PathBuf },
    /// An outline or moves (Phase 12).
    Edit(Box<BoardEditPlan>),
    /// A routed Specctra session (12.7).
    Autoroute(PendingSession),
    /// Changes to the design rules (14.3).
    Rules(Box<rules::RulesPlan>),
}

/// A board plan waiting to be applied.
pub struct PendingUpdate {
    pub session_id: String,
    pub fingerprint: String,
    pub applied_with: Option<String>,
    pub work: PendingWork,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct XyMm {
    pub x_mm: f64,
    pub y_mm: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbOutlineArgs {
    pub session_id: String,
    /// Board width and height in mm. Leave both out to fit the outline around the footprints.
    #[serde(default)]
    pub width_mm: Option<f64>,
    #[serde(default)]
    pub height_mm: Option<f64>,
    /// Top-left corner of a sized outline, in mm. Default: centred on the footprints.
    #[serde(default)]
    pub origin: Option<XyMm>,
    /// Margin around the footprints for a fitted outline (default 2 mm).
    #[serde(default)]
    pub margin_mm: Option<f64>,
    /// Corner radius in mm (default 0, square corners).
    #[serde(default)]
    pub corner_radius_mm: Option<f64>,
    /// Replace the outline the board already has (its Edge.Cuts drawings are deleted).
    #[serde(default)]
    pub replace: bool,
    /// Mounting holes: "none" (default) or "corners" (one in each corner, hole_inset_mm from both
    /// edges). Or give hole_positions instead.
    #[serde(default)]
    pub mounting_holes: Option<String>,
    /// Hole centres in mm, inside the outline.
    #[serde(default)]
    pub hole_positions: Option<Vec<XyMm>>,
    /// How far a corner hole's centre is from both edges (default 4 mm).
    #[serde(default)]
    pub hole_inset_mm: Option<f64>,
    /// The hole footprint, Library:Name (default MountingHole:MountingHole_3.2mm_M3).
    #[serde(default)]
    pub hole_footprint: Option<String>,
    /// Grid the corners and holes snap to, in mm (default 0.5; 0 for none).
    #[serde(default)]
    pub grid_mm: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveArg {
    /// The footprint's reference, e.g. U1.
    pub reference: String,
    /// Absolute place of the footprint's anchor, in mm.
    #[serde(default)]
    pub x_mm: Option<f64>,
    #[serde(default)]
    pub y_mm: Option<f64>,
    /// Or an offset from where it is, in mm.
    #[serde(default)]
    pub dx_mm: Option<f64>,
    #[serde(default)]
    pub dy_mm: Option<f64>,
    /// Absolute turn in degrees, counter-clockwise on screen.
    #[serde(default)]
    pub rotation_deg: Option<f64>,
    /// Or a turn added to the current one.
    #[serde(default)]
    pub rotate_by_deg: Option<f64>,
    /// "front" or "back"; a change flips the footprint as KiCad does.
    #[serde(default)]
    pub side: Option<String>,
    /// Lock (true) or unlock (false) the footprint.
    #[serde(default)]
    pub lock: Option<bool>,
    /// Move it although it is locked. Locked footprints are refused otherwise.
    #[serde(default)]
    pub allow_locked: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttributeArg {
    /// The footprint's reference, e.g. H1.
    pub reference: String,
    /// Do not populate: kept off the assembly.
    #[serde(default)]
    pub dnp: Option<bool>,
    /// Left out of the BOM.
    #[serde(default)]
    pub exclude_from_bom: Option<bool>,
    /// Left out of the position (pick-and-place) file.
    #[serde(default)]
    pub exclude_from_pos: Option<bool>,
    /// Has no part in the schematic.
    #[serde(default)]
    pub board_only: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbAttributesArgs {
    pub session_id: String,
    /// Footprints to change; an attribute left out stays as it is.
    #[serde(default)]
    pub footprints: Vec<AttributeArg>,
    /// Also mark every mounting hole as board only and left out of the BOM and the position file.
    #[serde(default)]
    pub mounting_holes: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PadEditArg {
    /// The pad's number, e.g. "1". Every pad of the footprint with that number is changed.
    pub number: String,
    /// The hole's diameter, mm (a through-hole pad only). With drill_height_mm the hole is a slot.
    #[serde(default)]
    pub drill_mm: Option<f64>,
    /// A slot's height, mm; needs drill_mm as its width.
    #[serde(default)]
    pub drill_height_mm: Option<f64>,
    /// The copper's width and height before the pad is turned, mm (give both).
    #[serde(default)]
    pub width_mm: Option<f64>,
    #[serde(default)]
    pub height_mm: Option<f64>,
    /// How a zone connects: inherited, none, thermal, full, or pth_thermal.
    #[serde(default)]
    pub zone_connection: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextEditArg {
    /// "reference" or "value".
    pub field: String,
    /// F.SilkS, B.SilkS, F.Fab, B.Fab, F.CrtYd, B.CrtYd, F.Adhes, B.Adhes, F.Cu, or B.Cu.
    #[serde(default)]
    pub layer: Option<String>,
    /// Character height, mm (the width follows).
    #[serde(default)]
    pub height_mm: Option<f64>,
    /// Stroke width, mm.
    #[serde(default)]
    pub thickness_mm: Option<f64>,
    /// Absolute place on the board, mm (give both).
    #[serde(default)]
    pub x_mm: Option<f64>,
    #[serde(default)]
    pub y_mm: Option<f64>,
    /// Or an offset from where it is, mm (one alone is fine; the other is 0).
    #[serde(default)]
    pub dx_mm: Option<f64>,
    #[serde(default)]
    pub dy_mm: Option<f64>,
    /// Turn on the board, degrees counter-clockwise on screen.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Show (true) or hide (false) it.
    #[serde(default)]
    pub visible: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FootprintEditArg {
    /// The footprint's reference, e.g. J1.
    pub reference: String,
    #[serde(default)]
    pub pads: Vec<PadEditArg>,
    #[serde(default)]
    pub texts: Vec<TextEditArg>,
    /// Edit it although it is locked.
    #[serde(default)]
    pub allow_locked: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbFootprintEditArgs {
    pub session_id: String,
    pub footprints: Vec<FootprintEditArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrackSelectArg {
    /// Only these nets.
    #[serde(default)]
    pub nets: Vec<String>,
    /// Only these copper layers (vias are on every layer, so a layer does not narrow them).
    #[serde(default)]
    pub layers: Vec<String>,
    /// Only items inside this rectangle (a track when both its ends are).
    #[serde(default)]
    pub area: Option<RectMm>,
    /// Only these tracks and vias, by id (from pcb_list_tracks). A locked item is deleted or
    /// re-widened only when it is named here.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Include tracks (default true).
    #[serde(default)]
    pub tracks: Option<bool>,
    /// Include vias (default true).
    #[serde(default)]
    pub vias: Option<bool>,
    /// Every track and via on the board, as an explicit choice.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbTrackEditArgs {
    pub session_id: String,
    /// Which tracks and vias. Needed with an action.
    #[serde(default)]
    pub select: Option<TrackSelectArg>,
    /// delete, set_width (with width_mm), lock, or unlock.
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub width_mm: Option<f64>,
    /// Clean-ups, on the whole board and never on locked items: any of `dangling_vias`,
    /// `zero_length`, `duplicates`.
    #[serde(default)]
    pub cleanup: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbRulesArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetClassArg {
    /// The class name. A class that does not exist is created (give at least one value); `Default` is
    /// changed in place.
    pub name: String,
    #[serde(default)]
    pub clearance_mm: Option<f64>,
    #[serde(default)]
    pub track_width_mm: Option<f64>,
    #[serde(default)]
    pub via_diameter_mm: Option<f64>,
    #[serde(default)]
    pub via_drill_mm: Option<f64>,
    #[serde(default)]
    pub diff_pair_width_mm: Option<f64>,
    #[serde(default)]
    pub diff_pair_gap_mm: Option<f64>,
    /// Lower numbers win when a net is in more than one class.
    #[serde(default)]
    pub priority: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssignArg {
    /// A net on the board, e.g. GND or /VBUS. Or a pattern, not both.
    #[serde(default)]
    pub net: Option<String>,
    /// A wildcard pattern over net names: `*` for any run, `?` for one character (e.g. `/VBUS*`).
    #[serde(default)]
    pub pattern: Option<String>,
    /// The net class they go in.
    pub class: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MinimumsArg {
    #[serde(default)]
    pub clearance_mm: Option<f64>,
    #[serde(default)]
    pub track_width_mm: Option<f64>,
    /// Smallest via (copper) diameter.
    #[serde(default)]
    pub via_diameter_mm: Option<f64>,
    /// Smallest via ring: (diameter - drill) / 2.
    #[serde(default)]
    pub via_annular_mm: Option<f64>,
    /// Smallest drilled hole.
    #[serde(default)]
    pub through_hole_mm: Option<f64>,
    #[serde(default)]
    pub hole_to_hole_mm: Option<f64>,
    /// Copper to the board edge.
    #[serde(default)]
    pub copper_edge_mm: Option<f64>,
    /// Copper to a hole.
    #[serde(default)]
    pub hole_clearance_mm: Option<f64>,
    #[serde(default)]
    pub silk_clearance_mm: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaskPasteArg {
    /// How far the solder mask opening reaches past a pad.
    #[serde(default)]
    pub mask_expansion_mm: Option<f64>,
    /// The narrowest mask web between two openings.
    #[serde(default)]
    pub mask_min_width_mm: Option<f64>,
    #[serde(default)]
    pub mask_to_copper_mm: Option<f64>,
    /// Paste margin (negative shrinks the stencil opening).
    #[serde(default)]
    pub paste_margin_mm: Option<f64>,
    /// Paste margin as a fraction of the pad (-0.1 is 10 % smaller).
    #[serde(default)]
    pub paste_margin_ratio: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbRulesPlanArgs {
    pub session_id: String,
    /// Net classes to create or change (only the values given change).
    #[serde(default)]
    pub net_classes: Vec<NetClassArg>,
    /// Net classes to remove (their assignments go with them). Not `Default`.
    #[serde(default)]
    pub remove_net_classes: Vec<String>,
    /// Which nets go in which class, by name or by pattern.
    #[serde(default)]
    pub assign: Vec<AssignArg>,
    /// Nets or patterns whose assignment is removed.
    #[serde(default)]
    pub unassign: Vec<String>,
    /// The board's minimums (what DRC enforces whatever the classes say).
    #[serde(default)]
    pub minimums: Option<MinimumsArg>,
    /// Solder mask and paste margins (live sessions only).
    #[serde(default)]
    pub mask_paste: Option<MaskPasteArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbMoveArgs {
    pub session_id: String,
    pub moves: Vec<MoveArg>,
    /// Grid absolute places snap to, in mm (default 0.5; 0 for none). Offsets are not snapped.
    #[serde(default)]
    pub grid_mm: Option<f64>,
}

fn mm(v: i64) -> f64 {
    round2(v as f64 / 1e6)
}

fn pt(p: Point) -> serde_json::Value {
    serde_json::json!({ "x": mm(p.x.0), "y": mm(p.y.0) })
}

fn rect(r: Rect) -> serde_json::Value {
    serde_json::json!({ "min": pt(r.min), "max": pt(r.max), "width": mm(r.width().0), "height": mm(r.height().0) })
}

pub(super) fn board_provenance(board: &BoardSession) -> Vec<Provenance> {
    vec![Provenance {
        origin: match board.provider() {
            Provider::File => Origin::ProjectFile,
            _ => Origin::KicadIpc,
        },
        kicad_version: Some(board.kicad_version().to_owned()),
        provider: Some(provider_name(board.provider()).into()),
        document: Some(board.document_path().display().to_string()),
        revision: None,
    }]
}

/// A fingerprint of what is on the board, so a plan made earlier can be told from a stale one.
fn fingerprint(board: &Board) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for f in &board.footprints {
        (
            &f.reference,
            &f.lib_id,
            f.position.x.0,
            f.position.y.0,
            f.layer.as_str(),
            (f.rotation_deg * 1000.0).round() as i64,
            f.locked,
            (f.dnp, f.exclude_from_bom, f.exclude_from_pos, f.board_only),
        )
            .hash(&mut h);
        for p in &f.pads {
            (&p.number, &p.net).hash(&mut h);
        }
    }
    board.tracks.len().hash(&mut h);
    board.vias.len().hash(&mut h);
    for t in &board.tracks {
        (&t.id, t.width.0, t.locked).hash(&mut h);
    }
    for v in &board.vias {
        (&v.id, v.locked).hash(&mut h);
    }
    board.zones.len().hash(&mut h);
    for e in &board.edges {
        (&e.id, e.locked).hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// The schematic next to a board (same name), or the one given.
pub(super) fn schematic_for(board: &BoardSession, given: Option<&str>) -> Result<PathBuf, ToolError> {
    if let Some(p) = given {
        let p = PathBuf::from(p);
        if !p.is_absolute() || p.extension().and_then(|e| e.to_str()) != Some("kicad_sch") || !p.is_file() {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "schematic_path must be the absolute path of an existing .kicad_sch file",
            ));
        }
        return Ok(p);
    }
    let sibling = board.document_path().with_extension("kicad_sch");
    if sibling.is_file() {
        Ok(sibling)
    } else {
        Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("no schematic found at {}; pass schematic_path", sibling.display()),
        ))
    }
}

/// Courtyards from the footprint libraries for footprints whose own shapes are not known.
fn fill_courtyards(board: &mut Board, libs: &kicad_session::footprints::FootprintLibraries) {
    let mut cache: BTreeMap<String, Option<Rect>> = BTreeMap::new();
    for f in board.footprints.iter_mut().filter(|f| f.courtyard.is_none()) {
        let local = cache
            .entry(f.lib_id.clone())
            .or_insert_with(|| libs.get(&f.lib_id).and_then(|fp| fp.courtyard))
            .to_owned();
        if let Some(mut local) = local {
            if f.is_back() {
                local = Rect {
                    min: Point::new(kicad_domain::Nm(-local.max.x.0), local.min.y),
                    max: Point::new(kicad_domain::Nm(-local.min.x.0), local.max.y),
                };
            }
            f.courtyard = Some(place_rect(f.position, f.rotation_deg, local));
            f.courtyard_local = Some(local);
        }
    }
}

#[tool_router(router = pcb_router, vis = "pub(super)")]
impl KicadAiServer {
    #[tool(
        name = "pcb_open",
        description = "Open a board (.kicad_pcb) in a new board session. headless: a private KiCad 11 \nserver reads it and can update it from the schematic; file: the file itself is read (KiCad 10 or 11: \nreading, DRC, render); gui: the PCB editor the user has open, after they pressed \"KiCad AI: connect\" \nin it. Returns the session id. Board sessions use the pcb_* tools; schematic tools need a schematic \nsession from session_open."
    )]
    pub async fn pcb_open(&self, Parameters(args): Parameters<PcbOpenArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let path = args.pcb_path.as_ref().map(PathBuf::from);
        if path.as_ref().is_some_and(|p| !p.is_absolute()) {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::InvalidArgument, "pcb_path must be an absolute path"),
            );
        }
        if args.read_only && !matches!(args.provider, ProviderArg::File) {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::InvalidArgument, "read_only is for the file provider"),
            );
        }
        let installs = self.installs().await.to_vec();
        let opened = match args.provider {
            ProviderArg::Headless | ProviderArg::File => {
                let Some(path) = path else {
                    return envelope::error(
                        &cid,
                        &ToolError::new(
                            ErrorCode::InvalidArgument,
                            "pcb_path is required for headless and file sessions",
                        ),
                    );
                };
                if matches!(args.provider, ProviderArg::File) {
                    BoardSession::open_file(&installs, &path)
                        .await
                        .map(|b| b.with_read_only(args.read_only))
                } else {
                    BoardSession::open_headless(&self.state.options.session, &installs, &path).await
                }
            }
            ProviderArg::Gui => match self.pick_pcb_registration(path.as_deref()).await {
                Ok(registration) => {
                    BoardSession::open_gui(&self.state.options.session, &installs, &registration, path.as_deref()).await
                }
                Err(e) => Err(e),
            },
        };
        match opened {
            Ok(board) => {
                let board = Arc::new(board);
                let status = board.status().await;
                let prov = board_provenance(&board);
                self.state.boards.lock().await.insert(board.id().to_owned(), board);
                envelope::ok(&cid, status, prov, vec![])
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_close",
        description = "Close a board session and stop its KiCad server. Refuses while the board has \nunsaved changes unless discard_unsaved is true. A gui session only disconnects."
    )]
    pub async fn pcb_close(&self, Parameters(args): Parameters<PcbCloseArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        match board.close(args.discard_unsaved).await {
            Ok(discarded) => {
                self.state.boards.lock().await.remove(&args.session_id);
                self.state
                    .pending_updates
                    .lock()
                    .await
                    .retain(|_, p| p.session_id != args.session_id);
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
        name = "pcb_reload",
        description = "Read the board from its file again: after another program, a version control checkout, or the \\neditor's own save changed the file, or to drop changes made in the open board. pcb_info and \\nsession_status say when the file changed (file_changed_on_disk). With unsaved changes it refuses and \\nlists what would be lost; pass discard_unsaved: true to go ahead. Plans made before the reload are \\ndropped. A file session reads the file each time. Not saved: the file is never written."
    )]
    pub async fn pcb_reload(&self, Parameters(args): Parameters<PcbCloseArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        match board.reload(args.discard_unsaved).await {
            Ok(report) => {
                self.state
                    .pending_updates
                    .lock()
                    .await
                    .retain(|_, p| p.session_id != args.session_id);
                envelope::ok(&cid, report, board_provenance(&board), vec![])
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(name = "pcb_save", description = "Save the board of a live session to its file.")]
    pub async fn pcb_save(&self, Parameters(args): Parameters<PcbSessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        match board.save().await {
            Ok(()) => envelope::ok(
                &cid,
                serde_json::json!({ "saved": board.document_path().display().to_string() }),
                board_provenance(&board),
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_info",
        description = "The board at a glance: counts of footprints, pads, tracks, vias, zones, and nets; \\nthe copper layers; the outline's box and size in mm (none if the board has no outline yet); and the \\nsession's status. Read-only."
    )]
    pub async fn pcb_info(&self, Parameters(args): Parameters<PcbSessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        match board.read().await {
            Ok(data) => envelope::ok(
                &cid,
                serde_json::json!({
                    "status": board.status().await,
                    "counts": data.counts(),
                    "copper_layers": data.copper_layers,
                    "outline": data.outline.map(rect),
                    "units": "mm; x grows right, y grows down",
                }),
                board_provenance(&board),
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_list_footprints",
        description = "Every footprint on the board: reference, value, library id, side, position (mm), \\nrotation, locked, do-not-populate, pad count, and courtyard size; optionally every pad with its place \\nand net. With detail: full, also each pad's shape, size, drill, and turn, the courtyard outline, and the \\ntext and fields with layer, size, place, and visibility (narrow it with references). Read-only."
    )]
    pub async fn pcb_list_footprints(&self, Parameters(args): Parameters<PcbFootprintsArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            let full = detail_wanted(args.detail.as_deref())?;
            let mut data = board.read().await?;
            let libs = self.footprint_libraries_in(board.document_path().parent()).await;
            fill_courtyards(&mut data, &libs);
            let detail = if full { Some(board.read_detail().await?) } else { None };
            Ok::<_, ToolError>((data, detail))
        }
        .await;
        match result {
            Ok((data, detail)) => {
                let mut footprints: Vec<_> = data
                    .footprints
                    .iter()
                    .filter(|f| args.references.is_empty() || args.references.contains(&f.reference))
                    .collect();
                footprints.sort_by_key(|f| kicad_domain::bom::natural(&f.reference));
                let rows: Vec<_> = footprints
                    .iter()
                    .map(|f| {
                        let mut row = footprint_json(f, args.include_pads);
                        if let Some(d) = detail.as_ref().and_then(|d| d.footprint(&f.reference)) {
                            row["detail"] = footprint_detail_json(d);
                        }
                        row
                    })
                    .collect();
                envelope::ok(
                    &cid,
                    serde_json::json!({ "count": rows.len(), "footprints": rows }),
                    board_provenance(&board),
                    vec![],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_get_footprint",
        description = "One footprint with all its pads (number, kind, place in mm, net) and its courtyard; with detail: full, also each pad's shape, size, drill, and turn, the courtyard outline, and the text and fields. Read-only."
    )]
    pub async fn pcb_get_footprint(&self, Parameters(args): Parameters<PcbFootprintArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            let full = detail_wanted(args.detail.as_deref())?;
            let mut data = board.read().await?;
            let libs = self.footprint_libraries_in(board.document_path().parent()).await;
            fill_courtyards(&mut data, &libs);
            let detail = if full { Some(board.read_detail().await?) } else { None };
            Ok::<_, ToolError>((data, detail))
        }
        .await;
        match result {
            Ok((data, detail)) => match data.footprints.iter().find(|f| f.reference == args.reference) {
                Some(f) => {
                    let mut row = footprint_json(f, true);
                    if let Some(d) = detail.as_ref().and_then(|d| d.footprint(&f.reference)) {
                        row["detail"] = footprint_detail_json(d);
                    }
                    envelope::ok(&cid, row, board_provenance(&board), vec![])
                }
                None => envelope::error(
                    &cid,
                    &ToolError::new(
                        ErrorCode::SymbolNotFound,
                        format!("no footprint {} on the board (use pcb_list_footprints)", args.reference),
                    ),
                ),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_list_drawings",
        description = "The drawings and text on the board itself (not in footprints): lines, rectangles, circles, arcs, \\npolygons, and text, each with its layer, points in mm, line width, fill, and lock, and text with its size and \\nturn. Narrow by layers (e.g. Edge.Cuts, F.SilkS) and kinds; the counts per layer cover everything. \\nRead-only."
    )]
    pub async fn pcb_list_drawings(&self, Parameters(args): Parameters<PcbDrawingsArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        match board.read_detail().await {
            Ok(d) => {
                let limit = args.limit.unwrap_or(200).clamp(1, 1000);
                let layer_ok = |l: &str| args.layers.is_empty() || args.layers.iter().any(|x| x == l);
                let kind_ok = |k: &str| args.kinds.is_empty() || args.kinds.iter().any(|x| x == k);
                let mut by_layer: BTreeMap<String, usize> = BTreeMap::new();
                for s in &d.shapes {
                    *by_layer.entry(s.layer.clone()).or_default() += 1;
                }
                for t in &d.texts {
                    *by_layer.entry(t.layer.clone()).or_default() += 1;
                }
                let shapes: Vec<_> = d
                    .shapes
                    .iter()
                    .filter(|s| layer_ok(&s.layer) && kind_ok(s.kind))
                    .collect();
                let texts: Vec<_> = d
                    .texts
                    .iter()
                    .filter(|t| layer_ok(&t.layer) && kind_ok("text"))
                    .collect();
                envelope::ok(
                    &cid,
                    serde_json::json!({
                        "counts_by_layer": by_layer,
                        "shapes": shapes.iter().take(limit).map(|s| shape_detail_json(s)).collect::<Vec<_>>(),
                        "texts": texts.iter().take(limit).map(|t| text_detail_json(t)).collect::<Vec<_>>(),
                        "shapes_matching": shapes.len(),
                        "texts_matching": texts.len(),
                        "units": "mm; x grows right, y grows down",
                    }),
                    board_provenance(&board),
                    vec![],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_list_nets",
        description = "Every net with the pads on it (REF.PAD), and how many tracks, vias, and zones \\ncarry it. Read-only."
    )]
    pub async fn pcb_list_nets(&self, Parameters(args): Parameters<PcbSessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        match board.read().await {
            Ok(data) => {
                let mut nets: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
                for name in &data.nets {
                    let pads: Vec<String> = data
                        .footprints
                        .iter()
                        .flat_map(|f| {
                            f.pads
                                .iter()
                                .filter(|p| &p.net == name)
                                .map(move |p| format!("{}.{}", f.reference, p.number))
                        })
                        .collect();
                    nets.insert(
                        name,
                        serde_json::json!({
                            "pads": pads,
                            "tracks": data.tracks.iter().filter(|t| &t.net == name).count(),
                            "vias": data.vias.iter().filter(|v| &v.net == name).count(),
                            "zones": data.zones.iter().filter(|z| &z.net == name).count(),
                        }),
                    );
                }
                envelope::ok(
                    &cid,
                    serde_json::json!({ "count": nets.len(), "nets": nets }),
                    board_provenance(&board),
                    vec![],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_list_tracks",
        description = "Tracks (start, end, width, layer, net), vias, and zones of the board, optionally \\nfor one net. Read-only."
    )]
    pub async fn pcb_list_tracks(&self, Parameters(args): Parameters<PcbTracksArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        match board.read().await {
            Ok(data) => {
                let limit = args.limit.unwrap_or(200).clamp(1, 1000);
                let keep = |net: &str| args.net.as_deref().is_none_or(|n| n == net);
                let tracks: Vec<_> = data.tracks.iter().filter(|t| keep(&t.net)).collect();
                let rows: Vec<_> = tracks
                    .iter()
                    .take(limit)
                    .map(|t| {
                        serde_json::json!({
                            "id": t.id, "from": pt(t.start), "to": pt(t.end), "width": mm(t.width.0),
                            "layer": t.layer, "net": t.net, "locked": t.locked,
                        })
                    })
                    .collect();
                let vias: Vec<_> = data
                    .vias
                    .iter()
                    .filter(|v| keep(&v.net))
                    .map(|v| {
                        serde_json::json!({
                            "id": v.id, "at": pt(v.at), "diameter": mm(v.diameter.0), "drill": mm(v.drill.0), "net": v.net,
                            "locked": v.locked,
                        })
                    })
                    .collect();
                let zones: Vec<_> = data
                    .zones
                    .iter()
                    .filter(|z| keep(&z.net))
                    .map(|z| {
                        serde_json::json!({
                            "id": z.id, "name": z.name, "layers": z.layers, "net": z.net, "priority": z.priority, "locked": z.locked,
                        })
                    })
                    .collect();
                envelope::ok(
                    &cid,
                    serde_json::json!({
                        "track_count": tracks.len(), "tracks": rows, "vias": vias, "zones": zones,
                        "truncated": tracks.len() > limit,
                    }),
                    board_provenance(&board),
                    vec![],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_run_drc",
        description = "Run the design rules check on the board as it is now (through kicad-cli, on a \\nsnapshot of a live board, with the zones refilled first unless refill_zones is false) and return the \\ncounts by type and severity, the violations, and the missing connections (unrouted). A big board can \\nhave hundreds of violations: summary_only gives the counts alone, and severity, types, and limit narrow \\nthe list (the counts stay whole and say how many were left out). With schematic_parity, also the \\ndifferences from the schematic. The board's own rules are used. Read-only."
    )]
    pub async fn pcb_run_drc(&self, Parameters(args): Parameters<PcbDrcArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let view = DrcView {
            summary_only: args.summary_only,
            severity: args.severity.clone(),
            types: args.types.clone().unwrap_or_default(),
            limit: args.limit.unwrap_or(100).min(1000),
        };
        match self
            .run_drc(&cid, &board, args.schematic_parity, args.refill_zones.unwrap_or(true))
            .await
        {
            Ok(report) => envelope::ok(
                &cid,
                self.hinted(drc_json_view(&report, &view)),
                board_provenance(&board),
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_render",
        description = "Render the board as it is now in 3D to a PNG and return the image, so placement can \\nbe checked visually. Needs a KiCad 11 kicad-cli."
    )]
    pub async fn pcb_render(&self, Parameters(args): Parameters<PcbRenderArgs>) -> CallToolResult {
        use base64::Engine;
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let side = args.side.clone().unwrap_or_else(|| "top".into());
        if !["top", "bottom", "left", "right", "front", "back"].contains(&side.as_str()) {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    "side is top, bottom, left, right, front, or back",
                ),
            );
        }
        let (width, height) = (args.width.unwrap_or(1600), args.height.unwrap_or(900));
        if !(200..=3200).contains(&width) || !(200..=2400).contains(&height) {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::InvalidArgument, "width is 200-3200 and height 200-2400"),
            );
        }
        let Some(cli) = board.render_cli().cloned() else {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    "no kicad-cli that can render a board (KiCad 10's cannot); install KiCad 11 or the 10.99 nightly",
                ),
            );
        };
        let dir = match self.scratch_dir(board.id(), "pcb-render", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            let copy = board.save_copy(&dir).await?;
            let png = dir.join("board.png");
            kicad_cli_runner::pcb::render_png(&cli, &copy, &png, &side, width, height, Duration::from_secs(180))
                .await
                .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
            std::fs::read(&png).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(bytes) => {
                let mut r = envelope::ok(
                    &cid,
                    serde_json::json!({ "format": "png", "side": side, "width": width, "height": height, "bytes": bytes.len() }),
                    board_provenance(&board),
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
        name = "pcb_review",
        description = "Review the board: DRC results and unrouted connections, footprints that overlap or \\nreach outside the outline, a missing outline, parts and footprints that the schematic and the board do not \\nshare (the schematic next to the board, or schematic_path), connectors far from the edge, decoupling \\ncapacitors far from the IC they serve, and pads on no net. Courtyards come from the footprints \\n(estimated when unknown). Findings are warnings to look at, not proof a board is wrong. Read-only."
    )]
    pub async fn pcb_review(&self, Parameters(args): Parameters<PcbReviewArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            let mut data = board.read().await?;
            let libs = self.footprint_libraries_in(board.document_path().parent()).await;
            fill_courtyards(&mut data, &libs);
            let schematic = match schematic_for(&board, args.schematic_path.as_deref()) {
                Ok(path) => {
                    let text = std::fs::read_to_string(&path).map_err(|e| {
                        ToolError::new(ErrorCode::KicadError, format!("cannot read {}: {e}", path.display()))
                    })?;
                    kicad_sexpr::Document::parse(text).ok().map(|d| (path, d.design()))
                }
                Err(e) if args.schematic_path.is_some() => return Err(e),
                Err(_) => None,
            };
            let issues = kicad_domain::board::review(&data, schematic.as_ref().map(|(_, d)| d));
            let drc = self.run_drc(&cid, &board, false, true).await.ok();
            Ok::<_, ToolError>((data, issues, drc, schematic.map(|(p, _)| p)))
        }
        .await;
        match result {
            Ok((data, issues, drc, schematic)) => envelope::ok(
                &cid,
                serde_json::json!({
                    "footprints": data.footprints.len(),
                    "outline": data.outline.map(rect),
                    "schematic_compared": schematic.map(|p| p.display().to_string()),
                    "issues": issues,
                    "drc": drc.as_ref().map(|d| self.hinted(drc_json(d))),
                    "drc_note": if drc.is_none() { Some("DRC could not be run on this board") } else { None },
                }),
                board_provenance(&board),
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_update_from_schematic_plan",
        description = "Plan updating the board from its schematic: KiCad's own netlist import is run as a \\ndry run, so nothing changes, and its report says which footprints would be added, which fields and nets \\nwould change. New footprints arrive stacked near the origin, as in KiCad; placing them is a later step. \\nThe schematic file on disk is used (save it first). Needs a live KiCad 11 session (headless or gui). \\nReturns a plan_id for pcb_apply_plan."
    )]
    pub async fn pcb_update_from_schematic_plan(&self, Parameters(args): Parameters<PcbUpdateArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            if !board.is_live() {
                return Err(ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    "updating from the schematic needs a live KiCad 11 session; open the board with provider headless or gui",
                ));
            }
            let schematic = schematic_for(&board, args.schematic_path.as_deref())?;
            let cli = self
                .installs()
                .await
                .iter()
                .filter(|i| i.version.series() >= 11)
                .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
                .cloned()
                .ok_or_else(|| ToolError::new(ErrorCode::KicadNotFound, "no KiCad 11 kicad-cli was found"))?;
            let dir = self.scratch_dir(board.id(), "pcb-update", &cid)?;
            let netlist = dir.join("update.net");
            kicad_cli_runner::netlist::export_netlist(&cli, &schematic, &netlist, Duration::from_secs(180))
                .await
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot export the netlist: {e}")))?;
            let dry = board.import_netlist(&netlist, true).await?;
            let fp = fingerprint(&board.read().await?);
            Ok((schematic, netlist, dry, fp))
        }
        .await;
        match result {
            Ok((schematic, netlist, dry, fp)) => {
                let plan_id = self.next_pcb_plan_id();
                self.state.pending_updates.lock().await.insert(
                    plan_id.clone(),
                    PendingUpdate {
                        session_id: board.id().to_owned(),
                        fingerprint: fp,
                        applied_with: None,
                        work: PendingWork::Update {
                            netlist,
                            schematic: schematic.clone(),
                        },
                    },
                );
                let can_apply = dry.errors == 0;
                envelope::ok(
                    &cid,
                    serde_json::json!({
                        "plan_id": plan_id,
                        "circuit": "update-from-schematic",
                        "can_apply": can_apply,
                        "schematic": schematic.display().to_string(),
                        "new_footprints": dry.new_footprints,
                        "errors": dry.errors,
                        "warnings": dry.warnings,
                        "report": dry.report,
                        "note": "Nothing has changed. Apply with pcb_apply_plan after the user confirms, then pcb_save.",
                    }),
                    board_provenance(&board),
                    vec![],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_outline_plan",
        description = "Plan a board outline on Edge.Cuts: a rectangle (rounded with corner_radius_mm), of \\nwidth_mm x height_mm at origin (default centred on the parts), or fitted around the footprints with \\nmargin_mm (default 2 mm). Optional mounting holes in the corners (mounting_holes: \"corners\") or at \\nhole_positions, named H1, H2, ... A board that has an outline is refused unless replace is true. Corners \\nand holes snap to grid_mm (0.5). The plan shows the outline, the holes, and footprints outside the new \\noutline or hit by a hole. Needs a live KiCad 11 session. Apply with pcb_apply_plan."
    )]
    pub async fn pcb_outline_plan(&self, Parameters(args): Parameters<PcbOutlineArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let mut data = board.read().await?;
            let libs = self.footprint_libraries_in(board.document_path().parent()).await;
            fill_courtyards(&mut data, &libs);
            let size = match (args.width_mm, args.height_mm) {
                (Some(w), Some(h)) => Some((nm_arg("width_mm", w)?, nm_arg("height_mm", h)?)),
                (None, None) => None,
                _ => {
                    return Err(bad_arg(
                        "give both width_mm and height_mm, or neither to fit the outline",
                    ))
                }
            };
            let hole_footprint = args
                .hole_footprint
                .clone()
                .unwrap_or_else(|| board_edit::DEFAULT_HOLE_FOOTPRINT.to_owned());
            let holes = match (args.mounting_holes.as_deref(), &args.hole_positions) {
                (Some(_), Some(_)) => return Err(bad_arg("give mounting_holes or hole_positions, not both")),
                (None | Some("none"), None) => HolePlacement::None,
                (Some("corners"), None) => HolePlacement::Corners {
                    inset: nm_arg(
                        "hole_inset_mm",
                        args.hole_inset_mm.unwrap_or(board_edit::DEFAULT_HOLE_INSET_MM),
                    )?,
                },
                (Some(other), None) => return Err(bad_arg(format!("mounting_holes is none or corners, not {other}"))),
                (None, Some(points)) => HolePlacement::At(
                    points
                        .iter()
                        .map(|p| point_arg(p.x_mm, p.y_mm))
                        .collect::<Result<_, _>>()?,
                ),
            };
            let hole_courtyard = if matches!(holes, HolePlacement::None) {
                None
            } else {
                let fp = libs.get(&hole_footprint).ok_or_else(|| {
                    ToolError::new(
                        ErrorCode::SymbolNotFound,
                        format!("no footprint {hole_footprint} in the libraries (use fp_search)"),
                    )
                })?;
                fp.courtyard
            };
            let request = OutlineRequest {
                size,
                origin: args.origin.as_ref().map(|o| point_arg(o.x_mm, o.y_mm)).transpose()?,
                margin: nm_arg("margin_mm", args.margin_mm.unwrap_or(board_edit::DEFAULT_FIT_MARGIN_MM))?,
                corner_radius: nm_arg("corner_radius_mm", args.corner_radius_mm.unwrap_or(0.0))?,
                replace: args.replace,
                holes,
                hole_footprint,
                hole_courtyard,
                grid_mm: args.grid_mm.unwrap_or(board_edit::DEFAULT_GRID_MM),
            };
            let plan = board_edit::plan_outline(&data, &request)?;
            Ok::<_, ToolError>((plan, fingerprint(&data)))
        }
        .await;
        match result {
            Ok((plan, fp)) => self.store_edit_plan(&cid, &board, plan, fp).await,
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_footprint_attrs_plan",
        description = "Plan changing footprint attributes: dnp (do not populate), exclude_from_bom, exclude_from_pos 
(left out of the position file), and board_only (no part in the schematic), on named footprints 
(an attribute left out stays as it is), and with mounting_holes: true on every mounting hole, so holes from 
boards made with v0.7.0 stop appearing as parts to place. Nothing moves. Needs a live KiCad 11 session. 
Apply with pcb_apply_plan; the attributes are read back."
    )]
    pub async fn pcb_footprint_attrs_plan(&self, Parameters(args): Parameters<PcbAttributesArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let data = board.read().await?;
            let requests: Vec<_> = args
                .footprints
                .iter()
                .map(|a| board_edit::AttributeRequest {
                    reference: a.reference.clone(),
                    dnp: a.dnp,
                    exclude_from_bom: a.exclude_from_bom,
                    exclude_from_pos: a.exclude_from_pos,
                    board_only: a.board_only,
                })
                .collect();
            let plan = board_edit::plan_attributes(&data, &requests, args.mounting_holes)?;
            Ok::<_, ToolError>((plan, fingerprint(&data)))
        }
        .await;
        match result {
            Ok((plan, fp)) => self.store_edit_plan(&cid, &board, plan, fp).await,
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_footprint_edit_plan",
        description = "Plan changing a placed footprint's pads and text without touching the library: a pad's hole \n(drill_mm, or a slot with drill_height_mm), copper size (width_mm and height_mm), and zone connection \n(inherited, none, thermal, full, pth_thermal); and the reference or value text's layer (silkscreen, fab, \ncourtyard), height, stroke, place (x_mm and y_mm, or dx_mm and dy_mm from where it is), turn, and visibility. \nA pad is named by number and every pad with that number changes. The plan refuses a hole on a surface-mount \npad, a hole as large as the copper, and unequal sides on a round pad, and warns about a thin annular ring, \nsilkscreen too small to print, and text on the other side from its part. Footprints are not moved (use \npcb_move_plan) and nets are not changed. Needs a live KiCad 11 session. Apply with pcb_apply_plan: one change \n(one undo step in the editor), zones refilled, every pad and text read back, DRC before and after (a worse DRC \nrolls the change back)."
    )]
    pub async fn pcb_footprint_edit_plan(&self, Parameters(args): Parameters<PcbFootprintEditArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let data = board.read().await?;
            let detail = board.read_detail().await?;
            let requests = args
                .footprints
                .iter()
                .map(footprint_edit_request)
                .collect::<Result<Vec<_>, ToolError>>()?;
            let plan = footprint_edit::plan_footprint_edits(&data, &detail, &requests)?;
            Ok::<_, ToolError>((plan, fingerprint(&data)))
        }
        .await;
        match result {
            Ok((plan, fp)) => self.store_edit_plan(&cid, &board, plan, fp).await,
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_track_edit_plan",
        description = "Plan editing the copper already on the board: delete (rip up) tracks and vias, give tracks a new \\nwidth, or lock and unlock them. select picks what: nets, layers, an area (rect in mm), ids (from \\npcb_list_tracks), tracks/vias (both by default), or all: true. Locked items are safe: a delete or width \\nchange skips them (the plan lists which) unless they are named in ids. cleanup (any of dangling_vias, \\nzero_length, duplicates) removes what a router leaves behind, on the whole board, never touching locked \\nitems. The plan lists every item touched and a count per net. Needs a live KiCad 11 session. Apply with \\npcb_apply_plan: one change (one undo step in the editor), read back, with DRC before and after; \\ndeleting tracks shows up as new unrouted connections. Routes can be made locked with pcb_route_plan."
    )]
    pub async fn pcb_track_edit_plan(&self, Parameters(args): Parameters<PcbTrackEditArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let data = board.read().await?;
            let select = match &args.select {
                Some(s) => track_selector(s)?,
                None => track_edit::Selector::default(),
            };
            let action = match (args.action.as_deref(), args.width_mm) {
                (None, None) => None,
                (None, Some(_)) => return Err(bad_arg("width_mm needs action: set_width")),
                (Some("delete"), None) => Some(track_edit::Action::Delete),
                (Some("lock"), None) => Some(track_edit::Action::Lock),
                (Some("unlock"), None) => Some(track_edit::Action::Unlock),
                (Some("set_width"), Some(w)) => Some(track_edit::Action::SetWidth(nm_arg("width_mm", w)?)),
                (Some("set_width"), None) => return Err(bad_arg("set_width needs width_mm")),
                (Some(a @ ("delete" | "lock" | "unlock")), Some(_)) => {
                    return Err(bad_arg(format!("{a} does not take width_mm")))
                }
                (Some(other), _) => {
                    return Err(bad_arg(format!(
                        "action is delete, set_width, lock, or unlock, not '{other}'"
                    )))
                }
            };
            let mut cleanup = track_edit::Cleanup::default();
            for c in &args.cleanup {
                match c.as_str() {
                    "dangling_vias" => cleanup.dangling_vias = true,
                    "zero_length" => cleanup.zero_length = true,
                    "duplicates" => cleanup.duplicates = true,
                    other => {
                        return Err(bad_arg(format!(
                            "cleanup is dangling_vias, zero_length, or duplicates, not '{other}'"
                        )))
                    }
                }
            }
            let (plan, notes) = track_edit::plan_track_edit(&data, &select, action, cleanup)?;
            let deletes = plan
                .operations
                .iter()
                .any(|o| matches!(o, BoardOp::RemoveTrack { .. } | BoardOp::RemoveVia { .. }));
            let extras = serde_json::json!({
                "per_net": per_net(&plan),
                "skipped_locked": notes.skipped_locked,
                "already_as_asked": notes.unchanged,
                "checkpoint": if deletes {
                    Some("this deletes copper: apply with checkpoint: \"before the rip-up\" (or call checkpoint_create) to be able to take it back with checkpoint_restore")
                } else { None },
            });
            Ok::<_, ToolError>((plan, extras, fingerprint(&data)))
        }
        .await;
        match result {
            Ok((plan, extras, fp)) => self.store_edit_plan_extras(&cid, &board, plan, fp, Some(extras)).await,
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_rules",
        description = "Read the board's design rules: the net classes (clearance, track width, via diameter and drill, \\ndifferential pair width and gap, priority), which nets are in which class (direct assignments, then \\npatterns, else Default), the board minimums (clearance, track width, via size and ring, hole, hole to \\nhole, copper to edge and to hole, silk), the solder mask and paste margins (live sessions), and the \\nstackup (layers, thickness, material; live sessions). Read-only. Change them with pcb_rules_plan."
    )]
    pub async fn pcb_rules(&self, Parameters(args): Parameters<PcbRulesArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            let state = board.read_rules().await?;
            let data = board.read().await?;
            let stackup = board.read_stackup().await?;
            Ok::<_, ToolError>((state, data.nets, stackup))
        }
        .await;
        match result {
            Ok((state, nets, stackup)) => envelope::ok(
                &cid,
                serde_json::json!({
                    "net_classes": state.classes.iter().map(|c| serde_json::json!({
                        "name": c.name, "priority": c.priority,
                        "values_mm": c.values.iter().map(|(f, v)| (rules::field_name(*f).replace(' ', "_"), mm(v.0))).collect::<BTreeMap<_, _>>(),
                        "nets": nets_in_class(&state, &nets, &c.name),
                    })).collect::<Vec<_>>(),
                    "assignments": state.assignments,
                    "patterns": state.patterns.iter().map(|(p, k)| serde_json::json!({ "pattern": p, "class": k })).collect::<Vec<_>>(),
                    "minimums_mm": state.minimums.iter().map(|(f, v)| (rules::min_name(*f).replace(' ', "_"), mm(v.0))).collect::<BTreeMap<_, _>>(),
                    "mask_paste_mm": if board.is_file_session() { serde_json::Value::Null } else {
                        serde_json::json!({
                            "values": state.mask.iter().map(|(f, v)| (rules::mask_name(*f).replace(' ', "_"), mm(v.0))).collect::<BTreeMap<_, _>>(),
                            "paste_margin_ratio": state.paste_ratio,
                        })
                    },
                    "stackup": stackup,
                    "note": "Change with pcb_rules_plan.",
                }),
                board_provenance(&board),
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_rules_plan",
        description = "Plan changes to the design rules: net_classes (create or change values: clearance_mm, track_width_mm, \\nvia_diameter_mm, via_drill_mm, diff_pair_width_mm, diff_pair_gap_mm, priority), remove_net_classes, \\nassign (a net or a wildcard pattern to a class), unassign, minimums (clearance, track width, via size \\nand ring, hole sizes, copper to edge), and mask_paste (solder mask and paste margins). The plan lists \\neach change with the value it replaces, and warns when a class is below a board minimum (DRC would flag \\nevery track made with it). Apply with pcb_apply_plan: in a live session the rules change in the open \\nboard (pcb_save writes them to the project); with no editor, a file session edits only these keys of \\nthe .kicad_pro file after kicad-cli has accepted a copy, and keeps a .thingsvu-bak copy. Rules are \\nread back; DRC before and after is reported, not judged. Mask and paste margins need a live session."
    )]
    pub async fn pcb_rules_plan(&self, Parameters(args): Parameters<PcbRulesPlanArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            let current = board.read_rules().await?;
            let data = board.read().await?;
            let request = rules_request(&args)?;
            if board.is_file_session() && (!request.mask.is_empty() || request.paste_ratio.is_some()) {
                return Err(ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    "solder mask and paste margins are stored in the board file; open the board with a live KiCad 11 session to change them",
                ));
            }
            let plan = rules::plan_rules(&current, &data.nets, &request)?;
            Ok::<_, ToolError>((plan, rules_fingerprint(&current)))
        }
        .await;
        match result {
            Ok((plan, fp)) => {
                let plan_id = self.next_pcb_plan_id();
                let warnings = plan.warnings.clone();
                let body = serde_json::json!({
                    "plan_id": plan_id,
                    "kind": "rules",
                    "can_apply": true,
                    "changes": plan.changes.iter().map(rule_change_json).collect::<Vec<_>>(),
                    "warnings": plan.warnings,
                    "via": if board.is_file_session() { "the .kicad_pro file (a backup is kept)" } else { "the open board; pcb_save writes the project" },
                    "note": "Nothing has changed. Apply with pcb_apply_plan after the user confirms.",
                });
                self.state.pending_updates.lock().await.insert(
                    plan_id,
                    PendingUpdate {
                        session_id: board.id().to_owned(),
                        fingerprint: fp,
                        applied_with: None,
                        work: PendingWork::Rules(Box::new(plan)),
                    },
                );
                envelope::ok(&cid, body, board_provenance(&board), warnings)
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_move_plan",
        description = "Plan moving named footprints: each entry gives a reference and an absolute place \n(x_mm, y_mm) or an offset (dx_mm, dy_mm), a turn (rotation_deg, or rotate_by_deg), a side (front or \nback; a flip as in KiCad), and lock true or false. Absolute places snap to grid_mm (0.5; 0 for none); \noffsets are exact. Locked footprints are refused unless allow_locked is true. The plan lists every move \nwith the overlaps and footprints outside the outline before and after. Needs a live KiCad 11 session. \nApply with pcb_apply_plan; every pad is read back."
    )]
    pub async fn pcb_move_plan(&self, Parameters(args): Parameters<PcbMoveArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let mut data = board.read().await?;
            let libs = self.footprint_libraries_in(board.document_path().parent()).await;
            fill_courtyards(&mut data, &libs);
            let requests = args.moves.iter().map(move_request).collect::<Result<Vec<_>, _>>()?;
            let plan = board_edit::plan_moves(&data, &requests, args.grid_mm.unwrap_or(board_edit::DEFAULT_GRID_MM))?;
            Ok::<_, ToolError>((plan, fingerprint(&data)))
        }
        .await;
        match result {
            Ok((plan, fp)) => self.store_edit_plan(&cid, &board, plan, fp).await,
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_place_plan",
        description = "Plan placing the footprints inside the board outline: the IC with the most pads in \\nthe middle, connectors on the nearest edge (or the one an edge hint names), decoupling capacitors beside \\nthe IC pin on their supply, the other parts near what they connect to; then an improvement loop on \\nestimated wire length with overlaps and parts outside the outline heavily penalised. Deterministic. \\nLocked footprints and mounting holes stay; hints: keep, place (exact), near, edge, groups, side, only. Parts \\ninside a rectangle drawn on the schematic are kept together. The plan lists every move and the measured \\nresult before and after (wire length, overlaps, outside, decoupling distances, connector edge distances). \\nNeeds an outline (pcb_outline_plan) and a live KiCad 11 session. Apply with pcb_apply_plan; adjust with \\npcb_move_plan."
    )]
    pub async fn pcb_place_plan(&self, Parameters(args): Parameters<PcbPlaceArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let mut data = board.read().await?;
            let libs = self.footprint_libraries_in(board.document_path().parent()).await;
            fill_courtyards(&mut data, &libs);
            let mut groups = args.groups.clone();
            if args.use_schematic_blocks.unwrap_or(true) {
                let schematic = match schematic_for(&board, args.schematic_path.as_deref()) {
                    Ok(p) => Some(p),
                    Err(e) if args.schematic_path.is_some() => return Err(e),
                    Err(_) => None,
                };
                if let Some(path) = schematic {
                    if let Ok(Ok(doc)) = std::fs::read_to_string(&path).map(kicad_sexpr::Document::parse) {
                        let on_board: std::collections::BTreeSet<&str> =
                            data.footprints.iter().map(|f| f.reference.as_str()).collect();
                        for block in placement::schematic_blocks(&doc.design()) {
                            let block: Vec<String> =
                                block.into_iter().filter(|r| on_board.contains(r.as_str())).collect();
                            if block.len() >= 2 && !groups.contains(&block) {
                                groups.push(block);
                            }
                        }
                    }
                }
            }
            let hints = PlaceHints {
                keep: args.keep.clone(),
                place: args
                    .place
                    .iter()
                    .map(|p| {
                        Ok(PinnedPlace {
                            reference: p.reference.clone(),
                            at: point_arg(p.x_mm, p.y_mm)?,
                            rotation_deg: p.rotation_deg,
                        })
                    })
                    .collect::<Result<_, ToolError>>()?,
                near: args
                    .near
                    .iter()
                    .map(|n| NearHint {
                        reference: n.reference.clone(),
                        target: n.target.clone(),
                        max_mm: n.max_mm.unwrap_or(5.0),
                    })
                    .collect(),
                edge: args
                    .edge
                    .iter()
                    .map(|e| {
                        placement::Edge::parse(&e.edge)
                            .map(|edge| (e.reference.clone(), edge))
                            .ok_or_else(|| bad_arg(format!("{}: edge is left, right, top, or bottom", e.reference)))
                    })
                    .collect::<Result<_, _>>()?,
                groups,
                side: args
                    .side
                    .iter()
                    .map(|s| match s.side.as_str() {
                        "front" => Ok((s.reference.clone(), false)),
                        "back" => Ok((s.reference.clone(), true)),
                        other => Err(bad_arg(format!("{}: side is front or back, not {other}", s.reference))),
                    })
                    .collect::<Result<_, _>>()?,
                only: args.only.clone(),
            };
            let defaults = PlaceOptions::default();
            let options = PlaceOptions {
                grid_mm: args.grid_mm.unwrap_or(defaults.grid_mm),
                edge_clearance_mm: args.edge_clearance_mm.unwrap_or(defaults.edge_clearance_mm),
                spacing_mm: args.spacing_mm.unwrap_or(defaults.spacing_mm),
                ..defaults
            };
            let (plan, report) = placement::plan_placement(&data, &hints, &options)?;
            Ok::<_, ToolError>((plan, report, fingerprint(&data)))
        }
        .await;
        match result {
            Ok((plan, report, fp)) => {
                let extra = serde_json::to_value(&report).unwrap_or_default();
                self.store_edit_plan_with(&cid, &board, plan, fp, Some(extra)).await
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_route_plan",
        description = "Plan tracks and vias for named nets. Each route is a list of points (pads as \\nREF.PAD, or x_mm/y_mm); a point's layer applies to the segment leaving it, and a change of layer puts a \\nvia there from the net class (none at a through-hole pad). A route of two points with no layers is given \\na path: straight if clear, else a 45° dogleg or an L, on the asked layer or either outer layer, the \\nshortest that keeps clearance. Widths, clearances, and vias come from the board's net classes and \\nminimums. Every segment and via is checked against other nets' pads, tracks, vias, and holes and the \\nboard edge; a route that cannot keep clearance refuses the whole plan with the reason. Zones are not \\nchecked (a pour is refilled around tracks). Needs a live KiCad 11 session. Apply with pcb_apply_plan, \\nwhich runs DRC before and after. \\nUse it for deliberate tracks: power, short links, fixed paths. It finds only simple paths. For nets with many pads or routes that must find their way around parts, use pcb_autoroute_plan (Freerouting); the route plan then places only what you name. To change tracks that are already on the board (rip up, widen, lock), use pcb_track_edit_plan; to see them, pcb_list_tracks."
    )]
    pub async fn pcb_route_plan(&self, Parameters(args): Parameters<PcbRouteArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let data = board.read().await?;
            let rules = board.routing_rules(&data.nets).await?;
            let requests = args
                .routes
                .iter()
                .enumerate()
                .map(|(i, r)| route_request(i, r))
                .collect::<Result<Vec<_>, _>>()?;
            let (plan, summaries) = routing::plan_routes(&data, &rules, &requests).map_err(|mut e| {
                if e.message.contains("cannot keep their clearance") {
                    e.message = format!(
                        "{} - pcb_route_plan only finds simple paths. {}",
                        e.message,
                        self.autoroute_hint()
                    );
                }
                e
            })?;
            let used: BTreeMap<&str, serde_json::Value> = summaries
                .iter()
                .map(|s| {
                    let r = rules.of(&s.net);
                    (
                        s.net.as_str(),
                        serde_json::json!({
                            "clearance": mm(r.clearance.0), "track_width": mm(r.track_width.0),
                            "via_diameter": mm(r.via_diameter.0), "via_drill": mm(r.via_drill.0),
                        }),
                    )
                })
                .collect();
            let measured = serde_json::json!({
                "routes": summaries,
                "rules": used,
                "copper_edge_clearance": mm(rules.copper_edge_clearance.0),
            });
            Ok::<_, ToolError>((plan, measured, fingerprint(&data)))
        }
        .await;
        match result {
            Ok((plan, measured, fp)) => self.store_edit_plan_with(&cid, &board, plan, fp, Some(measured)).await,
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_zone_plan",
        description = "Plan copper pours: for each zone a net (ground first), its copper layers (default \\nall), and the whole board or a rect or polygon in mm; clearance (default the net class's), minimum width \\n(0.25), pad connection thermal (default), solid, or none, thermal gap and spoke width (0.5), priority, \\nname. A net that already has a zone on those layers is refused unless replace is true; a locked zone is \\nnever deleted. The plan lists each zone, its area, the pads of the net inside it, and warnings (a pour \\nthat reaches no pad, overlapping pours of two nets with the same priority). Needs a live KiCad 11 \\nsession. Apply with pcb_apply_plan: the zones are created in one commit, then every zone is refilled \\n(a second undo step in the editor), read back, and checked with DRC before and after."
    )]
    pub async fn pcb_zone_plan(&self, Parameters(args): Parameters<PcbZoneArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let data = board.read().await?;
            let rules = board.routing_rules(&data.nets).await?;
            let requests = args
                .zones
                .iter()
                .enumerate()
                .map(|(i, z)| zone_request(i, z, &data.copper_layers))
                .collect::<Result<Vec<_>, _>>()?;
            let (plan, summaries) = zones::plan_zones(&data, &rules, &requests)?;
            let measured = serde_json::json!({ "zones": summaries });
            Ok::<_, ToolError>((plan, measured, fingerprint(&data)))
        }
        .await;
        match result {
            Ok((plan, measured, fp)) => self.store_edit_plan_with(&cid, &board, plan, fp, Some(measured)).await,
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_autoroute_plan",
        description = "Plan routing the board with Freerouting, the optional external autorouter \\n(only when configured: install.ps1 -WithFreerouting, or --freerouting <path>; the server never downloads it).\\nThe live board is exported as Specctra DSN (its net classes and rules go with it), routed in a separate \\nheadless process with a pass and time limit (default 20 passes, 120 s; no network calls), and the session \\nis summarized: per net the segments, vias, length, layers, and narrowest track, Freerouting's own unrouted \\nand violation counts, and the unlocked tracks and vias the import will replace (locked ones are kept). \\nA wire the router draws narrower than the board's minimum track width is widened to it (the plan says how many), \\nand vias that join fewer than two layers are removed after the import. Nothing changes. Apply with pcb_apply_plan: KiCad imports the session, and the board is read back and \\nchecked with DRC before and after; no footprint may move. \\nnets routes only those nets and exclude_nets leaves some alone (their tracks stay); keep_existing (default true) keeps every track and via already drawn and routes what is left, false rips up the unlocked ones of the nets it routes first (locked tracks always stay); net classes (pcb_rules_plan) give nets their widths; the time limit ends the router and everything it started. \\nThis is the tool for complex routing: multi-pad nets and anything that must find a way around parts. session_status says whether Freerouting is configured."
    )]
    pub async fn pcb_autoroute_plan(&self, Parameters(args): Parameters<PcbAutorouteArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            refuse_unless_live(&board)?;
            let router = self.state.options.freerouting.clone().ok_or_else(|| {
                ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    "no autorouter is configured: run the installer with -WithFreerouting (it sets one up), \
                     or start the server with --freerouting <path>; route with pcb_route_plan meanwhile",
                )
            })?;
            let passes = args.passes.unwrap_or(20);
            let limit = args.time_limit_s.unwrap_or(120);
            if !(1..=200).contains(&passes) || !(10..=900).contains(&limit) {
                return Err(bad_arg("passes is 1 to 200 and time_limit_s 10 to 900"));
            }
            let data = board.read().await?;
            let rules = board.routing_rules(&data.nets).await?;
            let dir = self.scratch_dir(board.id(), "autoroute", &cid)?;
            let (dsn, ses) = (dir.join("board.dsn"), dir.join("board.ses"));
            // From a snapshot of the live board: KiCad's own DSN export job reads the board file on
            // disk, so unsaved changes (a placement) would be routed as they were, and the session's
            // placement would move every part back on import (the Phase 12.7 probe).
            let snapshot = dir.join("snapshot");
            std::fs::create_dir_all(&snapshot).map_err(|e| {
                ToolError::new(
                    ErrorCode::KicadError,
                    format!("cannot create {}: {e}", snapshot.display()),
                )
            })?;
            let copy = board.save_copy(&snapshot).await?;
            let cli = board.render_cli().or(board.cli()).cloned().ok_or_else(|| {
                ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    "no kicad-cli that can export Specctra DSN (KiCad 11)",
                )
            })?;
            kicad_cli_runner::pcb::export_specctra(&cli, &copy, &dsn, Duration::from_secs(120))
                .await
                .map_err(|e| {
                    ToolError::new(
                        ErrorCode::KicadError,
                        format!("cannot export the board for the router: {e}"),
                    )
                })?;
            let min_width = mm(rules.min_track_width.0);
            // Which nets the router is asked for, and what it may rip up.
            let mut dsn_text = std::fs::read_to_string(&dsn)
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot read the exported board: {e}")))?;
            for n in args.nets.iter().chain(&args.exclude_nets) {
                if !data.nets.contains(n) {
                    return Err(bad_arg(format!("net '{n}' is not on the board (use pcb_list_nets)")));
                }
            }
            let in_dsn = kicad_sexpr::dsn::network_nets(&dsn_text);
            let routed: Vec<String> = in_dsn
                .iter()
                .filter(|n| (args.nets.is_empty() || args.nets.contains(n)) && !args.exclude_nets.contains(n))
                .cloned()
                .collect();
            if routed.is_empty() {
                return Err(bad_arg(
                    "no net is left to route (a net with one pad has nothing to connect; check nets and exclude_nets)",
                ));
            }
            let left_alone: Vec<String> = in_dsn.iter().filter(|n| !routed.contains(n)).cloned().collect();
            dsn_text = kicad_sexpr::dsn::without_pins_of(&dsn_text, &left_alone).0;
            let keep_existing = args.keep_existing.unwrap_or(true);
            let mut ripped = (0usize, 0usize);
            if !keep_existing {
                let per_nm = kicad_sexpr::dsn::units_per_mm(&dsn_text) / 1e6;
                let locked = |item: &kicad_sexpr::dsn::WiringItem| locked_in_dsn(item, &data, per_nm);
                let keep = |item: &kicad_sexpr::dsn::WiringItem| !routed.contains(&item.net) || locked(item);
                for item in kicad_sexpr::dsn::wiring_items(&dsn_text) {
                    if !keep(&item) {
                        if item.is_via {
                            ripped.1 += 1;
                        } else {
                            ripped.0 += 1;
                        }
                    }
                }
                dsn_text = kicad_sexpr::dsn::without_wiring(&dsn_text, keep).0;
            }
            std::fs::write(&dsn, &dsn_text)
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot write {}: {e}", dsn.display())))?;
            let scope = (routed, left_alone, keep_existing, ripped);
            let run = router
                .route(&dsn, &ses, passes, Duration::from_secs(limit))
                .await
                .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
            let read_ses = || {
                std::fs::read_to_string(&ses)
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot read the session: {e}")))
            };
            let read_err = |e: kicad_sexpr::ses::SesError| {
                ToolError::new(ErrorCode::KicadError, format!("cannot read the session: {e}"))
            };
            // The safety net: a wire the router still drew narrower is widened, and DRC after the
            // import checks the clearance of the wider track.
            let (widened_text, widened) = kicad_sexpr::ses::widen_wires(&read_ses()?, min_width).map_err(read_err)?;
            if widened > 0 {
                std::fs::write(&ses, &widened_text).map_err(|e| {
                    ToolError::new(ErrorCode::KicadError, format!("cannot write {}: {e}", ses.display()))
                })?;
            }
            let summary = kicad_sexpr::ses::summarize(&widened_text).map_err(read_err)?;
            let width_note = (min_width, widened);
            // What the session does to the tracks and vias that are there.
            let changes = session_changes(&data, &widened_text)?;
            Ok::<_, ToolError>((data, dir, ses, run, summary, width_note, scope, changes))
        }
        .await;
        let (data, dir, ses, run, summary, (min_width, widened), scope, changes) = match result {
            Ok(r) => r,
            Err(e) => return envelope::error(&cid, &e),
        };
        let mut warnings = Vec::new();
        if run.unrouted.unwrap_or(0) > 0 {
            warnings.push(format!(
                "Freerouting left {} connection(s) unrouted; more passes, a larger board, or a better placement may help",
                run.unrouted.unwrap_or(0)
            ));
        }
        if run.violations.unwrap_or(0) > 0 {
            warnings.push(format!(
                "Freerouting reports {} violation(s)",
                run.violations.unwrap_or(0)
            ));
        }
        if widened > 0 {
            warnings.push(format!(
                "{widened} wire(s) came back narrower than the board minimum of {min_width:.2} mm and were widened to it; \
                 DRC after applying checks their clearance"
            ));
        }
        let (routed_nets, left_alone, keep_existing, ripped) = scope;
        let (replaced_tracks, replaced_vias) = (changes.tracks_replaced, changes.vias_replaced);
        if replaced_tracks + replaced_vias > 0 {
            warnings.push(format!(
                "the import replaces {replaced_tracks} unlocked track(s) and {replaced_vias} via(s) with the router's; \
                 lock the ones to keep in KiCad (pcb_track_edit_plan lock), or name only the nets to route (nets)"
            ));
        }
        if !left_alone.is_empty() {
            warnings.push(format!(
                "{} net(s) are left alone: their tracks stay and their pads are obstacles",
                left_alone.len()
            ));
        }
        let plan_id = self.next_pcb_plan_id();
        let body = serde_json::json!({
            "plan_id": plan_id,
            "kind": "autoroute",
            "can_apply": true,
            "router": run,
            "session": summary,
            "minimum_width": { "mm": min_width, "wires_widened": widened },
            "routes": {
                "nets": routed_nets,
                "left_alone": left_alone,
                "keep_existing": keep_existing,
                "ripped_up": { "tracks": ripped.0, "vias": ripped.1 },
            },
            "replaces": {
                "tracks": replaced_tracks, "vias": replaced_vias,
                "unlocked_tracks_identical": changes.tracks_identical,
                "locked_tracks_kept": data.tracks.iter().filter(|t| t.locked).count(),
            },
            "checkpoint": if replaced_tracks + replaced_vias > 0 {
                Some("the import replaces tracks: apply with checkpoint: \"before the autoroute\" (or call checkpoint_create) to be able to take it back")
            } else { None },
            "warnings": warnings,
            "note": "Nothing has changed. Apply with pcb_apply_plan after the user confirms, then pcb_save.",
        });
        self.state.pending_updates.lock().await.insert(
            plan_id,
            PendingUpdate {
                session_id: board.id().to_owned(),
                fingerprint: fingerprint(&data),
                applied_with: None,
                work: PendingWork::Autoroute(PendingSession {
                    ses,
                    dir,
                    nets: summary
                        .nets
                        .iter()
                        .filter(|n| n.segments > 0)
                        .map(|n| n.net.clone())
                        .collect(),
                }),
            },
        );
        envelope::ok(&cid, body, board_provenance(&board), warnings)
    }

    #[tool(
        name = "pcb_apply_plan",
        description = "Apply a board plan (pcb_update_from_schematic_plan, pcb_outline_plan, pcb_move_plan, pcb_place_plan, \\npcb_footprint_attrs_plan, pcb_route_plan, pcb_zone_plan, \\npcb_track_edit_plan, pcb_autoroute_plan) \\nafter the user has reviewed it (confirm: true). Refused if the board changed since planning. An update runs \\nthe netlist import, then checks that every footprint of the schematic is there and every pad is on the net \\nthe netlist gives it; an outline, a move, a placement, a route, or a pour is one change (one undo step in the editor) and is read back: \\nthe outline, every footprint's place, turn, side, and lock, every pad's place and net, and DRC before and after (not for a move): \\nthe zones are refilled first, the counts by type are given before and after, and only the violations that are new are listed. A change that fails \\nthese checks is rolled back and the board read again to confirm it is as it was, unless \\nkeep_if_unverified is true. A headless board is not saved (use pcb_save); in the editor the change appears live and can be \\nundone there. Repeating the same idempotency_key returns the earlier result. With checkpoint: a label, the project is kept first (checkpoint_create), so a change that deletes can be taken back."
    )]
    pub async fn pcb_apply_plan(&self, Parameters(args): Parameters<PcbApplyArgs>) -> CallToolResult {
        let Some(label) = args.checkpoint.clone().filter(|_| args.confirm) else {
            return self.apply_plan_core(args).await;
        };
        // The way back is made first, once the request is known to be a real one.
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let checkpoint = match self.checkpoint_before_apply(&board, &label).await {
            Ok(c) => c,
            Err(e) => return envelope::error(&cid, &e),
        };
        let mut result = self.apply_plan_core(args).await;
        if let Some(serde_json::Value::Object(top)) = result.structured_content.as_mut() {
            if let Some(serde_json::Value::Object(body)) = top.get_mut("result") {
                body.insert("checkpoint".into(), checkpoint);
            }
            let value = serde_json::Value::Object(top.clone());
            result.content = vec![rmcp::model::ContentBlock::text(value.to_string())];
        }
        result
    }

    async fn apply_plan_core(&self, args: PcbApplyArgs) -> CallToolResult {
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
        if !(8..=128).contains(&args.idempotency_key.len()) {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::InvalidArgument, "idempotency_key must be 8-128 characters"),
            );
        }
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let (fp, work) = {
            let mut pending = self.state.pending_updates.lock().await;
            let Some(plan) = pending.get_mut(&args.plan_id) else {
                return envelope::error(
                    &cid,
                    &ToolError::new(
                        ErrorCode::StalePlan,
                        format!("no plan '{}'; make a new plan", args.plan_id),
                    ),
                );
            };
            if plan.session_id != board.id() {
                return envelope::error(
                    &cid,
                    &ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("{} belongs to another session", args.plan_id),
                    ),
                );
            }
            if let Some(key) = &plan.applied_with {
                return envelope::error(
                    &cid,
                    &ToolError::new(
                        ErrorCode::StalePlan,
                        format!("{} was already applied (key {key}); make a new plan", args.plan_id),
                    ),
                );
            }
            if let PendingWork::Rules(rules_plan) = &plan.work {
                let (fp, rules_plan) = (plan.fingerprint.clone(), rules_plan.clone());
                drop(pending);
                return self.apply_rules_plan(&cid, &board, &args, &fp, &rules_plan).await;
            }
            let work = match &plan.work {
                PendingWork::Rules(_) => unreachable!("handled above"),
                PendingWork::Update { netlist, schematic } => Err((netlist.clone(), schematic.clone())),
                PendingWork::Edit(edit) => Ok(Ok(edit.clone())),
                PendingWork::Autoroute(s) => Ok(Err((s.ses.clone(), s.dir.clone(), s.nets.clone()))),
            };
            (plan.fingerprint.clone(), work)
        };
        let (netlist, schematic) = match work {
            Ok(Ok(edit)) => return self.apply_board_edit(&cid, &board, &args, &fp, &edit).await,
            Ok(Err(session)) => return self.apply_autoroute(&cid, &board, &args, &fp, session).await,
            Err(update) => update,
        };
        let result = async {
            let before = board.read().await?;
            if fingerprint(&before) != fp {
                return Err(ToolError::new(
                    ErrorCode::StalePlan,
                    "the board changed since the plan was made; make a new plan",
                ));
            }
            let report = board.import_netlist(&netlist, false).await?;
            let after = board.read().await?;
            // The netlist the import used, to check the board against.
            let text = std::fs::read_to_string(&netlist)
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot read the netlist: {e}")))?;
            let expected = kicad_cli_runner::netlist::Netlist::parse(&text)
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot read the netlist: {e}")))?;
            Ok::<_, ToolError>((report, after, expected))
        }
        .await;
        match result {
            Ok((report, after, expected)) => {
                let mut problems: Vec<String> = Vec::new();
                for c in &expected.components {
                    if !after.footprints.iter().any(|f| f.reference == c.reference) {
                        problems.push(format!("{} has no footprint on the board", c.reference));
                    }
                }
                for net in &expected.nets {
                    for node in &net.nodes {
                        let pad = after
                            .footprints
                            .iter()
                            .find(|f| f.reference == node.reference)
                            .and_then(|f| f.pads.iter().find(|p| p.number == node.pin));
                        match pad {
                            Some(p) if p.net == net.name => {}
                            Some(p) => problems.push(format!(
                                "{}.{} is on net '{}', the netlist says '{}'",
                                node.reference, node.pin, p.net, net.name
                            )),
                            None => problems.push(format!("{}.{} has no pad on the board", node.reference, node.pin)),
                        }
                    }
                }
                if let Some(p) = self.state.pending_updates.lock().await.get_mut(&args.plan_id) {
                    p.applied_with = Some(args.idempotency_key.clone());
                }
                let verified = problems.is_empty() && report.errors == 0;
                let mut r = envelope::ok(
                    &cid,
                    serde_json::json!({
                        "applied": true,
                        "verified": verified,
                        "schematic": schematic.display().to_string(),
                        "new_footprints": report.new_footprints,
                        "errors": report.errors,
                        "warnings": report.warnings,
                        "report": report.report,
                        "footprints_on_board": after.footprints.len(),
                        "problems": problems,
                        "next": "Place the footprints, then save with pcb_save.",
                    }),
                    board_provenance(&board),
                    if verified {
                        vec![]
                    } else {
                        vec!["applied, but the board differs from the netlist; see problems".to_owned()]
                    },
                );
                if !verified {
                    r = envelope::ok_with_state(
                        &cid,
                        kicad_domain::ResultState::Partial,
                        serde_json::json!({ "applied": true, "verified": false, "problems": problems, "report": report.report }),
                        board_provenance(&board),
                        vec!["applied, but the board differs from the netlist; see problems".to_owned()],
                    );
                }
                r
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }
}

impl KicadAiServer {
    pub(super) async fn board(&self, id: &str) -> Result<Arc<BoardSession>, ToolError> {
        self.state.boards.lock().await.get(id).cloned().ok_or_else(|| {
            ToolError::new(
                ErrorCode::NoSession,
                format!("no board session '{id}'; open one with pcb_open (session_status lists sessions)"),
            )
        })
    }

    fn next_pcb_plan_id(&self) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        format!(
            "pcbplan-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// The registered PCB editor to use: the one with `path` open, or the only one.
    async fn pick_pcb_registration(
        &self,
        path: Option<&Path>,
    ) -> Result<kicad_session::registry::Registration, ToolError> {
        let live = self.live_registrations().await;
        let with_boards: Vec<_> = live
            .into_iter()
            .filter(|r| r.documents.iter().any(|d| d.kind == "pcb"))
            .collect();
        if with_boards.is_empty() {
            return Err(ToolError::new(
                ErrorCode::NoSession,
                "no KiCad PCB editor is registered: open the board in KiCad 11 and press the \"KiCad AI\" toolbar button in the PCB Editor",
            ));
        }
        if let Some(path) = path {
            let folder = path.parent().map(|p| p.to_string_lossy().to_lowercase());
            let stem = path.file_stem().map(|s| s.to_string_lossy().to_lowercase());
            if let Some(r) = with_boards.iter().find(|r| {
                r.documents.iter().any(|d| {
                    d.kind == "pcb"
                        && d.project.as_ref().map(|p| p.to_lowercase()) == stem
                        && d.project_path
                            .as_ref()
                            .map(|p| p.trim_end_matches(['\\', '/']).to_lowercase())
                            == folder
                })
            }) {
                return Ok(r.clone());
            }
        }
        match with_boards.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "several KiCad PCB editors are registered; pass pcb_path to choose one",
            )),
        }
    }

    /// DRC on a snapshot of the board.
    async fn run_drc(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        parity: bool,
        refill_zones: bool,
    ) -> Result<kicad_cli_runner::pcb::DrcReport, ToolError> {
        let cli = board
            .cli()
            .cloned()
            .ok_or_else(|| ToolError::new(ErrorCode::FeatureUnavailable, "no matching kicad-cli install"))?;
        let dir = self.scratch_dir(board.id(), "pcb-drc", cid)?;
        let result = async {
            let copy = board.save_copy(&dir).await?;
            // The schematic next to the board lets parity checks compare the two.
            if parity {
                if let Ok(s) = schematic_for(board, None) {
                    if let Some(name) = s.file_name() {
                        let _ = std::fs::copy(&s, dir.join(name));
                    }
                }
            }
            kicad_cli_runner::pcb::run_drc(
                &cli,
                &copy,
                &dir.join("drc.json"),
                parity,
                refill_zones,
                Duration::from_secs(300),
            )
            .await
            .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        result
    }
}

impl KicadAiServer {
    /// Applies changes to the design rules, reads them back, and reports DRC before and after.
    async fn apply_rules_plan(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        args: &PcbApplyArgs,
        fp: &str,
        plan: &rules::RulesPlan,
    ) -> CallToolResult {
        let result = async {
            let before = board.read_rules().await?;
            if rules_fingerprint(&before) != fp {
                return Err(ToolError::new(
                    ErrorCode::StalePlan,
                    "the rules changed since the plan was made; make a new plan",
                ));
            }
            let drc_before = self.run_drc(cid, board, false, true).await.ok();
            board.apply_rules(plan).await?;
            let after = board.read_rules().await?;
            let drc_after = self.run_drc(cid, board, false, true).await.ok();
            Ok::<_, ToolError>((after, drc_before, drc_after))
        }
        .await;
        let (after, drc_before, drc_after) = match result {
            Ok(r) => r,
            Err(e) => return envelope::error(cid, &e),
        };
        if let Some(p) = self.state.pending_updates.lock().await.get_mut(&args.plan_id) {
            p.applied_with = Some(args.idempotency_key.clone());
        }
        let problems = rules::verify_rules(plan, &after);
        let verified = problems.is_empty();
        let drc = match (&drc_before, &drc_after) {
            (Some(b), Some(a)) => {
                let mut cmp = drc_comparison(b, a);
                cmp["after"] = self.hinted(drc_json_view(
                    a,
                    &DrcView {
                        summary_only: true,
                        ..DrcView::default()
                    },
                ));
                Some(cmp)
            }
            _ => None,
        };
        let undo = match board.provider() {
            Provider::File => "the project file was replaced; the earlier one is next to it as .kicad_pro.thingsvu-bak".to_owned(),
            Provider::Gui => "changed in the open editor; pcb_save writes the project (the editor can undo the board, not the project rules)".to_owned(),
            _ => "not saved yet; pcb_save writes the rules to the project, closing with discard_unsaved drops them".to_owned(),
        };
        let body = serde_json::json!({
            "applied": true,
            "verified": verified,
            "kind": "rules",
            "changes": plan.changes.len(),
            "problems": problems,
            "drc": drc,
            "undo": undo,
            "note": "DRC is reported, not judged: tighter rules are meant to show new violations. Rules are not rolled back automatically.",
            "next": "Check with pcb_rules, then pcb_save.",
        });
        if verified {
            envelope::ok(cid, body, board_provenance(board), vec![])
        } else {
            envelope::ok_with_state(
                cid,
                kicad_domain::ResultState::Partial,
                body,
                board_provenance(board),
                vec!["applied, but the rules read back differently; see problems".to_owned()],
            )
        }
    }
}

fn rules_fingerprint(state: &rules::RulesState) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{state:?}").hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The nets in a class, as KiCad resolves them: a direct assignment, then the first pattern that
/// matches, else the Default class.
fn nets_in_class(state: &rules::RulesState, nets: &[String], class: &str) -> Vec<String> {
    nets.iter()
        .filter(|n| {
            let resolved = state
                .assignments
                .get(*n)
                .and_then(|v| v.first().cloned())
                .or_else(|| {
                    state
                        .patterns
                        .iter()
                        .find(|(p, _)| rules::glob_matches(p, n))
                        .map(|(_, k)| k.clone())
                })
                .unwrap_or_else(|| rules::DEFAULT_CLASS.to_owned());
            resolved == class
        })
        .cloned()
        .collect()
}

fn rules_request(a: &PcbRulesPlanArgs) -> Result<rules::RulesRequest, ToolError> {
    use rules::{ClassField, MaskField, MinField};
    let opt = |name: &str, v: Option<f64>| v.map(|x| nm_arg(name, x)).transpose();
    let mut request = rules::RulesRequest {
        remove_classes: a.remove_net_classes.clone(),
        unassign: a.unassign.clone(),
        ..Default::default()
    };
    for c in &a.net_classes {
        let mut values = BTreeMap::new();
        for (field, name, v) in [
            (ClassField::Clearance, "clearance_mm", c.clearance_mm),
            (ClassField::TrackWidth, "track_width_mm", c.track_width_mm),
            (ClassField::ViaDiameter, "via_diameter_mm", c.via_diameter_mm),
            (ClassField::ViaDrill, "via_drill_mm", c.via_drill_mm),
            (ClassField::DiffPairWidth, "diff_pair_width_mm", c.diff_pair_width_mm),
            (ClassField::DiffPairGap, "diff_pair_gap_mm", c.diff_pair_gap_mm),
        ] {
            if let Some(v) = opt(name, v)? {
                values.insert(field, v);
            }
        }
        request.classes.push(rules::ClassRequest {
            name: c.name.clone(),
            priority: c.priority,
            values,
        });
    }
    for x in &a.assign {
        request.assign.push(rules::AssignRequest {
            net: x.net.clone(),
            pattern: x.pattern.clone(),
            class: x.class.clone(),
        });
    }
    if let Some(m) = &a.minimums {
        for (field, name, v) in [
            (MinField::Clearance, "clearance_mm", m.clearance_mm),
            (MinField::TrackWidth, "track_width_mm", m.track_width_mm),
            (MinField::ViaDiameter, "via_diameter_mm", m.via_diameter_mm),
            (MinField::ViaAnnular, "via_annular_mm", m.via_annular_mm),
            (MinField::ThroughHole, "through_hole_mm", m.through_hole_mm),
            (MinField::HoleToHole, "hole_to_hole_mm", m.hole_to_hole_mm),
            (MinField::CopperEdge, "copper_edge_mm", m.copper_edge_mm),
            (MinField::HoleClearance, "hole_clearance_mm", m.hole_clearance_mm),
            (MinField::SilkClearance, "silk_clearance_mm", m.silk_clearance_mm),
        ] {
            if let Some(v) = opt(name, v)? {
                request.minimums.insert(field, v);
            }
        }
    }
    if let Some(m) = &a.mask_paste {
        for (field, name, v) in [
            (MaskField::Expansion, "mask_expansion_mm", m.mask_expansion_mm),
            (MaskField::MinWidth, "mask_min_width_mm", m.mask_min_width_mm),
            (MaskField::ToCopper, "mask_to_copper_mm", m.mask_to_copper_mm),
            (MaskField::PasteMargin, "paste_margin_mm", m.paste_margin_mm),
        ] {
            if let Some(v) = opt(name, v)? {
                request.mask.insert(field, v);
            }
        }
        request.paste_ratio = m.paste_margin_ratio;
    }
    Ok(request)
}

fn rule_change_json(c: &rules::RuleChange) -> serde_json::Value {
    use rules::RuleOp::*;
    let mut v = serde_json::to_value(c).unwrap_or_default();
    // Sizes are in nm in the model; show mm.
    if let ClassSet { value, .. } | MinSet { value, .. } | MaskSet { value, .. } = &c.op {
        v["value"] = serde_json::json!(mm(value.0));
    }
    v
}

fn detail_wanted(detail: Option<&str>) -> Result<bool, ToolError> {
    match detail {
        None | Some("summary") => Ok(false),
        Some("full") => Ok(true),
        Some(other) => Err(bad_arg(format!("detail is summary or full, not '{other}'"))),
    }
}

fn xy(p: Point) -> serde_json::Value {
    serde_json::json!({ "x": mm(p.x.0), "y": mm(p.y.0) })
}

fn text_detail_json(t: &kicad_domain::board_detail::TextDetail) -> serde_json::Value {
    serde_json::json!({
        "id": t.id, "name": t.name, "text": t.text, "layer": t.layer, "at": xy(t.at),
        "angle_deg": round2(t.angle_deg), "height": mm(t.height.0), "thickness": mm(t.thickness.0),
        "visible": t.visible, "locked": t.locked,
    })
}

fn shape_detail_json(s: &kicad_domain::board_detail::ShapeDetail) -> serde_json::Value {
    serde_json::json!({
        "id": s.id, "kind": s.kind, "layer": s.layer, "points": s.points.iter().map(|p| xy(*p)).collect::<Vec<_>>(),
        "width": mm(s.width.0), "filled": s.filled, "locked": s.locked,
    })
}

fn footprint_detail_json(d: &kicad_domain::board_detail::FootprintDetail) -> serde_json::Value {
    serde_json::json!({
        "pads": d.pads.iter().map(|p| serde_json::json!({
            "number": p.number, "kind": p.kind, "shape": p.shape, "at": xy(p.at),
            "angle_deg": round2(p.angle_deg),
            "size": p.size.map(|(w, h)| serde_json::json!({ "x": mm(w.0), "y": mm(h.0) })),
            "drill": p.drill.map(|(w, h)| serde_json::json!({ "x": mm(w.0), "y": mm(h.0) })),
            "corner_ratio": p.corner_ratio, "layers": p.layers, "net": p.net, "zone_connection": p.zone_connection,
        })).collect::<Vec<_>>(),
        "courtyard": d.courtyard.iter().map(shape_detail_json).collect::<Vec<_>>(),
        "texts": d.texts.iter().map(text_detail_json).collect::<Vec<_>>(),
        "other_graphics_by_layer": d.graphics,
    })
}

/// Is this track or via of the DSN one the board has locked? (Locked tracks are written like the
/// others, as `protect`.)
fn locked_in_dsn(item: &kicad_sexpr::dsn::WiringItem, board: &Board, per_nm: f64) -> bool {
    let unit = |p: Point| {
        (
            (p.x.0 as f64 * per_nm).round() as i64,
            (-(p.y.0 as f64) * per_nm).round() as i64,
        )
    };
    let near = |a: (i64, i64), b: (i64, i64)| (a.0 - b.0).abs() <= 2 && (a.1 - b.1).abs() <= 2;
    if item.is_via {
        return board
            .vias
            .iter()
            .any(|v| v.locked && v.net == item.net && item.points.first().is_some_and(|p| near(unit(v.at), *p)));
    }
    item.points.windows(2).all(|w| {
        board.tracks.iter().any(|t| {
            t.locked
                && t.net == item.net
                && t.layer == item.layer
                && ((near(unit(t.start), w[0]) && near(unit(t.end), w[1]))
                    || (near(unit(t.start), w[1]) && near(unit(t.end), w[0])))
        })
    })
}

/// What a routed session does to the unlocked tracks and vias that are on the board now: the ones
/// it writes again exactly as they are, and the ones it replaces with others.
struct SessionChanges {
    tracks_identical: usize,
    tracks_replaced: usize,
    vias_replaced: usize,
}

fn session_changes(board: &Board, ses_text: &str) -> Result<SessionChanges, ToolError> {
    let (segments, vias, scale) = kicad_sexpr::ses::items(ses_text)
        .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot read the session: {e}")))?;
    let per_nm = scale / 1e6;
    let unit = |p: Point| {
        (
            (p.x.0 as f64 * per_nm).round() as i64,
            (-(p.y.0 as f64) * per_nm).round() as i64,
        )
    };
    let near = |a: (i64, i64), b: (i64, i64)| (a.0 - b.0).abs() <= 2 && (a.1 - b.1).abs() <= 2;
    let mut out = SessionChanges {
        tracks_identical: 0,
        tracks_replaced: 0,
        vias_replaced: 0,
    };
    for t in board.tracks.iter().filter(|t| !t.locked) {
        let (a, b) = (unit(t.start), unit(t.end));
        let same = segments.iter().any(|s| {
            s.net == t.net
                && s.layer == t.layer
                && (s.width - (t.width.0 as f64 * per_nm).round() as i64).abs() <= 2
                && ((near(s.a, a) && near(s.b, b)) || (near(s.a, b) && near(s.b, a)))
        });
        if same {
            out.tracks_identical += 1;
        } else {
            out.tracks_replaced += 1;
        }
    }
    for v in board.vias.iter().filter(|v| !v.locked) {
        let at = unit(v.at);
        if !vias.iter().any(|s| s.net == v.net && near(s.at, at)) {
            out.vias_replaced += 1;
        }
    }
    Ok(out)
}

fn track_selector(s: &TrackSelectArg) -> Result<track_edit::Selector, ToolError> {
    let area = match &s.area {
        Some(r) => Some(
            Rect::from_points([point_arg(r.min.x_mm, r.min.y_mm)?, point_arg(r.max.x_mm, r.max.y_mm)?])
                .expect("two corners"),
        ),
        None => None,
    };
    Ok(track_edit::Selector {
        nets: s.nets.clone(),
        layers: s.layers.clone(),
        area,
        ids: s.ids.clone(),
        tracks: s.tracks.unwrap_or(true),
        vias: s.vias.unwrap_or(true),
        all: s.all,
    })
}

/// What a copper edit touches, counted per net.
fn per_net(plan: &BoardEditPlan) -> serde_json::Value {
    #[derive(Default)]
    struct Count {
        tracks_removed: usize,
        vias_removed: usize,
        length_removed_mm: f64,
        widths_changed: usize,
        locked: usize,
        unlocked: usize,
    }
    let mut nets: BTreeMap<&str, Count> = BTreeMap::new();
    for op in &plan.operations {
        match op {
            BoardOp::RemoveTrack { net, start, end, .. } => {
                let c = nets.entry(net).or_default();
                c.tracks_removed += 1;
                c.length_removed_mm += ((end.x.0 - start.x.0) as f64).hypot((end.y.0 - start.y.0) as f64) / 1e6;
            }
            BoardOp::RemoveVia { net, .. } => nets.entry(net).or_default().vias_removed += 1,
            BoardOp::SetTrackWidth { net, .. } => nets.entry(net).or_default().widths_changed += 1,
            BoardOp::SetLocked { net, locked, .. } => {
                let c = nets.entry(net).or_default();
                if *locked {
                    c.locked += 1;
                } else {
                    c.unlocked += 1;
                }
            }
            _ => {}
        }
    }
    nets.into_iter()
        .map(|(net, c)| {
            (
                if net.is_empty() {
                    "(no net)".to_owned()
                } else {
                    net.to_owned()
                },
                serde_json::json!({
                    "tracks_removed": c.tracks_removed, "vias_removed": c.vias_removed,
                    "length_removed_mm": round2(c.length_removed_mm), "widths_changed": c.widths_changed,
                    "locked": c.locked, "unlocked": c.unlocked,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

fn footprint_json(f: &kicad_domain::board::BoardFootprint, pads: bool) -> serde_json::Value {
    let mut v = serde_json::json!({
        "reference": f.reference, "value": f.value, "library_id": f.lib_id, "side": if f.is_back() { "back" } else { "front" },
        "at": pt(f.position), "rotation_deg": round2(f.rotation_deg), "locked": f.locked, "dnp": f.dnp,
        "pad_count": f.pads.len(),
        "courtyard": f.courtyard.map(rect),
    });
    if pads {
        v["pads"] = f
            .pads
            .iter()
            .map(|p| {
                serde_json::json!({
                    "number": p.number, "kind": p.kind, "at": pt(p.at), "net": p.net,
                    "copper": p.copper().map(rect), "layers": p.layers,
                })
            })
            .collect();
    }
    v
}

/// What of a DRC report to list; the counts are always whole.
struct DrcView {
    summary_only: bool,
    severity: Option<String>,
    types: Vec<String>,
    limit: usize,
}

impl Default for DrcView {
    fn default() -> Self {
        Self {
            summary_only: false,
            severity: None,
            types: vec![],
            limit: 100,
        }
    }
}

fn drc_violation_json(v: &kicad_cli_runner::pcb::DrcViolation) -> serde_json::Value {
    serde_json::json!({
        "type": v.kind, "severity": v.severity, "description": v.description,
        "items": v.items.iter().map(|i| serde_json::json!({
            "what": i.description, "at": i.pos.as_ref().map(|p| serde_json::json!({ "x": p.x, "y": p.y })) })).collect::<Vec<_>>(),
    })
}

fn drc_json(report: &kicad_cli_runner::pcb::DrcReport) -> serde_json::Value {
    drc_json_view(report, &DrcView::default())
}

fn drc_json_view(report: &kicad_cli_runner::pcb::DrcReport, view: &DrcView) -> serde_json::Value {
    let mut by_type: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_severity: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    for v in &report.violations {
        *by_type.entry(v.kind.as_str()).or_default() += 1;
        *by_severity
            .entry(v.severity.as_str())
            .or_default()
            .entry(v.kind.as_str())
            .or_default() += 1;
    }
    let wanted = |v: &&kicad_cli_runner::pcb::DrcViolation| {
        view.severity.as_ref().is_none_or(|s| &v.severity == s)
            && (view.types.is_empty() || view.types.iter().any(|t| t == &v.kind))
    };
    let matching: Vec<_> = report.violations.iter().filter(wanted).collect();
    let mut out = serde_json::json!({
        "kicad_version": report.kicad_version,
        "errors": report.count("error"),
        "warnings": report.count("warning"),
        "by_type": by_type,
        "by_severity": by_severity,
        "unrouted": report.unconnected_items.len(),
    });
    if !view.summary_only {
        out["violations"] = matching
            .iter()
            .take(view.limit)
            .map(|v| drc_violation_json(v))
            .collect();
        out["violations_matching"] = matching.len().into();
        if matching.len() > view.limit {
            out["violations_note"] = format!(
                "{} of {} matching violations are listed; narrow with severity or types, or raise limit",
                view.limit,
                matching.len()
            )
            .into();
        }
        out["schematic_parity"] = report
            .schematic_parity
            .iter()
            .take(50)
            .map(drc_violation_json)
            .collect();
    }
    out["unrouted_items"] = report
        .unconnected_items
        .iter()
        .take(if view.summary_only { 10 } else { 50 })
        .map(drc_violation_json)
        .collect();
    out
}

/// A violation's identity for before/after comparison: its type, severity, and what and where
/// its items are.
fn drc_key(v: &kicad_cli_runner::pcb::DrcViolation) -> String {
    let mut items: Vec<String> = v
        .items
        .iter()
        .map(|i| match &i.pos {
            Some(p) => format!("{}@{:.2},{:.2}", i.description, p.x, p.y),
            None => i.description.clone(),
        })
        .collect();
    items.sort();
    format!("{}|{}|{}", v.kind, v.severity, items.join(";"))
}

/// The violations in `after` that `before` did not have (a violation that was there before and
/// still is does not count).
fn new_violations<'a>(
    before: &kicad_cli_runner::pcb::DrcReport,
    after: &'a kicad_cli_runner::pcb::DrcReport,
) -> Vec<&'a kicad_cli_runner::pcb::DrcViolation> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for v in before.violations.iter().chain(&before.unconnected_items) {
        *seen.entry(drc_key(v)).or_default() += 1;
    }
    after
        .violations
        .iter()
        .chain(&after.unconnected_items)
        .filter(|v| match seen.get_mut(&drc_key(v)) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// The result of a DRC before and after a change: counts, and only the violations that are new.
fn drc_comparison(
    before: &kicad_cli_runner::pcb::DrcReport,
    after: &kicad_cli_runner::pcb::DrcReport,
) -> serde_json::Value {
    fn counts(r: &kicad_cli_runner::pcb::DrcReport) -> BTreeMap<&str, usize> {
        let mut m: BTreeMap<&str, usize> = BTreeMap::new();
        for v in &r.violations {
            *m.entry(v.kind.as_str()).or_default() += 1;
        }
        m
    }
    let new = new_violations(before, after);
    serde_json::json!({
        "unrouted_before": before.unconnected_items.len(),
        "unrouted_after": after.unconnected_items.len(),
        "errors_before": before.count("error"),
        "errors_after": after.count("error"),
        "by_type_before": counts(before),
        "by_type_after": counts(after),
        "new_violations_total": new.len(),
        "new_violations": new.iter().take(30).map(|v| drc_violation_json(v)).collect::<Vec<_>>(),
        "new_violations_note": if new.len() > 30 {
            Some(format!("{} more new violations are not listed; see pcb_run_drc with types", new.len() - 30))
        } else {
            None
        },
        "unchanged_violations": after.violations.len() + after.unconnected_items.len() - new.len(),
    })
}

fn zone_request(i: usize, z: &ZoneArg, copper: &[String]) -> Result<zones::ZoneRequest, ToolError> {
    let label = format!("zone {}", i + 1);
    let area = match (&z.rect, &z.polygon) {
        (Some(_), Some(_)) => return Err(bad_arg(format!("{label}: give rect or polygon, not both"))),
        (Some(r), None) => zones::ZoneArea::Rect(
            Rect::from_points([point_arg(r.min.x_mm, r.min.y_mm)?, point_arg(r.max.x_mm, r.max.y_mm)?])
                .expect("two corners"),
        ),
        (None, Some(p)) => {
            zones::ZoneArea::Polygon(p.iter().map(|q| point_arg(q.x_mm, q.y_mm)).collect::<Result<_, _>>()?)
        }
        (None, None) => zones::ZoneArea::Board,
    };
    let opt = |v: Option<f64>, what: &str| v.map(|x| nm_arg(what, x)).transpose();
    Ok(zones::ZoneRequest {
        net: z.net.clone(),
        layers: z.layers.clone().unwrap_or_else(|| copper.to_vec()),
        area,
        clearance: opt(z.clearance_mm, "clearance_mm")?,
        min_width: opt(z.min_width_mm, "min_width_mm")?,
        connection: match z.pad_connection.as_deref() {
            None => zones::PadConnection::Thermal,
            Some(s) => zones::PadConnection::parse(s)
                .ok_or_else(|| bad_arg(format!("{label}: pad_connection is thermal, solid, or none, not {s}")))?,
        },
        thermal_gap: opt(z.thermal_gap_mm, "thermal_gap_mm")?,
        spoke_width: opt(z.spoke_width_mm, "spoke_width_mm")?,
        priority: z.priority,
        name: z.name.clone(),
        replace: z.replace,
    })
}

fn route_request(i: usize, r: &RouteArg) -> Result<routing::RouteRequest, ToolError> {
    let points = r
        .points
        .iter()
        .map(|p| {
            let at = match (&p.pad, p.x_mm, p.y_mm) {
                (Some(pad), None, None) => {
                    let (reference, number) = pad
                        .split_once('.')
                        .ok_or_else(|| bad_arg(format!("route {}: a pad is REF.PAD, like U1.2, not {pad}", i + 1)))?;
                    routing::RouteEnd::Pad {
                        reference: reference.to_owned(),
                        number: number.to_owned(),
                    }
                }
                (None, Some(x), Some(y)) => routing::RouteEnd::At(point_arg(x, y)?),
                _ => {
                    return Err(bad_arg(format!(
                        "route {}: each point is a pad or x_mm and y_mm",
                        i + 1
                    )))
                }
            };
            Ok(routing::RoutePoint {
                at,
                layer: p.layer.clone(),
            })
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    Ok(routing::RouteRequest {
        net: r.net.clone(),
        points,
        layer: r.layer.clone(),
        width: r.width_mm.map(|w| nm_arg("width_mm", w)).transpose()?,
        locked: r.locked,
    })
}

fn bad_arg(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

fn nm_arg(name: &str, mm: f64) -> Result<kicad_domain::Nm, ToolError> {
    if !mm.is_finite() || mm.abs() > 2000.0 {
        return Err(bad_arg(format!("{name} must be a number of mm up to 2000")));
    }
    Ok(kicad_domain::Nm((mm * 1e6).round() as i64))
}

fn point_arg(x_mm: f64, y_mm: f64) -> Result<Point, ToolError> {
    Ok(Point::new(nm_arg("x_mm", x_mm)?, nm_arg("y_mm", y_mm)?))
}

fn refuse_unless_live(board: &BoardSession) -> Result<(), ToolError> {
    if board.is_live() {
        Ok(())
    } else {
        Err(ToolError::new(
            ErrorCode::FeatureUnavailable,
            "editing a board needs a live KiCad 11 session; open it with provider headless or gui (a file session only reads)",
        ))
    }
}

fn move_request(m: &MoveArg) -> Result<MoveRequest, ToolError> {
    let pair = |x: Option<f64>, y: Option<f64>, what: &str| match (x, y) {
        (Some(x), Some(y)) => point_arg(x, y).map(Some),
        (None, None) => Ok(None),
        _ => Err(bad_arg(format!("{}: give both {what}", m.reference))),
    };
    let angle = |a: Option<f64>| match a {
        Some(d) if !d.is_finite() || d.abs() > 3600.0 => Err(bad_arg(format!("{}: a turn is in degrees", m.reference))),
        other => Ok(other),
    };
    Ok(MoveRequest {
        reference: m.reference.clone(),
        at: pair(m.x_mm, m.y_mm, "x_mm and y_mm")?,
        offset: pair(m.dx_mm, m.dy_mm, "dx_mm and dy_mm")?,
        rotation_deg: angle(m.rotation_deg)?,
        rotate_by_deg: angle(m.rotate_by_deg)?,
        back: match m.side.as_deref() {
            None => None,
            Some("front") => Some(false),
            Some("back") => Some(true),
            Some(other) => return Err(bad_arg(format!("{}: side is front or back, not {other}", m.reference))),
        },
        lock: m.lock,
        allow_locked: m.allow_locked,
    })
}

fn footprint_edit_request(arg: &FootprintEditArg) -> Result<footprint_edit::FootprintEditRequest, ToolError> {
    let who = |what: &str| format!("{}: {what}", arg.reference);
    let pair = |x: Option<f64>, y: Option<f64>, what: &str| match (x, y) {
        (Some(x), Some(y)) => point_arg(x, y).map(Some),
        (None, None) => Ok(None),
        _ => Err(bad_arg(who(&format!("give both {what}")))),
    };
    let size = |w: Option<f64>, h: Option<f64>, name: &str| match (w, h) {
        (Some(w), Some(h)) => {
            if !(0.05..=50.0).contains(&w) || !(0.05..=50.0).contains(&h) {
                return Err(bad_arg(who(&format!("{name} is 0.05 to 50 mm"))));
            }
            Ok(Some((nm_arg(name, w)?, nm_arg(name, h)?)))
        }
        (None, None) => Ok(None),
        _ => Err(bad_arg(who("give width_mm and height_mm together"))),
    };
    let mut pads = Vec::new();
    for p in &arg.pads {
        let drill = match (p.drill_mm, p.drill_height_mm) {
            (None, None) => None,
            (None, Some(_)) => return Err(bad_arg(who("drill_height_mm needs drill_mm"))),
            (Some(w), h) => size(Some(w), Some(h.unwrap_or(w)), "drill_mm")?,
        };
        pads.push(footprint_edit::PadEdit {
            number: p.number.clone(),
            drill,
            size: size(p.width_mm, p.height_mm, "width_mm and height_mm")?,
            zone_connection: p.zone_connection.clone(),
        });
    }
    let mut texts = Vec::new();
    for t in &arg.texts {
        let angle = match t.angle_deg {
            Some(d) if !d.is_finite() || d.abs() > 3600.0 => {
                return Err(bad_arg(who("angle_deg is a turn in degrees")))
            }
            other => other,
        };
        texts.push(footprint_edit::TextEdit {
            field: t.field.clone(),
            layer: t.layer.clone(),
            height: t.height_mm.map(|v| nm_arg("height_mm", v)).transpose()?,
            thickness: t.thickness_mm.map(|v| nm_arg("thickness_mm", v)).transpose()?,
            at: pair(t.x_mm, t.y_mm, "x_mm and y_mm")?,
            offset: match (t.dx_mm, t.dy_mm) {
                (None, None) => None,
                (dx, dy) => Some(point_arg(dx.unwrap_or(0.0), dy.unwrap_or(0.0))?),
            },
            angle_deg: angle,
            visible: t.visible,
        });
    }
    Ok(footprint_edit::FootprintEditRequest {
        reference: arg.reference.clone(),
        pads,
        texts,
        allow_locked: arg.allow_locked,
    })
}

fn placement_json(p: &board_edit::Placement) -> serde_json::Value {
    serde_json::json!({ "at": pt(p.at), "rotation_deg": round2(p.rotation_deg), "side": if p.back { "back" } else { "front" } })
}

fn op_json(op: &BoardOp) -> serde_json::Value {
    match op {
        BoardOp::RemoveEdge { id, kind } => {
            serde_json::json!({ "op": "remove_outline_drawing", "id": id, "kind": kind })
        }
        BoardOp::AddOutline {
            rect: r,
            corner_radius,
            line_width,
        } => serde_json::json!({
            "op": "add_outline", "outline": rect(*r), "corner_radius": mm(corner_radius.0), "line_width": mm(line_width.0),
            "layer": "Edge.Cuts",
        }),
        BoardOp::PlaceFootprint {
            reference, lib_id, at, ..
        } => {
            serde_json::json!({ "op": "place_footprint", "reference": reference, "footprint": lib_id, "at": pt(*at) })
        }
        BoardOp::AddTrack {
            net,
            start,
            end,
            width,
            layer,
            locked,
        } => serde_json::json!({
            "op": "add_track", "net": net, "from": pt(*start), "to": pt(*end), "width": mm(width.0), "layer": layer,
            "locked": locked,
        }),
        BoardOp::AddVia {
            net,
            at,
            diameter,
            drill,
            locked,
        } => serde_json::json!({
            "op": "add_via", "net": net, "at": pt(*at), "diameter": mm(diameter.0), "drill": mm(drill.0),
            "locked": locked,
        }),
        BoardOp::RemoveTrack {
            id,
            net,
            layer,
            start,
            end,
        } => serde_json::json!({
            "op": "remove_track", "id": id, "net": net, "layer": layer, "from": pt(*start), "to": pt(*end),
        }),
        BoardOp::SetTrackWidth {
            id,
            net,
            layer,
            from,
            to,
        } => serde_json::json!({
            "op": "set_track_width", "id": id, "net": net, "layer": layer, "from": mm(from.0), "to": mm(to.0),
        }),
        BoardOp::SetLocked { id, item, net, locked } => serde_json::json!({
            "op": if *locked { "lock" } else { "unlock" }, "item": item, "id": id, "net": net,
        }),
        BoardOp::AddZone(z) => serde_json::json!({
            "op": "add_zone", "name": z.name, "net": z.net, "layers": z.layers,
            "outline": z.outline.iter().map(|p| pt(*p)).collect::<Vec<_>>(),
            "clearance": mm(z.clearance.0), "min_width": mm(z.min_width.0), "pad_connection": z.connection,
            "thermal_gap": mm(z.thermal_gap.0), "spoke_width": mm(z.spoke_width.0), "priority": z.priority,
        }),
        BoardOp::RemoveZone { id, name } => serde_json::json!({ "op": "remove_zone", "id": id, "name": name }),
        BoardOp::RemoveVia { id, net, at } => {
            serde_json::json!({ "op": "remove_via", "id": id, "net": net, "at": pt(*at) })
        }
        BoardOp::SetAttributes {
            reference, from, to, ..
        } => serde_json::json!({ "op": "set_attributes", "reference": reference, "from": from, "to": to }),
        BoardOp::EditPad {
            reference,
            number,
            count,
            from,
            set,
            ..
        } => {
            let values = |v: &footprint_edit::PadValues| {
                serde_json::json!({
                    "drill": v.drill.map(|(w, h)| if w == h {
                        serde_json::json!(mm(w.0))
                    } else {
                        serde_json::json!({ "width": mm(w.0), "height": mm(h.0) })
                    }),
                    "copper": v.size.map(|(w, h)| serde_json::json!({ "width": mm(w.0), "height": mm(h.0) })),
                    "zone_connection": v.zone_connection,
                })
            };
            serde_json::json!({
                "op": "edit_pad", "reference": reference, "pad": number, "pads_with_number": count,
                "from": values(from), "set": values(set),
            })
        }
        BoardOp::EditText {
            reference,
            field,
            from,
            to,
            ..
        } => {
            let values = |v: &footprint_edit::TextValues| {
                serde_json::json!({
                    "layer": v.layer, "height": mm(v.height.0), "thickness": mm(v.thickness.0), "at": pt(v.at),
                    "angle_deg": round2(v.angle_deg), "visible": v.visible,
                })
            };
            serde_json::json!({
                "op": "edit_text", "reference": reference, "field": field.to_lowercase(),
                "from": values(from), "to": values(to),
            })
        }
        BoardOp::MoveFootprint {
            reference,
            from,
            to,
            locked,
            ..
        } => serde_json::json!({
            "op": "move_footprint", "reference": reference, "from": placement_json(from), "to": placement_json(to),
            "locked": locked,
        }),
    }
}

impl KicadAiServer {
    /// What to tell the assistant about routing what is left: the autorouter when it is set up, how
    /// to set it up when it is not.
    fn autoroute_hint(&self) -> String {
        if self.state.options.freerouting.is_some() {
            "pcb_route_plan places the tracks you specify (power, short links); pcb_autoroute_plan routes the rest              with Freerouting, which is configured"
                .to_owned()
        } else {
            "pcb_route_plan places the tracks you specify (power, short links); pcb_autoroute_plan routes the rest              but needs Freerouting, which is not configured: install.ps1 -FreeroutingOnly sets it up (see the              README), then restart the server"
                .to_owned()
        }
    }

    /// A DRC result with a hint added when connections are left unrouted.
    fn hinted(&self, mut drc: serde_json::Value) -> serde_json::Value {
        if drc["unrouted"].as_u64().unwrap_or(0) > 0 {
            drc["hint"] = serde_json::json!(self.autoroute_hint());
        }
        drc
    }

    /// Keeps an outline or move plan for pcb_apply_plan and shows it.
    async fn store_edit_plan(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        plan: BoardEditPlan,
        fp: String,
    ) -> CallToolResult {
        self.store_edit_plan_with(cid, board, plan, fp, None).await
    }

    /// As `store_edit_plan`, with a placement's measured result shown as `measured`.
    async fn store_edit_plan_with(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        plan: BoardEditPlan,
        fp: String,
        measured: Option<serde_json::Value>,
    ) -> CallToolResult {
        let extras = measured.map(|m| serde_json::json!({ "measured": m }));
        self.store_edit_plan_extras(cid, board, plan, fp, extras).await
    }

    /// As `store_edit_plan`, with more fields (an object) added to what the user is shown.
    async fn store_edit_plan_extras(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        plan: BoardEditPlan,
        fp: String,
        extras: Option<serde_json::Value>,
    ) -> CallToolResult {
        let plan_id = self.next_pcb_plan_id();
        let mut body = serde_json::json!({
            "plan_id": plan_id,
            "kind": plan.kind,
            "can_apply": true,
            "operations": plan.operations.iter().map(op_json).collect::<Vec<_>>(),
            "before": plan.before,
            "after": plan.after,
            "warnings": plan.warnings,
            "units": "mm; x grows right, y grows down; turns counter-clockwise on screen",
            "note": "Nothing has changed. Apply with pcb_apply_plan after the user confirms, then pcb_save.",
        });
        if let Some(serde_json::Value::Object(extra)) = extras {
            for (k, v) in extra {
                body[k] = v;
            }
        }
        let warnings = plan.warnings.clone();
        self.state.pending_updates.lock().await.insert(
            plan_id,
            PendingUpdate {
                session_id: board.id().to_owned(),
                fingerprint: fp,
                applied_with: None,
                work: PendingWork::Edit(Box::new(plan)),
            },
        );
        envelope::ok(cid, body, board_provenance(board), warnings)
    }

    /// Applies an outline or move plan, reads the board back, and compares it with the plan.
    async fn apply_board_edit(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        args: &PcbApplyArgs,
        fp: &str,
        plan: &BoardEditPlan,
    ) -> CallToolResult {
        let result = async {
            let before = board.read().await?;
            if fingerprint(&before) != fp {
                return Err(ToolError::new(
                    ErrorCode::StalePlan,
                    "the board changed since the plan was made; make a new plan",
                ));
            }
            let snapshot = board.snapshot().await?;
            // DRC before and after, except for a move the user asked for by name: its plan already
            // shows the overlaps it makes.
            let judged_by_drc = !matches!(plan.kind, "move" | "attributes");
            let drc_before = if judged_by_drc {
                self.run_drc(cid, board, false, true).await.ok()
            } else {
                None
            };
            let report = board.apply_edit(plan).await?;
            let drc_after = if judged_by_drc {
                self.run_drc(cid, board, false, true).await.ok()
            } else {
                None
            };
            let mut after = board.read().await?;
            let libs = self.footprint_libraries_in(board.document_path().parent()).await;
            fill_courtyards(&mut after, &libs);
            Ok::<_, ToolError>((before, snapshot, report, after, drc_before, drc_after))
        }
        .await;
        let (before, snapshot, report, after, drc_before, drc_after) = match result {
            Ok(r) => r,
            Err(e) => return envelope::error(cid, &e),
        };
        if let Some(p) = self.state.pending_updates.lock().await.get_mut(&args.plan_id) {
            p.applied_with = Some(args.idempotency_key.clone());
        }
        let mut problems = board_edit::verify_edit(plan, &after);
        if plan.kind == "footprint" {
            match board.read_detail().await {
                Ok(detail) => problems.extend(footprint_edit::verify(&plan.operations, &detail)),
                Err(e) => problems.push(format!(
                    "the footprints' pads and text could not be read back: {}",
                    e.message
                )),
            }
        }
        let drc = match (&drc_before, &drc_after) {
            (Some(b), Some(a)) => {
                problems.extend(drc_worse(b, a));
                let mut cmp = drc_comparison(b, a);
                cmp["after"] = self.hinted(drc_json_view(
                    a,
                    &DrcView {
                        summary_only: true,
                        ..DrcView::default()
                    },
                ));
                Some(cmp)
            }
            _ if !matches!(plan.kind, "move" | "attributes") => {
                problems.push("DRC could not be run before and after; check with pcb_run_drc".to_owned());
                None
            }
            _ => None,
        };
        let verified = problems.is_empty();
        let undo = match board.provider() {
            Provider::Gui => "one undo step in the PCB editor (Ctrl+Z)",
            _ => "not saved yet; pcb_save writes it, closing with discard_unsaved drops it",
        };
        let mut body = serde_json::json!({
            "applied": true,
            "verified": verified,
            "kind": plan.kind,
            "report": report,
            "measured_after": board_edit::measure(&after),
            "drc": drc,
            "problems": problems,
            "undo": undo,
            "next": "Check with pcb_review or pcb_render, then save with pcb_save.",
        });
        if verified {
            return envelope::ok(cid, body, board_provenance(board), plan.warnings.clone());
        }
        self.finish_unverified(cid, board, args, &snapshot, &before, &mut body)
            .await
    }

    /// A change that failed its checks: rolled back (unless the caller keeps it), and the board
    /// read again to confirm it is as it was.
    async fn finish_unverified(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        args: &PcbApplyArgs,
        snapshot: &kicad_session::BoardSnapshot,
        before: &Board,
        body: &mut serde_json::Value,
    ) -> CallToolResult {
        use kicad_domain::ResultState;
        if args.keep_if_unverified {
            return envelope::ok_with_state(
                cid,
                ResultState::Partial,
                body.clone(),
                board_provenance(board),
                vec!["applied and kept, but it failed its checks; see problems".to_owned()],
            );
        }
        let rollback = match board.restore(snapshot).await {
            Ok(report) => {
                let restored = board.read().await.is_ok_and(|now| same_board(&now, before));
                serde_json::json!({ "rolled_back": true, "restored": restored, "restore": report })
            }
            Err(e) => serde_json::json!({ "rolled_back": false, "restored": false, "error": e.message }),
        };
        let restored = rollback["restored"] == true;
        body["applied"] = serde_json::json!(!restored);
        body["rollback"] = rollback;
        body["undo"] = serde_json::json!(if restored {
            "nothing to undo: the change was rolled back"
        } else {
            "the rollback did not complete; read the board (pcb_list_footprints, pcb_list_tracks) and undo in KiCad"
        });
        envelope::ok_with_state(
            cid,
            if restored {
                ResultState::Rejected
            } else {
                ResultState::Unknown
            },
            body.clone(),
            board_provenance(board),
            vec![if restored {
                "the change failed its checks and was rolled back; the board is as it was (see problems)".to_owned()
            } else {
                "the change failed its checks and the rollback did not complete; see rollback".to_owned()
            }],
        )
    }
}

/// DRC error types that have more violations after a change than before.
fn drc_worse(before: &kicad_cli_runner::pcb::DrcReport, after: &kicad_cli_runner::pcb::DrcReport) -> Vec<String> {
    let count = |r: &kicad_cli_runner::pcb::DrcReport| {
        let mut m: BTreeMap<String, usize> = BTreeMap::new();
        for v in r.violations.iter().filter(|v| v.severity == "error") {
            *m.entry(v.kind.clone()).or_default() += 1;
        }
        m
    };
    let (b, a) = (count(before), count(after));
    a.iter()
        .filter(|(k, n)| **n > b.get(*k).copied().unwrap_or(0))
        .map(|(k, n)| {
            format!(
                "DRC: {} more '{k}' error(s) than before ({n} now)",
                n - b.get(k).copied().unwrap_or(0)
            )
        })
        .collect()
}

impl KicadAiServer {
    /// Imports a routed session, reads the board back, and checks it with DRC before and after.
    async fn apply_autoroute(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        args: &PcbApplyArgs,
        fp: &str,
        session: (PathBuf, PathBuf, Vec<String>),
    ) -> CallToolResult {
        let (ses, dir, nets) = session;
        let result = async {
            let before = board.read().await?;
            if fingerprint(&before) != fp {
                return Err(ToolError::new(
                    ErrorCode::StalePlan,
                    "the board changed since the plan was made; make a new plan",
                ));
            }
            let snapshot = board.snapshot().await?;
            let drc_before = self.run_drc(cid, board, false, true).await.ok();
            let imported = board.import_session(&ses).await?;
            // Vias the router left that join fewer than two layers carry nothing: remove them.
            let mut after = board.read().await?;
            let loose: Vec<String> = board_edit::dangling_vias(&after).iter().map(|v| v.id.clone()).collect();
            let mut cleanup = (0usize, Vec::new());
            if !loose.is_empty() {
                let removal = board_edit::plan_remove_vias(&after, &loose)?;
                match board.apply_edit(&removal).await {
                    Ok(_) => {
                        after = board.read().await?;
                        cleanup = (loose.len(), board_edit::verify_edit(&removal, &after));
                    }
                    Err(e) => cleanup.1.push(format!(
                        "removing {} dangling via(s) failed: {}",
                        loose.len(),
                        e.message
                    )),
                }
            }
            let drc_after = self.run_drc(cid, board, false, true).await.ok();
            Ok::<_, ToolError>((before, snapshot, imported, after, drc_before, drc_after, cleanup))
        }
        .await;
        let (before, snapshot, imported, after, drc_before, drc_after, (vias_removed, cleanup_problems)) = match result
        {
            Ok(r) => r,
            Err(e) => return envelope::error(cid, &e),
        };
        if let Some(p) = self.state.pending_updates.lock().await.get_mut(&args.plan_id) {
            p.applied_with = Some(args.idempotency_key.clone());
        }
        let _ = std::fs::remove_dir_all(&dir);
        let mut problems = cleanup_problems;
        if imported.skipped > 0 {
            problems.push(format!("KiCad skipped {} item(s) of the session", imported.skipped));
        }
        for f in &after.footprints {
            if let Some(b) = before.footprints.iter().find(|b| b.id == f.id) {
                if b.position != f.position || b.layer != f.layer {
                    problems.push(format!("{} moved during the import", f.reference));
                }
            }
        }
        for net in &nets {
            if !after.tracks.iter().any(|t| &t.net == net) {
                problems.push(format!("net {net} has no track after the import"));
            }
        }
        let drc = match (&drc_before, &drc_after) {
            (Some(b), Some(a)) => {
                problems.extend(drc_worse(b, a));
                let mut cmp = drc_comparison(b, a);
                cmp["after"] = self.hinted(drc_json_view(
                    a,
                    &DrcView {
                        summary_only: true,
                        ..DrcView::default()
                    },
                ));
                Some(cmp)
            }
            _ => {
                problems.push("DRC could not be run before and after; check with pcb_run_drc".to_owned());
                None
            }
        };
        let verified = problems.is_empty();
        let mut body = serde_json::json!({
            "applied": true,
            "verified": verified,
            "kind": "autoroute",
            "tracks": after.tracks.len(),
            "vias": after.vias.len(),
            "dangling_vias_removed": vias_removed,
            "skipped": imported.skipped,
            "report": imported.report,
            "drc": drc,
            "problems": problems,
            "undo": match board.provider() {
                Provider::Gui => "KiCad's own import step in the PCB editor (Ctrl+Z)",
                _ => "not saved yet; pcb_save writes it, closing with discard_unsaved drops it",
            },
            "next": "Check with pcb_review or pcb_render, then save with pcb_save.",
        });
        if verified {
            return envelope::ok(cid, body, board_provenance(board), vec![]);
        }
        self.finish_unverified(cid, board, args, &snapshot, &before, &mut body)
            .await
    }
}

/// The same board by content: every footprint's place, turn, side, lock, and pads, and the
/// tracks, vias, zones, and outline (items recreated by a rollback get new ids).
fn same_board(a: &Board, b: &Board) -> bool {
    let fps = |x: &Board| {
        let mut v: Vec<String> = x
            .footprints
            .iter()
            .map(|f| {
                format!(
                    "{} {:?} {:.3} {} {} {:?} {:?}",
                    f.reference,
                    f.position,
                    f.rotation_deg,
                    f.layer,
                    f.locked,
                    (f.dnp, f.exclude_from_bom, f.exclude_from_pos, f.board_only),
                    f.pads.iter().map(|p| (&p.number, p.at, &p.net)).collect::<Vec<_>>()
                )
            })
            .collect();
        v.sort();
        v
    };
    let tracks = |x: &Board| {
        let mut v: Vec<String> = x
            .tracks
            .iter()
            .map(|t| format!("{:?} {:?} {} {} {}", t.start, t.end, t.width.0, t.layer, t.net))
            .chain(
                x.vias
                    .iter()
                    .map(|v| format!("via {:?} {} {}", v.at, v.diameter.0, v.net)),
            )
            .chain(
                x.zones
                    .iter()
                    .map(|z| format!("zone {} {} {:?}", z.name, z.net, z.layers)),
            )
            .collect();
        v.sort();
        v
    };
    fps(a) == fps(b) && tracks(a) == tracks(b) && a.outline == b.outline && a.edges.len() == b.edges.len()
}

#[cfg(test)]
mod drc_tests {
    use super::*;
    use kicad_cli_runner::pcb::DrcReport;

    fn violation(kind: &str, severity: &str, at: f64) -> String {
        format!(
            r#"{{"type":"{kind}","severity":"{severity}","description":"d","items":[{{"description":"Track [GND]","pos":{{"x":{at},"y":1.0}}}}]}}"#
        )
    }

    fn report(violations: &[String], unrouted: usize) -> DrcReport {
        let unconnected: Vec<String> = (0..unrouted)
            .map(|i| format!(r#"{{"description":"missing","items":[{{"description":"Pad {i}"}}]}}"#))
            .collect();
        DrcReport::parse(&format!(
            r#"{{"kicad_version":"10","violations":[{}],"unconnected_items":[{}]}}"#,
            violations.join(","),
            unconnected.join(",")
        ))
        .unwrap()
    }

    #[test]
    fn a_summary_has_counts_and_no_list() {
        let r = report(
            &[
                violation("clearance", "error", 1.0),
                violation("silk_overlap", "warning", 2.0),
            ],
            3,
        );
        let v = drc_json_view(
            &r,
            &DrcView {
                summary_only: true,
                ..DrcView::default()
            },
        );
        assert_eq!(v["errors"], 1);
        assert_eq!(v["by_type"]["clearance"], 1);
        assert_eq!(v["by_severity"]["warning"]["silk_overlap"], 1);
        assert!(v.get("violations").is_none());
        assert_eq!(v["unrouted"], 3);
    }

    #[test]
    fn the_list_is_narrowed_and_says_what_was_left_out() {
        let many: Vec<String> = (0..5).map(|i| violation("clearance", "error", i as f64)).collect();
        let mut all = many.clone();
        all.push(violation("silk_overlap", "warning", 9.0));
        let r = report(&all, 0);
        let v = drc_json_view(
            &r,
            &DrcView {
                severity: Some("error".into()),
                limit: 2,
                ..DrcView::default()
            },
        );
        assert_eq!(v["violations"].as_array().unwrap().len(), 2);
        assert_eq!(v["violations_matching"], 5);
        assert!(v["violations_note"].as_str().unwrap().contains("2 of 5"));
        assert_eq!(v["by_type"]["silk_overlap"], 1, "the counts stay whole");
        let v = drc_json_view(
            &r,
            &DrcView {
                types: vec!["silk_overlap".into()],
                ..DrcView::default()
            },
        );
        assert_eq!(v["violations"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn only_new_violations_are_listed() {
        let before = report(
            &[
                violation("clearance", "error", 1.0),
                violation("clearance", "error", 2.0),
            ],
            2,
        );
        let after = report(
            &[
                violation("clearance", "error", 1.0),
                violation("clearance", "error", 3.0),
                violation("shorting_items", "error", 4.0),
            ],
            1,
        );
        let cmp = drc_comparison(&before, &after);
        // The one at 3.0 and the short are new; the one at 1.0 was there before; the unrouted
        // connection that is left was there before too.
        assert_eq!(cmp["new_violations_total"], 2, "{cmp}");
        assert_eq!(cmp["unchanged_violations"], 2, "{cmp}");
        assert_eq!(cmp["errors_before"], 2);
        assert_eq!(cmp["errors_after"], 3);
        assert_eq!(cmp["by_type_after"]["shorting_items"], 1);
    }
}
