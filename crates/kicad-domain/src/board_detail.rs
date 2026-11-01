// SPDX-License-Identifier: GPL-3.0-or-later
//! The geometry of a board in full (Phase 14.4): every pad's shape, size, drill, and turn, the
//! courtyard outline, the text of each footprint with its layer, size, place, and visibility, and
//! the drawings and text on the board itself. Read separately from [`crate::board::Board`], which
//! keeps only what planning needs.

use crate::geometry::{Nm, Point, Rect};

#[derive(Debug, Clone, PartialEq)]
pub struct PadDetail {
    pub number: String,
    /// `smd`, `thru_hole`, `np_thru_hole`, or `connect`.
    pub kind: String,
    /// `circle`, `rect`, `oval`, `trapezoid`, `roundrect`, `chamfered_rect`, or `custom`.
    pub shape: String,
    /// The pad's centre on the board.
    pub at: Point,
    /// The pad's turn on the board, degrees counter-clockwise on screen.
    pub angle_deg: f64,
    /// Width and height before the pad is turned.
    pub size: Option<(Nm, Nm)>,
    /// The drill, width and height (equal for a round hole).
    pub drill: Option<(Nm, Nm)>,
    /// How round the corners of a `roundrect` are, as a fraction of the shorter side.
    pub corner_ratio: Option<f64>,
    pub layers: Vec<String>,
    pub net: String,
    /// How a zone connects to the pad: `inherited`, `none`, `thermal`, `full`, or `pth_thermal`.
    pub zone_connection: String,
}

/// A piece of text: a footprint's reference, value, or other field, or free text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextDetail {
    pub id: String,
    /// `Reference`, `Value`, `Datasheet`, `Description`, another field's name, or `text`.
    pub name: String,
    pub text: String,
    pub layer: String,
    pub at: Point,
    pub angle_deg: f64,
    /// Character height.
    pub height: Nm,
    pub thickness: Nm,
    pub visible: bool,
    pub locked: bool,
}

/// A drawn line, rectangle, circle, arc, polygon, or curve.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeDetail {
    pub id: String,
    /// `line`, `rect`, `circle`, `arc`, `polygon`, `bezier`, or `other`.
    pub kind: &'static str,
    pub layer: String,
    /// A line: start, end. A rectangle: two corners. A circle: the centre and a point on it. An arc:
    /// start, mid, end. A polygon: its outline. A curve: start, two controls, end.
    pub points: Vec<Point>,
    pub width: Nm,
    pub filled: bool,
    pub locked: bool,
}

impl ShapeDetail {
    /// The box around the shape's points (a circle's box is not its radius).
    pub fn bounds(&self) -> Option<Rect> {
        Rect::from_points(self.points.iter().copied())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FootprintDetail {
    pub reference: String,
    pub pads: Vec<PadDetail>,
    /// The shapes on the courtyard layers, on the board.
    pub courtyard: Vec<ShapeDetail>,
    /// Fields and free text.
    pub texts: Vec<TextDetail>,
    /// Other drawings of the footprint (silkscreen, fabrication), by layer.
    pub graphics: std::collections::BTreeMap<String, usize>,
}

/// Everything the detail reads cover.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardDetail {
    pub footprints: Vec<FootprintDetail>,
    /// Drawings on the board itself (not in a footprint).
    pub shapes: Vec<ShapeDetail>,
    /// Text on the board itself.
    pub texts: Vec<TextDetail>,
}

impl BoardDetail {
    pub fn footprint(&self, reference: &str) -> Option<&FootprintDetail> {
        self.footprints.iter().find(|f| f.reference == reference)
    }
}
