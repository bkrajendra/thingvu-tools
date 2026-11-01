// SPDX-License-Identifier: GPL-3.0-or-later
//! The geometry of a board from its file in full (Phase 14.4): see `kicad_domain::board_detail`.
//! Items of a footprint are stored in the footprint's own coordinates and are placed on the board
//! here.

use std::collections::BTreeMap;

use kicad_domain::board::place_point;
use kicad_domain::board_detail::{BoardDetail, FootprintDetail, PadDetail, ShapeDetail, TextDetail};
use kicad_domain::{Nm, Point};

use crate::node::{parse, Node};
use crate::pcb::{is_yes, layer_of, net_of, nm, placement, point, BoardError};

const SHAPES: &[&str] = &["line", "rect", "circle", "arc", "poly", "curve", "bezier"];

fn kind_of(head: &str) -> Option<&'static str> {
    let name = head.strip_prefix("fp_").or_else(|| head.strip_prefix("gr_"))?;
    if !SHAPES.contains(&name) {
        return None;
    }
    Some(match name {
        "line" => "line",
        "rect" => "rect",
        "circle" => "circle",
        "arc" => "arc",
        "poly" => "polygon",
        _ => "bezier",
    })
}

fn stroke_width(item: &Node) -> Nm {
    let w = item
        .child("stroke")
        .and_then(|s| s.child_f64("width"))
        .or_else(|| item.child_f64("width"))
        .unwrap_or(0.0);
    nm(w)
}

fn filled(item: &Node) -> bool {
    match item.child("fill") {
        Some(f) => f
            .child_text("type")
            .or_else(|| f.arg_text(0))
            .is_some_and(|v| v != "none" && v != "no"),
        None => false,
    }
}

fn shape(item: &Node, kind: &'static str, map: &dyn Fn(Point) -> Point) -> ShapeDetail {
    let pt = |key: &str| item.child(key).and_then(point).map(map);
    let points: Vec<Point> = match kind {
        "line" | "rect" => [pt("start"), pt("end")].into_iter().flatten().collect(),
        "circle" => [pt("center"), pt("end")].into_iter().flatten().collect(),
        "arc" => [pt("start"), pt("mid"), pt("end")].into_iter().flatten().collect(),
        _ => item
            .child("pts")
            .map(|p| p.children("xy").filter_map(point).map(map).collect())
            .unwrap_or_default(),
    };
    ShapeDetail {
        id: item
            .child_text("uuid")
            .or_else(|| item.child_text("tstamp"))
            .unwrap_or_default()
            .to_owned(),
        kind,
        layer: layer_of(item),
        points,
        width: stroke_width(item),
        filled: filled(item),
        locked: is_yes(item, "locked"),
    }
}

fn hidden(item: &Node) -> bool {
    is_yes(item, "hide") || item.child("effects").is_some_and(|e| is_yes(e, "hide"))
}

/// A text-like item: `property`, `fp_text`, `gr_text`.
fn text(item: &Node, name: String, text: &str, map: &dyn Fn(Point) -> Point) -> Option<TextDetail> {
    let at = item.child("at")?;
    let font = item.child("effects").and_then(|e| e.child("font"));
    Some(TextDetail {
        id: item
            .child_text("uuid")
            .or_else(|| item.child_text("tstamp"))
            .unwrap_or_default()
            .to_owned(),
        name,
        text: text.to_owned(),
        layer: layer_of(item),
        at: map(point(at)?),
        angle_deg: at.arg_f64(2).unwrap_or(0.0),
        height: nm(font
            .and_then(|f| f.child("size"))
            .and_then(|s| s.arg_f64(0))
            .unwrap_or(1.0)),
        thickness: nm(font.and_then(|f| f.child_f64("thickness")).unwrap_or(0.15)),
        visible: !hidden(item),
        locked: is_yes(item, "locked"),
    })
}

