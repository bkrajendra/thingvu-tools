// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying operation plans, saving, and export jobs.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kicad_domain::geometry::Orientation;
use kicad_domain::{
    Capability, Dir, ErrorCode, ExistingDocument, LibraryId, LibraryPin, LibrarySymbol, MutationState, Nm, Operation,
    OperationPlan, Point, Rect, ToolError,
};
use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, DeleteItems, EndCommit,
    GetItemsFromLibrary, GetItemsResponse, ItemStatusCode, PlaceFromLibraryResponse, SaveCopyOfDocument, SaveDocument,
    SaveOptions, UpdateItems, UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    graphic_shape, poly_line_node, GraphicShape, ItemHeader, Kiid as ProtoKiid, LibraryIdentifier, LibraryType, Text,
    Vector2,
};
use kicad_ipc::proto::kiapi::schematic::commands::PlaceSymbolFromLibrary;
use kicad_ipc::proto::kiapi::schematic::types::{
    Junction, LocalLabel, NoConnectMarker, SchematicGraphicShape, SchematicLabelSpinStyle, SchematicLine,
    SchematicLineType, SchematicPin, SchematicPinOrientation, SchematicSymbol, SchematicSymbolInstance,
    SchematicSymbolOrientation, SchematicSymbolTransform, SchematicSymbolType, SchematicText,
};
use kicad_ipc::Client;
use prost::Name;
use prost_types::{Any, FieldMask};
use serde::Serialize;

use crate::errors::from_ipc_error;
use crate::schematic::symbol_info;
use crate::session::Session;

/// How long a headless server may still be loading libraries before "not found" is final, when
/// looking up symbols for a plan.
const LIBRARY_LOAD_GRACE: Duration = Duration::from_secs(90);
/// At apply time the plan's symbols were already found in the library, so "not found" is final
/// almost at once.
const PLACE_GRACE: Duration = Duration::from_secs(5);

fn vector(p: Point) -> Option<Vector2> {
    Some(Vector2 {
        x_nm: p.x.0,
        y_nm: p.y.0,
    })
}

/// The placement orientation, or `None` for 0° so KiCad uses its default.
fn sso(orientation: Orientation) -> Option<i32> {
    let o = match orientation.rotation_deg {
        90 => SchematicSymbolOrientation::Sso90,
        180 => SchematicSymbolOrientation::Sso180,
        270 => SchematicSymbolOrientation::Sso270,
        _ => return None,
    };
    Some(o as i32)
}

