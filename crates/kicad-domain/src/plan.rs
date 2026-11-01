// SPDX-License-Identifier: GPL-3.0-or-later
//! Operation plans: the exact changes a circuit spec turns into, computed before anything is
//! applied (plan §7.2, §8.3). Geometry comes from the layout engine in [`crate::layout`].

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::circuit::CircuitSpec;
use crate::geometry::{Dir, Nm, Orientation, Point, Rect};
use crate::ids::{LibraryId, LogicalId};
use crate::layout;
use crate::schematic::ShapeKind;
use crate::text::{HAlign, VAlign};
use crate::validation::{has_errors, validate_circuit, Finding, Severity};

/// Upper bound on operations in one plan (plan §10 suggested 50; drawn schematics need more).
pub const MAX_OPERATIONS: usize = 1500;

/// Geometry of a library symbol, from KiCad's library before anything is placed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibrarySymbol {
    pub library_id: LibraryId,
    /// Reference prefix from the library (`R`, `Q`, `U`, `#PWR`, …).
    pub reference_prefix: String,
    pub unit_count: u32,
    /// A power symbol (`power:+5V`, `power:GND`, `PWR_FLAG`): names or flags a net.
    pub is_power: bool,
    /// Bounding box of the symbol's graphics around its origin (y down), if it has any.
    pub body: Option<Rect>,
    pub pins: Vec<LibraryPin>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibraryPin {
    pub number: String,
    pub name: String,
    /// Connection point relative to the symbol origin (y down), unrotated.
    pub local: Point,
    /// 0 = shared by all units.
    pub unit: u32,
    /// Direction the pin points away from the symbol body, unrotated.
    pub outward: Dir,
    /// KiCad electrical type, e.g. `passive`, `power_in`.
    pub electrical_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    PlaceSymbol {
        component_id: LogicalId,
        library_id: LibraryId,
        reference: String,
        value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        footprint: Option<String>,
        /// Datasheet, Description, and custom fields written with the part (custom ones hidden).
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        fields: BTreeMap<String, String>,
        position: Point,
        orientation: Orientation,
    },
    /// A power symbol (`+5V`, `GND`, …) that names a net; not part of the circuit spec.
    PlacePowerSymbol {
        net: String,
        library_id: LibraryId,
        reference: String,
        position: Point,
        orientation: Orientation,
    },
    AddWire {
        net: String,
        start: Point,
        end: Point,
    },
    AddLabel {
        net: String,
        position: Point,
        /// Direction the label text extends from its anchor.
        direction: Dir,
    },
    AddJunction {
        position: Point,
    },
    /// A no-connect flag on a pin that stays unconnected.
    AddNoConnect {
        position: Point,
    },
    /// A free text note.
    AddText {
        text: String,
        position: Point,
        size: Nm,
        angle_deg: u16,
        h: HAlign,
        v: VAlign,
    },
    /// A line, rectangle, or circle (not a wire).
    AddShape {
        kind: ShapeKind,
        stroke: Nm,
        filled: bool,
    },
}

/// The pins a net must contain after applying, as `REF.PIN`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ExpectedNet {
    pub name: String,
    pub pins: BTreeSet<String>,
    /// The name is not checked: the plan removes a label, and the net may fall back to another name.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub name_open: bool,
}

/// An existing symbol moved to a new place (and orientation). Its identity, fields, and
/// footprint stay; wires are not dragged along, so a plan that moves symbols also replaces the
/// drawing around them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolMove {
    /// The symbol's id in the document (its UUID).
    pub id: String,
    pub reference: String,
    pub from: Point,
    pub from_orientation: Orientation,
    pub to: Point,
    pub orientation: Orientation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovedKind {
    Wire,
    Label,
    Junction,
    /// A no-connect flag; the plan draws it again on its pin.
    NoConnect,
    /// A power symbol that only names a net (`#PWR…`); the plan draws it again where it belongs.
    PowerSymbol,
    /// A part the user asked to delete.
    Symbol,
    /// A free text note.
    Text,
    /// A drawn line, rectangle, or circle.
    Shape,
}