pub fn read_detail(source: &str) -> Result<BoardDetail, BoardError> {
    let root = parse(source)?;
    if root.head() != Some("kicad_pcb") {
        return Err(BoardError::NotABoard);
    }
    let mut copper: Vec<String> = Vec::new();
    if let Some(layers) = root.child("layers") {
        for layer in layers.items().iter().filter(|l| l.is_list()) {
            if let (Some(name), Some(kind)) = (layer.arg_text(0), layer.arg_text(1)) {
                if matches!(kind, "signal" | "power" | "mixed" | "jumper") {
                    copper.push(name.to_owned());
                }
            }
        }
    }
    let table: BTreeMap<i64, String> = root
        .children("net")
        .filter_map(|n| Some((n.arg_text(0)?.parse().ok()?, n.arg_text(1)?.to_owned())))
        .collect();
    let mut detail = BoardDetail::default();
    let identity = |p: Point| p;

    for item in root.items() {
        match item.head() {
            Some("footprint") | Some("module") => {
                let (origin, rotation) = placement(item);
                let map = |p: Point| place_point(origin, rotation, p);
                let mut fp = FootprintDetail::default();
                for child in item.items().iter().filter(|c| c.is_list()) {
                    match child.head() {
                        Some("property") => {
                            if let (Some(name), Some(value)) = (child.arg_text(0), child.arg_text(1)) {
                                if name == "Reference" {
                                    fp.reference = value.to_owned();
                                }
                                fp.texts.extend(text(child, name.to_owned(), value, &map));
                            }
                        }
                        Some("fp_text") => {
                            if let (Some(kind), Some(value)) = (child.arg_text(0), child.arg_text(1)) {
                                let name = match kind {
                                    "reference" => {
                                        fp.reference = value.to_owned();
                                        "Reference"
                                    }
                                    "value" => "Value",
                                    _ => "text",
                                };
                                fp.texts.extend(text(child, name.to_owned(), value, &map));
                            }
                        }
                        Some("pad") => {
                            let (Some(number), Some(at)) = (child.arg_text(0), child.child("at")) else {
                                continue;
                            };
                            let mut layers: Vec<String> = Vec::new();
                            for l in child
                                .child("layers")
                                .map(|l| l.items())
                                .unwrap_or_default()
                                .iter()
                                .skip(1)
                            {
                                match l.text() {
                                    Some("*.Cu") | Some("F&B.Cu") => layers.extend(copper.iter().cloned()),
                                    Some(n) if n.ends_with(".Cu") => layers.push(n.to_owned()),
                                    _ => {}
                                }
                            }
                            layers.dedup();
                            let drill = child.child("drill").and_then(|d| {
                                // `(drill 0.8)` or `(drill oval 0.8 1.6)`, with optional `(offset …)`.
                                let nums: Vec<f64> = d
                                    .items()
                                    .iter()
                                    .skip(1)
                                    .filter_map(|n| n.text()?.parse().ok())
                                    .collect();
                                match nums[..] {
                                    [a] => Some((nm(a), nm(a))),
                                    [a, b, ..] => Some((nm(a), nm(b))),
                                    _ => None,
                                }
                            });
                            fp.pads.push(PadDetail {
                                number: number.to_owned(),
                                kind: child.arg_text(1).unwrap_or("smd").to_owned(),
                                shape: match child.arg_text(2).unwrap_or("rect") {
                                    "circle" => "circle",
                                    "rect" => "rect",
                                    "oval" => "oval",
                                    "trapezoid" => "trapezoid",
                                    "roundrect" => "roundrect",
                                    "chamfered_rect" => "chamfered_rect",
                                    _ => "custom",
                                }
                                .to_owned(),
                                at: map(point(at).unwrap_or_default()),
                                angle_deg: at.arg_f64(2).unwrap_or(0.0),
                                size: child
                                    .child("size")
                                    .and_then(|s| Some((nm(s.arg_f64(0)?), nm(s.arg_f64(1)?)))),
                                drill,
                                corner_ratio: child.child_f64("roundrect_rratio"),
                                layers,
                                net: net_of(child, &table),
                                // `(zone_connect n)`: inherited when the pad does not say.
                                zone_connection: match child.child_f64("zone_connect") {
                                    Some(0.0) => "none",
                                    Some(1.0) => "thermal",
                                    Some(2.0) => "full",
                                    Some(3.0) => "pth_thermal",
                                    _ => "inherited",
                                }
                                .to_owned(),
                            });
                        }
                        Some(head) => {
                            let Some(kind) = kind_of(head) else {
                                continue;
                            };
                            let s = shape(child, kind, &map);
                            if s.layer.ends_with(".CrtYd") {
                                fp.courtyard.push(s);
                            } else {
                                *fp.graphics.entry(s.layer).or_default() += 1;
                            }
                        }
                        None => {}
                    }
                }
                detail.footprints.push(fp);
            }
            Some("gr_text") => {
                if let Some(value) = item.arg_text(0) {
                    detail.texts.extend(text(item, "text".into(), value, &identity));
                }
            }
            Some(head) => {
                if let Some(kind) = kind_of(head) {
                    detail.shapes.push(shape(item, kind, &identity));
                }
            }
            None => {}
        }
    }
    Ok(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = r#"(kicad_pcb (version 20260206) (generator "pcbnew")
	(layers (0 "F.Cu" signal) (2 "B.Cu" signal))
	(net 0 "") (net 1 "GND")
	(gr_line (start 0 0) (end 40 0) (stroke (width 0.05) (type solid)) (layer "Edge.Cuts") (uuid "e1"))
	(gr_text "v1" (at 5 5 90) (layer "F.SilkS") (uuid "t1") (effects (font (size 1.5 1.5) (thickness 0.2))))
	(footprint "Resistor_SMD:R_0603" (layer "F.Cu") (at 10 20 90) (uuid "f1")
		(property "Reference" "R1" (at 0 -1.5 90) (layer "F.SilkS") (uuid "p1") (effects (font (size 1 1) (thickness 0.15))))
		(property "Value" "10k" (at 0 1.5 90) (layer "F.Fab") (hide yes) (uuid "p2") (effects (font (size 0.8 0.8) (thickness 0.1))))
		(fp_line (start -1 -1) (end 1 -1) (stroke (width 0.12) (type solid)) (layer "F.SilkS") (uuid "s1"))
		(fp_rect (start -1.5 -0.8) (end 1.5 0.8) (stroke (width 0.05) (type solid)) (fill no) (layer "F.CrtYd") (uuid "c1"))
		(pad "1" smd roundrect (at -0.8 0 90) (size 0.9 0.95) (layers "F.Cu" "F.Paste" "F.Mask") (roundrect_rratio 0.25) (net 1 "GND") (uuid "d1"))
		(pad "2" thru_hole oval (at 0.8 0) (size 1.6 2.4) (drill oval 0.9 1.7) (zone_connect 2) (layers "*.Cu" "*.Mask") (uuid "d2"))
	)
)"#;

    #[test]
    fn pads_text_and_courtyard_are_read_and_placed() {
        let d = read_detail(BOARD).unwrap();
        let r1 = d.footprint("R1").unwrap();
        // Turned a quarter: the pad at local (-0.8, 0) lands 0.8 mm below the origin (y grows down).
        let p1 = &r1.pads[0];
        assert_eq!(
            (p1.number.as_str(), p1.shape.as_str(), p1.kind.as_str()),
            ("1", "roundrect", "smd")
        );
        assert_eq!(p1.at, Point::from_mm(10.0, 20.8).unwrap());
        assert_eq!(p1.size, Some((nm(0.9), nm(0.95))));
        assert_eq!(p1.corner_ratio, Some(0.25));
        assert_eq!(p1.net, "GND");
        assert_eq!(p1.layers, ["F.Cu"]);
        let p2 = &r1.pads[1];
        assert_eq!(p2.drill, Some((nm(0.9), nm(1.7))));
        assert_eq!(p2.layers, ["F.Cu", "B.Cu"]);
        assert_eq!(
            (p1.zone_connection.as_str(), p2.zone_connection.as_str()),
            ("inherited", "full")
        );
        assert_eq!(r1.courtyard.len(), 1);
        assert_eq!(r1.courtyard[0].kind, "rect");
        let b = r1.courtyard[0].bounds().unwrap();
        assert_eq!((b.width(), b.height()), (nm(1.6), nm(3.0)), "placed and turned");
        assert_eq!(r1.graphics["F.SilkS"], 1);
        let value = r1.texts.iter().find(|t| t.name == "Value").unwrap();
        assert!(!value.visible && value.height == nm(0.8) && value.layer == "F.Fab");
        let reference = r1.texts.iter().find(|t| t.name == "Reference").unwrap();
        assert!(reference.visible && reference.thickness == nm(0.15));
        assert_eq!(
            reference.at,
            Point::from_mm(8.5, 20.0).unwrap(),
            "above the part, turned a quarter counter-clockwise: to its left"
        );
    }

    #[test]
    fn board_drawings_and_text_are_listed() {
        let d = read_detail(BOARD).unwrap();
        assert_eq!(d.shapes.len(), 1);
        assert_eq!((d.shapes[0].kind, d.shapes[0].layer.as_str()), ("line", "Edge.Cuts"));
        assert_eq!(d.shapes[0].width, nm(0.05));
        assert_eq!(d.texts.len(), 1);
        assert_eq!(
            (d.texts[0].text.as_str(), d.texts[0].angle_deg, d.texts[0].height),
            ("v1", 90.0, nm(1.5))
        );
        assert!(read_detail("(kicad_sch)").is_err());
    }
}
