// SPDX-License-Identifier: GPL-3.0-or-later
//! The geometry of the board in full (Phase 14.4), from a live KiCad or from the file.

use kicad_domain::board_detail::{BoardDetail, FootprintDetail, PadDetail, ShapeDetail, TextDetail};
use kicad_domain::{ErrorCode, Nm, Point, ToolError};
use kicad_ipc::proto::kiapi::board::types::{
    BoardGraphicShape, BoardText, BoardTextBox, Field, FootprintInstance, Pad, PadStackShape, ZoneConnectionStyle,
};
use kicad_ipc::proto::kiapi::common::commands::{GetItems, GetItemsResponse};
use kicad_ipc::proto::kiapi::common::types::{
    graphic_shape::Geometry, poly_line_node, ItemHeader, KiCadObjectType, Text, TextAttributes,
};
use prost::Name;

use super::{at, layer_name, net_name, BoardSession};
use crate::errors::from_ipc_error;

fn dist(v: &Option<kicad_ipc::proto::kiapi::common::types::Distance>) -> Nm {
    Nm(v.as_ref().map(|d| d.value_nm).unwrap_or(0))
}

fn text_detail(id: String, name: &str, layer: i32, text: &Option<Text>, visible: bool, locked: bool) -> TextDetail {
    let t = text.clone().unwrap_or_default();
    let a: TextAttributes = t.attributes.clone().unwrap_or_default();
    TextDetail {
        id,
        name: name.to_owned(),
        text: t.text.clone(),
        layer: layer_name(layer),
        at: at(&t.position),
        angle_deg: a.angle.map(|d| d.value_degrees).unwrap_or(0.0),
        height: Nm(a.size.map(|s| s.y_nm).unwrap_or(0)),
        thickness: dist(&a.stroke_width),
        visible,
        locked,
    }
}

fn field_detail(field: &Option<Field>) -> Option<TextDetail> {
    let f = field.as_ref()?;
    let board_text: Option<&BoardText> = f.text.as_ref();
    let bt = board_text?;
    Some(text_detail(
        bt.id.as_ref().map(|k| k.value.clone()).unwrap_or_default(),
        &f.name,
        bt.layer,
        &bt.text,
        f.visible,
        bt.locked == 2,
    ))
}

