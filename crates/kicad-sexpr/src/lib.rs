// SPDX-License-Identifier: GPL-3.0-or-later
//! KiCad's s-expression files, for the KiCad 10 fallback (plan §5.3): a parser that keeps source
//! positions, symbol library and schematic readers, and a writer that adds planned items to a
//! `.kicad_sch` text without touching anything else in it.

pub mod dsn;
pub mod emit;
pub mod fab;
pub mod fp;
pub mod node;
pub mod pcb;
pub mod pcb_detail;
pub mod sch;
pub mod ses;
pub mod sheet;
pub mod sym;

pub use emit::{add_plan, set_property, EmitError};
pub use node::{mm, parse, quote, Kind, Node, ParseError};
pub use sch::{empty_schematic, DocError, Document, PlacedSymbol, MAX_VERSION, MIN_VERSION};
pub use sym::{describe, property, SymError, SymbolLibrary};
