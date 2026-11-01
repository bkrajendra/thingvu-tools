// SPDX-License-Identifier: GPL-3.0-or-later
//! Edits to a footprint as placed on the board (Phase 15.2): a pad's hole, copper size, and zone
//! connection, and the reference and value text's layer, size, place, turn, and visibility. Planning
//! is pure; the backend sends the footprint back with the changes, and [`verify`] compares the
//! detail read afterwards with what the plan set.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::board::Board;
use crate::board_detail::{BoardDetail, FootprintDetail, PadDetail, TextDetail};
use crate::board_edit::{measure, normalize_deg, BoardEditPlan, BoardOp};
use crate::geometry::{Nm, Point};
use crate::result::{ErrorCode, ToolError};

/// The ways a zone can connect to a pad.
pub const ZONE_CONNECTIONS: [&str; 5] = ["inherited", "none", "thermal", "full", "pth_thermal"];
/// Read-back tolerance for sizes and places (KiCad rounds to its internal grid).
const TOLERANCE_NM: i64 = 2_000;
const ANGLE_TOLERANCE_DEG: f64 = 0.01;
/// The ring of copper most fabs want around a plated hole.
const MIN_ANNULAR_RING_NM: i64 = 150_000;
/// The smallest silkscreen most fabs print legibly: 0.8 mm tall, 0.15 mm stroke.
const MIN_SILK_HEIGHT_NM: i64 = 800_000;
const MIN_SILK_STROKE_NM: i64 = 120_000;

/// The layers a footprint's text can be on.
const TEXT_LAYERS: [&str; 10] = [
    "F.SilkS", "B.SilkS", "F.Fab", "B.Fab", "F.CrtYd", "B.CrtYd", "F.Adhes", "B.Adhes", "F.Cu", "B.Cu",
];

/// A pad's hole, copper size, and zone connection; a field is none when it is not set or known.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PadValues {
    /// Hole width and height (equal for a round hole).
    pub drill: Option<(Nm, Nm)>,
    /// Copper width and height before the pad is turned.
    pub size: Option<(Nm, Nm)>,
    pub zone_connection: Option<String>,
}

/// A text's layer, size, place, turn, and visibility.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextValues {
    pub layer: String,
    /// Character height.
    pub height: Nm,
    pub thickness: Nm,
    pub at: Point,
    pub angle_deg: f64,
    pub visible: bool,
}

impl TextValues {
    fn of(t: &TextDetail) -> TextValues {
        TextValues {
            layer: t.layer.clone(),
            height: t.height,
            thickness: t.thickness,
            at: t.at,
            angle_deg: normalize_deg(t.angle_deg),
            visible: t.visible,
        }
    }

    fn same_as(&self, other: &TextValues) -> bool {
        self.layer == other.layer
            && (self.height.0 - other.height.0).abs() <= TOLERANCE_NM
            && (self.thickness.0 - other.thickness.0).abs() <= TOLERANCE_NM
            && near(self.at, other.at)
            && angle_eq(self.angle_deg, other.angle_deg)
            && self.visible == other.visible
    }
}

fn near(a: Point, b: Point) -> bool {
    (a.x.0 - b.x.0).abs() <= TOLERANCE_NM && (a.y.0 - b.y.0).abs() <= TOLERANCE_NM
}

fn angle_eq(a: f64, b: f64) -> bool {
    let d = normalize_deg(a - b);
    d < ANGLE_TOLERANCE_DEG || 360.0 - d < ANGLE_TOLERANCE_DEG
}

fn pair_eq(a: (Nm, Nm), b: (Nm, Nm)) -> bool {
    (a.0 .0 - b.0 .0).abs() <= TOLERANCE_NM && (a.1 .0 - b.1 .0).abs() <= TOLERANCE_NM
}

// ---------------------------------------------------------------- requests

/// What to change on the pads that carry `number`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PadEdit {
    pub number: String,
    pub drill: Option<(Nm, Nm)>,
    pub size: Option<(Nm, Nm)>,
    pub zone_connection: Option<String>,
}

