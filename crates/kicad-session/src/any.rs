// SPDX-License-Identifier: GPL-3.0-or-later
//! A session of either kind, so tools work the same through a live KiCad 11 connection and
//! through a schematic file (plan §3.1: "selection order per operation").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kicad_cli_runner::KicadInstall;
use kicad_domain::{
    CapabilitySet, ExistingDocument, LibraryId, LibrarySymbol, NetInfo, OperationPlan, SymbolInfo, SymbolPins,
    ToolError,
};

use crate::file::FileSession;
use crate::mutate::ApplyReport;
use crate::schematic::OpenDocument;
use crate::session::{Provider, Session, SessionStatus};

pub enum AnySession {
    /// A KiCad 11 API connection, headless or to the editor.
    Ipc(Box<Session>),
    /// A schematic file edited while no editor has it open.
    File(Box<FileSession>),
}

impl From<Session> for AnySession {
    fn from(session: Session) -> Self {
        Self::Ipc(Box::new(session))
    }
}

impl From<FileSession> for AnySession {
    fn from(session: FileSession) -> Self {
        Self::File(Box::new(session))
    }
}

impl AnySession {
    pub fn id(&self) -> &str {
        match self {
            Self::Ipc(s) => s.id(),
            Self::File(s) => s.id(),
        }
    }

    pub fn provider(&self) -> Provider {
        match self {
            Self::Ipc(s) => s.provider(),
            Self::File(s) => s.provider(),
        }
    }

    pub fn kicad_version(&self) -> &str {
        match self {
            Self::Ipc(s) => s.kicad_version(),
            Self::File(s) => s.kicad_version(),
        }
    }

    pub fn document_path(&self) -> &Path {
        match self {
            Self::Ipc(s) => s.document_path(),
            Self::File(s) => s.document_path(),
        }
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        match self {
            Self::Ipc(s) => s.capabilities(),
            Self::File(s) => s.capabilities(),
        }
    }

    /// The `kicad-cli` that reads this session's document (ERC, netlist).
    pub fn cli(&self) -> Option<&KicadInstall> {
        match self {
            Self::Ipc(s) => s.cli(),
            Self::File(s) => s.cli(),
        }
    }

    /// The `kicad-cli` that renders PNG images, if there is one (KiCad 10's cannot).
    pub fn render_cli(&self) -> Option<&KicadInstall> {
        match self {
            Self::Ipc(s) => s.cli(),
            Self::File(s) => s.render_cli(),
        }
    }

    pub async fn status(&self) -> SessionStatus {
        match self {
            Self::Ipc(s) => s.status().await,
            Self::File(s) => s.status().await,
        }
    }

    pub async fn close(&self, discard_unsaved: bool) -> Result<bool, ToolError> {
        match self {
            Self::Ipc(s) => s.close(discard_unsaved).await,
            Self::File(s) => s.close(discard_unsaved).await,
        }
    }

    /// Reads the document from its file again (see `Session::reload`).
    pub async fn reload(&self, discard_unsaved: bool) -> Result<crate::freshness::ReloadReport, ToolError> {
        match self {
            Self::Ipc(s) => s.reload(discard_unsaved).await,
            Self::File(s) => s.reload(discard_unsaved).await,
        }
    }

    pub async fn open_documents(&self) -> Result<Vec<OpenDocument>, ToolError> {
        match self {
            Self::Ipc(s) => s.open_documents().await,
            Self::File(s) => s.open_documents().await,
        }
    }

    pub async fn library_symbols(&self, ids: &[LibraryId]) -> Result<BTreeMap<LibraryId, LibrarySymbol>, ToolError> {
        match self {
            Self::Ipc(s) => s.library_symbols(ids).await,
            Self::File(s) => s.library_symbols(ids).await,
        }
    }

    pub async fn existing_document(&self) -> Result<ExistingDocument, ToolError> {
        match self {
            Self::Ipc(s) => s.existing_document().await,
            Self::File(s) => s.existing_document().await,
        }
    }

    pub async fn fingerprint(&self) -> Result<String, ToolError> {
        match self {
            Self::Ipc(s) => s.fingerprint().await,
            Self::File(s) => s.fingerprint().await,
        }
    }

    pub async fn list_symbols(&self) -> Result<Vec<SymbolInfo>, ToolError> {
        match self {
            Self::Ipc(s) => s.list_symbols().await,
            Self::File(s) => s.list_symbols().await,
        }
    }

    pub async fn symbol_pins(&self, selector: &str) -> Result<SymbolPins, ToolError> {
        match self {
            Self::Ipc(s) => s.symbol_pins(selector).await,
            Self::File(s) => s.symbol_pins(selector).await,
        }
    }