/// A field of an existing part set to a new text: a standard field (`Value`, `Footprint`,
/// `Datasheet`, `Description`), a custom field (created hidden when the part has none of that
/// name), or one of the flags `DNP`, `Exclude from BOM`, `Exclude from board` (`yes` or `no`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PropertyChange {
    /// The symbol's id in the document (its UUID).
    pub id: String,
    pub reference: String,
    pub name: String,
    /// The text before, when the part had the field.
    pub from: Option<String>,
    pub to: String,
}

/// The names that are flags, not text fields.
pub const FLAG_FIELDS: [&str; 3] = ["DNP", "Exclude from BOM", "Exclude from board"];

/// Text of an existing part (its reference or value) moved to another place or hidden. The text
/// keeps its size and angle; only where it sits (and how it is justified) changes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldChange {
    /// The symbol's id in the document (its UUID).
    pub id: String,
    pub reference: String,
    /// `Reference` or `Value`.
    pub field: String,
    pub from: Point,
    pub position: Point,
    pub h: HAlign,
    pub v: VAlign,
    /// Hide the text (it stays in the symbol).
    pub hide: bool,
}

/// An existing drawing item the plan deletes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Removal {
    pub kind: RemovedKind,
    pub id: String,
    /// What it is, for the person reviewing the plan.
    pub description: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct OperationPlan {
    pub circuit: String,
    /// Symbols, power symbols, wires, labels, and junctions to add.
    pub operations: Vec<Operation>,
    /// Existing symbols to move (tidying an existing schematic).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub moves: Vec<SymbolMove>,
    /// Existing items to delete (tidying an existing schematic).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removals: Vec<Removal>,
    /// Existing parts' text to move or hide.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub field_changes: Vec<FieldChange>,
    /// Fields of existing parts set to new text (part numbers, footprints, flags).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub property_changes: Vec<PropertyChange>,
    /// A change of the sheet's page and title block (nothing else is in such a plan).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<crate::sheet::SheetChange>,
    pub expected_nets: Vec<ExpectedNet>,
    /// Component logical id → planned reference.
    pub references: BTreeMap<LogicalId, String>,
    pub findings: Vec<Finding>,
    /// Nets the layout could not wire and joined with labels instead.
    pub fallback_nets: Vec<String>,
    /// Geometric checks of the drawing the plan produces (shorts, overlaps, wires through parts).
    pub readability: crate::readability::Readability,
    /// The same checks on the drawing as it is now, when the plan tidies an existing schematic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readability_before: Option<crate::readability::Readability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct OperationCounts {
    pub moves: usize,
    pub removals: usize,
    pub field_changes: usize,
    pub property_changes: usize,
    pub texts: usize,
    pub shapes: usize,
    pub symbols: usize,
    pub power_symbols: usize,
    pub wires: usize,
    pub labels: usize,
    pub junctions: usize,
    pub no_connects: usize,
}

impl OperationPlan {
    pub fn counts(&self) -> OperationCounts {
        let mut c = OperationCounts {
            moves: self.moves.len(),
            removals: self.removals.len(),
            field_changes: self.field_changes.len(),
            property_changes: self.property_changes.len(),
            ..Default::default()
        };
        for op in &self.operations {
            match op {
                Operation::PlaceSymbol { .. } => c.symbols += 1,
                Operation::PlacePowerSymbol { .. } => c.power_symbols += 1,
                Operation::AddWire { .. } => c.wires += 1,
                Operation::AddLabel { .. } => c.labels += 1,
                Operation::AddJunction { .. } => c.junctions += 1,
                Operation::AddNoConnect { .. } => c.no_connects += 1,
                Operation::AddText { .. } => c.texts += 1,
                Operation::AddShape { .. } => c.shapes += 1,
            }
        }
        c
    }
}

/// What already exists in the target document, so the plan does not collide with it.
#[derive(Debug, Clone, Default)]
pub struct ExistingDocument {
    pub references: BTreeSet<String>,
    /// Lowest occupied y: new parts are placed below it.
    pub max_y: Option<Nm>,
}

pub(crate) fn error(code: &'static str, message: String, objects: Vec<String>) -> Finding {
    Finding {
        severity: Severity::Error,
        code,
        message,
        objects,
    }
}

