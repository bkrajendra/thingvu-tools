// SPDX-License-Identifier: GPL-3.0-or-later
//! The file provider (plan §5.3, KiCad 10 fallback): a schematic edited as a file while no KiCad
//! editor has it open. Reads come from the file, connectivity from `kicad-cli`'s netlister, and
//! changes are written to a copy that `kicad-cli` must accept — with exactly the planned
//! connectivity — before it atomically replaces the original.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use kicad_cli_runner::{netlist, KicadInstall};
use kicad_domain::{
    Backend, Capability, CapabilitySet, ErrorCode, ExistingDocument, Kiid, LibraryId, LibrarySymbol, MutationState,
    NetInfo, NetPin, Operation, OperationPlan, PinInfo, PointMm, Support, SymbolInfo, SymbolPins, ToolError,
};
use kicad_sexpr::{
    add_plan, describe, empty_schematic, DocError, Document, Node, PlacedSymbol, SymbolLibrary, MAX_VERSION,
    MIN_VERSION,
};
use tokio::sync::Mutex;

use crate::freshness::{Freshness, ReloadReport};
use crate::mutate::{verify_nets, ApplyReport, PlacedComponent};
use crate::schematic::{natural_key, natural_reference, OpenDocument};
use crate::session::{
    check_target_with, design_counts, lock_file, new_session_id, OpenTarget, Provider, SessionConfig, SessionStatus,
};

/// The newest schematic format a `kicad-cli` of this series reads and writes.
fn format_of(series: u32) -> u32 {
    if series >= 11 {
        MAX_VERSION
    } else {
        MIN_VERSION
    }
}

/// The oldest install that can read a file of `format`: a file stays readable by every KiCad
/// that can read the one that edits it.
fn pick_cli(installs: &[KicadInstall], format: Option<u32>) -> Option<&KicadInstall> {
    installs
        .iter()
        .filter(|i| i.version.series() >= 10 && format_of(i.version.series()) >= format.unwrap_or(0))
        .min_by_key(|i| (i.version.series(), i.version.minor, i.version.patch))
}

fn doc_error(e: DocError) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, e.to_string())
}

fn io_error(what: &str, path: &Path, e: std::io::Error) -> ToolError {
    ToolError::new(ErrorCode::KicadError, format!("cannot {what} {}: {e}", path.display()))
}

fn kiid(value: &str) -> Result<Kiid, ToolError> {
    Kiid::new(value).map_err(|e| ToolError::new(ErrorCode::KicadError, format!("the file has a bad object id: {e}")))
}

fn symbol_info(s: &PlacedSymbol) -> Result<SymbolInfo, ToolError> {
    Ok(SymbolInfo {
        id: kiid(&s.uuid)?,
        reference: s.reference.clone(),
        value: s.value.clone(),
        library_id: s.library_id.clone(),
        footprint: s.footprint.clone(),
        position_mm: PointMm::from(s.position),
        orientation: s.orientation,
        unit: s.unit,
        sheet_path: "/".into(),
    })
}