#[derive(Debug, Clone, Serialize)]
pub struct PlacedComponent {
    pub component_id: String,
    pub reference: String,
    pub symbol_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetCheck {
    pub net: String,
    pub expected: BTreeSet<String>,
    /// Pins of the KiCad net that contains the first expected pin.
    pub actual: BTreeSet<String>,
    pub kicad_net_name: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub placed: Vec<PlacedComponent>,
    pub wires_created: usize,
    pub labels_created: usize,
    /// KiCad undo steps this apply produced (0 when the session has no commits).
    pub undo_steps: u32,
    pub verification: Vec<NetCheck>,
    pub verified: bool,
    /// Where the previous version of the file was kept (file sessions only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
    /// Existing symbols moved and drawing items deleted (a tidy).
    #[serde(skip_serializing_if = "is_zero")]
    pub moved: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub removed: usize,
    /// Reference or value text moved or hidden.
    #[serde(skip_serializing_if = "is_zero")]
    pub fields_changed: usize,
    /// Positions and turns read back from the document and compared with the plan.
    #[serde(skip_serializing_if = "is_zero")]
    pub placements_checked: usize,
    /// What differed (empty when every placement was as planned).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub placement_problems: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Items created so far, for cleanup when an apply fails part way.
#[derive(Default)]
struct Created {
    /// Created in the current step; dropped with its commit if the step fails.
    pending: Vec<String>,
    /// Created by steps that completed; must be deleted explicitly on a later failure.
    committed: Vec<String>,
}

mod edit;

impl Session {
    /// Pin geometry of library symbols, before placement. Missing entries are left out.
    pub async fn library_symbols(&self, ids: &[LibraryId]) -> Result<BTreeMap<LibraryId, LibrarySymbol>, ToolError> {
        let item_ids: Vec<LibraryIdentifier> = ids
            .iter()
            .map(|id| LibraryIdentifier {
                library_nickname: id.library().into(),
                entry_name: id.entry().into(),
            })
            .collect();
        let started = Instant::now();
        let response: GetItemsResponse = loop {
            let result = {
                let mut inner = self.inner.lock().await;
                inner
                    .client
                    .call::<_, GetItemsResponse>(&GetItemsFromLibrary {
                        r#type: LibraryType::LtSymbol as i32,
                        document: Some(self.document.clone()),
                        item_ids: item_ids.clone(),
                    })
                    .await
            };
            match result {
                // Libraries may still be loading in a headless server.
                Ok(r) if r.items.len() < ids.len() && started.elapsed() < LIBRARY_LOAD_GRACE && r.items.is_empty() => {
                    tokio::time::sleep(Duration::from_millis(500)).await
                }
                Ok(r) => break r,
                Err(kicad_ipc::Error::Status { ref message, .. })
                    if message.contains("not found") && started.elapsed() < LIBRARY_LOAD_GRACE =>
                {
                    tokio::time::sleep(Duration::from_millis(500)).await
                }
                Err(e) => return Err(from_ipc_error(&e, false)),
            }
        };
        let mut out = BTreeMap::new();
        for any in &response.items {
            let Ok(sym) = any.to_msg::<SchematicSymbol>() else {
                continue;
            };
            let Some(id) = &sym.id else { continue };
            let Ok(library_id) = LibraryId::parse(&format!("{}:{}", id.library_nickname, id.entry_name)) else {
                continue;
            };
            let mut pins = Vec::new();
            let mut outline: Vec<Point> = Vec::new();
            for child in &sym.items {
                let Some(item) = &child.item else { continue };
                if item.type_url == SchematicGraphicShape::type_url() {
                    if let Ok(shape) = item.to_msg::<SchematicGraphicShape>() {
                        outline.extend(shape_points(shape.shape.as_ref()));
                    }
                    continue;
                }
                if item.type_url != SchematicPin::type_url() {
                    continue;
                }
                let Ok(pin) = item.to_msg::<SchematicPin>() else {
                    continue;
                };
                let p = pin.position.unwrap_or_default();
                let local = Point::new(Nm(p.x_nm), Nm(p.y_nm));
                pins.push(LibraryPin {
                    outward: outward(pin.orientation, local),
                    electrical_type: crate::schematic::electrical_type(pin.electrical_type).to_owned(),
                    number: pin.number,
                    name: pin.name,
                    local,
                    unit: child.unit.map(|u| u.unit.max(0) as u32).unwrap_or(0),
                });
            }
            let prefix = sym
                .reference_field
                .as_ref()
                .and_then(|f| f.text.as_ref())
                .map(|t| t.text.trim_end_matches(['?', '*']).to_owned())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "U".into());
            let is_power = matches!(
                SchematicSymbolType::try_from(sym.r#type),
                Ok(SchematicSymbolType::SstGlobalPower | SchematicSymbolType::SstLocalPower)
            );
            out.insert(
                library_id.clone(),
                LibrarySymbol {
                    library_id,
                    reference_prefix: prefix,
                    unit_count: sym.unit_count.max(1),
                    is_power,
                    body: Rect::from_points(outline),
                    pins,
                },
            );
        }
        Ok(out)
    }

    /// References and extent of what is already in the document.
    pub async fn existing_document(&self) -> Result<ExistingDocument, ToolError> {
        let symbols = self.list_symbols().await?;
        Ok(ExistingDocument {
            references: symbols.iter().map(|s| s.reference.clone()).collect(),
            max_y: symbols
                .iter()
                .map(|s| Nm::from_mm(s.position_mm.y).unwrap_or_default())
                .max(),
        })
    }

    /// The page and title block of the open document.
    pub async fn sheet(&self) -> Result<kicad_domain::sheet::SheetSettings, ToolError> {
        let mut inner = self.inner.lock().await;
        crate::sheet::read(&mut inner.client, &self.document).await
    }

    /// Sets the page and title block and reads them back. Nothing else in the document is touched,
    /// so there is nothing to verify beyond the read-back.
    async fn apply_sheet(&self, change: &kicad_domain::sheet::SheetChange) -> Result<ApplyReport, ToolError> {
        let after = {
            let mut inner = self.inner.lock().await;
            crate::sheet::apply(&mut inner.client, &self.document, change).await?
        };
        let problems = if after.same_as(&change.to) {
            vec![]
        } else {
            vec![format!(
                "KiCad now has {:?}, not what was planned ({:?})",
                kicad_domain::sheet::page_text(&after.page),
                kicad_domain::sheet::page_text(&change.to.page)
            )]
        };
        Ok(ApplyReport {
            placed: vec![],
            wires_created: 0,
            labels_created: 0,
            undo_steps: 0,
            verification: vec![],
            verified: problems.is_empty(),
            backup_path: None,
            moved: 0,
            removed: 0,
            fields_changed: 0,
            placements_checked: change.describe().len(),
            placement_problems: problems,
        })
    }

    /// A fingerprint of the document's symbols and connectivity. A plan made against one
    /// fingerprint is stale once it changes.
    pub async fn fingerprint(&self) -> Result<String, ToolError> {
        let symbols = self.list_symbols().await?;
        let nets = if self.capabilities().is_available(Capability::SchNetlist) {
            self.netlist().await?
        } else {
            vec![]
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for s in &symbols {
            (
                s.id.as_str(),
                &s.reference,
                &s.value,
                &s.library_id,
                s.position_mm.x.to_bits(),
                s.position_mm.y.to_bits(),
            )
                .hash(&mut h);
            (
                s.orientation.rotation_deg,
                s.orientation.mirror_x,
                s.orientation.mirror_y,
            )
                .hash(&mut h);
        }
        for n in &nets {
            n.name.hash(&mut h);
            for p in &n.pins {
                (&p.reference, &p.pin).hash(&mut h);
            }
            n.other_items.hash(&mut h);
        }
        // The drawing too: a tidy plan deletes particular wires, labels, and junctions.
        let design = self.design().await?;
        for w in &design.wires {
            (&w.id, w.start.x.0, w.start.y.0, w.end.x.0, w.end.y.0).hash(&mut h);
        }
        for l in &design.labels {
            (&l.id, &l.text, l.position.x.0, l.position.y.0).hash(&mut h);
        }
        for j in &design.junctions {
            (&j.id, j.position.x.0, j.position.y.0).hash(&mut h);
        }
        Ok(format!("{:016x}", h.finish()))
    }

    /// Applies `plan` if the document still has `expected_fingerprint`. Grouped into one commit
    /// where KiCad supports commits; otherwise created items are deleted again if a step fails.
    /// Connectivity is read back and compared with the plan's expected nets.
    pub async fn apply_plan(&self, plan: &OperationPlan, expected_fingerprint: &str) -> Result<ApplyReport, ToolError> {
        for c in [Capability::SchPlaceSymbol, Capability::SchEditItems] {
            if !self.capabilities().is_available(c) {
                return Err(ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    format!("this session cannot do {c:?}"),
                ));
            }
        }
        if kicad_domain::validation::has_errors(&plan.findings) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "the plan has errors and cannot be applied",
            ));
        }
        let current = self.fingerprint().await?;
        if current != expected_fingerprint {
            return Err(ToolError::new(
                ErrorCode::StalePlan,
                "the schematic changed since the plan was made; make a new plan",
            ));
        }
        if let Some(change) = &plan.sheet {
            return self.apply_sheet(change).await;
        }

