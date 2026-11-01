// SPDX-License-Identifier: GPL-3.0-or-later
//! Board edits (Phase 12): an outline with mounting holes, and footprints moved, turned, flipped,
//! and locked. Planning is pure: a plan lists every operation and the board it should leave,
//! measured (overlaps, footprints outside the outline) before and after; the backends apply the
//! operations and [`verify_edit`] compares what KiCad reports afterwards with the plan.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::board::{place_point, place_rect, Board, BoardFootprint};
use crate::geometry::{Nm, Point, Rect};
use crate::result::{ErrorCode, ToolError};

/// The placement grid when none is given (decision D7).
pub const DEFAULT_GRID_MM: f64 = 0.5;
/// The outline's line width, as KiCad draws a new board edge.
pub const EDGE_WIDTH_NM: i64 = 50_000;
/// The margin around the parts for a fitted outline when none is given.
pub const DEFAULT_FIT_MARGIN_MM: f64 = 2.0;
/// How far a corner mounting hole's centre sits from both edges when no inset is given.
pub const DEFAULT_HOLE_INSET_MM: f64 = 4.0;
pub const DEFAULT_HOLE_FOOTPRINT: &str = "MountingHole:MountingHole_3.2mm_M3";
/// Read-back tolerance for places (KiCad rounds to its internal grid).
const PLACE_TOLERANCE_NM: i64 = 2_000;
const ANGLE_TOLERANCE_DEG: f64 = 0.01;

/// Where a footprint is: its anchor, its turn (degrees, counter-clockwise on screen, 0 to 360),
/// and its side.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Placement {
    pub at: Point,
    pub rotation_deg: f64,
    pub back: bool,
}

impl Placement {
    pub fn of(f: &BoardFootprint) -> Placement {
        Placement {
            at: f.position,
            rotation_deg: normalize_deg(f.rotation_deg),
            back: f.is_back(),
        }
    }
}

/// The attributes of a footprint that decide whether it is a part to buy and place.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FootprintFlags {
    /// Do not populate.
    pub dnp: bool,
    pub exclude_from_bom: bool,
    /// Left out of the position (pick-and-place) file.
    pub exclude_from_pos: bool,
    /// Has no part in the schematic.
    pub board_only: bool,
}

impl FootprintFlags {
    pub fn of(f: &BoardFootprint) -> FootprintFlags {
        FootprintFlags {
            dnp: f.dnp,
            exclude_from_bom: f.exclude_from_bom,
            exclude_from_pos: f.exclude_from_pos,
            board_only: f.board_only,
        }
    }
}

/// One change to the board, in the order it is applied.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BoardOp {
    /// Delete a drawing on `Edge.Cuts` (an outline being replaced).
    RemoveEdge { id: String, kind: String },
    /// A rectangle on `Edge.Cuts`, with rounded corners when `corner_radius` is above zero.
    AddOutline {
        rect: Rect,
        corner_radius: Nm,
        line_width: Nm,
    },
    /// A footprint from the library, named `reference` (mounting holes).
    PlaceFootprint {
        reference: String,
        lib_id: String,
        at: Point,
        /// Its reference text goes below it (a hole near the top edge), else above.
        reference_below: bool,
    },
    /// A track (straight segment) on a copper layer.
    AddTrack {
        net: String,
        start: Point,
        end: Point,
        width: Nm,
        layer: String,
        /// Created locked.
        locked: bool,
    },
    /// A through via.
    AddVia {
        net: String,
        at: Point,
        diameter: Nm,
        drill: Nm,
        /// Created locked.
        locked: bool,
    },
    /// A track deleted.
    RemoveTrack {
        id: String,
        net: String,
        layer: String,
        start: Point,
        end: Point,
    },
    /// A track given another width.
    SetTrackWidth {
        id: String,
        net: String,
        layer: String,
        from: Nm,
        to: Nm,
    },
    /// A track or via locked or unlocked.
    SetLocked {
        id: String,
        /// `track` or `via`.
        item: &'static str,
        net: String,
        locked: bool,
    },
    /// A via deleted (a dangling one left by a router).
    RemoveVia { id: String, net: String, at: Point },
    /// A copper pour.
    AddZone(Box<crate::zones::ZoneSpec>),
    /// A zone being replaced.
    RemoveZone { id: String, name: String },
    /// A footprint's attributes changed (nothing moves).
    SetAttributes {
        reference: String,
        id: String,
        from: FootprintFlags,
        to: FootprintFlags,
    },
    /// The pads numbered `number` on a footprint given another hole, copper size, or zone
    /// connection (Phase 15.2); `from` is how the first of them is now.
    EditPad {
        reference: String,
        id: String,
        number: String,
        /// How many pads carry the number.
        count: usize,
        from: crate::footprint_edit::PadValues,
        set: crate::footprint_edit::PadValues,
    },
    /// A footprint's reference or value text given another layer, size, place, turn, or visibility.
    EditText {
        reference: String,
        id: String,
        /// `Reference` or `Value`.
        field: &'static str,
        from: crate::footprint_edit::TextValues,
        to: crate::footprint_edit::TextValues,
    },
    /// A footprint moved, turned, or flipped, and its lock set.
    MoveFootprint {
        reference: String,
        id: String,
        from: Placement,
        to: Placement,
        /// The lock it ends with.
        locked: bool,
    },
}