fn fingerprint_of(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn library_key(id: &LibraryId) -> String {
    format!("{}:{}", id.library(), id.entry())
}

fn file_capabilities(cli: &KicadInstall, render: Option<&KicadInstall>) -> CapabilitySet {
    let mut caps = CapabilitySet::default();
    let file = |note: &str| Support::via(Backend::ProjectFile).with_note(note);
    let via_cli = || Support::via(Backend::KicadCli).with_note(format!("kicad-cli {}", cli.version.full));
    caps.set(Capability::SessionStatus, Support::via(Backend::ProjectFile));
    caps.set(
        Capability::SessionOpenDocuments,
        file("the session's one schematic file"),
    );
    caps.set(
        Capability::DocumentLifecycle,
        Support::unavailable("a file session has no editor document"),
    );
    caps.set(
        Capability::Commits,
        Support::unavailable("changes are verified in a copy, then the file is replaced in one step"),
    );
    caps.set(
        Capability::SchReadHierarchy,
        Support::unavailable("only the root schematic file is read"),
    );
    caps.set(Capability::SchReadSymbols, Support::via(Backend::ProjectFile));
    caps.set(Capability::SchNetlist, via_cli());
    caps.set(
        Capability::SchPlaceSymbol,
        file("symbols come from the file itself or the KiCad install's libraries"),
    );
    caps.set(Capability::SchEditItems, Support::via(Backend::ProjectFile));
    caps.set(
        Capability::SchSave,
        file("every applied plan is written to the file immediately"),
    );
    caps.set(Capability::SchErc, via_cli());
    caps.set(Capability::SchExportNetlist, via_cli());
    caps.set(
        Capability::SchRender,
        match render {
            Some(r) => Support::via(Backend::KicadCli).with_note(format!("kicad-cli {} (PNG)", r.version.full)),
            None => Support::unavailable("kicad-cli 10 cannot export PNG; install KiCad 11 (or 10.99) to render"),
        },
    );
    caps
}

/// Writes `bytes` to `path` and flushes them to disk before returning.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Removes verification copies that an interrupted apply left behind (ours, and old enough that
/// no other session can still be using them).
fn remove_stale_candidates(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(".thingsvu-") && name.ends_with(".kicad_sch")) {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok());
        if age.is_some_and(|a| a > Duration::from_secs(600)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub struct FileSession {
    id: String,
    document_path: PathBuf,
    cli: KicadInstall,
    render_cli: Option<KicadInstall>,
    symbol_dir: PathBuf,
    runtime_dir: PathBuf,
    cli_timeout: Duration,
    capabilities: CapabilitySet,
    libraries: StdMutex<HashMap<String, Arc<SymbolLibrary>>>,
    /// Serializes changes to the file.
    writing: Mutex<()>,
    scratch_counter: AtomicU64,
    /// When the file was last read or written by this session.
    freshness: Freshness,
    /// Opened for reading only: a file an editor has open may be opened, and nothing is written.
    read_only: bool,
}

impl FileSession {
    /// Opens a schematic that no editor has open, or creates a new empty one. The `kicad-cli`
    /// used is the oldest install that can read the file.
    pub async fn open(
        config: &SessionConfig,
        installs: &[KicadInstall],
        target: OpenTarget,
    ) -> Result<Self, ToolError> {
        Self::open_with(config, installs, target, false).await
    }

    /// As [`FileSession::open`]; with `read_only` the file may be one an editor has open, and no
    /// change is ever written to it.
    pub async fn open_with(
        config: &SessionConfig,
        installs: &[KicadInstall],
        target: OpenTarget,
        read_only: bool,
    ) -> Result<Self, ToolError> {
        if read_only && matches!(target, OpenTarget::New(_)) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "read_only cannot create a new schematic",
            ));
        }
        let path = check_target_with(&target, read_only)?;
        let existing_format = match &target {
            OpenTarget::Existing(_) => {
                let text = std::fs::read_to_string(&path).map_err(|e| io_error("read", &path, e))?;
                Some(Document::parse(text).map_err(doc_error)?.version())
            }
            OpenTarget::New(_) => None,
        };
        let cli = pick_cli(installs, existing_format).cloned().ok_or_else(|| {
            ToolError::new(
                ErrorCode::KicadNotFound,
                match existing_format {
                    Some(f) => format!("no installed KiCad can read schematic format {f}"),
                    None => "no KiCad 10 or newer install was found".to_owned(),
                },
            )
        })?;
        if let Some(dir) = path.parent() {
            remove_stale_candidates(dir);
        }
        if matches!(target, OpenTarget::New(_)) {
            let text = empty_schematic(
                format_of(cli.version.series()),
                &format!("{}.{}", cli.version.major, cli.version.minor),
            );
            std::fs::write(&path, text).map_err(|e| io_error("create", &path, e))?;
        }
        let render_cli = installs
            .iter()
            .filter(|i| i.version.series() >= 11)
            .min_by_key(|i| (i.version.minor, i.version.patch))
            .cloned();
        let symbol_dir = std::env::var_os("KICAD_SYMBOL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                cli.cli
                    .parent()
                    .and_then(Path::parent)
                    .map(|root| root.join("share").join("kicad").join("symbols"))
                    .unwrap_or_default()
            });
        let capabilities = file_capabilities(&cli, render_cli.as_ref());
        Ok(Self {
            id: new_session_id(),
            cli,
            render_cli,
            symbol_dir,
            runtime_dir: config.runtime_dir.clone(),
            cli_timeout: config.cli_timeout,
            capabilities,
            libraries: StdMutex::new(HashMap::new()),
            writing: Mutex::new(()),
            scratch_counter: AtomicU64::new(0),
            freshness: Freshness::new(&path),
            read_only,
            document_path: path,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn provider(&self) -> Provider {
        Provider::File
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    pub fn cli(&self) -> Option<&KicadInstall> {
        Some(&self.cli)
    }

    /// The install that renders PNG images, when there is one.
    pub fn render_cli(&self) -> Option<&KicadInstall> {
        self.render_cli.as_ref()
    }

    pub fn kicad_version(&self) -> &str {
        &self.cli.version.full
    }

    pub fn document_path(&self) -> &Path {
        &self.document_path
    }

    pub async fn status(&self) -> SessionStatus {
        SessionStatus {
            session_id: self.id.clone(),
            provider: Provider::File,
            kicad_version: self.cli.version.full.clone(),
            instance_fingerprint: None,
            document: Some(self.document_path.clone()),
            server_pid: None,
            connected: self.document_path.is_file(),
            unsaved_changes: Some(false),
            file_changed_on_disk: self.freshness.changed(&self.document_path),
            read_only: self.read_only,
            freshness_note: (self.freshness.changed(&self.document_path) == Some(true))
                .then(|| "the file changed since it was last read; the next read sees it".to_owned()),
            cli: Some(self.cli.cli.display().to_string()),
            capabilities: self.capabilities.clone(),
        }
    }

    /// Nothing is held open, and nothing is ever unsaved.
    pub async fn close(&self, _discard_unsaved: bool) -> Result<bool, ToolError> {
        Ok(false)
    }

    /// A file session reads the file each time; a reload only notes that the file is read again.
    pub async fn reload(&self, _discard_unsaved: bool) -> Result<ReloadReport, ToolError> {
        let changed = self.freshness.changed(&self.document_path);
        let design = self.read()?.design();
        Ok(ReloadReport {
            reloaded: true,
            provider: Provider::File,
            had_unsaved_changes: Some(false),
            file_changed_on_disk: changed,
            dropped: vec![],
            counts_after: design_counts(&design),
            note: "a file session reads the file each time; nothing was held".into(),
        })
    }

    fn refuse_if_read_only(&self) -> Result<(), ToolError> {
        if self.read_only {
            Err(ToolError::new(
                ErrorCode::ReadOnlyMode,
                "this file session was opened read-only (read_only: true); open it again without that to change the schematic",
            ))
        } else {
            Ok(())
        }
    }

    pub async fn open_documents(&self) -> Result<Vec<OpenDocument>, ToolError> {
        Ok(vec![OpenDocument {
            kind: "schematic",
            project: self.document_path.file_stem().map(|s| s.to_string_lossy().into_owned()),
            project_path: self.document_path.parent().map(|p| p.display().to_string()),
            file: self.document_path.file_name().map(|s| s.to_string_lossy().into_owned()),
        }])
    }

    fn read_bytes(&self) -> Result<Vec<u8>, ToolError> {
        self.freshness.reset(&self.document_path);
        std::fs::read(&self.document_path).map_err(|e| io_error("read", &self.document_path, e))
    }

    fn read(&self) -> Result<Document, ToolError> {
        self.freshness.reset(&self.document_path);
        let text =
            std::fs::read_to_string(&self.document_path).map_err(|e| io_error("read", &self.document_path, e))?;
        Document::parse(text).map_err(doc_error)
    }

    fn library(&self, nickname: &str) -> Option<Arc<SymbolLibrary>> {
        if let Some(lib) = self.libraries.lock().ok()?.get(nickname) {
            return Some(lib.clone());
        }
        if nickname.is_empty() || nickname.contains(['/', '\\', ':']) || nickname.starts_with('.') {
            return None;
        }
        // The project's own library table wins, as it does in KiCad; then the install's libraries.
        let path = self
            .project_library(nickname)
            .unwrap_or_else(|| self.symbol_dir.join(format!("{nickname}.kicad_sym")));
        let text = std::fs::read_to_string(path).ok()?;
        let lib = Arc::new(SymbolLibrary::parse(&text).ok()?);
        self.libraries.lock().ok()?.insert(nickname.to_owned(), lib.clone());
        Some(lib)
    }

    /// A library the schematic's project defines in its `sym-lib-table`.
    fn project_library(&self, nickname: &str) -> Option<PathBuf> {
        let dir = self.document_path.parent()?;
        let table = kicad_sexpr::parse(&std::fs::read_to_string(dir.join("sym-lib-table")).ok()?).ok()?;
        let entry = table.children("lib").find(|l| l.child_text("name") == Some(nickname))?;
        let mut uri = entry.child_text("uri")?.replace("${KIPRJMOD}", &dir.to_string_lossy());
        // Any other variable must be one the environment defines.
        while let Some(start) = uri.find("${") {
            let end = start + uri[start..].find('}')?;
            let value = std::env::var(&uri[start + 2..end]).ok()?;
            uri.replace_range(start..=end, &value);
        }
        Some(PathBuf::from(uri))
    }

    /// The flattened definition of `id`: the file's own copy when it has one (so planning sees
    /// what the file will contain), else the install's library.
    fn definition(&self, doc: &Document, id: &LibraryId) -> Result<Node, ToolError> {
        if let Some(def) = doc.embedded(&library_key(id)) {
            return Ok(def.clone());
        }
        self.library(id.library())
            .and_then(|lib| lib.embeddable(id.library(), id.entry()).ok())
            .ok_or_else(|| {
                ToolError::new(
                    ErrorCode::SymbolNotFound,
                    format!(
                        "symbol {} is not in the schematic, the project's sym-lib-table, or the KiCad libraries at {} \
                         (libraries from the user's global table are not read)",
                        library_key(id),
                        self.symbol_dir.display()
                    ),
                )
            })
    }

    /// Pin geometry of library symbols, before placement. Missing entries are left out.
    pub async fn library_symbols(&self, ids: &[LibraryId]) -> Result<BTreeMap<LibraryId, LibrarySymbol>, ToolError> {
        let doc = self.read()?;
        Ok(ids
            .iter()
            .filter_map(|id| Some((id.clone(), describe(id, &self.definition(&doc, id).ok()?))))
            .collect())
    }

    pub async fn existing_document(&self) -> Result<ExistingDocument, ToolError> {
        let symbols = self.read()?.symbols();
        Ok(ExistingDocument {
            references: symbols.iter().map(|s| s.reference.clone()).collect(),
            max_y: symbols.iter().map(|s| s.position.y).max(),
        })
    }

    /// Every part, wire, label, and junction in the file, and what tidying cannot handle.
    pub async fn design(&self) -> Result<kicad_domain::SchematicDesign, ToolError> {
        Ok(self.read()?.design())
    }

    /// A plan made against one fingerprint is stale once the file changes in any way.
    pub async fn fingerprint(&self) -> Result<String, ToolError> {
        Ok(fingerprint_of(&self.read_bytes()?))
    }

    pub async fn list_symbols(&self) -> Result<Vec<SymbolInfo>, ToolError> {
        let mut symbols = self
            .read()?
            .symbols()
            .iter()
            .map(symbol_info)
            .collect::<Result<Vec<_>, _>>()?;
        symbols.sort_by_key(|s| natural_reference(&s.reference));
        Ok(symbols)
    }

    /// Pins of one symbol, selected by uuid or by a reference that matches exactly one symbol.
    pub async fn symbol_pins(&self, selector: &str) -> Result<SymbolPins, ToolError> {
        let doc = self.read()?;
        let by_id = Kiid::new(selector).ok();
        let symbols = doc.symbols();
        let matches: Vec<&PlacedSymbol> = symbols
            .iter()
            .filter(|s| match &by_id {
                Some(id) => s.uuid.eq_ignore_ascii_case(id.as_str()),
                None => s.reference == selector,
            })
            .collect();
        let symbol = match matches.as_slice() {
            [one] => *one,
            [] => {
                return Err(ToolError::new(
                    ErrorCode::SymbolNotFound,
                    format!("no symbol matches '{selector}'"),
                ))
            }
            many => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "'{selector}' matches {} symbols; use one of their ids: {}",
                        many.len(),
                        many.iter().map(|s| s.uuid.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                ))
            }
        };
        let def = doc.embedded(&symbol.definition).ok_or_else(|| {
            ToolError::new(
                ErrorCode::KicadError,
                format!("the file has no definition for {}", symbol.definition),
            )
        })?;
        let id = LibraryId::parse(&symbol.library_id)
            .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("bad library id in the file: {e}")))?;
        let mut pins = Vec::new();
        for pin in describe(&id, def)
            .pins
            .iter()
            .filter(|p| p.unit == 0 || p.unit == symbol.unit)
        {
            let at = symbol.position
                + symbol.orientation.apply(pin.local).ok_or_else(|| {
                    ToolError::new(
                        ErrorCode::KicadError,
                        "a symbol has a rotation that is not a multiple of 90",
                    )
                })?;
            let pin_id = symbol
                .pins
                .iter()
                .find(|(number, _)| *number == pin.number)
                .map(|(_, uuid)| uuid.as_str())
                .unwrap_or(symbol.uuid.as_str());
            pins.push(PinInfo {
                id: kiid(pin_id)?,
                number: pin.number.clone(),
                name: pin.name.clone(),
                electrical_type: pin.electrical_type.clone(),
                position_mm: PointMm::from(at),
                local_mm: PointMm::from(pin.local),
            });
        }
        pins.sort_by_key(|p| natural_key(&p.number));
        Ok(SymbolPins {
            symbol: symbol_info(symbol)?,
            pins,
        })
    }

    /// A scratch folder that exists for one call.
    fn scratch(&self) -> Result<PathBuf, ToolError> {
        let dir = self
            .runtime_dir
            .join(&self.id)
            .join(format!("file-{}", self.scratch_counter.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).map_err(|e| io_error("create", &dir, e))?;
        Ok(dir)
    }

    /// Connectivity from `kicad-cli`'s netlister for the schematic at `path`.
    async fn netlist_of(&self, path: &Path, symbols: &[PlacedSymbol]) -> Result<Vec<NetInfo>, ToolError> {
        let dir = self.scratch()?;
        let result = netlist::export_netlist(&self.cli, path, &dir.join("out.net"), self.cli_timeout).await;
        let _ = std::fs::remove_dir_all(&dir);
        let netlist = result.map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
        let owner: HashMap<&str, &str> = symbols
            .iter()
            .map(|s| (s.reference.as_str(), s.uuid.as_str()))
            .collect();
        let mut nets = Vec::new();
        for net in &netlist.nets {
            let mut pins = Vec::new();
            for node in &net.nodes {
                let uuid = owner.get(node.reference.as_str()).copied().unwrap_or_default();
                pins.push(NetPin {
                    reference: node.reference.clone(),
                    pin: node.pin.clone(),
                    symbol_id: kiid(if uuid.is_empty() {
                        "00000000-0000-0000-0000-000000000000"
                    } else {
                        uuid
                    })?,
                });
            }
            pins.sort_by(|a, b| {
                (natural_reference(&a.reference), natural_key(&a.pin))
                    .cmp(&(natural_reference(&b.reference), natural_key(&b.pin)))
            });
            nets.push(NetInfo {
                name: net.name.clone(),
                pins,
                other_items: 0,
            });
        }
        nets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(nets)
    }

    pub async fn netlist(&self) -> Result<Vec<NetInfo>, ToolError> {
        let doc = self.read()?;
        let mut nets = self.netlist_of(&self.document_path, &doc.symbols()).await?;
        // The netlister leaves out power symbols and flags; the API's netlist has them, so add
        // them from the drawing and both kinds of session report the same thing.
        let design = doc.design();
        let ids: Vec<LibraryId> = design
            .symbols
            .iter()
            .filter(|s| s.reference.starts_with('#'))
            .filter_map(|s| LibraryId::parse(&s.library_id).ok())
            .chain(
                design
                    .symbols
                    .iter()
                    .filter(|s| !s.reference.starts_with('#'))
                    .filter_map(|s| LibraryId::parse(&s.library_id).ok()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let library = self.library_symbols(&ids).await?;
        kicad_domain::attach_power_pins(&design, &library, &mut nets);
        Ok(nets)
    }

    /// Adds `plan` to the file if it still has `expected_fingerprint`. The result is written to a
    /// copy first; the copy must load in `kicad-cli` and produce exactly the planned nets before
    /// it replaces the file. If it does not, the file is untouched.
    pub async fn apply_plan(&self, plan: &OperationPlan, expected_fingerprint: &str) -> Result<ApplyReport, ToolError> {
        if kicad_domain::validation::has_errors(&plan.findings) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "the plan has errors and cannot be applied",
            ));
        }
        self.refuse_if_read_only()?;
        let _writing = self.writing.lock().await;
        if lock_file(&self.document_path).is_some_and(|l| l.exists()) {
            return Err(ToolError::new(
                ErrorCode::DocumentOpenElsewhere,
                "the schematic is now open in a KiCad editor (lock file present); close it there first",
            ));
        }
        let bytes = self.read_bytes()?;
        if fingerprint_of(&bytes) != expected_fingerprint {
            return Err(ToolError::new(
                ErrorCode::StalePlan,
                "the schematic changed since the plan was made; make a new plan",
            ));
        }
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| ToolError::new(ErrorCode::InvalidArgument, "the schematic is not valid UTF-8"))?;
        if let Some(change) = &plan.sheet {
            return self.apply_sheet(change, &bytes, &text).await;
        }
        let doc = Document::parse(text).map_err(doc_error)?;

        let mut defs = BTreeMap::new();
        for op in &plan.operations {
            if let Operation::PlaceSymbol { library_id, .. } | Operation::PlacePowerSymbol { library_id, .. } = op {
                if !defs.contains_key(library_id) {
                    defs.insert(library_id.clone(), self.definition(&doc, library_id)?);
                }
            }
        }
        let stem = self
            .document_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let project = match doc.project_name() {
            Some(name) => name.to_owned(),
            None if self.document_path.with_extension("kicad_pro").is_file() => stem.clone(),
            None => String::new(),
        };
        let new_text = add_plan(&doc, plan, &defs, &project)
            .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot add the plan to the file: {e}")))?;

        // Verify in a copy beside the original, so relative sheet paths still resolve.
        let candidate = self.document_path.with_file_name(format!(
            ".thingsvu-{}-{}.kicad_sch",
            self.id,
            fingerprint_of(new_text.as_bytes())
        ));
        write_synced(&candidate, new_text.as_bytes()).map_err(|e| io_error("write", &candidate, e))?;
        let outcome = async {
            let reread = Document::parse(new_text.clone()).map_err(doc_error)?;
            let symbols = reread.symbols();
            let nets = self.netlist_of(&candidate, &symbols).await?;
            Ok::<_, ToolError>((verify_nets(plan, &nets, true), symbols))
        }
        .await;
        let (verification, symbols) = match outcome {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&candidate);
                return Err(e.with_mutation_state(MutationState::NotApplied));
            }
        };
        if let Some(bad) = verification.iter().find(|c| !c.ok) {
            let _ = std::fs::remove_file(&candidate);
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!(
                    "the new file's connectivity differs from the plan (net {} expected {:?}, found {:?}); \
                     the schematic was not changed",
                    bad.net, bad.expected, bad.actual
                ),
            )
            .with_mutation_state(MutationState::NotApplied));
        }

        let backup_path = self.replace_with_candidate(&bytes, &candidate, &stem)?;

        let by_reference: HashMap<&str, &str> = symbols
            .iter()
            .map(|s| (s.reference.as_str(), s.uuid.as_str()))
            .collect();
        let placed = plan
            .operations
            .iter()
            .filter_map(|op| match op {
                Operation::PlaceSymbol {
                    component_id,
                    reference,
                    ..
                } => Some(PlacedComponent {
                    component_id: component_id.to_string(),
                    reference: reference.clone(),
                    symbol_id: by_reference
                        .get(reference.as_str())
                        .copied()
                        .unwrap_or_default()
                        .to_owned(),
                }),
                _ => None,
            })
            .collect();
        let counts = plan.counts();
        Ok(ApplyReport {
            placed,
            wires_created: counts.wires,
            labels_created: counts.labels,
            undo_steps: 0,
            verification,
            verified: true,
            backup_path,
            moved: plan.moves.len(),
            removed: plan.removals.len(),
            fields_changed: plan.field_changes.len(),
            placements_checked: 0,
            placement_problems: vec![],
        })
    }

    /// The page and title block of the file.
    pub async fn sheet(&self) -> Result<kicad_domain::sheet::SheetSettings, ToolError> {
        let bytes = self.read_bytes()?;
        let text = String::from_utf8(bytes)
            .map_err(|_| ToolError::new(ErrorCode::InvalidArgument, "the schematic is not valid UTF-8"))?;
        kicad_sexpr::sheet::read(&text).map_err(|e| ToolError::new(ErrorCode::KicadError, e))
    }

    /// Writes a new page and title block into the file text. The new file must load in
    /// `kicad-cli` and read back as planned before it replaces the old one.
    async fn apply_sheet(
        &self,
        change: &kicad_domain::sheet::SheetChange,
        bytes: &[u8],
        text: &str,
    ) -> Result<ApplyReport, ToolError> {
        let not_applied = |e: ToolError| e.with_mutation_state(MutationState::NotApplied);
        let current =
            kicad_sexpr::sheet::read(text).map_err(|e| not_applied(ToolError::new(ErrorCode::KicadError, e)))?;
        if !current.same_as(&change.from) {
            return Err(ToolError::new(
                ErrorCode::StalePlan,
                "the page or title block changed since the plan was made; make a new plan",
            ));
        }
        let new_text = kicad_sexpr::sheet::write(text, &change.to)
            .map_err(|e| not_applied(ToolError::new(ErrorCode::KicadError, e)))?;
        let reread =
            kicad_sexpr::sheet::read(&new_text).map_err(|e| not_applied(ToolError::new(ErrorCode::KicadError, e)))?;
        if !reread.same_as(&change.to) {
            return Err(not_applied(ToolError::new(
                ErrorCode::KicadError,
                "the new file does not read back as planned; the schematic was not changed",
            )));
        }
        let stem = self
            .document_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let candidate = self.document_path.with_file_name(format!(
            ".thingsvu-{}-{}.kicad_sch",
            self.id,
            fingerprint_of(new_text.as_bytes())
        ));
        write_synced(&candidate, new_text.as_bytes()).map_err(|e| io_error("write", &candidate, e))?;
        let doc = Document::parse(new_text.clone()).map_err(doc_error)?;
        if let Err(e) = self.netlist_of(&candidate, &doc.symbols()).await {
            let _ = std::fs::remove_file(&candidate);
            return Err(not_applied(e));
        }
        let backup_path = self.replace_with_candidate(bytes, &candidate, &stem)?;
        Ok(ApplyReport {
            placed: vec![],
            wires_created: 0,
            labels_created: 0,
            undo_steps: 0,
            verification: vec![],
            verified: true,
            backup_path,
            moved: 0,
            removed: 0,
            fields_changed: 0,
            placements_checked: change.describe().len(),
            placement_problems: vec![],
        })
    }

    /// Replaces the schematic with the verified `candidate`, keeping the previous version. Refuses,
    /// and removes the candidate, if the file changed since `bytes` was read.
    fn replace_with_candidate(&self, bytes: &[u8], candidate: &Path, stem: &str) -> Result<Option<String>, ToolError> {
        // Nobody may have changed the file while it was being verified.
        if self.read_bytes().map(|now| fingerprint_of(&now)).ok().as_deref() != Some(&fingerprint_of(bytes)) {
            let _ = std::fs::remove_file(candidate);
            return Err(ToolError::new(
                ErrorCode::FileChangedOnDisk,
                "the schematic changed while the change was being verified; nothing was written, try again",
            )
            .with_mutation_state(MutationState::NotApplied));
        }

        // Keep the previous version where it can be recovered, then replace the file in one step.
        let backup = self
            .runtime_dir
            .join(&self.id)
            .join("backups")
            .join(format!("{stem}-{}.kicad_sch", fingerprint_of(bytes)));
        let backup_path = std::fs::create_dir_all(backup.parent().unwrap_or(Path::new(".")))
            .and_then(|_| std::fs::write(&backup, bytes))
            .ok()
            .map(|_| backup.display().to_string());
        if let Err(e) = std::fs::rename(candidate, &self.document_path) {
            let _ = std::fs::remove_file(candidate);
            return Err(io_error("replace", &self.document_path, e).with_mutation_state(MutationState::NotApplied));
        }
        Ok(backup_path)
    }

    /// Changes the value or footprint of one part. The new file must load in `kicad-cli` before it
    /// replaces the old one.
    pub async fn set_field(
        &self,
        selector: &str,
        field: crate::SymbolField,
        text: &str,
    ) -> Result<SymbolInfo, ToolError> {
        self.refuse_if_read_only()?;
        let _writing = self.writing.lock().await;
        if lock_file(&self.document_path).is_some_and(|l| l.exists()) {
            return Err(ToolError::new(
                ErrorCode::DocumentOpenElsewhere,
                "the schematic is now open in a KiCad editor (lock file present); close it there first",
            ));
        }
        let bytes = self.read_bytes()?;
        let doc = Document::parse(
            String::from_utf8(bytes.clone())
                .map_err(|_| ToolError::new(ErrorCode::InvalidArgument, "the schematic is not valid UTF-8"))?,
        )
        .map_err(doc_error)?;
        let symbols = doc.symbols();
        let target = crate::select_symbol(&symbols, selector, |s| (s.uuid.as_str(), s.reference.as_str()))?;
        let new_text = kicad_sexpr::set_property(&doc, &target.uuid, field.property_name(), text)
            .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot change the field: {e}")))?;

        let stem = self
            .document_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let candidate = self.document_path.with_file_name(format!(
            ".thingsvu-{}-{}.kicad_sch",
            self.id,
            fingerprint_of(new_text.as_bytes())
        ));
        write_synced(&candidate, new_text.as_bytes()).map_err(|e| io_error("write", &candidate, e))?;
        let reread = Document::parse(new_text.clone()).map_err(doc_error)?;
        let loaded = self.netlist_of(&candidate, &reread.symbols()).await;
        if let Err(e) = loaded {
            let _ = std::fs::remove_file(&candidate);
            return Err(e.with_mutation_state(MutationState::NotApplied));
        }
        self.replace_with_candidate(&bytes, &candidate, &stem)?;
        let changed = self.read()?.symbols();
        let changed = changed
            .iter()
            .find(|s| s.uuid == target.uuid)
            .ok_or_else(|| ToolError::new(ErrorCode::KicadError, "the part is missing after the change"))?;
        symbol_info(changed)
    }

    /// Changes are written when applied; there is nothing pending to save.
    pub async fn save(&self) -> Result<(), ToolError> {
        Ok(())
    }

    /// Copies the schematic (with its project file, if any) into `dir`, which must exist, and
    /// returns the copy's path.
    pub async fn save_copy(&self, dir: &Path) -> Result<PathBuf, ToolError> {
        let name = self
            .document_path
            .file_name()
            .map(|n| n.to_owned())
            .unwrap_or_else(|| "copy.kicad_sch".into());
        let target = dir.join(name);
        std::fs::copy(&self.document_path, &target).map_err(|e| io_error("copy", &self.document_path, e))?;
        let project = self.document_path.with_extension("kicad_pro");
        if project.is_file() {
            let _ = std::fs::copy(&project, dir.join(project.file_name().unwrap_or_default()));
        }
        Ok(target)
    }
}