        if !plan.moves.is_empty()
            || !plan.removals.is_empty()
            || !plan.field_changes.is_empty()
            || !plan.property_changes.is_empty()
        {
            return self.apply_edit(plan).await;
        }

        let use_commit = self.capabilities().is_available(Capability::Commits);
        let header = || {
            Some(ItemHeader {
                document: Some(self.document.clone()),
                ..Default::default()
            })
        };
        let mut inner = self.inner.lock().await;
        let client = &mut inner.client;
        let mut created = Created::default();

        // Step 1 places the symbols. Inside an open commit a newly placed symbol is not yet part
        // of the document, so field updates must wait for the next commit (docs/tasks.md, 3.4).
        let commit = begin_commit(client, use_commit, &header).await?;
        let placed = match place_symbols(client, &header, plan, &mut created).await {
            Ok(placed) => placed,
            Err(err) => {
                let _ = end_commit(client, commit, false, "", &header).await;
                return Err(undo(client, &header, &created, use_commit, err).await);
            }
        };
        if let Err(err) = end_commit(client, commit, true, &format!("AI: place {}", plan.circuit), &header).await {
            return Err(undo(client, &header, &created, use_commit, err).await);
        }
        created.committed.append(&mut created.pending);

        // Step 2 sets field values and adds wires and labels.
        let commit = begin_commit(client, use_commit, &header)
            .await
            .map_err(|e| e.with_mutation_state(MutationState::Applied))?;
        let (wires, labels) = match finish(client, &header, plan, &placed, &mut created).await {
            Ok(counts) => counts,
            Err(err) => {
                let _ = end_commit(client, commit, false, "", &header).await;
                return Err(undo(client, &header, &created, use_commit, err).await);
            }
        };
        if let Err(err) = end_commit(client, commit, true, &format!("AI: connect {}", plan.circuit), &header).await {
            return Err(undo(client, &header, &created, use_commit, err).await);
        }
        drop(inner);

