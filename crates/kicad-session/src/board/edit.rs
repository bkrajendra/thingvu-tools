// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying board edit plans (Phase 12) to a live board: the outline, mounting holes, and
//! footprints moved, turned, flipped, and locked, all in one commit (one undo step in the editor).
//!
//! KiCad 10.99 moves only a footprint's anchor when `UpdateItems` changes its position or
//! orientation: the pads, drawings, and text inside it are sent back with their old places on the
//! board, and KiCad keeps those. A footprint sent without them loses them. So a move turns and
//! shifts every item of the footprint here first (the Phase 12 probe), and the read-back checks
//! every pad.

use std::time::{Duration, Instant};

use kicad_domain::board::place_point;
use kicad_domain::board_edit::{BoardEditPlan, BoardOp};
use kicad_domain::{ErrorCode, MutationState, Nm, Point, Rect, ToolError};
use kicad_ipc::proto::kiapi::board::commands::{BoardFlipDirection, FlipItems, FlipItemsResponse};
use kicad_ipc::proto::kiapi::board::types::{
    BoardGraphicShape, BoardLayer, BoardText, BoardTextBox, DrillProperties, Field, FieldId, Footprint,
    FootprintAttributes, FootprintInstance, Net, Pad, PadStack, PadStackLayer, PadStackShape, PadStackType, Track, Via,
    ViaType, Zone,
};
use kicad_ipc::proto::kiapi::board::types::{DrillShape, ZoneConnectionSettings, ZoneConnectionStyle};
use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, DeleteItems, EndCommit, GetItems,
    GetItemsFromLibrary, GetItemsResponse, ItemDeletionStatus, ItemStatusCode, LoadAllLibraries, UpdateItems,
    UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    graphic_shape, poly_line_node, Angle, Distance, GraphicAttributes, GraphicRectangleAttributes, GraphicShape,
    ItemHeader, KiCadObjectType, Kiid as ProtoKiid, LibraryIdentifier, LibraryType, LockedState, PolyLine,
    PolyLineNode, PolySet, StrokeAttributes, TextAttributes, Vector2,
};
use kicad_ipc::Client;
use prost::Name;
use prost_types::Any;
use serde::Serialize;

use super::BoardSession;
use crate::errors::from_ipc_error;
use crate::session::Provider;

/// How long a headless server may still be loading footprint libraries before "not found" is final.
const LIBRARY_LOAD_GRACE: Duration = Duration::from_secs(90);

/// What applying a board edit did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EditReport {
    pub removed_edges: usize,
    pub outline_added: bool,
    /// Footprints placed from the library, by reference.
    pub placed: Vec<String>,
    pub moved: Vec<String>,
    pub flipped: Vec<String>,
    /// Footprints whose attributes were set, by reference.
    pub attributes_set: Vec<String>,
    /// Pads given another hole, size, or zone connection, and texts changed, by reference.
    pub pads_edited: Vec<String>,
    pub texts_edited: Vec<String>,
    /// Tracks, vias, and zones created.
    pub copper_items: usize,
    /// Tracks and vias given another width or lock.
    pub copper_changed: usize,
    /// Zones were refilled after the change.
    pub refilled: bool,
}

fn lock_state(locked: bool) -> i32 {
    if locked {
        LockedState::LsLocked as i32
    } else {
        LockedState::LsUnlocked as i32
    }
}

/// A copper zone with every setting given (a zone sent without them has zero clearance).
fn zone_message(z: &kicad_domain::zones::ZoneSpec) -> Result<Zone, ToolError> {
    use kicad_domain::zones::PadConnection;
    use kicad_ipc::proto::kiapi::board::types::{
        zone, CopperZoneSettings, IslandRemovalMode, ThermalSpokeSettings, ZoneConnectionSettings, ZoneConnectionStyle,
        ZoneFillMode, ZoneType,
    };
    use kicad_ipc::proto::kiapi::common::types::PolygonWithHoles;
    Ok(Zone {
        r#type: ZoneType::ZtCopper as i32,
        layers: z
            .layers
            .iter()
            .map(|l| board_layer(l).map(|b| b as i32))
            .collect::<Result<_, _>>()?,
        outline: Some(PolySet {
            polygons: vec![PolygonWithHoles {
                outline: Some(PolyLine {
                    nodes: z
                        .outline
                        .iter()
                        .map(|p| PolyLineNode {
                            geometry: Some(poly_line_node::Geometry::Point(Vector2 {
                                x_nm: p.x.0,
                                y_nm: p.y.0,
                            })),
                        })
                        .collect(),
                    closed: true,
                }),
                holes: vec![],
            }],
        }),
        name: z.name.clone(),
        priority: z.priority,
        settings: Some(zone::Settings::CopperSettings(CopperZoneSettings {
            net: Some(Net {
                name: z.net.clone(),
                ..Default::default()
            }),
            clearance: Some(Distance {
                value_nm: z.clearance.0,
            }),
            min_thickness: Some(Distance {
                value_nm: z.min_width.0,
            }),
            connection: Some(ZoneConnectionSettings {
                zone_connection: match z.connection {
                    PadConnection::Thermal => ZoneConnectionStyle::ZcsThermal,
                    PadConnection::Solid => ZoneConnectionStyle::ZcsFull,
                    PadConnection::None => ZoneConnectionStyle::ZcsNone,
                } as i32,
                thermal_spokes: Some(ThermalSpokeSettings {
                    width: Some(Distance {
                        value_nm: z.spoke_width.0,
                    }),
                    gap: Some(Distance {
                        value_nm: z.thermal_gap.0,
                    }),
                    ..Default::default()
                }),
            }),
            island_mode: IslandRemovalMode::IrmAlways as i32,
            fill_mode: ZoneFillMode::ZfmSolid as i32,
            ..Default::default()
        })),
        ..Default::default()
    })
}

/// `F.Cu` to `BL_F_Cu`.
fn board_layer(name: &str) -> Result<BoardLayer, ToolError> {
    BoardLayer::from_str_name(&format!("BL_{}", name.replace('.', "_")))
        .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgument, format!("unknown layer {name}")))
}

fn kerr(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::KicadError, message)
}

fn vector(p: Point) -> Option<Vector2> {
    Some(Vector2 {
        x_nm: p.x.0,
        y_nm: p.y.0,
    })
}

fn point(v: &Vector2) -> Point {
    Point::new(Nm(v.x_nm), Nm(v.y_nm))
}

