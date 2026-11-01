// SPDX-License-Identifier: GPL-3.0-or-later
//! The manufacturing tools (Phase 13): `pcb_export` (Gerbers, drill, positions, STEP, plots, the
//! mechanical model), `pcb_fab_package` (one zip for a board house), and `pcb_mechanical_info`.
//!
//! Every export works on one snapshot of the board: the checks, the outputs, and their read-back
//! all see the same file. Outputs are written to a scratch folder, read back, and only then copied
//! into the project; nothing is uploaded.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kicad_cli_runner::fab::{self, GerberJob, PlotFormat};
use kicad_cli_runner::KicadInstall;
use kicad_domain::board::Board;
use kicad_domain::fab::{blocking, pre_export, DrcSummary, FabCheck, FabFacts};
use kicad_domain::{ErrorCode, ResultState, ToolError};
use kicad_session::BoardSession;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::pcb_tools::{board_provenance, schematic_for};
use super::{bom_settings, envelope, BomPreset, KicadAiServer};

const EXPORT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportKind {
    /// Copper, paste, silkscreen, mask, and outline as X2 Gerbers, with the job file.
    Gerbers,
    /// Excellon drill files in mm, plated and non-plated separately (written next to the Gerbers).
    Drill,
    /// Pick-and-place file, CSV in mm, both sides, DNP parts left out.
    Positions,
    /// The board and its parts as STEP.
    Step,
    /// One PDF with a page per layer (copper, silkscreen, outline).
    Pdf,
    /// One SVG per layer (copper, silkscreen, outline), cut to the board.
    Svg,
    /// The mechanical model as JSON (outline, holes, keep-outs, connectors, heights).
    Mechanical,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbExportArgs {
    pub session_id: String,
    /// What to write (default gerbers, drill, positions).
    #[serde(default)]
    pub outputs: Option<Vec<ExportKind>>,
    /// Absolute folder inside the board's folder (default `fab` next to the board).
    #[serde(default)]
    pub output_dir: Option<String>,
    /// The schematic for the BOM check (default: the one next to the board).
    #[serde(default)]
    pub schematic_path: Option<String>,
    /// Export even though a check failed. Only after showing the failures to the user and
    /// hearing them accept; their acceptance is written to the checks report.
    #[serde(default)]
    pub accept_problems: bool,
    /// What the user said when accepting, for the checks report.
    #[serde(default)]
    pub acceptance_note: Option<String>,
}

fn jlcpcb() -> BomPreset {
    BomPreset::Jlcpcb
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbFabPackageArgs {
    pub session_id: String,
    /// jlcpcb (default): JLCPCB's BOM and placement (CPL) columns. generic: KiCad's columns.
    #[serde(default = "jlcpcb")]
    pub preset: BomPreset,
    /// Absolute folder inside the board's folder (default `fab` next to the board).
    #[serde(default)]
    pub output_dir: Option<String>,
    /// The schematic for the BOM (default: the one next to the board). The file on disk is used.
    #[serde(default)]
    pub schematic_path: Option<String>,
    /// As for pcb_export: only after the user accepted the failed checks.
    #[serde(default)]
    pub accept_problems: bool,
    #[serde(default)]
    pub acceptance_note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PcbMechanicalArgs {
    pub session_id: String,
}

/// One read-back check of the written files.
#[derive(Debug, Clone, Serialize)]
struct Verified {
    what: String,
    ok: bool,
    detail: String,
}

impl Verified {
    fn new(what: &str, ok: bool, detail: impl Into<String>) -> Verified {
        Verified {
            what: what.to_owned(),
            ok,
            detail: detail.into(),
        }
    }
}

/// The board as one snapshot, read and checked.
struct Snapshot {
    scratch: PathBuf,
    copy: PathBuf,
    stem: String,
    board: Board,
    facts: FabFacts,
    checks: Vec<FabCheck>,
    schematic: Option<PathBuf>,
    design: Option<kicad_domain::SchematicDesign>,
    cli: KicadInstall,
    /// A KiCad 11-series CLI, which can read Gerbers back.
    reader: Option<KicadInstall>,
}

fn kicad_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::new(ErrorCode::KicadError, e.to_string())
}

fn io_err(path: &Path, e: std::io::Error) -> ToolError {
    ToolError::new(ErrorCode::KicadError, format!("{}: {e}", path.display()))
}

fn drc_summary(report: &kicad_cli_runner::pcb::DrcReport) -> DrcSummary {
    let types = |severity: &str| {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for v in report.violations.iter().filter(|v| v.severity == severity) {
            *counts.entry(v.kind.as_str()).or_default() += 1;
        }
        counts.into_iter().map(|(k, n)| format!("{k} ({n})")).collect()
    };
    DrcSummary {
        errors: report.count("error"),
        warnings: report.count("warning"),
        unrouted: report.unconnected_items.len(),
        error_types: types("error"),
        warning_types: types("warning"),
    }
}

/// The folder to write into: `given` (absolute, inside the board's folder) or `fab` beside it.
fn output_dir(board: &BoardSession, given: Option<&str>) -> Result<PathBuf, ToolError> {
    let folder = board.document_path().parent().map(PathBuf::from).unwrap_or_default();
    let Some(given) = given else {
        return Ok(folder.join("fab"));
    };
    let p = PathBuf::from(given);
    let inside = p.is_absolute() && std::path::absolute(&p).is_ok_and(|a| a.starts_with(&folder));
    if !inside {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("output_dir must be an absolute folder inside {}", folder.display()),
        ));
    }
    Ok(p)
}