        let verification = self.verify(plan).await?;
        let verified = verification.iter().all(|c| c.ok);
        Ok(ApplyReport {
            placed: placed.into_iter().map(|(p, _)| p).collect(),
            wires_created: wires,
            labels_created: labels,
            undo_steps: if use_commit { 2 } else { 0 },
            verification,
            verified,
            backup_path: None,
            moved: 0,
            removed: 0,
            fields_changed: 0,
            placements_checked: 0,
            placement_problems: vec![],
        })
    }

    async fn verify(&self, plan: &OperationPlan) -> Result<Vec<NetCheck>, ToolError> {
        Ok(verify_nets(plan, &self.netlist().await?, false))
    }

    /// Saves the document to its own path.
    pub async fn save(&self) -> Result<(), ToolError> {
        if !self.capabilities().is_available(Capability::SchSave) {
            return Err(ToolError::new(
                ErrorCode::FeatureUnavailable,
                "this session cannot save",
            ));
        }
        let mut inner = self.inner.lock().await;
        inner
            .client
            .call_any(&SaveDocument {
                document: Some(self.document.clone()),
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        self.freshness.reset(self.document_path());
        Ok(())
    }

    /// Writes the current in-memory document (with its project) into `dir`, which must exist,
    /// and returns the schematic path there. The session's own file is not touched.
    pub async fn save_copy(&self, dir: &Path) -> Result<PathBuf, ToolError> {
        let name = self
            .document_path()
            .file_name()
            .map(|n| n.to_owned())
            .unwrap_or_else(|| "copy.kicad_sch".into());
        let target = dir.join(name);
        let mut inner = self.inner.lock().await;
        inner
            .client
            .call_any(&SaveCopyOfDocument {
                document: Some(self.document.clone()),
                path: target.display().to_string(),
                options: Some(SaveOptions {
                    overwrite: true,
                    include_project: true,
                }),
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        Ok(target)
    }
}

async fn place(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    library_id: &LibraryId,
    reference: &str,
    position: Point,
    orientation: Orientation,
) -> Result<SchematicSymbolInstance, ToolError> {
    let request = PlaceSymbolFromLibrary {
        header: header(),
        lib_id: Some(LibraryIdentifier {
            library_nickname: library_id.library().into(),
            entry_name: library_id.entry().into(),
        }),
        position: vector(position),
        orientation: sso(orientation),
        reference: Some(reference.into()),
        ..Default::default()
    };
    let started = Instant::now();
    loop {
        match client.call::<_, PlaceFromLibraryResponse>(&request).await {
            Ok(r) => {
                return r
                    .item
                    .ok_or_else(|| ToolError::new(ErrorCode::KicadError, "placement returned no item"))?
                    .to_msg::<SchematicSymbolInstance>()
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("undecodable placed symbol: {e}")))
            }
            Err(kicad_ipc::Error::Status { message, .. })
                if message.contains("not found") && started.elapsed() < PLACE_GRACE =>
            {
                tokio::time::sleep(Duration::from_millis(500)).await
            }
            Err(e) => {
                let mut err = from_ipc_error(&e, true);
                if err.code == ErrorCode::InvalidArgument && e.to_string().contains("not found") {
                    err.code = ErrorCode::SymbolNotFound;
                }
                return Err(err);
            }
        }
    }
}

/// Updates only the named fields: the embedded library definition is left out and a field mask
/// is sent, which avoids duplicated library symbols (docs/tasks.md, 2.6).
async fn update_fields(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    mut symbol: SchematicSymbolInstance,
    value: &str,
    footprint: Option<&str>,
    fields: &BTreeMap<String, String>,
    mirror: Option<Orientation>,
) -> Result<SchematicSymbolInstance, ToolError> {
    let mut paths = vec!["value_field".to_owned()];
    // Mirroring cannot be set at placement; it goes with the field update, masked to `transform`.
    if let Some(o) = mirror.filter(|o| o.mirror_x || o.mirror_y) {
        symbol.transform = Some(SchematicSymbolTransform {
            orientation: sso(o).unwrap_or(SchematicSymbolOrientation::Sso0 as i32),
            mirror_x: o.mirror_x,
            mirror_y: o.mirror_y,
        });
        paths.push("transform".into());
    }
    symbol.definition = None;
    if let Some(t) = symbol.value_field.as_mut().and_then(|f| f.text.as_mut()) {
        t.text = value.into();
    }
    if let Some(fp) = footprint {
        if let Some(t) = symbol.footprint_field.as_mut().and_then(|f| f.text.as_mut()) {
            t.text = fp.into();
            paths.push("footprint_field".into());
        }
    }
    if !fields.is_empty() {
        set_text_fields(&mut symbol, fields, &mut paths);
    }
    send_symbol_update(client, header, symbol, paths).await
}

/// Writes Datasheet, Description, and custom fields (new ones hidden) on a symbol, and adds the
/// paths of the parts it changed to the field mask.
fn set_text_fields(symbol: &mut SchematicSymbolInstance, fields: &BTreeMap<String, String>, paths: &mut Vec<String>) {
    let mut add_path = |p: &str| {
        if !paths.iter().any(|x| x == p) {
            paths.push(p.to_owned());
        }
    };
    for (name, text) in fields {
        let standard = match name.as_str() {
            "Datasheet" => Some(("datasheet_field", &mut symbol.datasheet_field)),
            "Description" => Some(("description_field", &mut symbol.description_field)),
            _ => None,
        };
        if let Some((path, field)) = standard {
            if let Some(t) = field.as_mut().and_then(|f| f.text.as_mut()) {
                t.text = text.clone();
                add_path(path);
                continue;
            }
        }
        match symbol.user_fields.iter_mut().find(|f| &f.name == name) {
            Some(f) => {
                if let Some(t) = f.text.as_mut() {
                    t.text = text.clone();
                }
            }
            None => symbol
                .user_fields
                .push(kicad_ipc::proto::kiapi::schematic::types::SchematicField {
                    name: name.clone(),
                    text: Some(kicad_ipc::proto::kiapi::common::types::Text {
                        position: symbol.position,
                        text: text.clone(),
                        ..Default::default()
                    }),
                    visible: false,
                    show_name: false,
                    allow_auto_place: true,
                    ..Default::default()
                }),
        }
        add_path("user_fields");
    }
}

/// Sends `symbol` to KiCad with a field mask naming the only parts that may change, and returns
/// what KiCad stored.
async fn send_symbol_update(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    symbol: SchematicSymbolInstance,
    paths: Vec<String>,
) -> Result<SchematicSymbolInstance, ToolError> {
    let mut h = header().unwrap_or_default();
    h.field_mask = Some(FieldMask { paths });
    let any = Any::from_msg(&symbol).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
    let r: UpdateItemsResponse = client
        .call(&UpdateItems {
            header: Some(h),
            items: vec![any],
        })
        .await
        .map_err(|e| from_ipc_error(&e, true))?;
    let result = r
        .updated_items
        .into_iter()
        .next()
        .ok_or_else(|| ToolError::new(ErrorCode::KicadError, "no update result"))?;
    let code = result.status.as_ref().map(|s| s.code).unwrap_or_default();
    if code != ItemStatusCode::IscOk as i32 {
        let message = result.status.map(|s| s.error_message).unwrap_or_default();
        return Err(ToolError::new(
            ErrorCode::KicadError,
            format!("KiCad rejected the field update: {message}"),
        ));
    }
    result
        .item
        .ok_or_else(|| ToolError::new(ErrorCode::KicadError, "update returned no item"))?
        .to_msg::<SchematicSymbolInstance>()
        .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))
}