/// What to change on a footprint's `reference` or `value` text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextEdit {
    /// `reference` or `value`.
    pub field: String,
    pub layer: Option<String>,
    pub height: Option<Nm>,
    pub thickness: Option<Nm>,
    /// Absolute place on the board.
    pub at: Option<Point>,
    /// Or an offset from where it is.
    pub offset: Option<Point>,
    pub angle_deg: Option<f64>,
    pub visible: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FootprintEditRequest {
    pub reference: String,
    pub pads: Vec<PadEdit>,
    pub texts: Vec<TextEdit>,
    /// Edit it although it is locked.
    pub allow_locked: bool,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

fn mm(n: Nm) -> String {
    format!("{:.3}", n.to_mm())
}

fn pair_text(p: (Nm, Nm)) -> String {
    if p.0 == p.1 {
        mm(p.0)
    } else {
        format!("{} x {}", mm(p.0), mm(p.1))
    }
}

fn pad_values(p: &PadDetail) -> PadValues {
    PadValues {
        drill: p.drill,
        size: p.size,
        zone_connection: Some(p.zone_connection.clone()),
    }
}

/// The edit as a list of what it sets, for the error and warning texts.
fn describe_set(set: &PadValues) -> String {
    let mut parts = Vec::new();
    if let Some(d) = set.drill {
        parts.push(format!("hole {}", pair_text(d)));
    }
    if let Some(s) = set.size {
        parts.push(format!("copper {}", pair_text(s)));
    }
    if let Some(z) = &set.zone_connection {
        parts.push(format!("zone connection {z}"));
    }
    parts.join(", ")
}

fn plan_pad(
    f: &FootprintDetail,
    edit: &PadEdit,
    warnings: &mut Vec<String>,
    expected: &mut Board,
    index: usize,
    id: &str,
) -> Result<Option<BoardOp>, ToolError> {
    let who = format!("{} pad {}", f.reference, edit.number);
    if edit.drill.is_none() && edit.size.is_none() && edit.zone_connection.is_none() {
        return Err(bad(format!(
            "{who}: name at least one of drill_mm, width_mm and height_mm, zone_connection"
        )));
    }
    if let Some(z) = &edit.zone_connection {
        if !ZONE_CONNECTIONS.contains(&z.as_str()) {
            return Err(bad(format!(
                "{who}: zone_connection is one of {}",
                ZONE_CONNECTIONS.join(", ")
            )));
        }
    }
    let pads: Vec<&PadDetail> = f.pads.iter().filter(|p| p.number == edit.number).collect();
    if pads.is_empty() {
        let mut numbers: Vec<&str> = f.pads.iter().map(|p| p.number.as_str()).collect();
        numbers.dedup();
        return Err(bad(format!(
            "{} has no pad {} (its pads: {})",
            f.reference,
            edit.number,
            numbers.join(", ")
        )));
    }
    let mut changing = 0;
    for pad in &pads {
        if edit.drill.is_some() && !matches!(pad.kind.as_str(), "thru_hole" | "np_thru_hole") {
            return Err(bad(format!(
                "{who} is {} and has no hole to size",
                if pad.kind == "smd" {
                    "surface mount"
                } else {
                    "an edge connector"
                }
            )));
        }
        let size = edit.size.or(pad.size);
        if let (Some(s), true) = (edit.size, pad.shape == "circle") {
            if s.0 != s.1 {
                return Err(bad(format!(
                    "{who} is a circle; give the same width and height ({} x {} was asked)",
                    mm(s.0),
                    mm(s.1)
                )));
            }
        }
        let drill = edit.drill.or(pad.drill);
        if pad.kind == "thru_hole" {
            if let (Some(s), Some(d)) = (size, drill) {
                let ring = (s.0 .0.min(s.1 .0) - d.0 .0.max(d.1 .0)) / 2;
                if ring <= 0 {
                    return Err(bad(format!(
                        "{who}: the hole ({}) is not smaller than the copper ({}); a plated hole needs a ring of copper around it",
                        pair_text(d),
                        pair_text(s)
                    )));
                }
                if ring < MIN_ANNULAR_RING_NM && (edit.drill.is_some() || edit.size.is_some()) {
                    warnings.push(format!(
                        "{who} would have an annular ring of {:.3} mm; most fabs want {:.2} mm or more",
                        ring as f64 / 1e6,
                        MIN_ANNULAR_RING_NM as f64 / 1e6
                    ));
                }
            }
        }
        let differs = edit.drill.is_some_and(|d| !pad.drill.is_some_and(|c| pair_eq(c, d)))
            || edit.size.is_some_and(|s| !pad.size.is_some_and(|c| pair_eq(c, s)))
            || edit.zone_connection.as_ref().is_some_and(|z| z != &pad.zone_connection);
        if differs {
            changing += 1;
        }
    }
    if changing == 0 {
        warnings.push(format!("{who} already has these values; nothing to change"));
        return Ok(None);
    }
    let set = PadValues {
        drill: edit.drill,
        size: edit.size,
        zone_connection: edit.zone_connection.clone(),
    };
    if let Some(s) = edit.size {
        for p in &mut expected.footprints[index].pads {
            if p.number == edit.number {
                p.size = Some(s);
            }
        }
    }
    if pads.len() > 1 {
        warnings.push(format!(
            "{} pads of {} are numbered {}; all of them get: {}",
            pads.len(),
            f.reference,
            edit.number,
            describe_set(&set)
        ));
    }
    Ok(Some(BoardOp::EditPad {
        reference: f.reference.clone(),
        id: id.to_owned(),
        number: edit.number.clone(),
        count: pads.len(),
        from: pad_values(pads[0]),
        set,
    }))
}

fn plan_text(
    f: &FootprintDetail,
    edit: &TextEdit,
    footprint_back: bool,
    warnings: &mut Vec<String>,
    id: &str,
) -> Result<Option<BoardOp>, ToolError> {
    let name = match edit.field.to_ascii_lowercase().as_str() {
        "reference" => "Reference",
        "value" => "Value",
        other => {
            return Err(bad(format!(
                "{}: field is reference or value, not {other}",
                f.reference
            )))
        }
    };
    let who = format!("{} {}", f.reference, name.to_lowercase());
    if edit.layer.is_none()
        && edit.height.is_none()
        && edit.thickness.is_none()
        && edit.at.is_none()
        && edit.offset.is_none()
        && edit.angle_deg.is_none()
        && edit.visible.is_none()
    {
        return Err(bad(format!(
            "{who}: name at least one of layer, height_mm, thickness_mm, x_mm and y_mm, dx_mm and dy_mm, angle_deg, visible"
        )));
    }
    if edit.at.is_some() && edit.offset.is_some() {
        return Err(bad(format!("{who}: give x_mm and y_mm or dx_mm and dy_mm, not both")));
    }
    let Some(text) = f.texts.iter().find(|t| t.name == name) else {
        return Err(bad(format!(
            "{} has no {} text on the board",
            f.reference,
            name.to_lowercase()
        )));
    };
    if let Some(layer) = &edit.layer {
        if !TEXT_LAYERS.contains(&layer.as_str()) {
            return Err(bad(format!("{who}: layer is one of {}", TEXT_LAYERS.join(", "))));
        }
    }
    if let Some(h) = edit.height {
        if !(100_000..=25_000_000).contains(&h.0) {
            return Err(bad(format!("{who}: height_mm is 0.1 to 25")));
        }
    }
    if let Some(t) = edit.thickness {
        if !(10_000..=5_000_000).contains(&t.0) {
            return Err(bad(format!("{who}: thickness_mm is 0.01 to 5")));
        }
    }
    let from = TextValues::of(text);
    let to = TextValues {
        layer: edit.layer.clone().unwrap_or_else(|| from.layer.clone()),
        height: edit.height.unwrap_or(from.height),
        thickness: edit.thickness.unwrap_or(from.thickness),
        at: match (edit.at, edit.offset) {
            (Some(a), _) => a,
            (None, Some(o)) => from.at + o,
            (None, None) => from.at,
        },
        angle_deg: edit.angle_deg.map_or(from.angle_deg, normalize_deg),
        visible: edit.visible.unwrap_or(from.visible),
    };
    if to.same_as(&from) {
        warnings.push(format!("{who} is already so; nothing to change"));
        return Ok(None);
    }
    if to.layer.contains("SilkS")
        && to.visible
        && (to.height.0 < MIN_SILK_HEIGHT_NM || to.thickness.0 < MIN_SILK_STROKE_NM)
    {
        warnings.push(format!(
            "{who} on silkscreen would be {} mm tall with a {} mm stroke; most fabs print {:.1} mm and {:.2} mm or more",
            mm(to.height),
            mm(to.thickness),
            MIN_SILK_HEIGHT_NM as f64 / 1e6,
            MIN_SILK_STROKE_NM as f64 / 1e6
        ));
    }
    if edit.layer.is_some() && to.layer.starts_with("B.") != footprint_back && to.layer != "F.Cu" && to.layer != "B.Cu"
    {
        warnings.push(format!(
            "{who} would be on {}, the other side from its footprint",
            to.layer
        ));
    }
    if to.thickness.0 * 2 > to.height.0 {
        warnings.push(format!(
            "{who}: a stroke of {} mm on text {} mm tall will blur",
            mm(to.thickness),
            mm(to.height)
        ));
    }
    Ok(Some(BoardOp::EditText {
        reference: f.reference.clone(),
        id: id.to_owned(),
        field: name,
        from,
        to,
    }))
}

/// Plans edits to the pads and text of named footprints. `detail` is the board's full geometry
/// (`BoardSession::read_detail`).
pub fn plan_footprint_edits(
    board: &Board,
    detail: &BoardDetail,
    requests: &[FootprintEditRequest],
) -> Result<BoardEditPlan, ToolError> {
    if requests.is_empty() {
        return Err(bad("nothing to edit; name at least one footprint"));
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
        let placed = &board.footprints[index];
        let Some(f) = detail.footprint(&req.reference) else {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!("{} is on the board but its geometry could not be read", req.reference),
            ));
        };
        if req.pads.is_empty() && req.texts.is_empty() {
            return Err(bad(format!("{}: name pads or texts to change", req.reference)));
        }
        if placed.locked && !req.allow_locked {
            return Err(bad(format!(
                "{} is locked; pass allow_locked: true to edit it",
                req.reference
            )));
        }
        let mut numbers = BTreeSet::new();
        for pad in &req.pads {
            if !numbers.insert(pad.number.as_str()) {
                return Err(bad(format!(
                    "{} pad {} is named twice; give one entry per pad number",
                    req.reference, pad.number
                )));
            }
            if let Some(op) = plan_pad(f, pad, &mut warnings, &mut expected, index, &placed.id)? {
                ops.push(op);
            }
        }
        let mut fields = BTreeSet::new();
        for text in &req.texts {
            if !fields.insert(text.field.to_ascii_lowercase()) {
                return Err(bad(format!(
                    "{} {} is named twice; give one entry per text",
                    req.reference, text.field
                )));
            }
            if let Some(op) = plan_text(f, text, placed.is_back(), &mut warnings, &placed.id)? {
                ops.push(op);
            }
        }
    }
    if ops.is_empty() {
        return Err(bad("every pad and text named already has these values"));
    }
    let measured = measure(board);
    Ok(BoardEditPlan {
        kind: "footprint",
        operations: ops,
        before: measured.clone(),
        after: measured,
        warnings,
        expected,
    })
}