/// Now, as `2026-10-26T09:07:33Z`.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Copies the files of `from` into `to`, replacing files of the same name. With `clear_old`, the
/// files there that an earlier export of this board wrote (named after it) are removed first, so
/// no stale Gerber layer is left to be uploaded with the new ones. Returns the paths written.
fn install(from: &Path, to: &Path, stem: &str, clear_old: bool) -> Result<Vec<PathBuf>, ToolError> {
    std::fs::create_dir_all(to).map_err(|e| io_err(to, e))?;
    for old in std::fs::read_dir(to).map_err(|e| io_err(to, e))?.flatten() {
        let name = old.file_name().to_string_lossy().into_owned();
        if clear_old
            && old.path().is_file()
            && (name.starts_with(&format!("{stem}-")) || name.starts_with(&format!("{stem}.")))
        {
            std::fs::remove_file(old.path()).map_err(|e| io_err(&old.path(), e))?;
        }
    }
    let mut out = Vec::new();
    for f in std::fs::read_dir(from).map_err(|e| io_err(from, e))?.flatten() {
        if f.path().is_file() {
            let target = to.join(f.file_name());
            std::fs::copy(f.path(), &target).map_err(|e| io_err(&target, e))?;
            out.push(target);
        }
    }
    out.sort();
    Ok(out)
}

fn zip_files(zip_path: &Path, entries: &[(String, PathBuf)]) -> Result<(), ToolError> {
    let file = std::fs::File::create(zip_path).map_err(|e| io_err(zip_path, e))?;
    let mut zw = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, path) in entries {
        let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
        zw.start_file(name.as_str(), options).map_err(kicad_err)?;
        zw.write_all(&bytes).map_err(|e| io_err(zip_path, e))?;
    }
    zw.finish().map_err(kicad_err)?;
    Ok(())
}