/// Opens a commit when the session supports commits.
async fn begin_commit(
    client: &mut Client,
    use_commit: bool,
    header: &impl Fn() -> Option<ItemHeader>,
) -> Result<Option<ProtoKiid>, ToolError> {
    if !use_commit {
        return Ok(None);
    }
    let r: BeginCommitResponse = client
        .call(&BeginCommit { header: header() })
        .await
        .map_err(|e| from_ipc_error(&e, false))?;
    Ok(r.id)
}

/// Commits (`keep`) or drops an open commit; does nothing without one.
async fn end_commit(
    client: &mut Client,
    commit: Option<ProtoKiid>,
    keep: bool,
    message: &str,
    header: &impl Fn() -> Option<ItemHeader>,
) -> Result<(), ToolError> {
    if commit.is_none() {
        return Ok(());
    }
    let action = if keep {
        CommitAction::CmaCommit
    } else {
        CommitAction::CmaDrop
    };
    client
        .call_any(&EndCommit {
            id: commit,
            action: action as i32,
            message: message.to_owned(),
            header: header(),
        })
        .await
        .map(|_| ())
        .map_err(|e| from_ipc_error(&e, true))
}

/// After a failed step: removes everything earlier steps created and reports the original error
/// with the resulting mutation state. A failed step's own commit has already been dropped.
async fn undo(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    created: &Created,
    use_commit: bool,
    err: ToolError,
) -> ToolError {
    // Without commits nothing was rolled back automatically, so the current step's items go too.
    let mut to_delete = created.committed.clone();
    if !use_commit {
        to_delete.extend(created.pending.iter().cloned());
    }
    let undone = if to_delete.is_empty() {
        true
    } else {
        let ids: Vec<ProtoKiid> = to_delete.iter().map(|id| ProtoKiid { value: id.clone() }).collect();
        // Delete inside its own commit where available, so the cleanup is one undo step too.
        match begin_commit(client, use_commit, header).await {
            Ok(commit) => {
                let deleted = client
                    .call_any(&DeleteItems {
                        header: header(),
                        item_ids: ids,
                    })
                    .await
                    .is_ok();
                let closed = end_commit(client, commit, deleted, "AI: undo failed apply", header)
                    .await
                    .is_ok();
                deleted && closed
            }
            Err(_) => false,
        }
    };
    let (state, note) = if undone {
        (MutationState::NotApplied, "all changes were undone")
    } else {
        (MutationState::Unknown, "cleanup failed; read the schematic back")
    };
    ToolError::new(err.code, format!("{} ({note})", err.message)).with_mutation_state(state)
}