/// Errors about components and pins that the symbol libraries do not support.
pub(crate) fn library_findings(spec: &CircuitSpec, library: &BTreeMap<LibraryId, LibrarySymbol>) -> Vec<Finding> {
    let mut findings = Vec::new();
    for c in &spec.components {
        match library.get(&c.library_id) {
            None => findings.push(error(
                "UNKNOWN_LIBRARY_SYMBOL",
                format!("{} is not in the KiCad symbol libraries", c.library_id),
                vec![c.id.to_string()],
            )),
            Some(sym) if sym.unit_count > 1 => findings.push(error(
                "MULTI_UNIT_UNSUPPORTED",
                format!(
                    "{} has {} units; multi-unit symbols are not supported yet",
                    c.library_id, sym.unit_count
                ),
                vec![c.id.to_string()],
            )),
            Some(_) => {}
        }
    }
    for net in &spec.nets {
        for pin_ref in &net.pins {
            let component = spec.component(&pin_ref.component_id).expect("validated");
            if let Some(symbol) = library.get(&component.library_id) {
                if !symbol
                    .pins
                    .iter()
                    .any(|p| p.number == pin_ref.pin_number && (p.unit == 0 || p.unit == 1))
                {
                    findings.push(error(
                        "UNKNOWN_PIN",
                        format!(
                            "{} has no pin '{}' (pins: {})",
                            component.library_id,
                            pin_ref.pin_number,
                            symbol
                                .pins
                                .iter()
                                .map(|p| p.number.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        vec![component.id.to_string(), net.name.clone()],
                    ));
                }
            }
        }
    }
    for nc in &spec.no_connects {
        let Some(component) = spec.component(&nc.component_id) else {
            continue;
        };
        if let Some(symbol) = library.get(&component.library_id) {
            if !symbol
                .pins
                .iter()
                .any(|p| p.number == nc.pin_number && (p.unit == 0 || p.unit == 1))
            {
                findings.push(error(
                    "UNKNOWN_PIN",
                    format!("{} has no pin '{}' (no_connects)", component.library_id, nc.pin_number),
                    vec![component.id.to_string()],
                ));
            }
        }
    }
    findings
}

/// Findings for a drawing's readability report: shorts are errors, the rest warnings.
pub(crate) fn readability_findings(readability: &crate::readability::Readability) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (a, b) in &readability.shorts {
        findings.push(error(
            "LAYOUT_SHORT",
            format!("the drawing would join nets '{a}' and '{b}'"),
            vec![a.clone(), b.clone()],
        ));
    }
    for (a, b) in &readability.overlapping_symbols {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "SYMBOLS_OVERLAP",
            message: format!("{a} and {b} overlap"),
            objects: vec![a.clone(), b.clone()],
        });
    }
    for (net, reference) in &readability.wires_through_symbols {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "WIRE_THROUGH_SYMBOL",
            message: format!("a wire of '{net}' passes through {reference}"),
            objects: vec![net.clone(), reference.clone()],
        });
    }
    for (a, b) in &readability.text_overlaps {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "TEXT_OVERLAP",
            message: format!("the text of {a} runs into {b}"),
            objects: vec![a.clone(), b.clone()],
        });
    }
    findings
}