    pub async fn design(&self) -> Result<kicad_domain::SchematicDesign, ToolError> {
        match self {
            Self::Ipc(s) => s.design().await,
            Self::File(s) => s.design().await,
        }
    }

    /// Changes the value or footprint of one part (an id, or a reference matching one part).
    pub async fn set_field(
        &self,
        selector: &str,
        field: crate::SymbolField,
        text: &str,
    ) -> Result<SymbolInfo, ToolError> {
        match self {
            Self::Ipc(s) => s.set_field(selector, field, text).await,
            Self::File(s) => s.set_field(selector, field, text).await,
        }
    }

    pub async fn netlist(&self) -> Result<Vec<NetInfo>, ToolError> {
        match self {
            Self::Ipc(s) => s.netlist().await,
            Self::File(s) => s.netlist().await,
        }
    }

    pub async fn apply_plan(&self, plan: &OperationPlan, expected_fingerprint: &str) -> Result<ApplyReport, ToolError> {
        let mut report = match self {
            Self::Ipc(s) => s.apply_plan(plan, expected_fingerprint).await,
            Self::File(s) => s.apply_plan(plan, expected_fingerprint).await,
        }?;
        // Every position and turn the plan asked for is read back from the document.
        if !plan.moves.is_empty()
            || !plan.field_changes.is_empty()
            || !plan.property_changes.is_empty()
            || plan.operations.iter().any(|op| {
                matches!(
                    op,
                    kicad_domain::Operation::PlaceSymbol { .. }
                        | kicad_domain::Operation::PlacePowerSymbol { .. }
                        | kicad_domain::Operation::AddNoConnect { .. }
                )
            })
        {
            match self.design().await {
                Ok(design) => {
                    let problems = placement_problems(plan, &design);
                    report.placements_checked = plan.moves.len()
                        + plan.field_changes.len()
                        + plan.property_changes.len()
                        + plan
                            .operations
                            .iter()
                            .filter(|op| {
                                matches!(
                                    op,
                                    kicad_domain::Operation::PlaceSymbol { .. }
                                        | kicad_domain::Operation::PlacePowerSymbol { .. }
                                )
                            })
                            .count();
                    if !problems.is_empty() {
                        report.verified = false;
                    }
                    report.placement_problems = problems;
                }
                Err(e) => {
                    report.verified = false;
                    report
                        .placement_problems
                        .push(format!("the placements could not be read back: {}", e.message));
                }
            }
        }
        Ok(report)
    }

    /// The page and title block as they are now.
    pub async fn sheet(&self) -> Result<kicad_domain::sheet::SheetSettings, ToolError> {
        match self {
            Self::Ipc(s) => s.sheet().await,
            Self::File(s) => s.sheet().await,
        }
    }

    pub async fn save(&self) -> Result<(), ToolError> {
        match self {
            Self::Ipc(s) => s.save().await,
            Self::File(s) => s.save().await,
        }
    }

    /// A copy of the document (with its project) in `dir`, for tools that read files.
    pub async fn save_copy(&self, dir: &Path) -> Result<PathBuf, ToolError> {
        match self {
            Self::Ipc(s) => s.save_copy(dir).await,
            Self::File(s) => s.save_copy(dir).await,
        }
    }
}

