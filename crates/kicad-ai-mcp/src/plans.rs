// SPDX-License-Identifier: GPL-3.0-or-later
//! Plans waiting to be applied, idempotency records, and the JSON shown to clients for a plan.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use kicad_domain::{Operation, OperationPlan, Point};
use serde_json::{json, Value};

pub struct StoredPlan {
    pub session_id: String,
    pub plan: OperationPlan,
    /// Document fingerprint the plan was made against.
    pub fingerprint: String,
    /// Idempotency key of the apply that consumed this plan.
    pub applied_with: Option<String>,
}

#[derive(Default)]
pub struct PlanStore {
    plans: BTreeMap<String, StoredPlan>,
    /// Idempotency key → (plan id, recorded tool result envelope).
    applies: BTreeMap<String, (String, Value)>,
}

static NEXT_PLAN: AtomicU64 = AtomicU64::new(1);

impl PlanStore {
    pub fn insert(&mut self, plan: StoredPlan) -> String {
        let id = format!("plan-{:04}", NEXT_PLAN.fetch_add(1, Ordering::Relaxed));
        self.plans.insert(id.clone(), plan);
        id
    }

    pub fn get(&self, id: &str) -> Option<&StoredPlan> {
        self.plans.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut StoredPlan> {
        self.plans.get_mut(id)
    }

    pub fn previous_apply(&self, key: &str) -> Option<&(String, Value)> {
        self.applies.get(key)
    }

    pub fn record_apply(&mut self, key: String, plan_id: String, envelope: Value) {
        self.applies.insert(key, (plan_id, envelope));
    }

    /// Plans of a closed session can never be applied.
    pub fn forget_session(&mut self, session_id: &str) {
        self.plans.retain(|_, p| p.session_id != session_id);
    }
}

fn mm(p: Point) -> Value {
    json!({ "x": p.x.to_mm(), "y": p.y.to_mm() })
}

/// An operation with coordinates in millimetres.
pub fn operation_view(op: &Operation) -> Value {
    match op {
        Operation::PlaceSymbol {
            component_id,
            library_id,
            reference,
            value,
            footprint,
            fields,
            position,
            orientation,
        } => json!({
            "op": "place_symbol",
            "component_id": component_id,
            "library_id": library_id.to_string(),
            "reference": reference,
            "value": value,
            "footprint": footprint,
            "fields": fields,
            "position_mm": mm(*position),
            "rotation_deg": orientation.rotation_deg,
        }),
        Operation::AddWire { net, start, end } => json!({
            "op": "add_wire", "net": net, "start_mm": mm(*start), "end_mm": mm(*end),
        }),
        Operation::AddLabel {
            net,
            position,
            direction,
        } => json!({
            "op": "add_label", "net": net, "position_mm": mm(*position), "direction": direction,
        }),
        Operation::PlacePowerSymbol {
            net,
            library_id,
            reference,
            position,
            orientation,
        } => json!({
            "op": "place_power_symbol",
            "net": net,
            "library_id": library_id.to_string(),
            "reference": reference,
            "position_mm": mm(*position),
            "rotation_deg": orientation.rotation_deg,
        }),
        Operation::AddJunction { position } => json!({ "op": "add_junction", "position_mm": mm(*position) }),
        Operation::AddNoConnect { position } => json!({ "op": "add_no_connect", "position_mm": mm(*position) }),
        Operation::AddText {
            text,
            position,
            size,
            angle_deg,
            ..
        } => json!({
            "op": "add_text", "text": text, "position_mm": mm(*position),
            "size_mm": size.to_mm(), "rotation_deg": angle_deg,
        }),
        Operation::AddShape { kind, stroke, filled } => json!({
            "op": "add_shape", "shape": kind, "stroke_mm": stroke.to_mm(), "filled": filled,
        }),
    }
}

fn move_view(m: &kicad_domain::plan::SymbolMove) -> Value {
    json!({
        "reference": m.reference,
        "id": m.id,
        "from_mm": mm(m.from),
        "to_mm": mm(m.to),
        "rotation_deg": m.orientation.rotation_deg,
        "mirror_x": m.orientation.mirror_x,
        "mirror_y": m.orientation.mirror_y,
    })
}

pub fn plan_view(plan_id: &str, plan: &OperationPlan) -> Value {
    let can_apply = !kicad_domain::validation::has_errors(&plan.findings);
    json!({
        "plan_id": plan_id,
        "circuit": plan.circuit,
        "can_apply": can_apply,
        "counts": plan.counts(),
        "fallback_nets": plan.fallback_nets,
        "readability": plan.readability,
        "readability_before": plan.readability_before,
        "references": plan.references,
        "findings": plan.findings,
        "moves": plan.moves.iter().map(move_view).collect::<Vec<_>>(),
        "removals": plan.removals,
        "sheet": plan.sheet.as_ref().map(|c| json!({
            "changes": c.describe(), "from": c.from, "to": c.to,
        })),
        "property_changes": plan.property_changes.iter().map(|c| json!({
            "reference": c.reference, "field": c.name, "from": c.from, "to": c.to,
        })).collect::<Vec<_>>(),
        "field_changes": plan.field_changes.iter().map(|c| json!({
            "reference": c.reference, "field": c.field, "from_mm": mm(c.from),
            "to_mm": mm(c.position), "hide": c.hide,
        })).collect::<Vec<_>>(),
        "operations": plan.operations.iter().map(operation_view).collect::<Vec<_>>(),
        "expected_nets": plan.expected_nets,
    })
}