/// Turns `spec` into a plan. `library` must contain every component's symbol, plus the power
/// symbols available for net names (see [`layout::library_ids_for`]). A plan whose findings
/// include an error must not be applied.
pub fn plan_circuit(
    spec: &CircuitSpec,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    existing: &ExistingDocument,
) -> OperationPlan {
    let mut findings = validate_circuit(spec);
    let mut plan = OperationPlan {
        circuit: spec.name.to_string(),
        ..Default::default()
    };
    if has_errors(&findings) {
        plan.findings = findings;
        return plan;
    }

    // Power flags become parts on their nets.
    let expanded = spec.expanded();
    let spec = &expanded;
    findings.extend(library_findings(spec, library));
    if has_errors(&findings) {
        plan.findings = findings;
        return plan;
    }

    // References: keep a free preferred one, otherwise the library prefix plus the next free number.
    let mut taken: BTreeSet<String> = existing.references.clone();
    for c in &spec.components {
        if let Some(r) = &c.reference_preference {
            if taken.contains(r) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: "REFERENCE_TAKEN",
                    message: format!("{r} already exists in the schematic; '{}' gets another reference", c.id),
                    objects: vec![c.id.to_string()],
                });
            }
        }
    }
    for c in &spec.components {
        let reference = match &c.reference_preference {
            Some(r) if !taken.contains(r) => r.clone(),
            _ => {
                let prefix = &library[&c.library_id].reference_prefix;
                // Power-library parts conventionally use two-digit numbers (#PWR01, #FLG01).
                let width = if prefix.starts_with('#') { 2 } else { 1 };
                (1..)
                    .map(|n| format!("{prefix}{n:0width$}"))
                    .find(|r| !taken.contains(r))
                    .expect("unbounded range")
            }
        };
        taken.insert(reference.clone());
        plan.references.insert(c.id.clone(), reference);
    }

    for net in &spec.nets {
        let pins = net
            .pins
            .iter()
            .map(|p| format!("{}.{}", plan.references[&p.component_id], p.pin_number))
            .collect();
        plan.expected_nets.push(ExpectedNet {
            name: net.name.clone(),
            pins,
            ..Default::default()
        });
    }

    let out = layout::layout(spec, library, &plan.references, &mut taken, existing.max_y);
    plan.operations = out.operations;
    plan.fallback_nets = out.fallback_nets;
    findings.extend(out.findings);

    // A no-connect flag on each pin that is meant to stay open.
    for nc in &spec.no_connects {
        let placed = plan.operations.iter().find_map(|op| match op {
            Operation::PlaceSymbol {
                component_id,
                library_id,
                position,
                orientation,
                ..
            } if component_id == &nc.component_id => Some((library_id, *position, *orientation)),
            _ => None,
        });
        let at = placed.and_then(|(id, position, orientation)| {
            let pin = library
                .get(id)?
                .pins
                .iter()
                .find(|p| p.number == nc.pin_number && (p.unit == 0 || p.unit == 1))?;
            Some(position + orientation.apply(pin.local)?)
        });
        if let Some(position) = at {
            if !plan
                .operations
                .iter()
                .any(|op| matches!(op, Operation::AddNoConnect { position: p } if *p == position))
            {
                plan.operations.push(Operation::AddNoConnect { position });
            }
        }
    }

    plan.readability = crate::readability::check(&plan, library);
    findings.extend(readability_findings(&plan.readability));
    if plan.operations.len() > MAX_OPERATIONS {
        findings.push(error(
            "PLAN_TOO_LARGE",
            format!(
                "the plan has {} operations; the limit is {MAX_OPERATIONS}",
                plan.operations.len()
            ),
            vec![],
        ));
    }
    plan.findings = findings;
    plan
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::geometry::GRID;

    const EXAMPLE: &str = include_str!("../../../docs/examples/two-resistor-series-chain.json");
    const REGULATOR: &str = include_str!("../../../docs/examples/regulator-3v3.json");

    fn mm(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    fn pin(number: &str, x: f64, y: f64, outward: Dir) -> LibraryPin {
        LibraryPin {
            number: number.into(),
            name: String::new(),
            local: mm(x, y),
            unit: 1,
            outward,
            electrical_type: "passive".into(),
        }
    }

    fn symbol(id: &str, prefix: &str, body: (f64, f64, f64, f64), pins: Vec<LibraryPin>) -> LibrarySymbol {
        LibrarySymbol {
            library_id: LibraryId::parse(id).unwrap(),
            reference_prefix: prefix.into(),
            unit_count: 1,
            is_power: id.starts_with("power:"),
            body: Rect::from_points([mm(body.0, body.1), mm(body.2, body.3)]),
            pins,
        }
    }

    /// Pin geometry as KiCad 10.99 reports it (y down).
    pub(crate) fn library() -> BTreeMap<LibraryId, LibrarySymbol> {
        let two_pin_vertical = |id: &str, prefix: &str| {
            symbol(
                id,
                prefix,
                (-1.0, -2.54, 1.0, 2.54),
                vec![pin("1", 0.0, -3.81, Dir::Up), pin("2", 0.0, 3.81, Dir::Down)],
            )
        };
        let two_pin_horizontal = |id: &str, prefix: &str| {
            symbol(
                id,
                prefix,
                (-1.27, -1.27, 1.27, 1.27),
                vec![pin("1", -3.81, 0.0, Dir::Left), pin("2", 3.81, 0.0, Dir::Right)],
            )
        };
        // Supplies draw above their pin, ground below it (y down); the pin is at the origin.
        let power = |id: &str, outward: Dir| {
            let body = if outward == Dir::Up {
                (-1.27, 0.25, 1.27, 2.54)
            } else {
                (-0.76, -2.54, 0.76, -0.25)
            };
            symbol(id, "#PWR", body, vec![pin("1", 0.0, 0.0, outward)])
        };
        [
            two_pin_vertical("Device:R", "R"),
            two_pin_vertical("Device:C", "C"),
            two_pin_vertical("Device:Fuse", "F"),
            two_pin_horizontal("Device:D_Schottky", "D"),
            two_pin_horizontal("Device:LED", "D"),
            symbol(
                "Connector_Generic:Conn_01x02",
                "J",
                (-1.27, -1.27, 1.27, 3.81),
                vec![pin("1", -5.08, 0.0, Dir::Left), pin("2", -5.08, 2.54, Dir::Left)],
            ),
            symbol(
                "Regulator_Linear:AMS1117-3.3",
                "U",
                (-5.08, -1.27, 5.08, 5.08),
                vec![
                    pin("1", 0.0, 7.62, Dir::Down),
                    pin("2", 7.62, 0.0, Dir::Right),
                    pin("3", -7.62, 0.0, Dir::Left),
                ],
            ),
            power("power:+5V", Dir::Down),
            power("power:+3V3", Dir::Down),
            power("power:GND", Dir::Up),
            power("power:PWR_FLAG", Dir::Down),
        ]
        .into_iter()
        .map(|s| (s.library_id.clone(), s))
        .collect()
    }

    fn all_points(plan: &OperationPlan) -> Vec<Point> {
        plan.operations
            .iter()
            .flat_map(|op| match op {
                Operation::PlaceSymbol { position, .. }
                | Operation::PlacePowerSymbol { position, .. }
                | Operation::AddLabel { position, .. }
                | Operation::AddJunction { position }
                | Operation::AddNoConnect { position } => vec![*position],
                Operation::AddWire { start, end, .. } => vec![*start, *end],
                Operation::AddText { position, .. } => vec![*position],
                Operation::AddShape { kind, .. } => {
                    let b = kind.bounds();
                    vec![b.min, b.max]
                }
            })
            .collect()
    }

    #[test]
    fn two_resistor_chain_is_wired_in_a_row() {
        let spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(!has_errors(&plan.findings), "{:?}", plan.findings);
        assert_eq!(plan.references.values().cloned().collect::<Vec<_>>(), ["R1", "R2"]);
        assert!(all_points(&plan).iter().all(|p| p.is_on_grid(GRID)));
        // MID joins R1.2 and R2.1 with a wire, not labels on both pins.
        assert!(plan.counts().wires >= 1, "{:?}", plan.counts());
        assert!(plan.fallback_nets.is_empty(), "{:?}", plan.fallback_nets);
        // Both resistors lie on one baseline, rotated horizontal.
        let rotations: Vec<u16> = plan
            .operations
            .iter()
            .filter_map(|o| match o {
                Operation::PlaceSymbol { orientation, .. } => Some(orientation.rotation_deg),
                _ => None,
            })
            .collect();
        assert!(rotations.iter().all(|r| *r == 90 || *r == 270), "{rotations:?}");
        assert_eq!(
            plan,
            plan_circuit(&spec, &library(), &ExistingDocument::default()),
            "deterministic"
        );
    }

    #[test]
    fn regulator_reads_left_to_right_with_power_symbols() {
        let spec = CircuitSpec::from_json(REGULATOR).unwrap();
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(!has_errors(&plan.findings), "{:?}", plan.findings);
        let counts = plan.counts();
        // Ground is drawn as symbols, never labels; +5V and +3V3 are named by power symbols.
        assert!(counts.power_symbols >= 9, "{counts:?}");
        assert!(plan
            .operations
            .iter()
            .all(|o| !matches!(o, Operation::AddLabel { net, .. } if net == "GND" || net == "+5V" || net == "+3V3")));
        assert!(plan.fallback_nets.is_empty(), "{:?}", plan.fallback_nets);
        // Main path order: J1 < F1 < D1 < U1 < R1 along x.
        let x_of = |r: &str| {
            plan.operations
                .iter()
                .find_map(|o| match o {
                    Operation::PlaceSymbol {
                        reference, position, ..
                    } if reference == r => Some(position.x),
                    _ => None,
                })
                .unwrap()
        };
        let order = ["J1", "F1", "D1", "U1", "R1"];
        assert!(
            order.windows(2).all(|w| x_of(w[0]) < x_of(w[1])),
            "{:?}",
            order.map(x_of)
        );
        // Input caps sit between D1 and U1, output caps after U1.
        assert!(x_of("C1") > x_of("D1") && x_of("C1") < x_of("U1"));
        assert!(x_of("C3") > x_of("U1"));
        assert!(counts.junctions >= 2, "{counts:?}");
    }

    #[test]
    fn avoids_existing_references_and_rows() {
        let spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        let existing = ExistingDocument {
            references: ["R1".to_owned()].into(),
            max_y: Some(Nm::from_mm(60.96).unwrap()),
        };
        let plan = plan_circuit(&spec, &library(), &existing);
        assert_eq!(plan.references.values().cloned().collect::<Vec<_>>(), ["R2", "R3"]);
        assert!(plan.findings.iter().any(|f| f.code == "REFERENCE_TAKEN"));
        let Operation::PlaceSymbol { position, .. } = &plan.operations[0] else {
            panic!()
        };
        assert!(position.y > Nm::from_mm(60.96).unwrap());
    }

    #[test]
    fn reports_unknown_pins_and_symbols() {
        let mut spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        spec.nets[0].pins[0].pin_number = "7".into();
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(plan.findings.iter().any(|f| f.code == "UNKNOWN_PIN"));
        assert!(plan_circuit(&spec, &BTreeMap::new(), &ExistingDocument::default())
            .findings
            .iter()
            .any(|f| f.code == "UNKNOWN_LIBRARY_SYMBOL"));
    }
}