/// Overlaps and footprints outside the outline, as the review counts them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LayoutMeasure {
    pub overlapping_pairs: Vec<String>,
    pub outside_outline: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardEditPlan {
    /// `outline`, `move`, `attributes`, `footprint`, ...
    pub kind: &'static str,
    pub operations: Vec<BoardOp>,
    pub before: LayoutMeasure,
    pub after: LayoutMeasure,
    pub warnings: Vec<String>,
    /// The board the plan should leave, for the read-back.
    #[serde(skip)]
    pub expected: Board,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

pub fn normalize_deg(deg: f64) -> f64 {
    let d = deg.rem_euclid(360.0);
    if (360.0 - d).abs() < 1e-9 {
        0.0
    } else {
        d
    }
}

pub(crate) fn grid_nm(grid_mm: f64) -> Result<Nm, ToolError> {
    if !(0.0..=10.0).contains(&grid_mm) {
        return Err(bad("grid_mm is 0 (no grid) to 10"));
    }
    Ok(Nm((grid_mm * 1e6).round() as i64))
}

pub(crate) fn snap(p: Point, grid: Nm) -> Point {
    if grid.0 == 0 {
        p
    } else {
        p.snap(grid)
    }
}

fn snap_down(v: Nm, grid: Nm) -> Nm {
    if grid.0 == 0 {
        v
    } else {
        Nm(v.0.div_euclid(grid.0) * grid.0)
    }
}

fn snap_up(v: Nm, grid: Nm) -> Nm {
    if grid.0 == 0 {
        v
    } else {
        Nm(-(-v.0).div_euclid(grid.0) * grid.0)
    }
}

/// Measures overlaps (courtyards, same side or through-hole) and parts outside the outline.
pub fn measure(board: &Board) -> LayoutMeasure {
    let extents: Vec<(&BoardFootprint, Rect)> = board
        .footprints
        .iter()
        .filter_map(|f| f.extent().map(|r| (f, r)))
        .collect();
    let through = |f: &BoardFootprint| f.pads.iter().any(|p| p.kind == "thru_hole" || p.kind == "np_thru_hole");
    let mut out = LayoutMeasure::default();
    for (i, (a, ra)) in extents.iter().enumerate() {
        for (b, rb) in &extents[i + 1..] {
            if (a.is_back() == b.is_back() || through(a) || through(b)) && ra.overlaps(*rb) {
                out.overlapping_pairs
                    .push(format!("{} and {}", a.reference, b.reference));
            }
        }
    }
    if let Some(o) = board.outline {
        out.outside_outline = extents
            .iter()
            .filter(|(_, r)| r.min.x < o.min.x || r.min.y < o.min.y || r.max.x > o.max.x || r.max.y > o.max.y)
            .map(|(f, _)| f.reference.clone())
            .collect();
    }
    out
}

/// The inverse of [`place_point`]: where `abs` is in the coordinates of a footprint at `origin`
/// turned by `deg`.
pub(crate) fn unplace_point(origin: Point, deg: f64, abs: Point) -> Point {
    let d = abs - origin;
    place_point(Point::default(), -deg, d)
}

/// The footprint as it would be at `to`: pads and courtyard moved with it. A change of side
/// mirrors the footprint's own coordinates top to bottom (y), as KiCad's flip does.
pub fn moved(f: &BoardFootprint, to: Placement) -> BoardFootprint {
    let from = Placement::of(f);
    let flip = from.back != to.back;
    let local = |abs: Point| {
        let l = unplace_point(from.at, from.rotation_deg, abs);
        if flip {
            Point::new(l.x, Nm(-l.y.0))
        } else {
            l
        }
    };
    let mut out = f.clone();
    out.position = to.at;
    out.rotation_deg = to.rotation_deg;
    out.layer = if to.back { "B.Cu" } else { "F.Cu" }.to_owned();
    for pad in &mut out.pads {
        pad.at = place_point(to.at, to.rotation_deg, local(pad.at));
        let own = pad.angle_deg - from.rotation_deg;
        pad.angle_deg = normalize_deg(to.rotation_deg + if flip { -own } else { own });
        if flip {
            for l in &mut pad.layers {
                *l = match l.as_str() {
                    "F.Cu" => "B.Cu".to_owned(),
                    "B.Cu" => "F.Cu".to_owned(),
                    other => other.to_owned(),
                };
            }
        }
    }
    let court_local = f.courtyard_local.map(|r| {
        if flip {
            Rect {
                min: Point::new(r.min.x, Nm(-r.max.y.0)),
                max: Point::new(r.max.x, Nm(-r.min.y.0)),
            }
        } else {
            r
        }
    });
    out.courtyard_local = court_local;
    out.courtyard = match (court_local, f.courtyard) {
        (Some(l), _) => Some(place_rect(to.at, to.rotation_deg, l)),
        // Only a placed box is known: move its corners with the part.
        (None, Some(c)) => Rect::from_points(
            [c.min, c.max, Point::new(c.min.x, c.max.y), Point::new(c.max.x, c.min.y)]
                .into_iter()
                .map(|p| place_point(to.at, to.rotation_deg, local(p))),
        ),
        (None, None) => None,
    };
    out
}

// ---------------------------------------------------------------- outline

#[derive(Debug, Clone, PartialEq)]
pub enum HolePlacement {
    None,
    /// One hole in each corner, `inset` from both edges.
    Corners {
        inset: Nm,
    },
    At(Vec<Point>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutlineRequest {
    /// Width and height; none fits the outline around the footprints.
    pub size: Option<(Nm, Nm)>,
    /// The top-left corner for a sized outline; none centres it on the footprints (or puts it at
    /// 100, 100 mm on an empty board).
    pub origin: Option<Point>,
    /// The margin around the footprints for a fitted outline.
    pub margin: Nm,
    pub corner_radius: Nm,
    /// Delete the outline the board has; without it a board with an outline is refused.
    pub replace: bool,
    pub holes: HolePlacement,
    pub hole_footprint: String,
    /// The hole footprint's courtyard in its own coordinates, from the library.
    pub hole_courtyard: Option<Rect>,
    pub grid_mm: f64,
}

/// Plans an outline (and mounting holes) for `board`.
pub fn plan_outline(board: &Board, req: &OutlineRequest) -> Result<BoardEditPlan, ToolError> {
    let grid = grid_nm(req.grid_mm)?;
    let mut warnings = Vec::new();
    let mut ops = Vec::new();
    if !board.edges.is_empty() {
        if !req.replace {
            return Err(bad(format!(
                "the board already has an outline ({} drawing(s) on Edge.Cuts); pass replace: true to replace it",
                board.edges.len()
            )));
        }
        if let Some(locked) = board.edges.iter().find(|e| e.locked) {
            return Err(bad(format!(
                "the outline has a locked {} ({}); unlock it in KiCad first",
                locked.kind, locked.id
            )));
        }
        for e in &board.edges {
            ops.push(BoardOp::RemoveEdge {
                id: e.id.clone(),
                kind: e.kind.clone(),
            });
        }
    }
    let parts = Rect::from_points(
        board
            .footprints
            .iter()
            .filter_map(|f| f.extent())
            .flat_map(|r| [r.min, r.max]),
    );
    let rect = match req.size {
        Some((w, h)) => {
            if w.0 < 1_000_000 || h.0 < 1_000_000 || w.0 > 2_000_000_000 || h.0 > 2_000_000_000 {
                return Err(bad("width and height are 1 to 2000 mm"));
            }
            let origin = match (req.origin, parts) {
                (Some(o), _) => snap(o, grid),
                (None, Some(p)) => snap(
                    Point::new(
                        Nm((p.min.x.0 + p.max.x.0) / 2 - w.0 / 2),
                        Nm((p.min.y.0 + p.max.y.0) / 2 - h.0 / 2),
                    ),
                    grid,
                ),
                (None, None) => Point::from_mm(100.0, 100.0).expect("valid"),
            };
            Rect {
                min: origin,
                max: Point::new(origin.x + w, origin.y + h),
            }
        }
        None => {
            if req.origin.is_some() {
                return Err(bad("origin needs a size; a fitted outline is placed around the parts"));
            }
            let Some(p) = parts else {
                return Err(bad("the board has no footprints to fit an outline around; give a size"));
            };
            if req.margin.0 < 0 || req.margin.0 > 100_000_000 {
                return Err(bad("margin_mm is 0 to 100"));
            }
            let r = p.inflate(req.margin);
            Rect {
                min: Point::new(snap_down(r.min.x, grid), snap_down(r.min.y, grid)),
                max: Point::new(snap_up(r.max.x, grid), snap_up(r.max.y, grid)),
            }
        }
    };
    let short = rect.width().0.min(rect.height().0);
    if req.corner_radius.0 < 0 || req.corner_radius.0 * 2 > short {
        return Err(bad(format!(
            "corner_radius_mm is 0 to half the shorter side ({:.2} mm)",
            short as f64 / 2e6
        )));
    }
    ops.push(BoardOp::AddOutline {
        rect,
        corner_radius: req.corner_radius,
        line_width: Nm(EDGE_WIDTH_NM),
    });

    // Mounting holes.
    let holes: Vec<Point> = match &req.holes {
        HolePlacement::None => vec![],
        HolePlacement::Corners { inset } => {
            if inset.0 * 2 >= short {
                return Err(bad("hole_inset_mm is too large for the board"));
            }
            if inset.0 < req.corner_radius.0 / 3 {
                warnings.push("the holes sit close to the rounded corners; check them on the render".into());
            }
            vec![
                Point::new(rect.min.x + *inset, rect.min.y + *inset),
                Point::new(rect.max.x - *inset, rect.min.y + *inset),
                Point::new(rect.max.x - *inset, rect.max.y - *inset),
                Point::new(rect.min.x + *inset, rect.max.y - *inset),
            ]
        }
        HolePlacement::At(points) => {
            for p in points {
                if p.x <= rect.min.x || p.y <= rect.min.y || p.x >= rect.max.x || p.y >= rect.max.y {
                    return Err(bad(format!(
                        "the hole at ({:.2}, {:.2}) mm is not inside the outline",
                        p.x.to_mm(),
                        p.y.to_mm()
                    )));
                }
            }
            points.iter().map(|p| snap(*p, grid)).collect()
        }
    };
    let mut expected = board.clone();
    expected.edges.clear();
    expected.outline = Some(rect);
    let taken: BTreeSet<&str> = board.footprints.iter().map(|f| f.reference.as_str()).collect();
    let mut n = 0;
    for at in holes {
        let reference = loop {
            n += 1;
            let r = format!("H{n}");
            if !taken.contains(r.as_str()) {
                break r;
            }
        };
        let hole = BoardFootprint {
            id: String::new(),
            reference: reference.clone(),
            value: String::new(),
            lib_id: req.hole_footprint.clone(),
            layer: "F.Cu".into(),
            position: at,
            rotation_deg: 0.0,
            locked: false,
            dnp: false,
            exclude_from_bom: false,
            exclude_from_pos: false,
            board_only: false,
            pads: vec![],
            courtyard: req.hole_courtyard.map(|c| c.translate(at)),
            courtyard_local: req.hole_courtyard,
        };
        if let Some(c) = hole.courtyard {
            let hits: Vec<&str> = board
                .footprints
                .iter()
                .filter(|f| f.extent().is_some_and(|e| e.overlaps(c)))
                .map(|f| f.reference.as_str())
                .collect();
            if !hits.is_empty() {
                warnings.push(format!(
                    "{reference} overlaps {}; move those parts or use a larger margin",
                    hits.join(", ")
                ));
            }
        }
        expected.footprints.push(hole);
        ops.push(BoardOp::PlaceFootprint {
            reference,
            lib_id: req.hole_footprint.clone(),
            at,
            reference_below: at.y.0 < (rect.min.y.0 + rect.max.y.0) / 2,
        });
    }
    let after = measure(&expected);
    if !after.outside_outline.is_empty() {
        warnings.push(format!(
            "{} footprint(s) would be outside the new outline: {}",
            after.outside_outline.len(),
            after.outside_outline.join(", ")
        ));
    }
    Ok(BoardEditPlan {
        kind: "outline",
        operations: ops,
        before: measure(board),
        after,
        warnings,
        expected,
    })
}

// ---------------------------------------------------------------- moves

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MoveRequest {
    pub reference: String,
    /// Absolute place of the anchor.
    pub at: Option<Point>,
    /// Or an offset from where it is (applied exactly, not snapped to the grid).
    pub offset: Option<Point>,
    /// Absolute turn, degrees.
    pub rotation_deg: Option<f64>,
    /// Or a turn added to the current one.
    pub rotate_by_deg: Option<f64>,
    /// `Some(true)` for the back side.
    pub back: Option<bool>,
    /// Lock or unlock it.
    pub lock: Option<bool>,
    /// Move it although it is locked (it stays locked unless `lock` says otherwise).
    pub allow_locked: bool,
}

/// Plans moving, turning, flipping, and locking named footprints.
pub fn plan_moves(board: &Board, requests: &[MoveRequest], grid_mm: f64) -> Result<BoardEditPlan, ToolError> {
    let grid = grid_nm(grid_mm)?;
    if requests.is_empty() {
        return Err(bad("nothing to move; name at least one footprint"));
    }
    let mut seen = BTreeSet::new();
    let mut expected = board.clone();
    let mut ops = Vec::new();
    let mut warnings = Vec::new();
    for req in requests {
        if !seen.insert(req.reference.as_str()) {
            return Err(bad(format!(
                "{} is named twice; give one entry per footprint",
                req.reference
            )));
        }
        let Some(index) = board.footprints.iter().position(|f| f.reference == req.reference) else {
            return Err(ToolError::new(
                ErrorCode::SymbolNotFound,
                format!("no footprint {} on the board (use pcb_list_footprints)", req.reference),
            ));
        };
        let f = &board.footprints[index];
        if req.at.is_some() && req.offset.is_some() {
            return Err(bad(format!("{}: give at or offset, not both", req.reference)));
        }
        if req.rotation_deg.is_some() && req.rotate_by_deg.is_some() {
            return Err(bad(format!(
                "{}: give rotation_deg or rotate_by_deg, not both",
                req.reference
            )));
        }
        let from = Placement::of(f);
        let at = match (req.at, req.offset) {
            (Some(a), _) => snap(a, grid),
            // An offset keeps the part's relation to its neighbours, so it is not snapped.
            (None, Some(o)) => from.at + o,
            (None, None) => from.at,
        };
        let rotation_deg = match (req.rotation_deg, req.rotate_by_deg) {
            (Some(r), _) => normalize_deg(r),
            (None, Some(by)) => normalize_deg(from.rotation_deg + by),
            (None, None) => from.rotation_deg,
        };
        let to = Placement {
            at,
            rotation_deg,
            back: req.back.unwrap_or(from.back),
        };
        let moves = to.at != from.at
            || (to.rotation_deg - from.rotation_deg).abs() > ANGLE_TOLERANCE_DEG
            || to.back != from.back;
        let locked = req.lock.unwrap_or(f.locked);
        if !moves && locked == f.locked {
            warnings.push(format!("{} is already there; nothing to change", f.reference));
            continue;
        }
        if moves && f.locked && !req.allow_locked && req.lock != Some(false) {
            return Err(bad(format!(
                "{} is locked; pass allow_locked: true (or lock: false) to move it",
                f.reference
            )));
        }
        if to.back != from.back && f.pads.iter().any(|p| p.kind == "smd") && f.reference.starts_with('J') {
            warnings.push(format!(
                "{} is a connector moved to the other side; check it still mates",
                f.reference
            ));
        }
        let mut placed = moved(f, to);
        placed.locked = locked;
        expected.footprints[index] = placed;
        ops.push(BoardOp::MoveFootprint {
            reference: f.reference.clone(),
            id: f.id.clone(),
            from,
            to,
            locked,
        });
    }
    if ops.is_empty() {
        return Err(bad("every footprint named is already where it was asked to be"));
    }
    let after = measure(&expected);
    let before = measure(board);
    let new_overlaps: Vec<&String> = after
        .overlapping_pairs
        .iter()
        .filter(|p| !before.overlapping_pairs.contains(p))
        .collect();
    if !new_overlaps.is_empty() {
        warnings.push(format!(
            "the moves make {} new overlap(s): {}",
            new_overlaps.len(),
            new_overlaps.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ")
        ));
    }
    Ok(BoardEditPlan {
        kind: "move",
        operations: ops,
        before,
        after,
        warnings,
        expected,
    })
}

// ---------------------------------------------------------------- attributes

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttributeRequest {
    pub reference: String,
    pub dnp: Option<bool>,
    pub exclude_from_bom: Option<bool>,
    pub exclude_from_pos: Option<bool>,
    pub board_only: Option<bool>,
}

/// The attributes a mounting hole should carry: no BOM line, no placement row, no schematic part.
pub fn hole_flags(current: FootprintFlags) -> FootprintFlags {
    FootprintFlags {
        exclude_from_bom: true,
        exclude_from_pos: true,
        board_only: true,
        ..current
    }
}

/// Mounting holes that are still listed as parts to buy or place (boards made by v0.7.0 had no
/// attributes on their holes).
pub fn unmarked_holes(board: &Board) -> Vec<&BoardFootprint> {
    board
        .footprints
        .iter()
        .filter(|f| f.is_mounting_hole())
        .filter(|f| !(f.exclude_from_pos && f.exclude_from_bom && f.board_only))
        .collect()
}

/// Plans setting attributes on named footprints, and with `mounting_holes` on every mounting hole
/// that is still listed as a part. An entry that names an attribute only changes that attribute.
/// Vias that join fewer than two copper layers, so they carry nothing: no track of their net
/// touches them on two layers, no pad of their net is under them, and no zone of their net covers
/// them on the layer a track does not reach. Routers leave a few behind; a via stitching a pour on
/// both layers is not one of them.
pub fn dangling_vias(board: &Board) -> Vec<&crate::board::BoardVia> {
    board
        .vias
        .iter()
        .filter(|via| {
            let reach = via.diameter.0 / 2 + PLACE_TOLERANCE_NM;
            let touches = |p: Point| {
                let (dx, dy) = ((p.x.0 - via.at.x.0) as f64, (p.y.0 - via.at.y.0) as f64);
                dx.hypot(dy) <= reach as f64
            };
            let under_pad =
                board.footprints.iter().flat_map(|f| &f.pads).any(|pad| {
                    pad.net == via.net && pad.copper().map_or_else(|| touches(pad.at), |r| r.contains(via.at))
                });
            if under_pad {
                return false;
            }
            let mut layers: BTreeSet<&str> = board
                .tracks
                .iter()
                .filter(|t| t.net == via.net && (touches(t.start) || touches(t.end)))
                .map(|t| t.layer.as_str())
                .collect();
            for z in board.zones.iter().filter(|z| z.net == via.net) {
                if z.bounds.is_some_and(|b| b.contains(via.at)) {
                    layers.extend(z.layers.iter().map(String::as_str));
                }
            }
            layers.len() < 2
        })
        .collect()
}

/// Deletes the vias in `ids`, as one plan.
pub fn plan_remove_vias(board: &Board, ids: &[String]) -> Result<BoardEditPlan, ToolError> {
    let mut operations = Vec::new();
    for id in ids {
        let via = board
            .vias
            .iter()
            .find(|v| &v.id == id)
            .ok_or_else(|| bad(format!("via {id} is not on the board")))?;
        operations.push(BoardOp::RemoveVia {
            id: id.clone(),
            net: via.net.clone(),
            at: via.at,
        });
    }
    if operations.is_empty() {
        return Err(bad("no vias to remove"));
    }
    let mut expected = board.clone();
    expected.vias.retain(|v| !ids.contains(&v.id));
    let measure = measure(board);
    Ok(BoardEditPlan {
        kind: "cleanup",
        operations,
        before: measure.clone(),
        after: measure,
        warnings: vec![],
        expected,
    })
}

pub fn plan_attributes(
    board: &Board,
    requests: &[AttributeRequest],
    mounting_holes: bool,
) -> Result<BoardEditPlan, ToolError> {
    if requests.is_empty() && !mounting_holes {
        return Err(bad(
            "nothing to change; name footprints with attributes, or pass mounting_holes: true",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut expected = board.clone();
    let mut ops = Vec::new();
    let mut warnings = Vec::new();
    let change = |index: usize, to: FootprintFlags, ops: &mut Vec<BoardOp>, expected: &mut Board| {
        let f = &board.footprints[index];
        let from = FootprintFlags::of(f);
        if from == to {
            return false;
        }
        let e = &mut expected.footprints[index];
        (e.dnp, e.exclude_from_bom, e.exclude_from_pos, e.board_only) =
            (to.dnp, to.exclude_from_bom, to.exclude_from_pos, to.board_only);
        ops.push(BoardOp::SetAttributes {
            reference: f.reference.clone(),
            id: f.id.clone(),
            from,
            to,
        });
        true
    };
    for req in requests {
        if !seen.insert(req.reference.as_str()) {
            return Err(bad(format!(
                "{} is named twice; give one entry per footprint",
                req.reference
            )));
        }
        let Some(index) = board.footprints.iter().position(|f| f.reference == req.reference) else {
            return Err(ToolError::new(
                ErrorCode::SymbolNotFound,
                format!("no footprint {} on the board (use pcb_list_footprints)", req.reference),
            ));
        };
        if req.dnp.is_none()
            && req.exclude_from_bom.is_none()
            && req.exclude_from_pos.is_none()
            && req.board_only.is_none()
        {
            return Err(bad(format!(
                "{}: name at least one of dnp, exclude_from_bom, exclude_from_pos, board_only",
                req.reference
            )));
        }
        let now = FootprintFlags::of(&board.footprints[index]);
        let to = FootprintFlags {
            dnp: req.dnp.unwrap_or(now.dnp),
            exclude_from_bom: req.exclude_from_bom.unwrap_or(now.exclude_from_bom),
            exclude_from_pos: req.exclude_from_pos.unwrap_or(now.exclude_from_pos),
            board_only: req.board_only.unwrap_or(now.board_only),
        };
        if !change(index, to, &mut ops, &mut expected) {
            warnings.push(format!(
                "{} already has these attributes; nothing to change",
                req.reference
            ));
        }
    }
    if mounting_holes {
        let mut found = false;
        for (index, f) in board.footprints.iter().enumerate() {
            if !f.is_mounting_hole() || seen.contains(f.reference.as_str()) {
                continue;
            }
            found = true;
            change(index, hole_flags(FootprintFlags::of(f)), &mut ops, &mut expected);
        }
        if !found {
            warnings.push("the board has no mounting holes".into());
        } else if ops.is_empty() {
            warnings.push("every mounting hole is already left out of the position file and the BOM".into());
        }
    }
    if ops.is_empty() {
        return Err(bad("every footprint named already has these attributes"));
    }
    let measure = measure(board);
    Ok(BoardEditPlan {
        kind: "attributes",
        operations: ops,
        before: measure.clone(),
        after: measure,
        warnings,
        expected,
    })
}

// ---------------------------------------------------------------- verification

/// Compares the board read back after applying with what the plan should leave: the outline, every
/// footprint the plan placed or moved (place, turn, side, lock, every pad's place), and every pad's
/// net (nothing in a plan changes a net). Returns the differences.
pub fn verify_edit(plan: &BoardEditPlan, after: &Board) -> Vec<String> {
    let mut problems = Vec::new();
    let near =
        |a: Point, b: Point| (a.x.0 - b.x.0).abs() <= PLACE_TOLERANCE_NM && (a.y.0 - b.y.0).abs() <= PLACE_TOLERANCE_NM;
    let angle_eq = |a: f64, b: f64| {
        let d = normalize_deg(a - b);
        d < ANGLE_TOLERANCE_DEG || 360.0 - d < ANGLE_TOLERANCE_DEG
    };
    let by_ref: BTreeMap<&str, &BoardFootprint> = after.footprints.iter().map(|f| (f.reference.as_str(), f)).collect();
    for op in &plan.operations {
        match op {
            BoardOp::AddOutline { rect, line_width, .. } => match after.outline {
                // The API's box includes half the line width.
                Some(o) if (o.min.x.0 - rect.min.x.0).abs() <= line_width.0
                    && (o.min.y.0 - rect.min.y.0).abs() <= line_width.0
                    && (o.max.x.0 - rect.max.x.0).abs() <= line_width.0
                    && (o.max.y.0 - rect.max.y.0).abs() <= line_width.0 => {}
                Some(o) => problems.push(format!(
                    "the outline is {:.2} x {:.2} mm at ({:.2}, {:.2}), the plan made {:.2} x {:.2} mm at ({:.2}, {:.2})",
                    o.width().to_mm(),
                    o.height().to_mm(),
                    o.min.x.to_mm(),
                    o.min.y.to_mm(),
                    rect.width().to_mm(),
                    rect.height().to_mm(),
                    rect.min.x.to_mm(),
                    rect.min.y.to_mm()
                )),
                None => problems.push("the board has no outline after applying".into()),
            },
            BoardOp::RemoveEdge { id, kind } => {
                if after.edges.iter().any(|e| &e.id == id) {
                    problems.push(format!("the old outline {kind} {id} is still there"));
                }
            }
            BoardOp::PlaceFootprint {
                reference, lib_id, at, ..
            } => match by_ref.get(reference.as_str()) {
                Some(f) => {
                    if !near(f.position, *at) {
                        problems.push(format!("{reference} is not at its planned place"));
                    }
                    if &f.lib_id != lib_id {
                        problems.push(format!("{reference} is {}, the plan placed {lib_id}", f.lib_id));
                    }
                }
                None => problems.push(format!("{reference} is not on the board")),
            },
            BoardOp::AddTrack {
                net,
                start,
                end,
                width,
                layer,
                locked,
            } => {
                let found = after.tracks.iter().any(|t| {
                    &t.net == net
                        && t.locked == *locked
                        && &t.layer == layer
                        && (t.width.0 - width.0).abs() <= PLACE_TOLERANCE_NM
                        && ((near(t.start, *start) && near(t.end, *end)) || (near(t.start, *end) && near(t.end, *start)))
                });
                if !found {
                    problems.push(format!(
                        "no {net} track from ({:.3}, {:.3}) to ({:.3}, {:.3}) mm on {layer} after applying",
                        start.x.to_mm(),
                        start.y.to_mm(),
                        end.x.to_mm(),
                        end.y.to_mm()
                    ));
                }
            }
            BoardOp::AddVia {
                net,
                at,
                diameter,
                drill,
                locked,
            } => {
                let found = after.vias.iter().any(|v| {
                    &v.net == net
                        && v.locked == *locked
                        && near(v.at, *at)
                        && (v.diameter.0 - diameter.0).abs() <= PLACE_TOLERANCE_NM
                        && (v.drill.0 - drill.0).abs() <= PLACE_TOLERANCE_NM
                });
                if !found {
                    problems.push(format!(
                        "no {net} via of {:.2}/{:.2} mm at ({:.3}, {:.3}) mm after applying",
                        diameter.to_mm(),
                        drill.to_mm(),
                        at.x.to_mm(),
                        at.y.to_mm()
                    ));
                }
            }
            BoardOp::AddZone(z) => {
                match after
                    .zones
                    .iter()
                    .find(|a| a.name == z.name && a.net == z.net && a.layers == z.layers)
                {
                    Some(a) if a.filled => {}
                    Some(_) => problems.push(format!(
                        "the zone {} is there but has no copper after the refill (nothing of {} inside it?)",
                        z.name, z.net
                    )),
                    None => problems.push(format!("no zone {} on {} after applying", z.name, z.layers.join(", "))),
                }
            }
            BoardOp::RemoveZone { id, name } => {
                if after.zones.iter().any(|a| &a.id == id) {
                    problems.push(format!("the old zone {name} ({id}) is still there"));
                }
            }
            BoardOp::RemoveTrack { id, net, start, end, .. } => {
                if after.tracks.iter().any(|t| &t.id == id) {
                    problems.push(format!(
                        "the {net} track from ({:.3}, {:.3}) to ({:.3}, {:.3}) mm ({id}) is still there",
                        start.x.to_mm(),
                        start.y.to_mm(),
                        end.x.to_mm(),
                        end.y.to_mm()
                    ));
                }
            }
            BoardOp::SetTrackWidth { id, net, to, .. } => match after.tracks.iter().find(|t| &t.id == id) {
                Some(t) if (t.width.0 - to.0).abs() <= PLACE_TOLERANCE_NM => {}
                Some(t) => problems.push(format!(
                    "a {net} track ({id}) is {:.3} mm wide, the plan made it {:.3} mm",
                    t.width.to_mm(),
                    to.to_mm()
                )),
                None => problems.push(format!("the {net} track {id} is gone after the width change")),
            },
            BoardOp::SetLocked { id, item, net, locked } => {
                let now = match *item {
                    "via" => after.vias.iter().find(|v| &v.id == id).map(|v| v.locked),
                    _ => after.tracks.iter().find(|t| &t.id == id).map(|t| t.locked),
                };
                match now {
                    Some(l) if l == *locked => {}
                    Some(_) => problems.push(format!(
                        "a {net} {item} ({id}) is {} after applying, the plan {} it",
                        if *locked { "unlocked" } else { "locked" },
                        if *locked { "locked" } else { "unlocked" }
                    )),
                    None => problems.push(format!("the {net} {item} {id} is gone after the lock change")),
                }
            }
            BoardOp::RemoveVia { id, net, at } => {
                if after.vias.iter().any(|v| &v.id == id) {
                    problems.push(format!(
                        "the {net} via at ({:.3}, {:.3}) mm ({id}) is still there",
                        at.x.to_mm(),
                        at.y.to_mm()
                    ));
                }
            }
            // Pads and text are compared on the board's full geometry (`footprint_edit::verify`).
            BoardOp::EditPad { .. } | BoardOp::EditText { .. } => {}
            BoardOp::SetAttributes { reference, to, .. } => match by_ref.get(reference.as_str()) {
                Some(f) if FootprintFlags::of(f) == *to => {}
                Some(f) => problems.push(format!(
                    "{reference} has {:?} after applying, the plan set {:?}",
                    FootprintFlags::of(f),
                    to
                )),
                None => problems.push(format!("{reference} is no longer on the board")),
            },
            BoardOp::MoveFootprint {
                reference, to, locked, ..
            } => {
                let Some(f) = by_ref.get(reference.as_str()) else {
                    problems.push(format!("{reference} is no longer on the board"));
                    continue;
                };
                if !near(f.position, to.at) {
                    problems.push(format!(
                        "{reference} is at ({:.3}, {:.3}) mm, the plan put it at ({:.3}, {:.3})",
                        f.position.x.to_mm(),
                        f.position.y.to_mm(),
                        to.at.x.to_mm(),
                        to.at.y.to_mm()
                    ));
                }
                if !angle_eq(f.rotation_deg, to.rotation_deg) {
                    problems.push(format!(
                        "{reference} is turned {:.1}°, the plan turned it {:.1}°",
                        f.rotation_deg, to.rotation_deg
                    ));
                }
                if f.is_back() != to.back {
                    problems.push(format!("{reference} is on the wrong side"));
                }
                if f.locked != *locked {
                    problems.push(format!(
                        "{reference} is {}, the plan left it {}",
                        if f.locked { "locked" } else { "unlocked" },
                        if *locked { "locked" } else { "unlocked" }
                    ));
                }
                if let Some(want) = plan.expected.footprints.iter().find(|e| &e.reference == reference) {
                    for pad in &want.pads {
                        match f.pads.iter().find(|p| p.number == pad.number && near(p.at, pad.at)) {
                            Some(_) => {}
                            None => problems.push(format!(
                                "{reference} pad {} is not where the move puts it ({:.3}, {:.3}) mm",
                                pad.number,
                                pad.at.x.to_mm(),
                                pad.at.y.to_mm()
                            )),
                        }
                    }
                }
            }
        }
    }
    // Nets: every pad that was on a net is still on it.
    for want in &plan.expected.footprints {
        let Some(f) = by_ref.get(want.reference.as_str()) else {
            continue;
        };
        for pad in want.pads.iter().filter(|p| !p.net.is_empty()) {
            if !f.pads.iter().any(|p| p.number == pad.number && p.net == pad.net) {
                problems.push(format!(
                    "{}.{} is no longer on net {}",
                    want.reference, pad.number, pad.net
                ));
            }
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{BoardEdge, BoardPad};

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    /// A two-pad part at `at`, pads 1 mm left and right, courtyard 2 x 1 mm.
    fn part(reference: &str, at: Point) -> BoardFootprint {
        BoardFootprint {
            id: format!("id-{reference}"),
            reference: reference.into(),
            value: "10k".into(),
            lib_id: "Resistor_SMD:R_0603".into(),
            layer: "F.Cu".into(),
            position: at,
            rotation_deg: 0.0,
            locked: false,
            dnp: false,
            exclude_from_bom: false,
            exclude_from_pos: false,
            board_only: false,
            pads: vec![
                BoardPad {
                    number: "1".into(),
                    kind: "smd".into(),
                    at: at + p(-1.0, 0.0),
                    net: "A".into(),
                    ..Default::default()
                },
                BoardPad {
                    number: "2".into(),
                    kind: "smd".into(),
                    at: at + p(1.0, 0.0),
                    net: "B".into(),
                    ..Default::default()
                },
            ],
            courtyard: Some(Rect::from_points([at + p(-2.0, -1.0), at + p(2.0, 1.0)]).unwrap()),
            courtyard_local: Some(Rect::from_points([p(-2.0, -1.0), p(2.0, 1.0)]).unwrap()),
        }
    }

    fn board() -> Board {
        Board {
            footprints: vec![part("R1", p(10.0, 10.0)), part("R2", p(20.0, 10.0))],
            nets: vec!["A".into(), "B".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_via_that_joins_fewer_than_two_layers_is_dangling() {
        use crate::board::{BoardTrack, BoardVia, BoardZone};
        let via = |id: &str, net: &str, at: Point| BoardVia {
            id: id.into(),
            at,
            diameter: Nm::from_mm(0.6).unwrap(),
            drill: Nm::from_mm(0.3).unwrap(),
            net: net.into(),
            locked: false,
        };
        let track = |net: &str, layer: &str, a: Point, b: Point| BoardTrack {
            id: format!("t-{layer}-{}", a.x.0),
            start: a,
            end: b,
            width: Nm::from_mm(0.25).unwrap(),
            layer: layer.into(),
            net: net.into(),
            locked: false,
        };
        let mut b = board();
        b.vias = vec![
            via("joined", "A", p(5.0, 5.0)),
            via("stub", "A", p(8.0, 5.0)),
            via("loose", "A", p(30.0, 30.0)),
            via("under-pad", "A", p(9.0, 10.0)),
            via("stitch", "B", p(40.0, 5.0)),
        ];
        b.tracks = vec![
            track("A", "F.Cu", p(5.0, 5.0), p(7.0, 5.0)),
            track("A", "B.Cu", p(5.0, 5.0), p(5.0, 8.0)),
            track("A", "F.Cu", p(8.0, 5.0), p(7.0, 5.0)),
        ];
        b.zones = vec![BoardZone {
            id: "z".into(),
            name: "gnd".into(),
            layers: vec!["F.Cu".into(), "B.Cu".into()],
            net: "B".into(),
            priority: 0,
            bounds: Some(Rect::from_points([p(35.0, 0.0), p(45.0, 10.0)]).unwrap()),
            locked: false,
            filled: true,
        }];
        let found: Vec<&str> = dangling_vias(&b).iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            found,
            ["stub", "loose"],
            "pad R1.1 is net A at (9, 10): under-pad is fine"
        );
        let plan = plan_remove_vias(&b, &["stub".into(), "loose".into()]).unwrap();
        assert_eq!(plan.kind, "cleanup");
        assert!(plan.expected.vias.iter().all(|v| v.id != "stub" && v.id != "loose"));
        let mut after = b.clone();
        after.vias.retain(|v| v.id != "stub");
        let problems = verify_edit(&plan, &after);
        assert!(problems.iter().any(|m| m.contains("loose")), "{problems:?}");
        assert!(!problems.iter().any(|m| m.contains("stub")), "{problems:?}");
    }

    fn outline_req() -> OutlineRequest {
        OutlineRequest {
            size: None,
            origin: None,
            margin: Nm::from_mm(2.0).unwrap(),
            corner_radius: Nm(0),
            replace: false,
            holes: HolePlacement::None,
            hole_footprint: DEFAULT_HOLE_FOOTPRINT.into(),
            hole_courtyard: Some(Rect::from_points([p(-3.0, -3.0), p(3.0, 3.0)]).unwrap()),
            grid_mm: 0.5,
        }
    }

    #[test]
    fn a_fitted_outline_surrounds_the_parts_on_the_grid() {
        let plan = plan_outline(&board(), &outline_req()).unwrap();
        let BoardOp::AddOutline { rect, .. } = plan.operations[0] else {
            panic!("an outline first")
        };
        assert_eq!(rect, Rect::from_points([p(6.0, 7.0), p(24.0, 13.0)]).unwrap());
        assert!(plan.after.outside_outline.is_empty());
        assert_eq!(plan.expected.outline, Some(rect));
    }

    #[test]
    fn a_sized_outline_is_centred_on_the_parts_and_holes_go_in_the_corners() {
        let mut req = outline_req();
        req.size = Some((Nm::from_mm(40.0).unwrap(), Nm::from_mm(30.0).unwrap()));
        req.corner_radius = Nm::from_mm(2.0).unwrap();
        req.holes = HolePlacement::Corners {
            inset: Nm::from_mm(4.0).unwrap(),
        };
        let plan = plan_outline(&board(), &req).unwrap();
        let BoardOp::AddOutline {
            rect, corner_radius, ..
        } = plan.operations[0]
        else {
            panic!()
        };
        assert_eq!(rect, Rect::from_points([p(-5.0, -5.0), p(35.0, 25.0)]).unwrap());
        assert_eq!(corner_radius, Nm::from_mm(2.0).unwrap());
        let holes: Vec<(&str, Point)> = plan
            .operations
            .iter()
            .filter_map(|o| match o {
                BoardOp::PlaceFootprint { reference, at, .. } => Some((reference.as_str(), *at)),
                _ => None,
            })
            .collect();
        assert_eq!(
            holes,
            vec![
                ("H1", p(-1.0, -1.0)),
                ("H2", p(31.0, -1.0)),
                ("H3", p(31.0, 21.0)),
                ("H4", p(-1.0, 21.0))
            ]
        );
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn an_existing_outline_is_replaced_only_when_asked_and_never_when_locked() {
        let mut b = board();
        b.edges.push(BoardEdge {
            id: "e1".into(),
            kind: "rectangle".into(),
            bounds: None,
            locked: false,
        });
        b.outline = Rect::from_points([p(0.0, 0.0), p(5.0, 5.0)]);
        assert!(plan_outline(&b, &outline_req()).is_err());
        let mut req = outline_req();
        req.replace = true;
        let plan = plan_outline(&b, &req).unwrap();
        assert!(matches!(&plan.operations[0], BoardOp::RemoveEdge { id, .. } if id == "e1"));
        // Both parts were outside the small outline before.
        assert_eq!(plan.before.outside_outline.len(), 2);
        b.edges[0].locked = true;
        assert!(plan_outline(&b, &req).is_err());
    }

    #[test]
    fn holes_that_hit_parts_and_bad_sizes_are_reported() {
        let mut req = outline_req();
        req.margin = Nm::from_mm(1.0).unwrap();
        req.holes = HolePlacement::Corners {
            inset: Nm::from_mm(1.0).unwrap(),
        };
        let plan = plan_outline(&board(), &req).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("H1 overlaps R1")),
            "{:?}",
            plan.warnings
        );
        let mut req = outline_req();
        req.size = Some((Nm::from_mm(10.0).unwrap(), Nm::from_mm(10.0).unwrap()));
        req.corner_radius = Nm::from_mm(6.0).unwrap();
        assert!(plan_outline(&board(), &req).is_err());
        let mut req = outline_req();
        req.holes = HolePlacement::At(vec![p(500.0, 0.0)]);
        assert!(plan_outline(&board(), &req).is_err());
        assert!(plan_outline(&Board::default(), &outline_req()).is_err());
    }

    #[test]
    fn a_move_turns_and_places_the_pads_with_the_part() {
        let req = MoveRequest {
            reference: "R1".into(),
            at: Some(p(30.2, 40.1)),
            rotation_deg: Some(90.0),
            ..Default::default()
        };
        let plan = plan_moves(&board(), &[req], 0.5).unwrap();
        let r1 = &plan.expected.footprints[0];
        // Snapped to the 0.5 mm grid.
        assert_eq!(r1.position, p(30.0, 40.0));
        // Turned 90° counter-clockwise on screen (y down): pad 1 at local (-1, 0) goes to (0, +1).
        assert_eq!(r1.pads[0].at, p(30.0, 41.0));
        assert_eq!(r1.pads[1].at, p(30.0, 39.0));
        assert_eq!(r1.courtyard, Rect::from_points([p(29.0, 38.0), p(31.0, 42.0)]));
    }

    #[test]
    fn a_flip_mirrors_the_part_top_to_bottom() {
        let mut b = board();
        b.footprints[0].pads[0].at = p(9.0, 9.5);
        let req = MoveRequest {
            reference: "R1".into(),
            back: Some(true),
            ..Default::default()
        };
        let plan = plan_moves(&b, &[req], 0.5).unwrap();
        let r1 = &plan.expected.footprints[0];
        assert!(r1.is_back());
        assert_eq!(r1.pads[0].at, p(9.0, 10.5));
    }

    #[test]
    fn offsets_turns_and_locks_combine() {
        let req = MoveRequest {
            reference: "R2".into(),
            offset: Some(p(0.0, 5.0)),
            rotate_by_deg: Some(-90.0),
            lock: Some(true),
            ..Default::default()
        };
        let plan = plan_moves(&board(), &[req], 0.5).unwrap();
        let BoardOp::MoveFootprint { to, locked, .. } = &plan.operations[0] else {
            panic!()
        };
        assert_eq!(to.at, p(20.0, 15.0));
        assert_eq!(to.rotation_deg, 270.0);
        assert!(*locked);
    }

    #[test]
    fn locked_unknown_twice_and_overlapping_moves_are_handled() {
        let mut b = board();
        b.footprints[0].locked = true;
        let to = |r: &str, x: f64| MoveRequest {
            reference: r.into(),
            at: Some(p(x, 10.0)),
            ..Default::default()
        };
        assert!(plan_moves(&b, &[to("R1", 0.0)], 0.5).is_err());
        let mut allowed = to("R1", 0.0);
        allowed.allow_locked = true;
        let plan = plan_moves(&b, &[allowed], 0.5).unwrap();
        assert!(matches!(
            plan.operations[0],
            BoardOp::MoveFootprint { locked: true, .. }
        ));
        assert!(plan_moves(&b, &[to("R9", 0.0)], 0.5).is_err());
        assert!(plan_moves(&b, &[to("R2", 0.0), to("R2", 1.0)], 0.5).is_err());
        let plan = plan_moves(&board(), &[to("R2", 11.0)], 0.5).unwrap();
        assert_eq!(plan.after.overlapping_pairs, vec!["R1 and R2".to_owned()]);
        assert!(plan.warnings.iter().any(|w| w.contains("new overlap")));
        // Lock only: no move, no lock check.
        let lock_only = MoveRequest {
            reference: "R1".into(),
            lock: Some(false),
            ..Default::default()
        };
        assert!(plan_moves(&b, &[lock_only], 0.5).is_ok());
    }

    #[test]
    fn the_read_back_is_compared_with_the_plan() {
        let req = MoveRequest {
            reference: "R1".into(),
            at: Some(p(30.0, 40.0)),
            rotation_deg: Some(90.0),
            ..Default::default()
        };
        let plan = plan_moves(&board(), &[req], 0.5).unwrap();
        assert!(verify_edit(&plan, &plan.expected).is_empty());
        // KiCad moved the anchor but left the pads behind.
        let mut wrong = plan.expected.clone();
        wrong.footprints[0].pads = board().footprints[0].pads.clone();
        let problems = verify_edit(&plan, &wrong);
        assert_eq!(problems.len(), 2, "{problems:?}");
        let mut wrong = plan.expected.clone();
        wrong.footprints[0].pads[1].net = "C".into();
        assert_eq!(
            verify_edit(&plan, &wrong),
            vec!["R1.2 is no longer on net B".to_owned()]
        );

        let plan = plan_outline(&board(), &outline_req()).unwrap();
        let mut after = plan.expected.clone();
        // Half the line width either side, as the API reports.
        after.outline = after.outline.map(|r| r.inflate(Nm(25_000)));
        assert!(verify_edit(&plan, &after).is_empty());
        after.outline = None;
        assert_eq!(verify_edit(&plan, &after).len(), 1);
    }

    fn hole(reference: &str, at: Point) -> BoardFootprint {
        let mut h = part(reference, at);
        h.lib_id = "MountingHole:MountingHole_3.2mm_M3".into();
        h.pads.clear();
        h
    }

    #[test]
    fn mounting_holes_without_attributes_are_found_and_marked() {
        let mut b = board();
        b.footprints.push(hole("H1", p(2.0, 2.0)));
        let mut marked = hole("H2", p(30.0, 2.0));
        (marked.exclude_from_bom, marked.exclude_from_pos, marked.board_only) = (true, true, true);
        b.footprints.push(marked);
        assert_eq!(
            unmarked_holes(&b)
                .iter()
                .map(|f| f.reference.as_str())
                .collect::<Vec<_>>(),
            ["H1"]
        );

        let plan = plan_attributes(&b, &[], true).unwrap();
        assert_eq!(plan.kind, "attributes");
        assert_eq!(plan.operations.len(), 1);
        let BoardOp::SetAttributes { reference, to, .. } = &plan.operations[0] else {
            panic!("{:?}", plan.operations)
        };
        assert_eq!(reference, "H1");
        assert!(to.exclude_from_bom && to.exclude_from_pos && to.board_only && !to.dnp);
        // What the plan expects verifies against itself, and a board that ignored it does not.
        assert!(verify_edit(&plan, &plan.expected).is_empty());
        assert_eq!(verify_edit(&plan, &b).len(), 1);
        assert!(unmarked_holes(&plan.expected).is_empty());
    }

    #[test]
    fn named_footprints_change_only_the_attributes_named() {
        let mut b = board();
        b.footprints[0].exclude_from_bom = true;
        let plan = plan_attributes(
            &b,
            &[AttributeRequest {
                reference: "R1".into(),
                dnp: Some(true),
                ..Default::default()
            }],
            false,
        )
        .unwrap();
        let BoardOp::SetAttributes { from, to, .. } = &plan.operations[0] else {
            panic!()
        };
        assert!(from.exclude_from_bom && !from.dnp);
        assert!(to.exclude_from_bom && to.dnp, "the BOM exclusion stays");
    }

    #[test]
    fn attribute_plans_refuse_nonsense() {
        let b = board();
        let req = |reference: &str, dnp| AttributeRequest {
            reference: reference.into(),
            dnp,
            ..Default::default()
        };
        assert!(plan_attributes(&b, &[], false).is_err());
        assert!(plan_attributes(&b, &[req("R9", Some(true))], false).is_err());
        assert!(plan_attributes(&b, &[req("R1", None)], false).is_err());
        assert!(plan_attributes(&b, &[req("R1", Some(true)), req("R1", Some(false))], false).is_err());
        assert!(
            plan_attributes(&b, &[req("R1", Some(false))], false).is_err(),
            "already so"
        );
    }
}