/// The names in a zip, read back.
fn zip_names(zip_path: &Path) -> Result<Vec<String>, ToolError> {
    let file = std::fs::File::open(zip_path).map_err(|e| io_err(zip_path, e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(kicad_err)?;
    (0..archive.len())
        .map(|i| archive.by_index(i).map(|f| f.name().to_owned()).map_err(kicad_err))
        .collect()
}

/// JLCPCB's placement (CPL) columns from KiCad's position file.
fn jlcpcb_cpl(kicad_csv: &str) -> String {
    let mut out = String::new();
    for (i, line) in kicad_csv.lines().enumerate() {
        if i == 0 {
            out.push_str("Designator,Val,Package,Mid X,Mid Y,Rotation,Layer\n");
            continue;
        }
        let line = line
            .strip_suffix(",top")
            .map(|l| format!("{l},Top"))
            .or_else(|| line.strip_suffix(",bottom").map(|l| format!("{l},Bottom")))
            .unwrap_or_else(|| line.to_owned());
        out.push_str(&line);
        out.push('\n');
    }
    out
}

impl Snapshot {
    /// Reads every written Gerber and drill file back and compares it with the board.
    async fn verify_gerbers(&self, dir: &Path, drill: bool) -> Vec<Verified> {
        let mut v = Vec::new();
        let files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        let copper = self.board.copper_layers.len();
        match files
            .iter()
            .find(|f| f.extension().is_some_and(|e| e == "gbrjob"))
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|t| GerberJob::parse(&t).ok())
        {
            Some(job) => {
                v.push(Verified::new(
                    "copper layers",
                    job.copper_layers == Some(copper as u32) && job.copper_files() == copper,
                    format!(
                        "the board has {copper}; the job file says {:?} and lists {}",
                        job.copper_layers,
                        job.copper_files()
                    ),
                ));
                v.push(Verified::new(
                    "outline file",
                    job.has_profile(),
                    if job.has_profile() {
                        "the job file lists the outline (Profile)"
                    } else {
                        "no outline file in the job"
                    },
                ));
            }
            None => v.push(Verified::new("job file", false, "no readable .gbrjob was written")),
        }
        let Some(reader) = &self.reader else {
            v.push(Verified::new(
                "read back",
                true,
                "not run: reading Gerbers back needs a KiCad 11-series kicad-cli",
            ));
            return v;
        };
        let (mut read, mut holes) = (0, 0);
        for f in files.iter().filter(|f| f.extension().is_some_and(|e| e != "gbrjob")) {
            match fab::gerber_info(reader, f, Duration::from_secs(60)).await {
                Ok(info) => {
                    read += 1;
                    if info.kind.contains("Drill") {
                        holes += info.item_count;
                    } else if file_name(f).contains("Edge_Cuts") && info.item_count == 0 {
                        v.push(Verified::new("outline drawn", false, "the outline file has no shapes"));
                    }
                }
                Err(e) => v.push(Verified::new("read back", false, format!("{}: {e}", file_name(f)))),
            }
        }
        v.push(Verified::new(
            "read back",
            true,
            format!("{read} files read by KiCad's Gerber reader without warnings"),
        ));
        if drill {
            let want = self.facts.holes.len();
            v.push(Verified::new(
                "drill holes",
                holes == want,
                format!("{holes} holes in the drill files, {want} on the board (pads and vias)"),
            ));
        }
        v
    }

    fn verify_positions(&self, csv: &Path) -> Verified {
        let rows = std::fs::read_to_string(csv)
            .map(|t| t.lines().skip(1).filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        let want = self
            .facts
            .footprints
            .iter()
            .filter(|f| !f.exclude_from_pos && !f.dnp && !f.board_only)
            .count();
        Verified::new(
            "positions",
            rows == want,
            format!("{rows} parts in the position file, {want} to place"),
        )
    }
}

impl KicadAiServer {
    /// Takes a snapshot of the board, reads it, runs DRC on it, and runs the pre-export checks.
    async fn fab_snapshot(
        &self,
        cid: &str,
        board: &Arc<BoardSession>,
        schematic_path: Option<&str>,
        need_schematic: bool,
    ) -> Result<Snapshot, ToolError> {
        let cli = board
            .cli()
            .cloned()
            .ok_or_else(|| ToolError::new(ErrorCode::FeatureUnavailable, "no matching kicad-cli install"))?;
        let schematic = match schematic_for(board, schematic_path) {
            Ok(p) => Some(p),
            Err(e) if schematic_path.is_some() || need_schematic => return Err(e),
            Err(_) => None,
        };
        let design = match &schematic {
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|e| io_err(p, e))?;
                let doc = kicad_sexpr::Document::parse(text)
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("{}: {e}", p.display())))?;
                Some(doc.design())
            }
            None => None,
        };
        let scratch = self.scratch_dir(board.id(), "fab", cid)?;
        let copy = board.save_copy(&scratch).await?;
        let text = std::fs::read_to_string(&copy).map_err(|e| io_err(&copy, e))?;
        let parsed = kicad_sexpr::pcb::read_board(&text).map_err(kicad_err)?;
        let facts = kicad_sexpr::fab::read_fab_facts(&text).map_err(kicad_err)?;
        let drc = kicad_cli_runner::pcb::run_drc(&cli, &copy, &scratch.join("drc.json"), false, true, EXPORT_TIMEOUT)
            .await
            .ok();
        let bom = design
            .as_ref()
            .map(|d| kicad_domain::bom::build(d, &Default::default()));
        let checks = pre_export(
            &facts,
            parsed.outline,
            bom.as_ref(),
            drc.as_ref().map(drc_summary).as_ref(),
        );
        Ok(Snapshot {
            stem: copy
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "board".into()),
            reader: board.render_cli().cloned(),
            scratch,
            copy,
            board: parsed,
            facts,
            checks,
            schematic,
            design,
            cli,
        })
    }

    /// The answer when a check failed and the user has not accepted it.
    fn checks_refused(cid: &str, board: &BoardSession, checks: &[FabCheck]) -> CallToolResult {
        envelope::ok_with_state(
            cid,
            ResultState::Rejected,
            serde_json::json!({
                "exported": false,
                "checks": checks,
                "blocking": blocking(checks).iter().map(|c| c.id).collect::<Vec<_>>(),
                "message": "Nothing was written: a check before export failed. Show the failures to the user; fix them, \
            or, if the user accepts them, call again with accept_problems: true and their words in acceptance_note.",
            }),
            board_provenance(board),
            vec![],
        )
    }

    /// Writes the checks report beside the outputs; returns its path.
    fn write_checks_report(
        out: &Path,
        snap: &Snapshot,
        accepted: Option<&str>,
        files: &[PathBuf],
        verified: &[Verified],
    ) -> Result<PathBuf, ToolError> {
        let report = serde_json::json!({
            "created_utc": utc_now(),
            "board": snap.copy.file_name().map(|n| n.to_string_lossy().into_owned()),
            "kicad_cli": snap.cli.version.full,
            "checks": snap.checks,
            "accepted_by_user": accepted.map(|note| serde_json::json!({
                "problems": blocking(&snap.checks).iter().map(|c| format!("{}: {}", c.id, c.message)).collect::<Vec<_>>(),
                "note": note,
            })),
            "files": files.iter().map(|f| serde_json::json!({
                "path": f.strip_prefix(out).unwrap_or(f).display().to_string().replace('\\', "/"),
                "bytes": std::fs::metadata(f).map(|m| m.len()).unwrap_or(0),
            })).collect::<Vec<_>>(),
            "verification": verified,
        });
        let path = out.join(format!("{}-checks.json", snap.stem));
        std::fs::create_dir_all(out).map_err(|e| io_err(out, e))?;
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap_or_default())
            .map_err(|e| io_err(&path, e))?;
        Ok(path)
    }

    /// Writes `kinds` for the snapshot into `stage` (subfolders gerbers, assembly, 3d, plots) and
    /// reads them back.
    async fn stage_outputs(
        &self,
        snap: &Snapshot,
        kinds: &[ExportKind],
        stage: &Path,
        step_in_project: Option<String>,
    ) -> Result<(Vec<Verified>, Vec<String>), ToolError> {
        let mut verified = Vec::new();
        let mut notes = Vec::new();
        let sub = |name: &str| -> Result<PathBuf, ToolError> {
            let d = stage.join(name);
            std::fs::create_dir_all(&d).map_err(|e| io_err(&d, e))?;
            Ok(d)
        };
        let has = |k: ExportKind| kinds.contains(&k);
        if has(ExportKind::Gerbers) || has(ExportKind::Drill) {
            let g = sub("gerbers")?;
            if has(ExportKind::Gerbers) {
                let layers = fab::fab_layers(&snap.board.copper_layers);
                fab::export_gerbers(&snap.cli, &snap.copy, &g, &layers, EXPORT_TIMEOUT)
                    .await
                    .map_err(kicad_err)?;
            }
            if has(ExportKind::Drill) {
                // Drill files go into the Gerber folder, as board houses expect them in one zip.
                let d = snap.scratch.join("drill");
                std::fs::create_dir_all(&d).map_err(|e| io_err(&d, e))?;
                for f in fab::export_drill(&snap.cli, &snap.copy, &d, EXPORT_TIMEOUT)
                    .await
                    .map_err(kicad_err)?
                {
                    std::fs::rename(&f, g.join(file_name(&f))).map_err(|e| io_err(&f, e))?;
                }
            }
            if has(ExportKind::Gerbers) {
                verified.extend(snap.verify_gerbers(&g, has(ExportKind::Drill)).await);
            }
        }
        if has(ExportKind::Positions) {
            let csv = sub("assembly")?.join(format!("{}-pos.csv", snap.stem));
            fab::export_positions(&snap.cli, &snap.copy, &csv, true, EXPORT_TIMEOUT)
                .await
                .map_err(kicad_err)?;
            verified.push(snap.verify_positions(&csv));
        }
        let mut step_path = step_in_project;
        if has(ExportKind::Step) {
            let step = sub("3d")?.join(format!("{}.step", snap.stem));
            let warnings = fab::export_step(&snap.cli, &snap.copy, &step, EXPORT_TIMEOUT)
                .await
                .map_err(kicad_err)?;
            let bytes = std::fs::metadata(&step).map(|m| m.len()).unwrap_or(0);
            verified.push(Verified::new("STEP", bytes > 0, format!("{bytes} bytes")));
            notes.extend(warnings.into_iter().map(|w| format!("STEP: {w}")));
            step_path = Some(format!("3d/{}.step", snap.stem));
        }
        for (kind, format) in [(ExportKind::Pdf, PlotFormat::Pdf), (ExportKind::Svg, PlotFormat::Svg)] {
            if has(kind) {
                let d = snap.scratch.join(format!("plot-{format:?}"));
                std::fs::create_dir_all(&d).map_err(|e| io_err(&d, e))?;
                let mut layers = snap.board.copper_layers.clone();
                layers.extend(["F.SilkS", "B.SilkS", "Edge.Cuts"].map(String::from));
                let files = fab::export_plot(&snap.cli, &snap.copy, &d, format, &layers, EXPORT_TIMEOUT)
                    .await
                    .map_err(kicad_err)?;
                let plots = sub("plots")?;
                for f in &files {
                    std::fs::rename(f, plots.join(file_name(f))).map_err(|e| io_err(f, e))?;
                }
                verified.push(Verified::new(
                    &format!("{format:?}").to_uppercase(),
                    !files.is_empty(),
                    format!("{} files", files.len()),
                ));
            }
        }
        if has(ExportKind::Mechanical) {
            let model = kicad_domain::mechanical::build(
                &snap.facts,
                snap.board.outline,
                &file_name(&snap.copy),
                step_path.as_deref(),
            );
            let path = sub("3d")?.join(format!("{}-mechanical.json", snap.stem));
            std::fs::write(&path, serde_json::to_string_pretty(&model).unwrap_or_default())
                .map_err(|e| io_err(&path, e))?;
        }
        Ok((verified, notes))
    }
}