/// The kind and bounding box of a board drawing, for the outline list.
pub(super) fn shape_bounds(shape: &GraphicShape) -> (&'static str, Option<Rect>) {
    let pts = |list: &[&Option<Vector2>]| Rect::from_points(list.iter().filter_map(|v| v.as_ref()).map(point));
    match &shape.geometry {
        Some(graphic_shape::Geometry::Segment(s)) => ("segment", pts(&[&s.start, &s.end])),
        Some(graphic_shape::Geometry::Rectangle(r)) => ("rectangle", pts(&[&r.top_left, &r.bottom_right])),
        Some(graphic_shape::Geometry::Arc(a)) => ("arc", pts(&[&a.start, &a.mid, &a.end])),
        Some(graphic_shape::Geometry::Circle(c)) => {
            let (Some(centre), Some(edge)) = (&c.center, &c.radius_point) else {
                return ("circle", None);
            };
            let r = ((edge.x_nm - centre.x_nm) as f64)
                .hypot((edge.y_nm - centre.y_nm) as f64)
                .round() as i64;
            let c = point(centre);
            (
                "circle",
                Rect::from_points([c - Point::new(Nm(r), Nm(r)), c + Point::new(Nm(r), Nm(r))]),
            )
        }
        Some(graphic_shape::Geometry::Polygon(p)) => (
            "polygon",
            Rect::from_points(p.polygons.iter().flat_map(|poly| {
                poly.outline
                    .iter()
                    .flat_map(|l| l.nodes.iter())
                    .flat_map(|n| match &n.geometry {
                        Some(poly_line_node::Geometry::Point(v)) => vec![point(v)],
                        Some(poly_line_node::Geometry::Arc(a)) => [&a.start, &a.mid, &a.end]
                            .iter()
                            .filter_map(|v| v.as_ref())
                            .map(point)
                            .collect(),
                        None => vec![],
                    })
            })),
        ),
        _ => ("other", None),
    }
}

/// A rigid move of everything in a footprint: from the anchor `from` to `to`, turned by `deg`
/// (counter-clockwise on screen) about the anchor.
struct Rigid {
    from: Point,
    to: Point,
    deg: f64,
}

impl Rigid {
    fn right_angle(&self) -> bool {
        let r = self.deg.rem_euclid(90.0);
        r < 1e-9 || 90.0 - r < 1e-9
    }

    fn point(&self, v: &mut Option<Vector2>) {
        if let Some(v) = v.as_mut() {
            let q = place_point(self.to, self.deg, point(v) - self.from);
            v.x_nm = q.x.0;
            v.y_nm = q.y.0;
        }
    }

    fn angle(&self, a: &mut Option<Angle>) {
        if self.deg == 0.0 {
            return;
        }
        let a = a.get_or_insert_with(Angle::default);
        a.value_degrees = kicad_domain::board_edit::normalize_deg(a.value_degrees + self.deg);
    }

    fn text(&self, t: &mut Option<kicad_ipc::proto::kiapi::common::types::Text>) {
        if let Some(t) = t.as_mut() {
            self.point(&mut t.position);
            if let Some(attrs) = t.attributes.as_mut() {
                self.angle(&mut attrs.angle);
            }
        }
    }

    fn board_text(&self, t: &mut Option<BoardText>) {
        if let Some(t) = t.as_mut() {
            self.text(&mut t.text);
        }
    }

    fn field(&self, f: &mut Option<Field>) {
        if let Some(f) = f.as_mut() {
            self.board_text(&mut f.text);
        }
    }

    fn line(&self, l: &mut PolyLine) {
        for n in &mut l.nodes {
            match &mut n.geometry {
                Some(poly_line_node::Geometry::Point(v)) => {
                    let mut o = Some(*v);
                    self.point(&mut o);
                    *v = o.expect("set");
                }
                Some(poly_line_node::Geometry::Arc(a)) => {
                    self.point(&mut a.start);
                    self.point(&mut a.mid);
                    self.point(&mut a.end);
                }
                None => {}
            }
        }
    }

    fn poly(&self, p: &mut PolySet) {
        for poly in &mut p.polygons {
            if let Some(o) = poly.outline.as_mut() {
                self.line(o);
            }
            for h in &mut poly.holes {
                self.line(h);
            }
        }
    }

    fn shape(&self, s: &mut GraphicShape) {
        match &mut s.geometry {
            Some(graphic_shape::Geometry::Segment(g)) => {
                self.point(&mut g.start);
                self.point(&mut g.end);
            }
            Some(graphic_shape::Geometry::Rectangle(g)) => {
                if self.right_angle() {
                    self.point(&mut g.top_left);
                    self.point(&mut g.bottom_right);
                    // Keep the corners in order after a turn.
                    if let (Some(a), Some(b)) = (g.top_left.as_mut(), g.bottom_right.as_mut()) {
                        let (x0, x1) = (a.x_nm.min(b.x_nm), a.x_nm.max(b.x_nm));
                        let (y0, y1) = (a.y_nm.min(b.y_nm), a.y_nm.max(b.y_nm));
                        (a.x_nm, a.y_nm, b.x_nm, b.y_nm) = (x0, y0, x1, y1);
                    }
                } else {
                    // A rectangle turned by another angle is a polygon, as in KiCad.
                    let (Some(a), Some(b)) = (g.top_left, g.bottom_right) else {
                        return;
                    };
                    let corners = [(a.x_nm, a.y_nm), (b.x_nm, a.y_nm), (b.x_nm, b.y_nm), (a.x_nm, b.y_nm)];
                    let mut line = PolyLine {
                        nodes: corners
                            .iter()
                            .map(|(x, y)| PolyLineNode {
                                geometry: Some(poly_line_node::Geometry::Point(Vector2 { x_nm: *x, y_nm: *y })),
                            })
                            .collect(),
                        closed: true,
                    };
                    self.line(&mut line);
                    s.geometry = Some(graphic_shape::Geometry::Polygon(PolySet {
                        polygons: vec![kicad_ipc::proto::kiapi::common::types::PolygonWithHoles {
                            outline: Some(line),
                            holes: vec![],
                        }],
                    }));
                }
            }
            Some(graphic_shape::Geometry::Arc(g)) => {
                self.point(&mut g.start);
                self.point(&mut g.mid);
                self.point(&mut g.end);
            }
            Some(graphic_shape::Geometry::Circle(g)) => {
                self.point(&mut g.center);
                self.point(&mut g.radius_point);
            }
            Some(graphic_shape::Geometry::Polygon(g)) => self.poly(g),
            Some(graphic_shape::Geometry::Bezier(g)) => {
                self.point(&mut g.start);
                self.point(&mut g.control1);
                self.point(&mut g.control2);
                self.point(&mut g.end);
            }
            Some(graphic_shape::Geometry::Ellipse(g)) => {
                self.point(&mut g.center);
                self.angle(&mut g.rotation);
            }
            Some(graphic_shape::Geometry::EllipseArc(g)) => {
                self.point(&mut g.center);
                self.angle(&mut g.rotation);
            }
            None => {}
        }
    }