#[cfg(test)]
mod debug_dump {
    #[test]
    #[ignore]
    fn dump_regulator() {
        let spec = crate::CircuitSpec::from_json(include_str!("../../../docs/examples/regulator-3v3.json")).unwrap();
        let plan = super::plan_circuit(&spec, &super::tests::library(), &Default::default());
        for op in &plan.operations {
            println!("{op:?}");
        }
    }
}

#[cfg(test)]
mod readability_tests {
    use super::tests::library;
    use super::*;

    fn plan(json: &str) -> OperationPlan {
        plan_circuit(
            &CircuitSpec::from_json(json).unwrap(),
            &library(),
            &ExistingDocument::default(),
        )
    }

    #[test]
    fn golden_circuits_are_clean() {
        for json in [
            include_str!("../../../docs/examples/two-resistor-series-chain.json"),
            include_str!("../../../docs/examples/regulator-3v3.json"),
        ] {
            let p = plan(json);
            assert!(p.readability.is_clean(), "{}: {:#?}", p.circuit, p.readability);
            assert!(
                p.readability.fallback_nets.is_empty(),
                "{}: {:?}",
                p.circuit,
                p.readability.fallback_nets
            );
        }
    }

    /// A circuit of `parts` parts on shared rails and signal nets (resistors, capacitors, LEDs,
    /// regulators).
    fn big_spec(parts: usize) -> CircuitSpec {
        let (r, c, d, u) = (parts / 3, parts / 3, parts * 4 / 15, parts / 15);
        let mut components = Vec::new();
        let mut nets: Vec<serde_json::Value> = Vec::new();
        let pin = |c: &str, n: &str| serde_json::json!({ "component_id": c, "pin_number": n });
        let (mut vin, mut gnd, mut v33) = (vec![], vec![], vec![]);
        for i in 0..r {
            components.push(serde_json::json!({ "id": format!("r{i}"), "library_id": "Device:R", "value": "10k" }));
            vin.push(pin(&format!("r{i}"), "1"));
        }
        for i in 0..c {
            components.push(serde_json::json!({ "id": format!("c{i}"), "library_id": "Device:C", "value": "100n" }));
            gnd.push(pin(&format!("c{i}"), "2"));
        }
        for i in 0..d {
            components.push(serde_json::json!({ "id": format!("d{i}"), "library_id": "Device:LED", "value": "red" }));
            gnd.push(pin(&format!("d{i}"), "2"));
        }
        for i in 0..u {
            components.push(serde_json::json!({ "id": format!("u{i}"), "library_id": "Regulator_Linear:AMS1117-3.3", "value": "3V3" }));
            vin.push(pin(&format!("u{i}"), "3"));
            gnd.push(pin(&format!("u{i}"), "1"));
            v33.push(pin(&format!("u{i}"), "2"));
        }
        for i in 0..r.min(c) {
            let mut pins = vec![pin(&format!("r{i}"), "2"), pin(&format!("c{i}"), "1")];
            if i < d {
                pins.push(pin(&format!("d{i}"), "1"));
            }
            nets.push(serde_json::json!({ "name": format!("S{i}"), "pins": pins }));
        }
        nets.push(serde_json::json!({ "name": "+5V", "pins": vin }));
        nets.push(serde_json::json!({ "name": "GND", "pins": gnd }));
        nets.push(serde_json::json!({ "name": "+3V3", "pins": v33 }));
        let json = serde_json::json!({ "schema_version": 1, "name": "big", "components": components, "nets": nets });
        CircuitSpec::from_json(&json.to_string()).unwrap()
    }