/// Step 1: place every symbol and power symbol. Returns each component placement with the
/// instance KiCad created; position, rotation, and reference are read back and checked.
async fn place_symbols(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    plan: &OperationPlan,
    created: &mut Created,
) -> Result<Vec<(PlacedComponent, SchematicSymbolInstance)>, ToolError> {
    let mut placed = Vec::new();
    for op in &plan.operations {
        let (component_id, library_id, reference, position, orientation) = match op {
            Operation::PlaceSymbol {
                component_id,
                library_id,
                reference,
                position,
                orientation,
                ..
            } => (Some(component_id), library_id, reference, position, orientation),
            Operation::PlacePowerSymbol {
                library_id,
                reference,
                position,
                orientation,
                ..
            } => (None, library_id, reference, position, orientation),
            _ => continue,
        };
        let symbol = place(client, header, library_id, reference, *position, *orientation).await?;
        let id = symbol.id.as_ref().map(|k| k.value.clone()).unwrap_or_default();
        created.pending.push(id.clone());
        let info = symbol_info(&symbol)?;
        let at = Point::new(
            Nm::from_mm(info.position_mm.x).unwrap_or_default(),
            Nm::from_mm(info.position_mm.y).unwrap_or_default(),
        );
        if info.reference != *reference || at != *position || info.orientation.rotation_deg != orientation.rotation_deg
        {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!(
                    "{reference} was placed as {} at ({}, {}) mm, rotated {}°",
                    info.reference, info.position_mm.x, info.position_mm.y, info.orientation.rotation_deg
                ),
            ));
        }
        if let Some(component_id) = component_id {
            placed.push((
                PlacedComponent {
                    component_id: component_id.to_string(),
                    reference: reference.clone(),
                    symbol_id: id,
                },
                symbol,
            ));
        }
    }
    Ok(placed)
}

fn label_spin(direction: Dir) -> i32 {
    (match direction {
        Dir::Left => SchematicLabelSpinStyle::SlssLeft,
        Dir::Right => SchematicLabelSpinStyle::SlssRight,
        Dir::Up => SchematicLabelSpinStyle::SlssUp,
        Dir::Down => SchematicLabelSpinStyle::SlssBottom,
    }) as i32
}

