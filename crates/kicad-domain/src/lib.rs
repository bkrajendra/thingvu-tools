// SPDX-License-Identifier: GPL-3.0-or-later
//! Stable domain model shared by the MCP server and every KiCad backend.
//!
//! Nothing here depends on how KiCad is reached (IPC, files, or CLI). Backends translate between
//! these types and KiCad's own representations.

pub mod arrange;
pub mod board;
pub mod board_detail;
pub mod board_edit;
pub mod bom;
pub mod capability;
pub mod changes;
pub mod circuit;
pub mod connectivity;
pub mod declutter;
pub mod edits;
pub mod fab;
pub mod footprint;
pub mod geometry;
pub mod ids;
pub mod layout;
pub mod mechanical;
pub mod parts;
pub mod pin_net;
pub mod placement;
pub mod plan;
pub mod readability;
pub mod reconnect;
pub mod result;
pub mod routing;
pub mod rules;
pub mod schematic;
pub mod text;
pub mod tidy;
pub mod track_edit;
pub mod validation;
pub mod zones;

pub use arrange::{plan_arrange, Mirror, PartEdit};
pub use capability::{Backend, Capability, CapabilitySet, Support};
pub use circuit::{CircuitSpec, ComponentSpec, NetSpec, PinRef};
pub use connectivity::attach_power_pins;
pub use declutter::plan_declutter;
pub use edits::{plan_draw, plan_edit_items, plan_labels, DrawItem, LabelAdd, LabelAt, LabelEdits};
pub use footprint::{plan_footprints, FootprintInfo, FootprintPad};
pub use geometry::{Dir, Nm, Orientation, Point, Rect, GRID};
pub use ids::{IdError, Kiid, LibraryId, LogicalId};
pub use layout::{is_ground_name, library_ids_for};
pub use plan::{
    plan_circuit, ExistingDocument, ExpectedNet, LibraryPin, LibrarySymbol, Operation, OperationCounts, OperationPlan,
};
pub use readability::Readability;
pub use result::{ErrorCode, MutationState, Origin, Provenance, ResultState, ToolError};
pub use schematic::{
    DesignJunction, DesignLabel, DesignNoConnect, DesignSymbol, DesignWire, NetInfo, NetPin, PinInfo, PointMm,
    SchematicDesign, SymbolInfo, SymbolPins,
};
pub use tidy::{
    design_library_ids, plan_reconnect, plan_tidy, plan_tidy_with_hints, tidy_spec, LayoutHints, NearHint, PlaceHint,
};
pub use validation::{validate_circuit, Finding, Severity};