    /// Moves the footprint and everything in it. Refuses a footprint holding an item it cannot
    /// move, rather than leave that item behind.
    fn footprint(&self, f: &mut FootprintInstance, reference: &str) -> Result<(), ToolError> {
        f.position = vector(self.to);
        self.angle(&mut f.orientation);
        for field in [
            &mut f.reference_field,
            &mut f.value_field,
            &mut f.datasheet_field,
            &mut f.description_field,
        ] {
            self.field(field);
        }
        let Some(d) = f.definition.as_mut() else {
            return Ok(());
        };
        for field in [
            &mut d.reference_field,
            &mut d.value_field,
            &mut d.datasheet_field,
            &mut d.description_field,
        ] {
            self.field(field);
        }
        let encode = |e: prost::EncodeError| kerr(e.to_string());
        let decode = |e: prost::DecodeError| kerr(e.to_string());
        for item in &mut d.items {
            let url = item.type_url.as_str();
            if url == Pad::type_url() {
                let mut p: Pad = item.to_msg().map_err(decode)?;
                self.point(&mut p.position);
                if let Some(stack) = p.pad_stack.as_mut() {
                    self.angle(&mut stack.angle);
                }
                *item = Any::from_msg(&p).map_err(encode)?;
            } else if url == BoardGraphicShape::type_url() {
                let mut s: BoardGraphicShape = item.to_msg().map_err(decode)?;
                if let Some(g) = s.shape.as_mut() {
                    self.shape(g);
                }
                *item = Any::from_msg(&s).map_err(encode)?;
            } else if url == BoardText::type_url() {
                let mut t: BoardText = item.to_msg().map_err(decode)?;
                self.text(&mut t.text);
                *item = Any::from_msg(&t).map_err(encode)?;
            } else if url == Field::type_url() {
                let mut t: Field = item.to_msg().map_err(decode)?;
                self.board_text(&mut t.text);
                *item = Any::from_msg(&t).map_err(encode)?;
            } else if url == BoardTextBox::type_url() {
                if !self.right_angle() {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("{reference} has a text box, which can only be turned by a multiple of 90°"),
                    ));
                }
                let mut t: BoardTextBox = item.to_msg().map_err(decode)?;
                if let Some(b) = t.textbox.as_mut() {
                    self.point(&mut b.top_left);
                    self.point(&mut b.bottom_right);
                    if let (Some(a), Some(c)) = (b.top_left.as_mut(), b.bottom_right.as_mut()) {
                        let (x0, x1) = (a.x_nm.min(c.x_nm), a.x_nm.max(c.x_nm));
                        let (y0, y1) = (a.y_nm.min(c.y_nm), a.y_nm.max(c.y_nm));
                        (a.x_nm, a.y_nm, c.x_nm, c.y_nm) = (x0, y0, x1, y1);
                    }
                    if let Some(attrs) = b.attributes.as_mut() {
                        self.angle(&mut attrs.angle);
                    }
                }
                *item = Any::from_msg(&t).map_err(encode)?;
            } else if url == Zone::type_url() {
                let mut z: Zone = item.to_msg().map_err(decode)?;
                if let Some(o) = z.outline.as_mut() {
                    self.poly(o);
                }
                for fill in &mut z.filled_polygons {
                    if let Some(s) = fill.shapes.as_mut() {
                        self.poly(s);
                    }
                }
                *item = Any::from_msg(&z).map_err(encode)?;
            } else if url.ends_with(".Footprint3DModel") {
                // Its offset and turn are the footprint's own, so it moves with the anchor.
            } else {
                return Err(ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    format!(
                        "{reference} holds an item this tool cannot move ({}); move it in KiCad",
                        url.rsplit('.').next().unwrap_or(url)
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn status_ok(status: &Option<kicad_ipc::proto::kiapi::common::commands::ItemStatus>) -> Result<(), String> {
    match status {
        Some(s) if s.code == ItemStatusCode::IscOk as i32 => Ok(()),
        Some(s) => Err(s.error_message.clone()),
        None => Err("no status".into()),
    }
}

/// A footprint instance at the origin, front side, from a library definition, with its reference
/// on the silkscreen above the courtyard and its value on the fabrication layer below (the
/// library's own field places are not in the definition KiCad returns).
fn new_instance(definition: Footprint, reference: &str, value: &str, reference_below: bool) -> FootprintInstance {
    let courtyard = Rect::from_points(
        definition
            .items
            .iter()
            .filter(|i| i.type_url == BoardGraphicShape::type_url())
            .filter_map(|i| i.to_msg::<BoardGraphicShape>().ok())
            .filter(|s| s.layer == BoardLayer::BlFCrtYd as i32)
            .filter_map(|s| s.shape.as_ref().and_then(|g| shape_bounds(g).1))
            .flat_map(|r| [r.min, r.max]),
    )
    .unwrap_or(Rect {
        min: Point::new(Nm(-1_000_000), Nm(-1_000_000)),
        max: Point::new(Nm(1_000_000), Nm(1_000_000)),
    });
    let (above, below) = (courtyard.min.y - Nm(1_000_000), courtyard.max.y + Nm(1_000_000));
    let field = |id: i32, name: &str, text: &str, y: Nm, layer: BoardLayer| {
        Some(Field {
            id: Some(FieldId { id }),
            name: name.into(),
            text: Some(BoardText {
                text: Some(kicad_ipc::proto::kiapi::common::types::Text {
                    position: vector(Point::new(Nm(0), y)),
                    attributes: Some(TextAttributes {
                        size: Some(Vector2 {
                            x_nm: 1_000_000,
                            y_nm: 1_000_000,
                        }),
                        stroke_width: Some(Distance { value_nm: 150_000 }),
                        visible: true,
                        angle: Some(Angle { value_degrees: 0.0 }),
                        ..Default::default()
                    }),
                    text: text.into(),
                    ..Default::default()
                }),
                layer: layer as i32,
                ..Default::default()
            }),
            visible: true,
            ..Default::default()
        })
    };
    FootprintInstance {
        position: vector(Point::default()),
        orientation: Some(Angle { value_degrees: 0.0 }),
        layer: BoardLayer::BlFCu as i32,
        locked: LockedState::LsUnlocked as i32,
        reference_field: field(
            0,
            "Reference",
            reference,
            if reference_below { below } else { above },
            BoardLayer::BlFSilkS,
        ),
        value_field: field(
            1,
            "Value",
            value,
            if reference_below { above } else { below },
            BoardLayer::BlFFab,
        ),
        definition: Some(definition),
        // As the library marks a mounting hole: no position file or BOM line, and no part in the
        // schematic (so an update from the schematic keeps it).
        attributes: Some(FootprintAttributes {
            not_in_schematic: true,
            exclude_from_position_files: true,
            exclude_from_bill_of_materials: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn footprints(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
) -> Result<Vec<FootprintInstance>, ToolError> {
    let r: GetItemsResponse = client
        .call(&GetItems {
            header: header(),
            types: vec![KiCadObjectType::KotPcbFootprint as i32],
        })
        .await
        .map_err(|e| from_ipc_error(&e, false))?;
    r.items
        .iter()
        .filter(|a| a.type_url == FootprintInstance::type_url())
        .map(|a| a.to_msg::<FootprintInstance>().map_err(|e| kerr(e.to_string())))
        .collect()
}

async fn update(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    items: Vec<FootprintInstance>,
) -> Result<(), ToolError> {
    if items.is_empty() {
        return Ok(());
    }
    let anys = items
        .iter()
        .map(|f| Any::from_msg(f).map_err(|e| kerr(e.to_string())))
        .collect::<Result<Vec<_>, _>>()?;
    let r: UpdateItemsResponse = client
        .call(&UpdateItems {
            header: header(),
            items: anys,
        })
        .await
        .map_err(|e| from_ipc_error(&e, true))?;
    let failed: Vec<String> = r
        .updated_items
        .iter()
        .filter_map(|u| status_ok(&u.status).err())
        .collect();
    if !failed.is_empty() || r.updated_items.len() != items.len() {
        return Err(kerr(format!(
            "KiCad updated {} of {} footprints: {}",
            r.updated_items.len() - failed.len(),
            items.len(),
            failed.join("; ")
        )));
    }
    Ok(())
}

impl BoardSession {
    /// Applies a board edit plan in one commit. On a failure the commit is dropped, so nothing
    /// stays changed; the error says whether that worked.
    pub async fn apply_edit(&self, plan: &BoardEditPlan) -> Result<EditReport, ToolError> {
        let ipc = self.ipc.as_ref().ok_or_else(|| {
            ToolError::new(
                ErrorCode::FeatureUnavailable,
                "editing a board needs a live KiCad 11 session (headless or gui); a file session only reads",
            )
        })?;
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let header = || {
            Some(ItemHeader {
                document: Some(document.clone()),
                ..Default::default()
            })
        };
        let places = plan
            .operations
            .iter()
            .any(|o| matches!(o, BoardOp::PlaceFootprint { .. }));
        if places && self.provider == Provider::Headless {
            let _ = ipc
                .client
                .call_any(&LoadAllLibraries {
                    r#type: vec![LibraryType::LtFootprint as i32],
                })
                .await;
        }
        let client = &mut ipc.client;
        let commit: BeginCommitResponse = client
            .call(&BeginCommit { header: header() })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let message = match plan.kind {
            "outline" => "AI: board outline",
            "place" => "AI: place footprints",
            "route" => "AI: route",
            "zone" => "AI: copper pour",
            "attributes" => "AI: footprint attributes",
            "footprint" => "AI: footprint pads and text",
            "cleanup" => "AI: remove dangling vias",
            "tracks" => "AI: edit tracks",
            _ => "AI: move footprints",
        };
        let result = apply_steps(client, &header, plan).await;
        let action = if result.is_ok() {
            CommitAction::CmaCommit
        } else {
            CommitAction::CmaDrop
        };
        let ended = client
            .call_any(&EndCommit {
                id: commit.id,
                action: action as i32,
                message: message.into(),
                header: header(),
            })
            .await;
        let zones_changed = plan.operations.iter().any(|o| {
            matches!(
                o,
                BoardOp::AddZone(_) | BoardOp::RemoveZone { .. } | BoardOp::EditPad { .. }
            )
        });
        match (result, ended) {
            (Ok(mut report), Ok(_)) if zones_changed => {
                // After the commit: a refill inside it does not fill a zone made in it.
                client
                    .call_any(&kicad_ipc::proto::kiapi::board::commands::RefillZones {
                        board: Some(document.clone()),
                        zones: vec![],
                    })
                    .await
                    .map_err(|e| {
                        ToolError::new(
                            ErrorCode::KicadError,
                            format!("the zones were created but the refill failed ({e}); refill them in KiCad (B)"),
                        )
                        .with_mutation_state(MutationState::Applied)
                    })?;
                report.refilled = true;
                Ok(report)
            }
            (Ok(report), Ok(_)) => Ok(report),
            (Ok(_), Err(e)) => Err(from_ipc_error(&e, true)),
            (Err(err), Ok(_)) => Err(ToolError::new(
                err.code,
                format!("{} (the change was dropped; nothing changed)", err.message),
            )
            .with_mutation_state(MutationState::NotApplied)),
            (Err(err), Err(_)) => Err(ToolError::new(
                err.code,
                format!("{} (dropping the change failed too; read the board back)", err.message),
            )
            .with_mutation_state(MutationState::Unknown)),
        }
    }
}

/// Sets the hole, copper size, and zone connection on every pad of `f` numbered `number`.
fn edit_pads(
    f: &mut FootprintInstance,
    reference: &str,
    number: &str,
    count: usize,
    set: &kicad_domain::footprint_edit::PadValues,
) -> Result<(), ToolError> {
    let def = f
        .definition
        .as_mut()
        .ok_or_else(|| kerr(format!("KiCad sent {reference} without its pads")))?;
    let mut found = 0;
    for item in def.items.iter_mut() {
        if item.type_url != Pad::type_url() {
            continue;
        }
        let mut pad: Pad = item.to_msg().map_err(|e| kerr(e.to_string()))?;
        if pad.number != number {
            continue;
        }
        found += 1;
        let stack = pad.pad_stack.get_or_insert_with(Default::default);
        if let Some((w, h)) = set.drill {
            let drill = stack.drill.get_or_insert_with(Default::default);
            drill.diameter = Some(Vector2 { x_nm: w.0, y_nm: h.0 });
            drill.shape = if w == h {
                DrillShape::DsCircle
            } else {
                DrillShape::DsOblong
            } as i32;
        }
        if let Some((w, h)) = set.size {
            for layer in stack.copper_layers.iter_mut() {
                layer.size = Some(Vector2 { x_nm: w.0, y_nm: h.0 });
            }
        }
        if let Some(z) = &set.zone_connection {
            let style = match z.as_str() {
                "none" => ZoneConnectionStyle::ZcsNone,
                "thermal" => ZoneConnectionStyle::ZcsThermal,
                "full" => ZoneConnectionStyle::ZcsFull,
                "pth_thermal" => ZoneConnectionStyle::ZcsPthThermal,
                _ => ZoneConnectionStyle::ZcsInherited,
            };
            stack
                .zone_settings
                .get_or_insert_with(ZoneConnectionSettings::default)
                .zone_connection = style as i32;
        }
        *item = Any::from_msg(&pad).map_err(|e| kerr(e.to_string()))?;
    }
    if found != count {
        return Err(ToolError::new(
            ErrorCode::StalePlan,
            format!("{reference} has {found} pad(s) numbered {number}, the plan found {count}; make a new plan"),
        ));
    }
    Ok(())
}

/// Sets the layer, size, place, turn, and visibility of a footprint's reference or value text.
fn edit_text(
    f: &mut FootprintInstance,
    reference: &str,
    field: &str,
    to: &kicad_domain::footprint_edit::TextValues,
) -> Result<(), ToolError> {
    let slot = if field == "Reference" {
        &mut f.reference_field
    } else {
        &mut f.value_field
    };
    let field = slot.as_mut().ok_or_else(|| {
        kerr(format!(
            "KiCad sent {reference} without its {} text",
            field.to_lowercase()
        ))
    })?;
    field.visible = to.visible;
    let board_text = field.text.get_or_insert_with(Default::default);
    board_text.layer = board_layer(&to.layer)? as i32;
    let text = board_text.text.get_or_insert_with(Default::default);
    text.position = vector(to.at);
    let attributes = text.attributes.get_or_insert_with(Default::default);
    let old = attributes.size.unwrap_or_default();
    // Width follows height, keeping the text's proportions.
    let width = if old.y_nm > 0 {
        (old.x_nm as f64 * to.height.0 as f64 / old.y_nm as f64).round() as i64
    } else {
        to.height.0
    };
    attributes.size = Some(Vector2 {
        x_nm: width,
        y_nm: to.height.0,
    });
    attributes.stroke_width = Some(Distance {
        value_nm: to.thickness.0,
    });
    attributes.angle = Some(Angle {
        value_degrees: to.angle_deg,
    });
    Ok(())
}

async fn apply_steps(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    plan: &BoardEditPlan,
) -> Result<EditReport, ToolError> {
    let mut report = EditReport::default();

    // 1. The old outline.
    let remove: Vec<ProtoKiid> = plan
        .operations
        .iter()
        .filter_map(|o| match o {
            BoardOp::RemoveEdge { id, .. }
            | BoardOp::RemoveZone { id, .. }
            | BoardOp::RemoveVia { id, .. }
            | BoardOp::RemoveTrack { id, .. } => Some(ProtoKiid { value: id.clone() }),
            _ => None,
        })
        .collect();
    if !remove.is_empty() {
        let n = remove.len();
        let r: kicad_ipc::proto::kiapi::common::commands::DeleteItemsResponse = client
            .call(&DeleteItems {
                header: header(),
                item_ids: remove,
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let failed: Vec<String> = r
            .deleted_items
            .iter()
            .filter(|d| d.status != ItemDeletionStatus::IdsOk as i32)
            .map(|d| {
                format!(
                    "{} ({:?})",
                    d.id.as_ref().map(|k| k.value.as_str()).unwrap_or("?"),
                    d.status()
                )
            })
            .collect();
        if !failed.is_empty() {
            return Err(kerr(format!(
                "KiCad did not delete the items the plan removes: {}",
                failed.join("; ")
            )));
        }
        report.removed_edges = n;
    }

    // 2. The new outline.
    for op in &plan.operations {
        let BoardOp::AddOutline {
            rect,
            corner_radius,
            line_width,
        } = op
        else {
            continue;
        };
        let shape = BoardGraphicShape {
            shape: Some(GraphicShape {
                attributes: Some(GraphicAttributes {
                    stroke: Some(StrokeAttributes {
                        width: Some(Distance { value_nm: line_width.0 }),
                        ..Default::default()
                    }),
                    fill: None,
                }),
                geometry: Some(graphic_shape::Geometry::Rectangle(GraphicRectangleAttributes {
                    top_left: vector(rect.min),
                    bottom_right: vector(rect.max),
                    corner_radius: Some(Distance {
                        value_nm: corner_radius.0,
                    }),
                })),
                ..Default::default()
            }),
            layer: BoardLayer::BlEdgeCuts as i32,
            ..Default::default()
        };
        let r: CreateItemsResponse = client
            .call(&CreateItems {
                header: header(),
                items: vec![Any::from_msg(&shape).map_err(|e| kerr(e.to_string()))?],
                ..Default::default()
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let created = r.created_items.first().map(|c| status_ok(&c.status));
        match created {
            Some(Ok(())) => report.outline_added = true,
            Some(Err(m)) => return Err(kerr(format!("KiCad did not create the outline: {m}"))),
            None => return Err(kerr("KiCad created no outline")),
        }
    }

    // 3. Footprints from the library (mounting holes). A footprint placed with
    // PlaceFootprintFromLibrary cannot be renamed until the commit ends (KiCad does not know it yet),
    // so the footprint is built here from the library's definition, named, moved to its place, and
    // created in the commit like any other item.
    for op in &plan.operations {
        let BoardOp::PlaceFootprint {
            reference,
            lib_id,
            at,
            reference_below,
        } = op
        else {
            continue;
        };
        let (library, name) = lib_id
            .split_once(':')
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgument, format!("{lib_id} is not Library:Name")))?;
        let request = GetItemsFromLibrary {
            r#type: LibraryType::LtFootprint as i32,
            document: header().and_then(|h| h.document),
            item_ids: vec![LibraryIdentifier {
                library_nickname: library.into(),
                entry_name: name.into(),
            }],
        };
        // A headless server answers "not found" while it is still loading the libraries.
        let started = Instant::now();
        let definition: Footprint = loop {
            let answer: Result<GetItemsResponse, _> = client.call(&request).await;
            let found = match &answer {
                Ok(r) => r.items.first().and_then(|a| a.to_msg::<Footprint>().ok()),
                Err(_) => None,
            };
            match (found, answer) {
                (Some(d), _) => break d,
                (None, Err(e)) if !e.to_string().contains("not found") => return Err(from_ipc_error(&e, false)),
                _ if started.elapsed() < LIBRARY_LOAD_GRACE => tokio::time::sleep(Duration::from_secs(2)).await,
                _ => return Err(kerr(format!("KiCad does not find {lib_id} in its libraries"))),
            }
        };
        let mut instance = new_instance(definition, reference, name, *reference_below);
        Rigid {
            from: Point::default(),
            to: *at,
            deg: 0.0,
        }
        .footprint(&mut instance, reference)?;
        let r: CreateItemsResponse = client
            .call(&CreateItems {
                header: header(),
                items: vec![Any::from_msg(&instance).map_err(|e| kerr(e.to_string()))?],
                ..Default::default()
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        match r.created_items.first().map(|c| status_ok(&c.status)) {
            Some(Ok(())) => report.placed.push(reference.clone()),
            Some(Err(m)) => return Err(kerr(format!("KiCad did not create {reference}: {m}"))),
            None => return Err(kerr(format!("KiCad created no {reference}"))),
        }
    }

    // 4. Tracks and vias, in one request. A via needs its pad stack spelled out: one sent without
    // sizes is created with none (the Phase 12 probe).
    let mut copper = Vec::new();
    for op in &plan.operations {
        let encoded = match op {
            BoardOp::AddTrack {
                net,
                start,
                end,
                width,
                layer,
                locked,
            } => Any::from_msg(&Track {
                start: vector(*start),
                end: vector(*end),
                width: Some(Distance { value_nm: width.0 }),
                layer: board_layer(layer)? as i32,
                locked: lock_state(*locked),
                net: Some(Net {
                    name: net.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            BoardOp::AddVia {
                net,
                at,
                diameter,
                drill,
                locked,
            } => Any::from_msg(&Via {
                position: vector(*at),
                locked: lock_state(*locked),
                net: Some(Net {
                    name: net.clone(),
                    ..Default::default()
                }),
                r#type: ViaType::VtThrough as i32,
                pad_stack: Some(PadStack {
                    r#type: PadStackType::PstNormal as i32,
                    layers: vec![BoardLayer::BlFCu as i32, BoardLayer::BlBCu as i32],
                    drill: Some(DrillProperties {
                        start_layer: BoardLayer::BlFCu as i32,
                        end_layer: BoardLayer::BlBCu as i32,
                        diameter: Some(Vector2 {
                            x_nm: drill.0,
                            y_nm: drill.0,
                        }),
                        ..Default::default()
                    }),
                    copper_layers: vec![PadStackLayer {
                        layer: BoardLayer::BlFCu as i32,
                        shape: PadStackShape::PssCircle as i32,
                        size: Some(Vector2 {
                            x_nm: diameter.0,
                            y_nm: diameter.0,
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            BoardOp::AddZone(z) => Any::from_msg(&zone_message(z)?),
            _ => continue,
        };
        copper.push(encoded.map_err(|e| kerr(e.to_string()))?);
    }
    if !copper.is_empty() {
        let n = copper.len();
        let r: CreateItemsResponse = client
            .call(&CreateItems {
                header: header(),
                items: copper,
                ..Default::default()
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let failed: Vec<String> = r
            .created_items
            .iter()
            .filter_map(|c| status_ok(&c.status).err())
            .collect();
        if !failed.is_empty() || r.created_items.len() != n {
            return Err(kerr(format!(
                "KiCad created {} of {n} tracks, vias, and zones: {}",
                r.created_items.len() - failed.len(),
                failed.join("; ")
            )));
        }
        report.copper_items = n;
    }

    // 4a. Tracks and vias already on the board: a new width, a lock. Sent as KiCad has them.
    let changes: Vec<&BoardOp> = plan
        .operations
        .iter()
        .filter(|o| matches!(o, BoardOp::SetTrackWidth { .. } | BoardOp::SetLocked { .. }))
        .collect();
    if !changes.is_empty() {
        let current = other_items(client, header).await?;
        let mut items = Vec::new();
        for op in &changes {
            let (id, kind) = match op {
                BoardOp::SetTrackWidth { id, .. } => (id, "track"),
                BoardOp::SetLocked { id, item, .. } => (id, *item),
                _ => continue,
            };
            let any = current.iter().find(|(i, _)| i == id).map(|(_, a)| a).ok_or_else(|| {
                ToolError::new(
                    ErrorCode::StalePlan,
                    format!("the {kind} {id} is no longer on the board; make a new plan"),
                )
            })?;
            let changed = if any.type_url == Via::type_url() {
                let mut v: Via = any.to_msg().map_err(|e| kerr(e.to_string()))?;
                if let BoardOp::SetLocked { locked, .. } = op {
                    v.locked = lock_state(*locked);
                }
                Any::from_msg(&v)
            } else if any.type_url == Track::type_url() {
                let mut t: Track = any.to_msg().map_err(|e| kerr(e.to_string()))?;
                match op {
                    BoardOp::SetTrackWidth { to, .. } => t.width = Some(Distance { value_nm: to.0 }),
                    BoardOp::SetLocked { locked, .. } => t.locked = lock_state(*locked),
                    _ => {}
                }
                Any::from_msg(&t)
            } else {
                let mut t: kicad_ipc::proto::kiapi::board::types::Arc =
                    any.to_msg().map_err(|e| kerr(e.to_string()))?;
                match op {
                    BoardOp::SetTrackWidth { to, .. } => t.width = Some(Distance { value_nm: to.0 }),
                    BoardOp::SetLocked { locked, .. } => t.locked = lock_state(*locked),
                    _ => {}
                }
                Any::from_msg(&t)
            };
            items.push(changed.map_err(|e| kerr(e.to_string()))?);
        }
        let n = items.len();
        let r: UpdateItemsResponse = client
            .call(&UpdateItems {
                header: header(),
                items,
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let failed: Vec<String> = r
            .updated_items
            .iter()
            .filter_map(|u| status_ok(&u.status).err())
            .collect();
        if !failed.is_empty() || r.updated_items.len() != n {
            return Err(kerr(format!(
                "KiCad updated {} of {n} tracks and vias: {}",
                r.updated_items.len() - failed.len(),
                failed.join("; ")
            )));
        }
        report.copper_changed = n;
    }

    // 4b. Attributes: the footprint is sent as KiCad has it with the flags changed (an update does
    // write them; checked live in Phase 13b).
    let attrs: Vec<(&String, &String, &kicad_domain::board_edit::FootprintFlags)> = plan
        .operations
        .iter()
        .filter_map(|o| match o {
            BoardOp::SetAttributes { reference, id, to, .. } => Some((reference, id, to)),
            _ => None,
        })
        .collect();
    if !attrs.is_empty() {
        let current = footprints(client, header).await?;
        let mut items = Vec::new();
        for (reference, id, to) in &attrs {
            let mut f = current
                .iter()
                .find(|f| f.id.as_ref().is_some_and(|k| &k.value == *id))
                .cloned()
                .ok_or_else(|| {
                    ToolError::new(
                        ErrorCode::StalePlan,
                        format!("{reference} is no longer on the board; make a new plan"),
                    )
                })?;
            let a = f.attributes.get_or_insert_with(Default::default);
            a.do_not_populate = to.dnp;
            a.exclude_from_bill_of_materials = to.exclude_from_bom;
            a.exclude_from_position_files = to.exclude_from_pos;
            a.not_in_schematic = to.board_only;
            items.push(f);
            report.attributes_set.push((*reference).clone());
        }
        update(client, header, items).await?;
    }

    // 4c. Pads and text (Phase 15.2): the footprint is sent as KiCad has it with its pads and fields
    // changed. A pad sent on its own is refused ("not a footprint"); checked live.
    let mut touched: Vec<&String> = Vec::new();
    for o in &plan.operations {
        if let BoardOp::EditPad { id, .. } | BoardOp::EditText { id, .. } = o {
            if !touched.contains(&id) {
                touched.push(id);
            }
        }
    }
    if !touched.is_empty() {
        let current = footprints(client, header).await?;
        let mut items = Vec::new();
        for id in touched {
            let mut f = current
                .iter()
                .find(|f| f.id.as_ref().is_some_and(|k| &k.value == id))
                .cloned()
                .ok_or_else(|| {
                    ToolError::new(
                        ErrorCode::StalePlan,
                        "a footprint in the plan is no longer on the board; make a new plan",
                    )
                })?;
            for o in &plan.operations {
                match o {
                    BoardOp::EditPad {
                        reference,
                        id: i,
                        number,
                        count,
                        set,
                        ..
                    } if i == id => {
                        edit_pads(&mut f, reference, number, *count, set)?;
                        report.pads_edited.push(format!("{reference} pad {number}"));
                    }
                    BoardOp::EditText {
                        reference,
                        id: i,
                        field,
                        to,
                        ..
                    } if i == id => {
                        edit_text(&mut f, reference, field, to)?;
                        report
                            .texts_edited
                            .push(format!("{reference} {}", field.to_lowercase()));
                    }
                    _ => {}
                }
            }
            items.push(f);
        }
        update(client, header, items).await?;
    }

    // 5. Moves: flips first (KiCad's own flip mirrors the footprint top to bottom), then every
    // footprint sent once with all its items turned and shifted, and its lock.
    let moves: Vec<(&String, &String, &kicad_domain::board_edit::Placement, bool)> = plan
        .operations
        .iter()
        .filter_map(|o| match o {
            BoardOp::MoveFootprint {
                reference,
                id,
                to,
                locked,
                ..
            } => Some((reference, id, to, *locked)),
            _ => None,
        })
        .collect();
    if moves.is_empty() {
        return Ok(report);
    }
    let current = footprints(client, header).await?;
    let find = |id: &str| current.iter().find(|f| f.id.as_ref().is_some_and(|k| k.value == id));
    let mut flip_ids = Vec::new();
    for (reference, id, to, _) in &moves {
        let f = find(id).ok_or_else(|| {
            ToolError::new(
                ErrorCode::StalePlan,
                format!("{reference} is no longer on the board; make a new plan"),
            )
        })?;
        let back = BoardLayer::try_from(f.layer).is_ok_and(|l| l == BoardLayer::BlBCu);
        if back != to.back {
            flip_ids.push(ProtoKiid { value: (*id).clone() });
            report.flipped.push((*reference).clone());
        }
    }
    if !flip_ids.is_empty() {
        let r: FlipItemsResponse = client
            .call(&FlipItems {
                header: header(),
                items: flip_ids,
                direction: BoardFlipDirection::BfdTopBottom as i32,
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let failed: Vec<String> = r
            .flipped_items
            .iter()
            .filter_map(|f| status_ok(&f.status).err())
            .collect();
        if !failed.is_empty() {
            return Err(kerr(format!(
                "KiCad did not flip every footprint: {}",
                failed.join("; ")
            )));
        }
    }
    let current = if report.flipped.is_empty() {
        current
    } else {
        footprints(client, header).await?
    };
    let mut items = Vec::new();
    for (reference, id, to, locked) in &moves {
        let mut f = current
            .iter()
            .find(|f| f.id.as_ref().is_some_and(|k| &k.value == *id))
            .cloned()
            .ok_or_else(|| kerr(format!("{reference} disappeared during the move")))?;
        let from = f.position.as_ref().map(point).unwrap_or_default();
        let deg = to.rotation_deg - f.orientation.as_ref().map(|a| a.value_degrees).unwrap_or(0.0);
        let rigid = Rigid {
            from,
            to: to.at,
            deg: if deg.abs() < 1e-9 { 0.0 } else { deg },
        };
        rigid.footprint(&mut f, reference)?;
        f.orientation = Some(Angle {
            value_degrees: to.rotation_deg,
        });
        f.locked = if *locked {
            LockedState::LsLocked
        } else {
            LockedState::LsUnlocked
        } as i32;
        items.push(f);
        report.moved.push((*reference).clone());
    }
    update(client, header, items).await?;
    Ok(report)
}

impl BoardSession {
    /// The rules routing follows: each net's class (clearance, track width, via), and the board's
    /// minimums (track width, copper to edge), from the live board.
    pub async fn routing_rules(&self, nets: &[String]) -> Result<kicad_domain::routing::BoardRules, ToolError> {
        use kicad_domain::routing::{BoardRules, NetRules};
        use kicad_ipc::proto::kiapi::board::commands::{
            BoardDesignRulesResponse, GetBoardDesignRules, GetNetClassForNets, NetClassForNetsResponse,
        };
        let ipc = self.ipc.as_ref().ok_or_else(|| {
            ToolError::new(
                ErrorCode::FeatureUnavailable,
                "routing needs a live KiCad 11 session (headless or gui)",
            )
        })?;
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let classes: NetClassForNetsResponse = ipc
            .client
            .call(&GetNetClassForNets {
                net: nets
                    .iter()
                    .map(|n| Net {
                        name: n.clone(),
                        ..Default::default()
                    })
                    .collect(),
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let rules: BoardDesignRulesResponse = ipc
            .client
            .call(&GetBoardDesignRules { board: Some(document) })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let defaults = NetRules::default();
        let mut out = BoardRules::default();
        for (net, class) in &classes.classes {
            let Some(b) = class.board.as_ref() else {
                continue;
            };
            let via = b.via_stack.as_ref();
            out.nets.insert(
                net.clone(),
                NetRules {
                    clearance: b.clearance.map(|d| Nm(d.value_nm)).unwrap_or(defaults.clearance),
                    track_width: b.track_width.map(|d| Nm(d.value_nm)).unwrap_or(defaults.track_width),
                    via_diameter: via
                        .and_then(|v| v.copper_layers.first())
                        .and_then(|l| l.size)
                        .map(|s| Nm(s.x_nm))
                        .filter(|n| n.0 > 0)
                        .unwrap_or(defaults.via_diameter),
                    via_drill: via
                        .and_then(|v| v.drill.as_ref())
                        .and_then(|d| d.diameter)
                        .map(|s| Nm(s.x_nm))
                        .filter(|n| n.0 > 0)
                        .unwrap_or(defaults.via_drill),
                },
            );
        }
        if let Some(c) = rules.rules.and_then(|r| r.constraints) {
            out.copper_edge_clearance = Nm(c.copper_edge_clearance.map(|d| d.value_nm).unwrap_or(500_000));
            out.min_track_width = Nm(c.min_track_width.map(|d| d.value_nm).unwrap_or(0));
            out.hole_clearance = Nm(c.hole_clearance.map(|d| d.value_nm).unwrap_or(0));
            // A board minimum clearance above a class's wins.
            let min = c.min_clearance.map(|d| d.value_nm).unwrap_or(0);
            for r in out.nets.values_mut() {
                r.clearance = Nm(r.clearance.0.max(min));
            }
            out.default.clearance = Nm(out.default.clearance.0.max(min));
        }
        Ok(out)
    }
}

/// What importing a routed session did.
#[derive(Debug, Clone, Serialize)]
pub struct SessionImport {
    /// Session items KiCad could not place (unknown reference, layer, or padstack).
    pub skipped: u32,
    pub report: String,
}

impl BoardSession {
    /// Imports a routed Specctra session: KiCad deletes the unlocked tracks and vias and adds the
    /// session's. KiCad makes it one change of its own.
    pub async fn import_session(&self, path: &std::path::Path) -> Result<SessionImport, ToolError> {
        use kicad_ipc::proto::kiapi::board::commands::{ImportSpecctraSession, ImportSpecctraSessionResponse};
        let ipc = self.ipc.as_ref().ok_or_else(|| {
            ToolError::new(
                ErrorCode::FeatureUnavailable,
                "autorouting needs a live KiCad 11 session (headless or gui)",
            )
        })?;
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let r: ImportSpecctraSessionResponse = ipc
            .client
            .call(&ImportSpecctraSession {
                board: Some(document),
                session_path: path.display().to_string(),
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        Ok(SessionImport {
            skipped: r.skipped_count,
            report: r.report,
        })
    }
}

/// The board's items as KiCad sent them, to put the board back after a change that failed its
/// checks (Phase 12.8). The API has no undo, so a rollback is a change of its own: what was added
/// is deleted, what was removed is created again from these copies, and every footprint that moved,
/// turned, or flipped is flipped back and sent as it was (with all its items at their old places).
pub struct BoardSnapshot {
    footprints: Vec<FootprintInstance>,
    /// Tracks, arcs, vias, zones, and drawings, by id.
    others: Vec<(String, Any)>,
}

/// What a rollback did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RestoreReport {
    pub deleted: usize,
    pub recreated: usize,
    pub footprints_restored: usize,
}

fn any_id(a: &Any) -> Option<String> {
    let url = a.type_url.as_str();
    let id = if url == Track::type_url() {
        a.to_msg::<Track>().ok()?.id
    } else if url == kicad_ipc::proto::kiapi::board::types::Arc::type_url() {
        a.to_msg::<kicad_ipc::proto::kiapi::board::types::Arc>().ok()?.id
    } else if url == Via::type_url() {
        a.to_msg::<Via>().ok()?.id
    } else if url == Zone::type_url() {
        a.to_msg::<Zone>().ok()?.id
    } else if url == BoardGraphicShape::type_url() {
        a.to_msg::<BoardGraphicShape>().ok()?.id
    } else if url == BoardText::type_url() {
        a.to_msg::<BoardText>().ok()?.id
    } else {
        None
    };
    id.map(|k| k.value)
}

async fn other_items(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
) -> Result<Vec<(String, Any)>, ToolError> {
    let r: GetItemsResponse = client
        .call(&GetItems {
            header: header(),
            types: [
                KiCadObjectType::KotPcbTrace,
                KiCadObjectType::KotPcbArc,
                KiCadObjectType::KotPcbVia,
                KiCadObjectType::KotPcbZone,
                KiCadObjectType::KotPcbShape,
                KiCadObjectType::KotPcbText,
            ]
            .map(|t| t as i32)
            .to_vec(),
        })
        .await
        .map_err(|e| from_ipc_error(&e, false))?;
    Ok(r.items
        .into_iter()
        .filter_map(|a| any_id(&a).map(|id| (id, a)))
        .collect())
}

impl BoardSession {
    /// Records the board's items before a change.
    pub async fn snapshot(&self) -> Result<BoardSnapshot, ToolError> {
        let ipc = self
            .ipc
            .as_ref()
            .ok_or_else(|| ToolError::new(ErrorCode::FeatureUnavailable, "a snapshot needs a live session"))?;
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let header = || {
            Some(ItemHeader {
                document: Some(document.clone()),
                ..Default::default()
            })
        };
        let client = &mut ipc.client;
        Ok(BoardSnapshot {
            footprints: footprints(client, &header).await?,
            others: other_items(client, &header).await?,
        })
    }

    /// Puts the board back as `snap` recorded it, in one change, and refills the zones.
    pub async fn restore(&self, snap: &BoardSnapshot) -> Result<RestoreReport, ToolError> {
        let ipc = self
            .ipc
            .as_ref()
            .ok_or_else(|| ToolError::new(ErrorCode::FeatureUnavailable, "a rollback needs a live session"))?;
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let header = || {
            Some(ItemHeader {
                document: Some(document.clone()),
                ..Default::default()
            })
        };
        let client = &mut ipc.client;
        let commit: BeginCommitResponse = client
            .call(&BeginCommit { header: header() })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let result = restore_steps(client, &header, snap).await;
        let action = if result.is_ok() {
            CommitAction::CmaCommit
        } else {
            CommitAction::CmaDrop
        };
        let ended = client
            .call_any(&EndCommit {
                id: commit.id,
                action: action as i32,
                message: "AI: roll back".into(),
                header: header(),
            })
            .await;
        let report = result?;
        ended.map_err(|e| from_ipc_error(&e, true))?;
        if snap.others.iter().any(|(_, a)| a.type_url == Zone::type_url()) {
            let _ = client
                .call_any(&kicad_ipc::proto::kiapi::board::commands::RefillZones {
                    board: Some(document.clone()),
                    zones: vec![],
                })
                .await;
        }
        Ok(report)
    }
}

async fn restore_steps(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    snap: &BoardSnapshot,
) -> Result<RestoreReport, ToolError> {
    let mut report = RestoreReport::default();
    let now_fps = footprints(client, header).await?;
    let now_others = other_items(client, header).await?;
    let fp_id = |f: &FootprintInstance| f.id.as_ref().map(|k| k.value.clone()).unwrap_or_default();
    let was_fp: std::collections::BTreeSet<String> = snap.footprints.iter().map(fp_id).collect();
    let was_other: std::collections::BTreeSet<&str> = snap.others.iter().map(|(id, _)| id.as_str()).collect();
    let now_other: std::collections::BTreeSet<&str> = now_others.iter().map(|(id, _)| id.as_str()).collect();

    // Added since: delete.
    let mut delete: Vec<ProtoKiid> = now_fps
        .iter()
        .map(fp_id)
        .filter(|id| !was_fp.contains(id))
        .map(|value| ProtoKiid { value })
        .collect();
    delete.extend(
        now_others
            .iter()
            .filter(|(id, _)| !was_other.contains(id.as_str()))
            .map(|(id, _)| ProtoKiid { value: id.clone() }),
    );
    if !delete.is_empty() {
        report.deleted = delete.len();
        let r: kicad_ipc::proto::kiapi::common::commands::DeleteItemsResponse = client
            .call(&DeleteItems {
                header: header(),
                item_ids: delete,
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        if r.deleted_items
            .iter()
            .any(|d| d.status != ItemDeletionStatus::IdsOk as i32)
        {
            return Err(kerr("KiCad did not delete everything the change added"));
        }
    }
    // Removed since: create again from the copies.
    let recreate: Vec<Any> = snap
        .others
        .iter()
        .filter(|(id, _)| !now_other.contains(id.as_str()))
        .map(|(_, a)| a.clone())
        .collect();
    if !recreate.is_empty() {
        report.recreated = recreate.len();
        let n = recreate.len();
        let r: CreateItemsResponse = client
            .call(&CreateItems {
                header: header(),
                items: recreate,
                ..Default::default()
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let failed: Vec<String> = r
            .created_items
            .iter()
            .filter_map(|c| status_ok(&c.status).err())
            .collect();
        if !failed.is_empty() || r.created_items.len() != n {
            return Err(kerr(format!(
                "KiCad did not recreate the removed items: {}",
                failed.join("; ")
            )));
        }
    }
    // Tracks and vias changed in place (width, lock): sent as they were.
    let edited: Vec<Any> = snap
        .others
        .iter()
        .filter(|(id, was)| {
            now_others.iter().any(|(n, now)| {
                n == id
                    && now.value != was.value
                    && (was.type_url == Track::type_url()
                        || was.type_url == Via::type_url()
                        || was.type_url == kicad_ipc::proto::kiapi::board::types::Arc::type_url())
            })
        })
        .map(|(_, a)| a.clone())
        .collect();
    if !edited.is_empty() {
        report.recreated += edited.len();
        client
            .call::<_, UpdateItemsResponse>(&UpdateItems {
                header: header(),
                items: edited,
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
    }
    // Footprints that changed: flipped back first, then sent as they were.
    let changed: Vec<&FootprintInstance> = snap
        .footprints
        .iter()
        .filter(|was| {
            now_fps.iter().find(|n| fp_id(n) == fp_id(was)).is_some_and(|now| {
                now.position != was.position
                    || now.layer != was.layer
                    || now.locked != was.locked
                    || now.attributes != was.attributes
                    || now.definition != was.definition
                    || now.reference_field != was.reference_field
                    || now.value_field != was.value_field
                    || now.orientation.map(|a| a.value_degrees) != was.orientation.map(|a| a.value_degrees)
            })
        })
        .collect();
    let flip: Vec<ProtoKiid> = changed
        .iter()
        .filter(|was| now_fps.iter().any(|n| fp_id(n) == fp_id(was) && n.layer != was.layer))
        .map(|was| ProtoKiid { value: fp_id(was) })
        .collect();
    if !flip.is_empty() {
        let r: FlipItemsResponse = client
            .call(&FlipItems {
                header: header(),
                items: flip,
                direction: BoardFlipDirection::BfdTopBottom as i32,
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        if r.flipped_items.iter().any(|f| status_ok(&f.status).is_err()) {
            return Err(kerr("KiCad did not flip a footprint back"));
        }
    }
    report.footprints_restored = changed.len();
    update(client, header, changed.into_iter().cloned().collect()).await?;
    Ok(report)
}