    /// W2: 150 parts plan in tens of milliseconds (23 ms in a release build, 671 operations), clean
    /// and within the operation limit; one part more is refused, and power flags are not parts.
    #[test]
    fn a_circuit_of_150_parts_is_planned_clean() {
        let started = std::time::Instant::now();
        let p = plan_circuit(&big_spec(150), &library(), &ExistingDocument::default());
        println!(
            "150 parts: {} operations in {:?}",
            p.operations.len(),
            started.elapsed()
        );
        assert!(!has_errors(&p.findings), "{:#?}", p.findings);
        assert!(p.readability.is_clean(), "{:#?}", p.readability);
        assert!(p.operations.len() <= MAX_OPERATIONS);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn one_part_over_the_limit_is_refused_but_flags_are_not_counted() {
        let over = plan_circuit(&big_spec(153), &library(), &ExistingDocument::default());
        assert!(
            over.findings.iter().any(|f| f.code == "TOO_MANY_COMPONENTS"),
            "{:?}",
            over.findings
        );
        let mut spec = big_spec(150);
        for i in 0..5 {
            spec.components.push(
                serde_json::from_value(serde_json::json!({
                    "id": format!("flag{i}"), "library_id": "power:PWR_FLAG", "value": "PWR_FLAG"
                }))
                .unwrap(),
            );
        }
        assert!(!validate_circuit(&spec).iter().any(|f| f.code == "TOO_MANY_COMPONENTS"));
    }

    /// 14.7: pins meant to stay open get a no-connect flag; a power flag is written once per named
    /// net, as a part on that net, and counts as no part.
    #[test]
    fn no_connects_and_power_flags_are_part_of_a_circuit() {
        let json = r#"{ "schema_version": 1, "name": "flags",
            "components": [
                { "id": "r1", "library_id": "Device:R", "value": "1k", "reference_preference": "R1" },
                { "id": "r2", "library_id": "Device:R", "value": "2k", "reference_preference": "R2" } ],
            "nets": [
                { "name": "+5V", "pins": [ { "component_id": "r1", "pin_number": "1" } ] },
                { "name": "MID", "pins": [ { "component_id": "r1", "pin_number": "2" }, { "component_id": "r2", "pin_number": "1" } ] } ],
            "no_connects": [ { "component_id": "r2", "pin_number": "2" } ],
            "power_flags": [ "+5V" ] }"#;
        let spec = CircuitSpec::from_json(json).unwrap();
        assert!(crate::layout::library_ids_for(&spec)
            .iter()
            .any(|id| id.to_string() == "power:PWR_FLAG"));
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(!has_errors(&plan.findings), "{:#?}", plan.findings);
        // The flag is a part on the +5V net.
        let flags: Vec<_> = plan
            .operations
            .iter()
            .filter(|o| matches!(o, Operation::PlaceSymbol { library_id, .. } if library_id.entry() == "PWR_FLAG"))
            .collect();
        assert_eq!(flags.len(), 1, "{:#?}", plan.operations);
        let five = plan.expected_nets.iter().find(|n| n.name == "+5V").unwrap();
        assert_eq!(five.pins.len(), 2, "R1.1 and the flag: {five:?}");
        // The no-connect flag is on pin 2 of R2.
        let r2 = plan
            .operations
            .iter()
            .find_map(|o| match o {
                Operation::PlaceSymbol {
                    reference,
                    position,
                    orientation,
                    ..
                } if reference == "R2" => Some((*position, *orientation)),
                _ => None,
            })
            .unwrap();
        let pin2 = library()[&LibraryId::parse("Device:R").unwrap()]
            .pins
            .iter()
            .find(|p| p.number == "2")
            .unwrap()
            .local;
        let want = r2.0 + r2.1.apply(pin2).unwrap();
        assert!(
            plan.operations
                .iter()
                .any(|o| matches!(o, Operation::AddNoConnect { position } if *position == want)),
            "{:#?}",
            plan.operations
        );
        assert_eq!(plan.counts().no_connects, 1);
        // The flag is added once, even when the circuit already has one on the net.
        let again = spec.expanded().expanded();
        assert_eq!(
            again
                .components
                .iter()
                .filter(|c| c.library_id.entry() == "PWR_FLAG")
                .count(),
            1
        );

        // Mistakes.
        let in_net = json.replace(
            r#""no_connects": [ { "component_id": "r2", "pin_number": "2" } ]"#,
            r#""no_connects": [ { "component_id": "r2", "pin_number": "1" } ]"#,
        );
        let found: Vec<_> = validate_circuit(&CircuitSpec::from_json(&in_net).unwrap())
            .into_iter()
            .map(|f| f.code)
            .collect();
        assert!(found.contains(&"NO_CONNECT_PIN_IN_NET"), "{found:?}");
        let ghost = json.replace(r#""power_flags": [ "+5V" ]"#, r#""power_flags": [ "+9V" ]"#);
        let found: Vec<_> = validate_circuit(&CircuitSpec::from_json(&ghost).unwrap())
            .into_iter()
            .map(|f| f.code)
            .collect();
        assert!(found.contains(&"POWER_FLAG_UNKNOWN_NET"), "{found:?}");
        let bad_pin = json.replace(
            r#""pin_number": "2" } ],
            "power_flags""#,
            r#""pin_number": "9" } ],
            "power_flags""#,
        );
        let plan = plan_circuit(
            &CircuitSpec::from_json(&bad_pin).unwrap(),
            &library(),
            &ExistingDocument::default(),
        );
        assert!(
            plan.findings.iter().any(|f| f.code == "UNKNOWN_PIN"),
            "{:#?}",
            plan.findings
        );
    }

    #[test]
    fn a_short_is_an_error() {
        let mut p = plan(include_str!("../../../docs/examples/two-resistor-series-chain.json"));
        // Join a MID wire to INPUT's label point by hand.
        let input_at = p
            .operations
            .iter()
            .find_map(|o| match o {
                Operation::AddLabel { net, position, .. } if net == "INPUT" => Some(*position),
                _ => None,
            })
            .unwrap();
        p.operations.push(Operation::AddWire {
            net: "MID".into(),
            start: input_at,
            end: input_at + Dir::Up.step(Nm(2_540_000)),
        });
        let report = crate::readability::check(&p, &library());
        assert!(!report.shorts.is_empty());
    }
}