/// Where the document differs from what the plan placed, moved, or changed.
fn placement_problems(plan: &OperationPlan, design: &kicad_domain::SchematicDesign) -> Vec<String> {
    let mut problems = Vec::new();
    let at = |p: kicad_domain::Point| format!("({:.2}, {:.2})", p.x.to_mm(), p.y.to_mm());
    for m in &plan.moves {
        match design.symbols.iter().find(|s| s.id.eq_ignore_ascii_case(&m.id)) {
            Some(s) if s.position == m.to && s.orientation.same_effect(m.orientation) => {}
            Some(s) => problems.push(format!(
                "{} is at {} turned {:?}, not at {} turned {:?} as planned",
                m.reference,
                at(s.position),
                s.orientation,
                at(m.to),
                m.orientation
            )),
            None => problems.push(format!("{} is missing after the move", m.reference)),
        }
    }
    for op in &plan.operations {
        if let kicad_domain::Operation::PlaceSymbol { reference, fields, .. } = op {
            if let Some(s) = design.symbols.iter().find(|s| &s.reference == reference) {
                for (name, text) in fields {
                    let now = s.properties.get(name);
                    if now != Some(text) {
                        problems.push(format!(
                            "{reference} field '{name}' reads back as {now:?}, not '{text}' as planned"
                        ));
                    }
                }
            }
        }
        let (reference, position, orientation) = match op {
            kicad_domain::Operation::PlaceSymbol {
                reference,
                position,
                orientation,
                ..
            }
            | kicad_domain::Operation::PlacePowerSymbol {
                reference,
                position,
                orientation,
                ..
            } => (reference, *position, *orientation),
            _ => continue,
        };
        // A new symbol may be numbered by KiCad; only complain when one by this reference is wrong.
        if let Some(s) = design.symbols.iter().find(|s| &s.reference == reference) {
            if s.position != position || !s.orientation.same_effect(orientation) {
                problems.push(format!(
                    "{reference} is at {} turned {:?}, not at {} turned {:?} as planned",
                    at(s.position),
                    s.orientation,
                    at(position),
                    orientation
                ));
            }
        }
    }
    for op in &plan.operations {
        if let kicad_domain::Operation::AddNoConnect { position } = op {
            if !design.no_connects.iter().any(|n| n.position == *position) {
                problems.push(format!(
                    "no no-connect flag at ({}, {}) after applying",
                    position.x.to_mm(),
                    position.y.to_mm()
                ));
            }
        }
    }
    for c in &plan.property_changes {
        let Some(symbol) = design.symbols.iter().find(|s| s.id.eq_ignore_ascii_case(&c.id)) else {
            problems.push(format!("{} is missing after its fields were set", c.reference));
            continue;
        };
        let flag = |on: bool| if on { "yes" } else { "no" };
        let now = match c.name.as_str() {
            "DNP" => Some(flag(symbol.dnp).to_owned()),
            "Exclude from BOM" => Some(flag(symbol.exclude_from_bom).to_owned()),
            "Exclude from board" => Some(flag(symbol.exclude_from_board).to_owned()),
            other => symbol.properties.get(other).cloned(),
        };
        if now.as_deref() != Some(c.to.as_str()) {
            problems.push(format!(
                "{} field '{}' reads back as {:?}, not '{}' as planned",
                c.reference, c.name, now, c.to
            ));
        }
    }
    for c in &plan.field_changes {
        let Some(symbol) = design.symbols.iter().find(|s| s.id.eq_ignore_ascii_case(&c.id)) else {
            problems.push(format!("{} is missing after its text was changed", c.reference));
            continue;
        };
        match symbol.fields.iter().find(|f| f.name == c.field) {
            Some(f) if c.hide && f.hidden => {}
            Some(f) if !c.hide && f.position == c.position && !f.hidden => {}
            Some(f) => problems.push(format!(
                "the {} text of {} is at {}{}, not {} as planned",
                c.field.to_lowercase(),
                c.reference,
                at(f.position),
                if f.hidden { " (hidden)" } else { "" },
                if c.hide { "hidden".to_owned() } else { at(c.position) }
            )),
            None => problems.push(format!("{} has no {} text after the change", c.reference, c.field)),
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_domain::plan::{FieldChange, SymbolMove};
    use kicad_domain::schematic::{DesignField, DesignSymbol};
    use kicad_domain::text::{HAlign, VAlign};
    use kicad_domain::{Nm, Orientation, Point, SchematicDesign};

    fn at(x: i64, y: i64) -> Point {
        Point::new(Nm(x * 1_270_000), Nm(y * 1_270_000))
    }

    fn design() -> SchematicDesign {
        SchematicDesign {
            symbols: vec![DesignSymbol {
                id: "AAAA".into(),
                reference: "R1".into(),
                position: at(10, 10),
                fields: vec![DesignField {
                    name: "Value".into(),
                    text: "10k".into(),
                    position: at(12, 10),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn mv(to: Point) -> SymbolMove {
        SymbolMove {
            id: "aaaa".into(),
            reference: "R1".into(),
            from: at(1, 1),
            from_orientation: Orientation::default(),
            to,
            orientation: Orientation::default(),
        }
    }

    #[test]
    fn a_part_where_the_plan_put_it_is_not_a_problem() {
        let plan = OperationPlan {
            moves: vec![mv(at(10, 10))],
            ..Default::default()
        };
        assert!(placement_problems(&plan, &design()).is_empty());
    }

    #[test]
    fn a_part_that_ended_up_elsewhere_is_reported() {
        let plan = OperationPlan {
            moves: vec![mv(at(20, 10))],
            ..Default::default()
        };
        let problems = placement_problems(&plan, &design());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("R1"));
    }

    #[test]
    fn moved_text_is_read_back_too() {
        let change = |position, hide| FieldChange {
            id: "AAAA".into(),
            reference: "R1".into(),
            field: "Value".into(),
            from: at(1, 1),
            position,
            h: HAlign::Left,
            v: VAlign::Center,
            hide,
        };
        let ok = OperationPlan {
            field_changes: vec![change(at(12, 10), false)],
            ..Default::default()
        };
        assert!(placement_problems(&ok, &design()).is_empty());
        let wrong = OperationPlan {
            field_changes: vec![change(at(30, 10), false), change(at(12, 10), true)],
            ..Default::default()
        };
        assert_eq!(placement_problems(&wrong, &design()).len(), 2);
    }
}