#[tool_router(router = fab_router, vis = "pub(super)")]
impl KicadAiServer {
    #[tool(
        name = "pcb_export",
        description = "Write manufacturing outputs for the board into the project (default the fab folder next to \
the board): gerbers, drill, positions (default these three), step, pdf, svg, mechanical. First checks the board: \
outline closed, DRC clean and all routed, every footprint has a BOM line (from the schematic), silkscreen on the \
board. If a check fails nothing is written and the failures are returned (result_state rejected): show them to \
the user, and only if they accept, call again with accept_problems: true and their words in acceptance_note. \
The outputs come from one snapshot of the board, are read back (layer count, outline, drill hits against the \
board's holes, positions against the parts; with a KiCad 11 kicad-cli every Gerber is read by KiCad's Gerber \
reader) and only then copied into the project. A checks report (<board>-checks.json) is written beside them. \
Nothing is uploaded."
    )]
    pub async fn pcb_export(&self, Parameters(args): Parameters<PcbExportArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let out = match output_dir(&board, args.output_dir.as_deref()) {
            Ok(o) => o,
            Err(e) => return envelope::error(&cid, &e),
        };
        let mut kinds = args
            .outputs
            .clone()
            .unwrap_or_else(|| vec![ExportKind::Gerbers, ExportKind::Drill, ExportKind::Positions]);
        kinds.sort();
        kinds.dedup();
        if kinds.is_empty() {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::InvalidArgument, "outputs names nothing to write"),
            );
        }
        let snap = match self
            .fab_snapshot(&cid, &board, args.schematic_path.as_deref(), false)
            .await
        {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            if !blocking(&snap.checks).is_empty() && !args.accept_problems {
                return Ok(None);
            }
            let stage = snap.scratch.join("out");
            let step_in_project = Some(format!("3d/{}.step", snap.stem)).filter(|p| out.join(p).is_file());
            let (verified, notes) = self.stage_outputs(&snap, &kinds, &stage, step_in_project).await?;
            if verified.iter().any(|v| !v.ok) {
                return Ok(Some((Vec::new(), verified, notes, None)));
            }
            let mut files = Vec::new();
            for sub in ["gerbers", "assembly", "3d", "plots"] {
                if stage.join(sub).is_dir() {
                    let clear_old = sub == "gerbers" && kinds.contains(&ExportKind::Gerbers);
                    files.extend(install(&stage.join(sub), &out.join(sub), &snap.stem, clear_old)?);
                }
            }
            let accepted = args.accept_problems.then(|| {
                args.acceptance_note
                    .clone()
                    .unwrap_or_else(|| "accepted (no note given)".into())
            });
            let report = Self::write_checks_report(&out, &snap, accepted.as_deref(), &files, &verified)?;
            Ok::<_, ToolError>(Some((files, verified, notes, Some(report))))
        }
        .await;
        let _ = std::fs::remove_dir_all(&snap.scratch);
        match result {
            Ok(None) => Self::checks_refused(&cid, &board, &snap.checks),
            Ok(Some((files, verified, notes, report))) => {
                let state = if report.is_some() {
                    ResultState::Done
                } else {
                    ResultState::Rejected
                };
                envelope::ok_with_state(
                    &cid,
                    state,
                    serde_json::json!({
                        "exported": report.is_some(),
                        "output_dir": out.display().to_string(),
                        "files": files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
                        "checks_report": report.map(|r| r.display().to_string()),
                        "checks": snap.checks,
                        "accepted_problems": args.accept_problems && !blocking(&snap.checks).is_empty(),
                        "verification": verified,
                        "message": if state == ResultState::Rejected {
                            Some("The outputs did not read back as expected (see verification); nothing was copied into the project.")
                        } else {
                            None
                        },
                    }),
                    board_provenance(&board),
                    notes,
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_fab_package",
        description = "Make the files a board house needs, from the board and its schematic, into the fab folder \
next to the board (or output_dir): <board>-gerbers.zip (Gerbers and drill, the file to upload for the bare \
board), <board>-bom.csv and <board>-cpl.csv (for assembly; preset jlcpcb uses JLCPCB's columns, generic \
KiCad's), and <board>-fab.zip holding all of them with the checks report. The same checks as pcb_export come \
first, with the same accept_problems rule. The schematic file on disk is used for the BOM (save it first). \
Every file is read back before it is copied into the project. Nothing is uploaded. Run bom_review first to find parts with no footprint or part number; use pcb_export when only one kind of file is wanted."
    )]
    pub async fn pcb_fab_package(&self, Parameters(args): Parameters<PcbFabPackageArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let out = match output_dir(&board, args.output_dir.as_deref()) {
            Ok(o) => o,
            Err(e) => return envelope::error(&cid, &e),
        };
        let snap = match self
            .fab_snapshot(&cid, &board, args.schematic_path.as_deref(), true)
            .await
        {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            if !blocking(&snap.checks).is_empty() && !args.accept_problems {
                return Ok(None);
            }
            let stage = snap.scratch.join("out");
            let kinds = [ExportKind::Gerbers, ExportKind::Drill, ExportKind::Positions];
            let (mut verified, notes) = self.stage_outputs(&snap, &kinds, &stage, None).await?;
            let stem = &snap.stem;
            // The BOM, through kicad-cli from the schematic file.
            let (schematic, design) = (
                snap.schematic.as_ref().expect("the package needs a schematic"),
                snap.design.as_ref().expect("read with the schematic"),
            );
            let settings = bom_settings(
                args.preset,
                &kicad_domain::bom::build(design, &Default::default()),
                design,
            );
            // The BOM as KiCad will group it, to count its lines.
            let bom = kicad_domain::bom::build(
                design,
                &kicad_domain::bom::BomOptions {
                    group_by: settings.group_by.clone(),
                    exclude_dnp: settings.exclude_dnp,
                },
            );
            let bom_csv = stage.join(format!("{stem}-bom.csv"));
            kicad_cli_runner::export::export_bom(&snap.cli, schematic, &bom_csv, &settings, EXPORT_TIMEOUT)
                .await
                .map_err(kicad_err)?;
            let lines = std::fs::read_to_string(&bom_csv)
                .map(|t| t.lines().skip(1).filter(|l| !l.trim().is_empty()).count())
                .unwrap_or(0);
            let want = bom.rows.len();
            verified.push(Verified::new(
                "BOM",
                lines == want,
                format!("{lines} lines in the BOM, {want} expected"),
            ));
            // The placement file, in the preset's columns.
            let pos = stage.join("assembly").join(format!("{stem}-pos.csv"));
            let cpl = stage.join(format!("{stem}-cpl.csv"));
            let text = std::fs::read_to_string(&pos).map_err(|e| io_err(&pos, e))?;
            let text = match args.preset {
                BomPreset::Jlcpcb => jlcpcb_cpl(&text),
                BomPreset::Generic => text,
            };
            std::fs::write(&cpl, text).map_err(|e| io_err(&cpl, e))?;
            // The Gerber zip, read back.
            let gerbers: Vec<PathBuf> = {
                let mut g: Vec<PathBuf> = std::fs::read_dir(stage.join("gerbers"))
                    .map_err(|e| io_err(&stage, e))?
                    .flatten()
                    .map(|e| e.path())
                    .collect();
                g.sort();
                g
            };
            let gerber_zip = stage.join(format!("{stem}-gerbers.zip"));
            zip_files(
                &gerber_zip,
                &gerbers.iter().map(|g| (file_name(g), g.clone())).collect::<Vec<_>>(),
            )?;
            let names = zip_names(&gerber_zip)?;
            verified.push(Verified::new(
                "Gerber zip",
                names.len() == gerbers.len(),
                format!("{} files in {}", names.len(), file_name(&gerber_zip)),
            ));
            if verified.iter().any(|v| !v.ok) {
                return Ok(Some((Vec::new(), verified, notes, None)));
            }
            std::fs::create_dir_all(&out).map_err(|e| io_err(&out, e))?;
            let mut files = Vec::new();
            for f in [&gerber_zip, &bom_csv, &cpl] {
                let target = out.join(file_name(f));
                std::fs::copy(f, &target).map_err(|e| io_err(&target, e))?;
                files.push(target);
            }
            let accepted = args.accept_problems.then(|| {
                args.acceptance_note
                    .clone()
                    .unwrap_or_else(|| "accepted (no note given)".into())
            });
            let report = Self::write_checks_report(&out, &snap, accepted.as_deref(), &files, &verified)?;
            let package = out.join(format!("{stem}-fab.zip"));
            let entries: Vec<(String, PathBuf)> = files
                .iter()
                .chain([&report])
                .map(|f| (file_name(f), f.clone()))
                .collect();
            zip_files(&package, &entries)?;
            if zip_names(&package)?.len() != entries.len() {
                return Err(ToolError::new(
                    ErrorCode::KicadError,
                    format!("{} did not read back whole", package.display()),
                ));
            }
            files.push(report.clone());
            files.push(package.clone());
            Ok::<_, ToolError>(Some((files, verified, notes, Some(package))))
        }
        .await;
        let _ = std::fs::remove_dir_all(&snap.scratch);
        match result {
            Ok(None) => Self::checks_refused(&cid, &board, &snap.checks),
            Ok(Some((files, verified, notes, package))) => {
                let state = if package.is_some() {
                    ResultState::Done
                } else {
                    ResultState::Rejected
                };
                envelope::ok_with_state(
                    &cid,
                    state,
                    serde_json::json!({
                        "exported": package.is_some(),
                        "package": package.map(|p| p.display().to_string()),
                        "files": files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
                        "preset": match args.preset { BomPreset::Jlcpcb => "jlcpcb", BomPreset::Generic => "generic" },
                        "checks": snap.checks,
                        "accepted_problems": args.accept_problems && !blocking(&snap.checks).is_empty(),
                        "verification": verified,
                        "message": if state == ResultState::Rejected {
                            Some("The files did not read back as expected (see verification); nothing was copied into the project.")
                        } else {
                            None
                        },
                    }),
                    board_provenance(&board),
                    notes,
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "pcb_mechanical_info",
        description = "The board as a mechanical model, for enclosures and mounting: outline and thickness, \
mounting holes, keep-out areas, connectors and the board edge each faces, and every part's courtyard and \
height above the board (from a Height field, else a typical height for the package, marked estimated). \
Millimetres, y down as in the PCB editor. Read-only; pcb_export with outputs [mechanical] writes the same as JSON."
    )]
    pub async fn pcb_mechanical_info(&self, Parameters(args): Parameters<PcbMechanicalArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let board = match self.board(&args.session_id).await {
            Ok(b) => b,
            Err(e) => return envelope::error(&cid, &e),
        };
        let dir = match self.scratch_dir(board.id(), "mechanical", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        let result = async {
            let copy = board.save_copy(&dir).await?;
            let text = std::fs::read_to_string(&copy).map_err(|e| io_err(&copy, e))?;
            let parsed = kicad_sexpr::pcb::read_board(&text).map_err(kicad_err)?;
            let facts = kicad_sexpr::fab::read_fab_facts(&text).map_err(kicad_err)?;
            let stem = copy
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let step = board
                .document_path()
                .parent()
                .map(|p| p.join("fab").join("3d").join(format!("{stem}.step")))
                .filter(|p| p.is_file())
                .map(|p| p.display().to_string());
            Ok::<_, ToolError>(kicad_domain::mechanical::build(
                &facts,
                parsed.outline,
                &board.document_path().display().to_string(),
                step.as_deref(),
            ))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(model) => envelope::ok(&cid, model, board_provenance(&board), vec![]),
            Err(e) => envelope::error(&cid, &e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kicad_positions_become_jlcpcb_columns() {
        let kicad = "Ref,Val,Package,PosX,PosY,Rot,Side\n\"C1\",\"10uF\",\"C_0805\",1.725000,-9.325000,0.000000,top\n\
                     \"U1\",\"X\",\"SOT\",2.0,-3.0,90.0,bottom\n";
        let cpl = jlcpcb_cpl(kicad);
        let lines: Vec<&str> = cpl.lines().collect();
        assert_eq!(lines[0], "Designator,Val,Package,Mid X,Mid Y,Rotation,Layer");
        assert!(lines[1].ends_with(",Top"));
        assert!(lines[2].ends_with(",Bottom"));
    }

    #[test]
    fn utc_now_is_iso() {
        let now = utc_now();
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.starts_with("20") && now.ends_with('Z'));
    }

    #[test]
    fn only_new_gerbers_clear_the_old_ones() {
        let dir = std::env::temp_dir().join(format!("thingsvu-install-{}", std::process::id()));
        let (from, to) = (dir.join("from"), dir.join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("b-mechanical.json"), "{}").unwrap();
        for old in ["b.step", "b-In1_Cu.g2", "notes.txt"] {
            std::fs::write(to.join(old), "old").unwrap();
        }
        install(&from, &to, "b", false).unwrap();
        assert!(to.join("b.step").is_file(), "a mechanical export keeps the STEP");
        install(&from, &to, "b", true).unwrap();
        assert!(!to.join("b-In1_Cu.g2").exists(), "a stale layer is removed");
        assert!(to.join("notes.txt").is_file() && to.join("b-mechanical.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zip_reads_back() {
        let dir = std::env::temp_dir().join(format!("thingsvu-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.gtl");
        std::fs::write(&a, "G04 test*\nM02*\n").unwrap();
        let z = dir.join("t.zip");
        zip_files(&z, &[("a.gtl".into(), a.clone()), ("b/a.gtl".into(), a)]).unwrap();
        assert_eq!(zip_names(&z).unwrap(), ["a.gtl", "b/a.gtl"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