// ---------------------------------------------------------------- verification

/// Compares the detail read after applying with what the operations set: every pad numbered as
/// named and every text. Returns the differences.
pub fn verify(operations: &[BoardOp], detail: &BoardDetail) -> Vec<String> {
    let mut problems = Vec::new();
    for op in operations {
        match op {
            BoardOp::EditPad {
                reference, number, set, ..
            } => {
                let Some(f) = detail.footprint(reference) else {
                    problems.push(format!("{reference} is no longer on the board"));
                    continue;
                };
                for pad in f.pads.iter().filter(|p| &p.number == number) {
                    let who = format!("{reference} pad {number}");
                    if let Some(d) = set.drill {
                        if !pad.drill.is_some_and(|now| pair_eq(now, d)) {
                            problems.push(format!(
                                "{who} has hole {}, the plan set {}",
                                pad.drill.map_or("none".to_owned(), pair_text),
                                pair_text(d)
                            ));
                        }
                    }
                    if let Some(s) = set.size {
                        if !pad.size.is_some_and(|now| pair_eq(now, s)) {
                            problems.push(format!(
                                "{who} has copper {}, the plan set {}",
                                pad.size.map_or("unknown".to_owned(), pair_text),
                                pair_text(s)
                            ));
                        }
                    }
                    if let Some(z) = &set.zone_connection {
                        if &pad.zone_connection != z {
                            problems.push(format!(
                                "{who} has zone connection {}, the plan set {z}",
                                pad.zone_connection
                            ));
                        }
                    }
                }
            }
            BoardOp::EditText {
                reference, field, to, ..
            } => {
                let Some(t) = detail
                    .footprint(reference)
                    .and_then(|f| f.texts.iter().find(|t| t.name == *field))
                else {
                    problems.push(format!("{reference} {} text is gone", field.to_lowercase()));
                    continue;
                };
                let now = TextValues::of(t);
                let who = format!("{reference} {}", field.to_lowercase());
                if now.layer != to.layer {
                    problems.push(format!("{who} is on {}, the plan put it on {}", now.layer, to.layer));
                }
                if (now.height.0 - to.height.0).abs() > TOLERANCE_NM {
                    problems.push(format!(
                        "{who} is {} mm tall, the plan made it {}",
                        mm(now.height),
                        mm(to.height)
                    ));
                }
                if (now.thickness.0 - to.thickness.0).abs() > TOLERANCE_NM {
                    problems.push(format!(
                        "{who} has a {} mm stroke, the plan made it {}",
                        mm(now.thickness),
                        mm(to.thickness)
                    ));
                }
                if !near(now.at, to.at) {
                    problems.push(format!(
                        "{who} is at ({:.3}, {:.3}) mm, the plan put it at ({:.3}, {:.3})",
                        now.at.x.to_mm(),
                        now.at.y.to_mm(),
                        to.at.x.to_mm(),
                        to.at.y.to_mm()
                    ));
                }
                if !angle_eq(now.angle_deg, to.angle_deg) {
                    problems.push(format!(
                        "{who} is turned {:.1}°, the plan turned it {:.1}°",
                        now.angle_deg, to.angle_deg
                    ));
                }
                if now.visible != to.visible {
                    problems.push(format!(
                        "{who} is {}, the plan made it {}",
                        if now.visible { "shown" } else { "hidden" },
                        if to.visible { "shown" } else { "hidden" }
                    ));
                }
            }
            _ => {}
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{BoardFootprint, BoardPad};

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    fn n(v: f64) -> Nm {
        Nm::from_mm(v).unwrap()
    }

    fn footprint(reference: &str) -> BoardFootprint {
        BoardFootprint {
            id: format!("id-{reference}"),
            reference: reference.into(),
            value: "x".into(),
            lib_id: "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical".into(),
            layer: "F.Cu".into(),
            position: p(10.0, 10.0),
            rotation_deg: 0.0,
            locked: false,
            dnp: false,
            exclude_from_bom: false,
            exclude_from_pos: false,
            board_only: false,
            pads: vec![
                BoardPad {
                    number: "1".into(),
                    kind: "thru_hole".into(),
                    at: p(10.0, 10.0),
                    net: "A".into(),
                    size: Some((n(1.7), n(1.7))),
                    ..Default::default()
                },
                BoardPad {
                    number: "2".into(),
                    kind: "thru_hole".into(),
                    at: p(10.0, 12.54),
                    net: "B".into(),
                    size: Some((n(1.7), n(1.7))),
                    ..Default::default()
                },
            ],
            courtyard: None,
            courtyard_local: None,
        }
    }

    fn pad(number: &str, kind: &str, shape: &str) -> PadDetail {
        PadDetail {
            number: number.into(),
            kind: kind.into(),
            shape: shape.into(),
            at: p(10.0, 10.0),
            angle_deg: 0.0,
            size: Some((n(1.7), n(1.7))),
            drill: if kind == "smd" { None } else { Some((n(1.0), n(1.0))) },
            corner_ratio: None,
            layers: vec!["F.Cu".into(), "B.Cu".into()],
            net: String::new(),
            zone_connection: "inherited".into(),
        }
    }

    fn text(name: &str, layer: &str) -> TextDetail {
        TextDetail {
            id: format!("t-{name}"),
            name: name.into(),
            text: "J1".into(),
            layer: layer.into(),
            at: p(10.0, 8.0),
            angle_deg: 0.0,
            height: n(1.0),
            thickness: n(0.15),
            visible: true,
            locked: false,
        }
    }

    fn setup() -> (Board, BoardDetail) {
        let board = Board {
            footprints: vec![footprint("J1")],
            ..Default::default()
        };
        let detail = BoardDetail {
            footprints: vec![FootprintDetail {
                reference: "J1".into(),
                pads: vec![
                    pad("1", "thru_hole", "rect"),
                    pad("2", "thru_hole", "circle"),
                    pad("3", "smd", "rect"),
                ],
                texts: vec![text("Reference", "F.SilkS"), text("Value", "F.Fab")],
                ..Default::default()
            }],
            ..Default::default()
        };
        (board, detail)
    }

    fn request(pads: Vec<PadEdit>, texts: Vec<TextEdit>) -> FootprintEditRequest {
        FootprintEditRequest {
            reference: "J1".into(),
            pads,
            texts,
            allow_locked: false,
        }
    }

    #[test]
    fn a_pad_gets_a_new_hole_size_and_zone_connection() {
        let (board, detail) = setup();
        let plan = plan_footprint_edits(
            &board,
            &detail,
            &[request(
                vec![PadEdit {
                    number: "1".into(),
                    drill: Some((n(1.1), n(1.1))),
                    size: Some((n(2.2), n(2.2))),
                    zone_connection: Some("full".into()),
                }],
                vec![],
            )],
        )
        .unwrap();
        assert_eq!(plan.kind, "footprint");
        let BoardOp::EditPad {
            count, from, set, id, ..
        } = &plan.operations[0]
        else {
            panic!("{:?}", plan.operations)
        };
        assert_eq!((*count, id.as_str()), (1, "id-J1"));
        assert_eq!(from.drill, Some((n(1.0), n(1.0))));
        assert_eq!(set.zone_connection.as_deref(), Some("full"));
        assert_eq!(plan.expected.footprints[0].pads[0].size, Some((n(2.2), n(2.2))));
        assert_eq!(plan.expected.footprints[0].pads[1].size, Some((n(1.7), n(1.7))));
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn nonsense_for_a_pad_is_refused_with_a_reason() {
        let (board, detail) = setup();
        let pad = |number: &str| PadEdit {
            number: number.into(),
            drill: Some((n(1.2), n(1.2))),
            ..Default::default()
        };
        let refused =
            |edit: PadEdit| plan_footprint_edits(&board, &detail, &[request(vec![edit], vec![])]).unwrap_err();
        assert!(refused(pad("3")).message.contains("surface mount"));
        assert!(refused(pad("9")).message.contains("no pad 9"));
        assert!(refused(PadEdit {
            number: "1".into(),
            drill: Some((n(1.8), n(1.8))),
            ..Default::default()
        })
        .message
        .contains("not smaller than the copper"));
        assert!(refused(PadEdit {
            number: "2".into(),
            size: Some((n(2.0), n(1.5))),
            ..Default::default()
        })
        .message
        .contains("circle"));
        assert!(refused(PadEdit {
            number: "1".into(),
            zone_connection: Some("sometimes".into()),
            ..Default::default()
        })
        .message
        .contains("zone_connection"));
        assert!(refused(PadEdit {
            number: "1".into(),
            ..Default::default()
        })
        .message
        .contains("at least one"));
    }

    #[test]
    fn a_thin_ring_is_a_warning_and_a_repeat_changes_nothing() {
        let (board, detail) = setup();
        let thin = PadEdit {
            number: "1".into(),
            drill: Some((n(1.5), n(1.5))),
            ..Default::default()
        };
        let plan = plan_footprint_edits(&board, &detail, &[request(vec![thin], vec![])]).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("annular ring")),
            "{:?}",
            plan.warnings
        );
        let same = PadEdit {
            number: "1".into(),
            drill: Some((n(1.0), n(1.0))),
            zone_connection: Some("inherited".into()),
            ..Default::default()
        };
        let err = plan_footprint_edits(&board, &detail, &[request(vec![same], vec![])]).unwrap_err();
        assert!(err.message.contains("already"));
    }

    #[test]
    fn text_moves_to_another_layer_gets_smaller_and_can_be_hidden() {
        let (board, detail) = setup();
        let plan = plan_footprint_edits(
            &board,
            &detail,
            &[request(
                vec![],
                vec![
                    TextEdit {
                        field: "Reference".into(),
                        layer: Some("F.Fab".into()),
                        offset: Some(p(0.0, -1.0)),
                        height: Some(n(0.5)),
                        ..Default::default()
                    },
                    TextEdit {
                        field: "value".into(),
                        visible: Some(false),
                        ..Default::default()
                    },
                ],
            )],
        )
        .unwrap();
        let BoardOp::EditText { field, from, to, .. } = &plan.operations[0] else {
            panic!()
        };
        assert_eq!(*field, "Reference");
        assert_eq!(from.layer, "F.SilkS");
        assert_eq!((to.layer.as_str(), to.at, to.height), ("F.Fab", p(10.0, 7.0), n(0.5)));
        let BoardOp::EditText { to, .. } = &plan.operations[1] else {
            panic!()
        };
        assert!(!to.visible);
        // Not on silkscreen any more, so no legibility warning.
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn small_silkscreen_and_the_wrong_side_are_warned_about() {
        let (board, detail) = setup();
        let plan = plan_footprint_edits(
            &board,
            &detail,
            &[request(
                vec![],
                vec![TextEdit {
                    field: "reference".into(),
                    height: Some(n(0.5)),
                    ..Default::default()
                }],
            )],
        )
        .unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("most fabs print")),
            "{:?}",
            plan.warnings
        );
        let plan = plan_footprint_edits(
            &board,
            &detail,
            &[request(
                vec![],
                vec![TextEdit {
                    field: "reference".into(),
                    layer: Some("B.SilkS".into()),
                    ..Default::default()
                }],
            )],
        )
        .unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("other side")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn requests_that_make_no_sense_are_refused() {
        let (mut board, detail) = setup();
        let text = |t: TextEdit| {
            plan_footprint_edits(&board, &detail, &[request(vec![], vec![t])])
                .unwrap_err()
                .message
        };
        assert!(text(TextEdit {
            field: "datasheet".into(),
            visible: Some(false),
            ..Default::default()
        })
        .contains("reference or value"));
        assert!(text(TextEdit {
            field: "value".into(),
            layer: Some("Edge.Cuts".into()),
            ..Default::default()
        })
        .contains("layer is one of"));
        assert!(text(TextEdit {
            field: "value".into(),
            at: Some(p(1.0, 1.0)),
            offset: Some(p(1.0, 1.0)),
            ..Default::default()
        })
        .contains("not both"));
        assert!(text(TextEdit {
            field: "value".into(),
            ..Default::default()
        })
        .contains("at least one"));
        assert!(plan_footprint_edits(&board, &detail, &[]).is_err());
        assert!(plan_footprint_edits(&board, &detail, &[request(vec![], vec![])]).is_err());
        let mut missing = request(vec![], vec![]);
        missing.reference = "J9".into();
        assert!(plan_footprint_edits(&board, &detail, &[missing]).is_err());
        board.footprints[0].locked = true;
        let locked = request(
            vec![PadEdit {
                number: "1".into(),
                zone_connection: Some("full".into()),
                ..Default::default()
            }],
            vec![],
        );
        assert!(plan_footprint_edits(&board, &detail, std::slice::from_ref(&locked))
            .unwrap_err()
            .message
            .contains("locked"));
        let mut allowed = locked;
        allowed.allow_locked = true;
        assert!(plan_footprint_edits(&board, &detail, &[allowed]).is_ok());
    }

    #[test]
    fn the_read_back_is_compared_with_the_plan() {
        let (board, detail) = setup();
        let plan = plan_footprint_edits(
            &board,
            &detail,
            &[request(
                vec![PadEdit {
                    number: "1".into(),
                    drill: Some((n(1.1), n(1.1))),
                    zone_connection: Some("none".into()),
                    ..Default::default()
                }],
                vec![TextEdit {
                    field: "reference".into(),
                    layer: Some("F.Fab".into()),
                    visible: Some(false),
                    ..Default::default()
                }],
            )],
        )
        .unwrap();
        // The board as it was: both edits are missing.
        let problems = verify(&plan.operations, &detail);
        assert_eq!(problems.len(), 4, "{problems:?}");
        // The board as the plan leaves it.
        let mut done = detail.clone();
        done.footprints[0].pads[0].drill = Some((n(1.1), n(1.1)));
        done.footprints[0].pads[0].zone_connection = "none".into();
        done.footprints[0].texts[0].layer = "F.Fab".into();
        done.footprints[0].texts[0].visible = false;
        assert!(verify(&plan.operations, &done).is_empty());
        // Within KiCad's rounding.
        done.footprints[0].pads[0].drill = Some((Nm(1_100_900), Nm(1_099_400)));
        assert!(verify(&plan.operations, &done).is_empty());
    }
}