/// Step 2: set field values, then add wires, labels, and junctions.
async fn finish(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    plan: &OperationPlan,
    placed: &[(PlacedComponent, SchematicSymbolInstance)],
    created: &mut Created,
) -> Result<(usize, usize), ToolError> {
    for op in &plan.operations {
        let Operation::PlaceSymbol {
            component_id,
            reference,
            value,
            footprint,
            fields,
            orientation,
            ..
        } = op
        else {
            continue;
        };
        let (p, symbol) = placed
            .iter()
            .find(|(p, _)| p.component_id == component_id.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::KicadError, format!("{reference} was not placed")))?;
        let updated = update_fields(
            client,
            header,
            symbol.clone(),
            value,
            footprint.as_deref(),
            fields,
            Some(*orientation),
        )
        .await
        .map_err(|mut e| {
            e.message = format!("{reference} ({}): {}", p.symbol_id, e.message);
            e
        })?;
        let info = symbol_info(&updated)?;
        if !info.orientation.same_effect(*orientation) {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!(
                    "{reference} transform read back as {:?} instead of {:?}",
                    info.orientation, orientation
                ),
            ));
        }
        if info.value != *value {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!("{reference} value read back as '{}' instead of '{value}'", info.value),
            ));
        }
    }

    let encode =
        |m: Result<Any, prost::EncodeError>| m.map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()));
    let mut items = Vec::new();
    let (mut wires, mut labels) = (0, 0);
    for op in &plan.operations {
        match op {
            Operation::AddWire { start, end, .. } => {
                wires += 1;
                items.push(encode(Any::from_msg(&SchematicLine {
                    start: vector(*start),
                    end: vector(*end),
                    r#type: SchematicLineType::SltWire as i32,
                    ..Default::default()
                }))?);
            }
            Operation::AddLabel {
                net,
                position,
                direction,
            } => {
                labels += 1;
                items.push(encode(Any::from_msg(&LocalLabel {
                    position: vector(*position),
                    text: Some(Text {
                        position: vector(*position),
                        text: net.clone(),
                        ..Default::default()
                    }),
                    spin_style: label_spin(*direction),
                    ..Default::default()
                }))?);
            }
            Operation::AddJunction { position } => {
                items.push(encode(Any::from_msg(&Junction {
                    position: vector(*position),
                    ..Default::default()
                }))?);
            }
            Operation::AddNoConnect { position } => {
                items.push(encode(Any::from_msg(&NoConnectMarker {
                    position: vector(*position),
                    ..Default::default()
                }))?);
            }
            Operation::AddText {
                text,
                position,
                size,
                angle_deg,
                h,
                v,
            } => {
                labels += 1;
                items.push(encode(Any::from_msg(&SchematicText {
                    text: Some(Text {
                        position: vector(*position),
                        text: text.clone(),
                        attributes: Some(kicad_ipc::proto::kiapi::common::types::TextAttributes {
                            size: vector(Point::new(*size, *size)),
                            angle: Some(kicad_ipc::proto::kiapi::common::types::Angle {
                                value_degrees: f64::from(*angle_deg),
                            }),
                            horizontal_alignment: match h {
                                kicad_domain::text::HAlign::Left => 1,
                                kicad_domain::text::HAlign::Center => 2,
                                kicad_domain::text::HAlign::Right => 3,
                            },
                            vertical_alignment: match v {
                                kicad_domain::text::VAlign::Top => 1,
                                kicad_domain::text::VAlign::Center => 2,
                                kicad_domain::text::VAlign::Bottom => 3,
                            },
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }))?);
            }
            Operation::AddShape { kind, stroke, filled } => {
                labels += 1;
                let stroke_attrs = Some(kicad_ipc::proto::kiapi::common::types::StrokeAttributes {
                    width: Some(kicad_ipc::proto::kiapi::common::types::Distance { value_nm: stroke.0 }),
                    ..Default::default()
                });
                match kind {
                    kicad_domain::schematic::ShapeKind::Line { start, end } => {
                        items.push(encode(Any::from_msg(&SchematicLine {
                            start: vector(*start),
                            end: vector(*end),
                            r#type: SchematicLineType::SltGraphic as i32,
                            stroke: stroke_attrs,
                            ..Default::default()
                        }))?);
                    }
                    _ => {
                        let geometry = match kind {
                            kicad_domain::schematic::ShapeKind::Rectangle { min, max } => {
                                graphic_shape::Geometry::Rectangle(
                                    kicad_ipc::proto::kiapi::common::types::GraphicRectangleAttributes {
                                        top_left: vector(*min),
                                        bottom_right: vector(*max),
                                        corner_radius: None,
                                    },
                                )
                            }
                            kicad_domain::schematic::ShapeKind::Circle { center, radius } => {
                                graphic_shape::Geometry::Circle(
                                    kicad_ipc::proto::kiapi::common::types::GraphicCircleAttributes {
                                        center: vector(*center),
                                        radius_point: vector(Point::new(center.x + *radius, center.y)),
                                    },
                                )
                            }
                            kicad_domain::schematic::ShapeKind::Line { .. } => unreachable!("handled above"),
                        };
                        items.push(encode(Any::from_msg(&SchematicGraphicShape {
                            shape: Some(GraphicShape {
                                attributes: Some(kicad_ipc::proto::kiapi::common::types::GraphicAttributes {
                                    stroke: stroke_attrs,
                                    fill: Some(kicad_ipc::proto::kiapi::common::types::GraphicFillAttributes {
                                        fill_type: if *filled {
                                            kicad_ipc::proto::kiapi::common::types::GraphicFillType::GftFilled as i32
                                        } else {
                                            kicad_ipc::proto::kiapi::common::types::GraphicFillType::GftUnfilled as i32
                                        },
                                        color: None,
                                    }),
                                }),
                                geometry: Some(geometry),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }))?);
                    }
                }
            }
            Operation::PlaceSymbol { .. } | Operation::PlacePowerSymbol { .. } => {}
        }
    }
    if items.is_empty() {
        return Ok((0, 0));
    }
    let expected = items.len();
    let r: CreateItemsResponse = client
        .call(&CreateItems {
            header: header(),
            items,
            ..Default::default()
        })
        .await
        .map_err(|e| from_ipc_error(&e, true))?;
    created.pending.extend(
        r.created_items
            .iter()
            .filter_map(|c| c.item.as_ref().and_then(item_kiid)),
    );
    let ok = r
        .created_items
        .iter()
        .filter(|c| c.status.as_ref().map(|s| s.code) == Some(ItemStatusCode::IscOk as i32))
        .count();
    if ok != expected {
        let reasons: BTreeSet<String> = r
            .created_items
            .iter()
            .filter_map(|c| c.status.as_ref())
            .filter(|s| s.code != ItemStatusCode::IscOk as i32)
            .map(|s| s.error_message.clone())
            .collect();
        return Err(ToolError::new(
            ErrorCode::KicadError,
            format!(
                "KiCad created {ok} of {expected} wires, labels, and junctions: {}",
                reasons.into_iter().collect::<Vec<_>>().join("; ")
            ),
        ));
    }
    Ok((wires, labels))
}

/// The KIID of a created wire, label, or junction.
fn item_kiid(any: &Any) -> Option<String> {
    if any.type_url == SchematicLine::type_url() {
        any.to_msg::<SchematicLine>().ok()?.id.map(|k| k.value)
    } else if any.type_url == LocalLabel::type_url() {
        any.to_msg::<LocalLabel>().ok()?.id.map(|k| k.value)
    } else if any.type_url == Junction::type_url() {
        any.to_msg::<Junction>().ok()?.id.map(|k| k.value)
    } else if any.type_url == NoConnectMarker::type_url() {
        any.to_msg::<NoConnectMarker>().ok()?.id.map(|k| k.value)
    } else if any.type_url == SchematicText::type_url() {
        any.to_msg::<SchematicText>().ok()?.id.map(|k| k.value)
    } else if any.type_url == SchematicGraphicShape::type_url() {
        any.to_msg::<SchematicGraphicShape>().ok()?.id.map(|k| k.value)
    } else {
        None
    }
}

/// Direction a pin points away from its symbol, from KiCad's pin orientation: the orientation
/// names the direction the pin line extends from its connection point into the body, so the
/// outward direction is the opposite (verified against Device:R in Phase 2).
fn outward(orientation: i32, local: Point) -> Dir {
    match SchematicPinOrientation::try_from(orientation) {
        Ok(SchematicPinOrientation::SpoRight) => Dir::Left,
        Ok(SchematicPinOrientation::SpoLeft) => Dir::Right,
        Ok(SchematicPinOrientation::SpoDown) => Dir::Up,
        Ok(SchematicPinOrientation::SpoUp) => Dir::Down,
        // Unknown: guess from where the pin sits relative to the symbol origin.
        _ if local.x.0.abs() >= local.y.0.abs() => {
            if local.x.0 < 0 {
                Dir::Left
            } else {
                Dir::Right
            }
        }
        _ if local.y.0 < 0 => Dir::Up,
        _ => Dir::Down,
    }
}

/// Compares the nets KiCad reports with the nets the plan expects. `kicad-cli`'s netlister
/// leaves power symbols and flags out, so a check against it passes `ignore_power_symbols`.
pub(crate) fn verify_nets(
    plan: &OperationPlan,
    nets: &[kicad_domain::NetInfo],
    ignore_power_symbols: bool,
) -> Vec<NetCheck> {
    let mut checks = Vec::new();
    for expected in &plan.expected_nets {
        let wanted: BTreeSet<String> = expected
            .pins
            .iter()
            .filter(|p| !(ignore_power_symbols && p.starts_with('#')))
            .cloned()
            .collect();
        let Some(first) = wanted.iter().next().cloned() else {
            continue;
        };
        let found = nets
            .iter()
            .find(|n| n.pins.iter().any(|p| format!("{}.{}", p.reference, p.pin) == first));
        let actual: BTreeSet<String> = found
            .map(|n| {
                n.pins
                    .iter()
                    .map(|p| format!("{}.{}", p.reference, p.pin))
                    // Power symbols the layout added (#PWR…) name the net; they are not circuit parts.
                    .filter(|r| !r.starts_with('#') || wanted.contains(r))
                    .collect()
            })
            .unwrap_or_default();
        let name_ok = found.is_some_and(|n| expected.name_open || n.name.trim_start_matches('/') == expected.name);
        checks.push(NetCheck {
            net: expected.name.clone(),
            ok: actual == wanted && name_ok,
            expected: wanted,
            actual,
            kicad_net_name: found.map(|n| n.name.clone()),
        });
    }
    checks
}

/// The points that bound a graphic shape, for the symbol body box.
fn shape_points(shape: Option<&GraphicShape>) -> Vec<Point> {
    let pt = |v: &Option<Vector2>| v.as_ref().map(|v| Point::new(Nm(v.x_nm), Nm(v.y_nm)));
    let Some(geometry) = shape.and_then(|s| s.geometry.as_ref()) else {
        return vec![];
    };
    match geometry {
        graphic_shape::Geometry::Segment(s) => [pt(&s.start), pt(&s.end)].into_iter().flatten().collect(),
        graphic_shape::Geometry::Rectangle(r) => [pt(&r.top_left), pt(&r.bottom_right)].into_iter().flatten().collect(),
        graphic_shape::Geometry::Arc(a) => [pt(&a.start), pt(&a.mid), pt(&a.end)].into_iter().flatten().collect(),
        graphic_shape::Geometry::Circle(c) => match (pt(&c.center), pt(&c.radius_point)) {
            (Some(center), Some(edge)) => {
                let r = Nm((((edge.x.0 - center.x.0) as f64).hypot((edge.y.0 - center.y.0) as f64)) as i64);
                vec![
                    Point::new(center.x - r, center.y - r),
                    Point::new(center.x + r, center.y + r),
                ]
            }
            _ => vec![],
        },
        graphic_shape::Geometry::Polygon(set) => set
            .polygons
            .iter()
            .filter_map(|p| p.outline.as_ref())
            .flat_map(|line| line.nodes.iter())
            .flat_map(|node| match &node.geometry {
                Some(poly_line_node::Geometry::Point(p)) => vec![Point::new(Nm(p.x_nm), Nm(p.y_nm))],
                Some(poly_line_node::Geometry::Arc(a)) => {
                    [pt(&a.start), pt(&a.mid), pt(&a.end)].into_iter().flatten().collect()
                }
                None => vec![],
            })
            .collect(),
        graphic_shape::Geometry::Bezier(b) => [pt(&b.start), pt(&b.control1), pt(&b.control2), pt(&b.end)]
            .into_iter()
            .flatten()
            .collect(),
        _ => vec![],
    }
}
