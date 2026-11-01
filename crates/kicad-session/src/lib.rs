// SPDX-License-Identifier: GPL-3.0-or-later
//! KiCad sessions for the MCP server (plan §3.1).
//!
//! A [`Session`] is one serialized connection to one KiCad instance and one target document,
//! with the capability set probed when it opened. Phase 1 implements the headless provider:
//! a `kicad-cli api-server` child owned by this process.

mod any;
pub mod board;
mod capability_map;
pub mod checkpoint;
mod design;
mod errors;
mod file;
pub mod footprints;
mod freshness;
mod headless;
#[cfg(windows)]
mod job;
mod mutate;
pub mod registry;
mod schematic;
mod session;
mod sheet;
pub mod symbols;

pub use any::AnySession;
pub use board::rules_from_project;
pub use board::{BoardSession, BoardSnapshot, BoardStatus, EditReport, ImportReport, RestoreReport};
pub use capability_map::capabilities_from_probe;
pub use errors::from_ipc_error;
pub use file::FileSession;
pub use freshness::ReloadReport;
pub use mutate::{ApplyReport, NetCheck, PlacedComponent};
pub use schematic::OpenDocument;
pub use session::{OpenTarget, Provider, Session, SessionConfig, SessionStatus};

/// A part's field that `sch_set_field` can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolField {
    Value,
    Footprint,
}

impl SymbolField {
    /// The property's name in KiCad.
    pub fn property_name(self) -> &'static str {
        match self {
            Self::Value => "Value",
            Self::Footprint => "Footprint",
        }
    }
}

/// The one symbol that `selector` (an id, or a reference matching exactly one) names.
pub(crate) fn select_symbol<'a, T>(
    symbols: &'a [T],
    selector: &str,
    keys: impl Fn(&'a T) -> (&'a str, &'a str),
) -> Result<&'a T, kicad_domain::ToolError> {
    use kicad_domain::{ErrorCode, ToolError};
    let by_id = kicad_domain::Kiid::new(selector).is_ok();
    let matches: Vec<&T> = symbols
        .iter()
        .filter(|s| {
            let (id, reference) = keys(s);
            if by_id {
                id.eq_ignore_ascii_case(selector)
            } else {
                reference == selector
            }
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err(ToolError::new(
            ErrorCode::SymbolNotFound,
            format!("no symbol matches '{selector}'"),
        )),
        many => Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!(
                "'{selector}' matches {} symbols; use one of their ids: {}",
                many.len(),
                many.iter().map(|s| keys(s).0).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}