fn shape_detail(s: &BoardGraphicShape) -> ShapeDetail {
    let shape = s.shape.clone().unwrap_or_default();
    let (kind, points): (&'static str, Vec<Point>) = match &shape.geometry {
        Some(Geometry::Segment(g)) => ("line", vec![at(&g.start), at(&g.end)]),
        Some(Geometry::Rectangle(g)) => ("rect", vec![at(&g.top_left), at(&g.bottom_right)]),
        Some(Geometry::Circle(g)) => ("circle", vec![at(&g.center), at(&g.radius_point)]),
        Some(Geometry::Arc(g)) => ("arc", vec![at(&g.start), at(&g.mid), at(&g.end)]),
        Some(Geometry::Bezier(g)) => (
            "bezier",
            vec![at(&g.start), at(&g.control1), at(&g.control2), at(&g.end)],
        ),
        Some(Geometry::Polygon(set)) => (
            "polygon",
            set.polygons
                .first()
                .and_then(|p| p.outline.as_ref())
                .map(|o| {
                    o.nodes
                        .iter()
                        .filter_map(|n| match &n.geometry {
                            Some(poly_line_node::Geometry::Point(p)) => Some(Point::new(Nm(p.x_nm), Nm(p.y_nm))),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        _ => ("other", vec![]),
    };
    let attributes = shape.attributes.unwrap_or_default();
    ShapeDetail {
        id: s.id.as_ref().map(|k| k.value.clone()).unwrap_or_default(),
        kind,
        layer: layer_name(s.layer),
        points,
        width: dist(&attributes.stroke.and_then(|st| st.width)),
        filled: attributes.fill.is_some_and(|f| f.fill_type >= 2),
        locked: s.locked == 2,
    }
}

fn pad_detail(p: &Pad, copper: &[String]) -> PadDetail {
    let stack = p.pad_stack.clone().unwrap_or_default();
    let first = stack.copper_layers.first();
    let shape = match first.map(|l| l.shape()) {
        Some(PadStackShape::PssCircle) => "circle",
        Some(PadStackShape::PssRectangle) => "rect",
        Some(PadStackShape::PssOval) => "oval",
        Some(PadStackShape::PssTrapezoid) => "trapezoid",
        Some(PadStackShape::PssRoundrect) => "roundrect",
        Some(PadStackShape::PssChamferedrect) => "chamfered_rect",
        _ => "custom",
    };
    PadDetail {
        number: p.number.clone(),
        kind: match p.r#type {
            1 => "thru_hole",
            2 => "smd",
            3 => "connect",
            4 => "np_thru_hole",
            _ => "smd",
        }
        .to_owned(),
        shape: shape.to_owned(),
        at: at(&p.position),
        angle_deg: stack.angle.map(|a| a.value_degrees).unwrap_or(0.0),
        size: first.and_then(|l| l.size).map(|v| (Nm(v.x_nm), Nm(v.y_nm))),
        drill: stack
            .drill
            .and_then(|d| d.diameter)
            .filter(|d| d.x_nm > 0 || d.y_nm > 0)
            .map(|d| (Nm(d.x_nm), Nm(d.y_nm))),
        corner_ratio: first
            .filter(|l| l.shape() == PadStackShape::PssRoundrect)
            .map(|l| l.corner_rounding_ratio),
        layers: if p.r#type == 1 {
            copper.to_vec()
        } else {
            stack
                .layers
                .iter()
                .map(|l| layer_name(*l))
                .filter(|n| n.ends_with(".Cu"))
                .collect()
        },
        net: net_name(&p.net),
        zone_connection: match stack.zone_settings.as_ref().map(|z| z.zone_connection()) {
            Some(ZoneConnectionStyle::ZcsNone) => "none",
            Some(ZoneConnectionStyle::ZcsThermal) => "thermal",
            Some(ZoneConnectionStyle::ZcsFull) => "full",
            Some(ZoneConnectionStyle::ZcsPthThermal) => "pth_thermal",
            _ => "inherited",
        }
        .to_owned(),
    }
}

impl BoardSession {
    /// Pads, courtyards, text, and drawings in full, as the board is now.
    pub async fn read_detail(&self) -> Result<BoardDetail, ToolError> {
        let Some(ipc) = &self.ipc else {
            let text = std::fs::read_to_string(&self.document_path).map_err(|e| {
                ToolError::new(
                    ErrorCode::KicadError,
                    format!("cannot read {}: {e}", self.document_path.display()),
                )
            })?;
            self.freshness.reset(&self.document_path);
            return kicad_sexpr::pcb_detail::read_detail(&text)
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot read the board file: {e}")));
        };
        let copper = self.read().await?.copper_layers;
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let items: GetItemsResponse = ipc
            .client
            .call(&GetItems {
                header: Some(ItemHeader {
                    document: Some(document),
                    ..Default::default()
                }),
                types: [
                    KiCadObjectType::KotPcbFootprint,
                    KiCadObjectType::KotPcbShape,
                    KiCadObjectType::KotPcbText,
                    KiCadObjectType::KotPcbTextbox,
                ]
                .map(|t| t as i32)
                .to_vec(),
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let bad = |what: &str, e: prost::DecodeError| {
            ToolError::new(ErrorCode::KicadError, format!("undecodable {what}: {e}"))
        };
        let mut detail = BoardDetail::default();
        for any in &items.items {
            let url = any.type_url.as_str();
            if url == FootprintInstance::type_url() {
                let f: FootprintInstance = any.to_msg().map_err(|e| bad("footprint", e))?;
                let mut fp = FootprintDetail {
                    reference: super::field_text(&f.reference_field),
                    ..Default::default()
                };
                for field in [
                    &f.reference_field,
                    &f.value_field,
                    &f.datasheet_field,
                    &f.description_field,
                ] {
                    fp.texts.extend(field_detail(field));
                }
                for item in f.definition.iter().flat_map(|d| d.items.iter()) {
                    let u = item.type_url.as_str();
                    if u == Pad::type_url() {
                        if let Ok(p) = item.to_msg::<Pad>() {
                            fp.pads.push(pad_detail(&p, &copper));
                        }
                    } else if u == BoardGraphicShape::type_url() {
                        if let Ok(s) = item.to_msg::<BoardGraphicShape>() {
                            let s = shape_detail(&s);
                            if s.layer.ends_with(".CrtYd") {
                                fp.courtyard.push(s);
                            } else {
                                *fp.graphics.entry(s.layer).or_default() += 1;
                            }
                        }
                    } else if u == Field::type_url() {
                        if let Ok(field) = item.to_msg::<Field>() {
                            fp.texts.extend(field_detail(&Some(field)));
                        }
                    } else if u == BoardText::type_url() {
                        if let Ok(t) = item.to_msg::<BoardText>() {
                            fp.texts.push(text_detail(
                                t.id.as_ref().map(|k| k.value.clone()).unwrap_or_default(),
                                "text",
                                t.layer,
                                &t.text,
                                true,
                                t.locked == 2,
                            ));
                        }
                    }
                }
                detail.footprints.push(fp);
            } else if url == BoardGraphicShape::type_url() {
                let s: BoardGraphicShape = any.to_msg().map_err(|e| bad("drawing", e))?;
                detail.shapes.push(shape_detail(&s));
            } else if url == BoardText::type_url() {
                let t: BoardText = any.to_msg().map_err(|e| bad("text", e))?;
                detail.texts.push(text_detail(
                    t.id.as_ref().map(|k| k.value.clone()).unwrap_or_default(),
                    "text",
                    t.layer,
                    &t.text,
                    true,
                    t.locked == 2,
                ));
            } else if url == BoardTextBox::type_url() {
                let t: BoardTextBox = any.to_msg().map_err(|e| bad("text box", e))?;
                let tb = t.textbox.clone().unwrap_or_default();
                let mut d = text_detail(
                    t.id.as_ref().map(|k| k.value.clone()).unwrap_or_default(),
                    "text_box",
                    t.layer,
                    &Some(Text {
                        position: tb.top_left,
                        attributes: tb.attributes,
                        text: tb.text,
                        ..Default::default()
                    }),
                    true,
                    t.locked == 2,
                );
                d.name = "text_box".into();
                detail.texts.push(d);
            }
        }
        Ok(detail)
    }
}
