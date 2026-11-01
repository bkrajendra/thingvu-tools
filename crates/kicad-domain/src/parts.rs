// SPDX-License-Identifier: GPL-3.0-or-later
//! Looking up parts (Phase 15.0, decision W1).
//!
//! The server makes no network calls of its own. Part data comes from an external parts service,
//! the pcbparts MCP, which the assistant has next to this server (the installer asks the user to
//! add it). `parts_lookup` is the stub that sits between the two: it turns the BOM's missing data
//! (`bom::review`) into the exact calls to make there and the fields to record afterwards. A
//! [`PartsSource`] describes where the answers come from, so a source built into this server can
//! replace the external one later without a change to the tool, its name, or its result shape:
//! it would answer in `results` and mark the request `answered` instead of `delegated`.

use serde::Serialize;
use serde_json::{json, Value};

use crate::bom::{BomReview, Lookup};

/// Where part data comes from.
pub trait PartsSource {
    /// What the assistant is told about the source.
    fn describe(&self) -> SourceInfo;
    /// What to do for one BOM line that needs data.
    fn request(&self, id: usize, lookup: &Lookup) -> LookupRequest;
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceInfo {
    /// `external` (a service the assistant calls) or `internal` (answered by this server).
    pub kind: &'static str,
    pub name: &'static str,
    /// Where the external service is.
    pub endpoint: &'static str,
    /// Whether looking a part up sends its value, package, or number to that service.
    pub uses_network: bool,
    /// What to tell the user when the assistant has no such tools.
    pub if_missing: &'static str,
    /// Whether this server can answer lookups itself.
    pub internal_available: bool,
}

/// One call the assistant makes on the external service.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    pub tool: &'static str,
    pub arguments: Value,
    pub purpose: &'static str,
}

/// A field to record on the parts of a line, and where its text comes from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordField {
    pub field: &'static str,
    pub from: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LookupRequest {
    pub id: String,
    pub references: Vec<String>,
    pub quantity: usize,
    pub value: String,
    pub needs: Vec<&'static str>,
    /// `delegated` (the assistant does the steps) or `answered` (the server did; see `results`).
    pub status: &'static str,
    pub steps: Vec<Step>,
    pub record: Vec<RecordField>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PartsLookup {
    pub source: SourceInfo,
    pub requests: Vec<LookupRequest>,
    /// How the answers are recorded.
    pub then: Value,
    pub notes: Vec<&'static str>,
}

/// The pcbparts MCP (https://pcbparts.dev): JLCPCB parts with stock and prices, Mouser and
/// DigiKey, no key needed.
pub struct PcbParts;

impl PartsSource for PcbParts {
    fn describe(&self) -> SourceInfo {
        SourceInfo {
            kind: "external",
            name: "pcbparts",
            endpoint: "https://pcbparts.dev/mcp",
            uses_network: true,
            if_missing: "Ask the user to add the pcbparts MCP: in Claude Code run `claude mcp add --transport http pcbparts https://pcbparts.dev/mcp`; in Claude Desktop add https://pcbparts.dev/mcp as a custom connector. The thingsvu-tools installer offers this step.",
            internal_available: false,
        }
    }

    fn request(&self, id: usize, lookup: &Lookup) -> LookupRequest {
        let mut steps = Vec::new();
        match &lookup.mpn {
            Some(mpn) => steps.push(Step {
                tool: "jlc_get_part",
                arguments: json!({ "mpn": mpn }),
                purpose:
                    "find the JLCPCB part for this manufacturer part number: its LCSC code, stock, price, datasheet",
            }),
            None => {
                let mut arguments = json!({ "query": search_query(lookup), "limit": 5 });
                if let Some(package) = package_hint(&lookup.footprint) {
                    arguments["package"] = json!(package);
                }
                steps.push(Step {
                    tool: "jlc_search",
                    arguments,
                    purpose: "find in-stock candidates; basic and preferred parts carry no extra fee",
                });
                steps.push(Step {
                    tool: "jlc_get_part",
                    arguments: json!({ "lcsc": "<LCSC code of the chosen candidate>" }),
                    purpose: "only when the search result does not show the manufacturer part number, manufacturer, or datasheet",
                });
            }
        }
        let mut record = Vec::new();
        if lookup.needs.contains(&"mpn") {
            record.push(RecordField {
                field: "MPN",
                from: "the manufacturer part number of the chosen part",
            });
            record.push(RecordField {
                field: "Manufacturer",
                from: "its manufacturer",
            });
        }
        if lookup.needs.contains(&"supplier") {
            record.push(RecordField {
                field: "LCSC",
                from: "its LCSC code (C followed by digits)",
            });
        }
        LookupRequest {
            id: format!("L{id}"),
            references: lookup.references.clone(),
            quantity: lookup.quantity,
            value: lookup.value.clone(),
            needs: lookup.needs.clone(),
            status: "delegated",
            steps,
            record,
        }
    }
}

/// The source in use: always the external one until an internal source exists.
pub fn active_source() -> &'static dyn PartsSource {
    &PcbParts
}

/// Turns the BOM review's lookups into requests. `only` limits them to lines that contain one of
/// those references (empty: every line).
pub fn plan_lookups(source: &dyn PartsSource, review: &BomReview, only: &[String]) -> PartsLookup {
    let requests: Vec<LookupRequest> = review
        .lookups
        .iter()
        .filter(|l| only.is_empty() || l.references.iter().any(|r| only.contains(r)))
        .enumerate()
        .map(|(i, l)| source.request(i + 1, l))
        .collect();
    PartsLookup {
        source: source.describe(),
        requests,
        then: json!({
            "tool": "sch_set_fields_plan",
            "edits": "one entry per reference of a request: { \"reference\": \"R2\", \"fields\": { \"MPN\": \"...\", \"Manufacturer\": \"...\", \"LCSC\": \"C...\" } }, all requests in one plan",
            "afterwards": "bom_review again: the lines that were answered no longer appear",
        }),
        notes: vec![
            "The server looked nothing up. The steps are calls for the assistant to make on the parts service.",
            "Take the part from the results, not from memory; if several fit, prefer a basic or preferred JLCPCB part in stock, and ask the user when the choice matters (voltage, tolerance, current).",
            "Before ordering, confirm stock of the final parts (jlc_stock_check).",
            "A line with no good match is better left without a number than given a wrong one.",
        ],
    }
}

/// The package to filter a search by: an imperial size (0603) or the first word of a package name
/// (SOT-23-5, SOIC-8), read from a footprint name like `Resistor_SMD:R_0603_1608Metric`.
pub fn package_hint(footprint: &str) -> Option<String> {
    let name = footprint.rsplit(':').next().unwrap_or_default();
    const SIZES: [&str; 8] = ["01005", "0201", "0402", "0603", "0805", "1206", "1210", "2512"];
    const FAMILIES: [&str; 10] = [
        "SOT", "SOIC", "SOP", "TSSOP", "MSOP", "QFN", "DFN", "LQFP", "TQFP", "SOD",
    ];
    let tokens: Vec<&str> = name.split('_').collect();
    if let Some(size) = tokens.iter().find(|t| SIZES.contains(t)) {
        return Some((*size).to_owned());
    }
    tokens
        .iter()
        .find(|t| FAMILIES.iter().any(|f| t.starts_with(f)))
        .map(|t| (*t).to_owned())
}

/// What to search for: the value, with the kind of part spelled out when the reference tells it.
pub fn search_query(lookup: &Lookup) -> String {
    let prefix: String = lookup
        .references
        .first()
        .map(|r| r.chars().take_while(|c| c.is_ascii_alphabetic()).collect())
        .unwrap_or_default();
    let kind = match prefix.as_str() {
        "R" => "resistor",
        "C" => "capacitor",
        "L" => "inductor",
        "FB" => "ferrite bead",
        "F" => "fuse",
        "Y" | "X" => "crystal",
        "LED" => "led",
        "D" => "diode",
        "SW" => "switch",
        "J" | "P" => "connector",
        _ => "",
    };
    let value = lookup.value.trim();
    if kind.is_empty() || value.to_lowercase().contains(kind) {
        value.to_owned()
    } else {
        format!("{value} {kind}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(references: &[&str], value: &str, footprint: &str, needs: &[&'static str], mpn: Option<&str>) -> Lookup {
        Lookup {
            references: references.iter().map(|s| (*s).to_owned()).collect(),
            quantity: references.len(),
            value: value.into(),
            footprint: footprint.into(),
            description: String::new(),
            query: String::new(),
            needs: needs.to_vec(),
            mpn: mpn.map(str::to_owned),
        }
    }

    #[test]
    fn the_package_comes_from_the_footprint_name() {
        assert_eq!(package_hint("Resistor_SMD:R_0603_1608Metric").as_deref(), Some("0603"));
        assert_eq!(package_hint("Capacitor_SMD:C_0805_2012Metric").as_deref(), Some("0805"));
        assert_eq!(
            package_hint("Package_TO_SOT_SMD:SOT-223-3_TabPin2").as_deref(),
            Some("SOT-223-3")
        );
        assert_eq!(
            package_hint("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm").as_deref(),
            Some("SOIC-8")
        );
        assert_eq!(package_hint("Connector_USB:USB_C_Receptacle_X"), None);
        assert_eq!(package_hint(""), None);
    }

    #[test]
    fn the_query_names_the_kind_of_part() {
        let r = lookup(&["R2", "R5"], "10k", "", &["mpn"], None);
        assert_eq!(search_query(&r), "10k resistor");
        let c = lookup(&["C1"], "100nF", "", &["mpn"], None);
        assert_eq!(search_query(&c), "100nF capacitor");
        let u = lookup(&["U1"], "AMS1117-3.3", "", &["mpn"], None);
        assert_eq!(search_query(&u), "AMS1117-3.3");
        let said = lookup(&["FB1"], "ferrite bead 600R", "", &["mpn"], None);
        assert_eq!(search_query(&said), "ferrite bead 600R");
    }

    #[test]
    fn a_line_without_a_number_is_searched_and_one_with_a_number_is_looked_up() {
        let src = PcbParts;
        let search = src.request(
            1,
            &lookup(
                &["R2"],
                "10k",
                "Resistor_SMD:R_0603_1608Metric",
                &["mpn", "supplier"],
                None,
            ),
        );
        assert_eq!(search.id, "L1");
        assert_eq!(search.status, "delegated");
        assert_eq!(search.steps[0].tool, "jlc_search");
        assert_eq!(search.steps[0].arguments["query"], "10k resistor");
        assert_eq!(search.steps[0].arguments["package"], "0603");
        let fields: Vec<&str> = search.record.iter().map(|f| f.field).collect();
        assert_eq!(fields, ["MPN", "Manufacturer", "LCSC"]);

        let known = src.request(
            2,
            &lookup(&["U1"], "AMS1117-3.3", "", &["supplier"], Some("AMS1117-3.3")),
        );
        assert_eq!(known.steps.len(), 1);
        assert_eq!(known.steps[0].tool, "jlc_get_part");
        assert_eq!(known.steps[0].arguments["mpn"], "AMS1117-3.3");
        let fields: Vec<&str> = known.record.iter().map(|f| f.field).collect();
        assert_eq!(fields, ["LCSC"], "only what is missing is recorded");
    }

    #[test]
    fn the_plan_can_be_limited_to_some_parts_and_names_its_source() {
        let review = BomReview {
            parts: 3,
            lines: 2,
            issues: vec![],
            lookups: vec![
                lookup(&["R1", "R2"], "10k", "", &["mpn"], None),
                lookup(&["C1"], "100nF", "", &["mpn"], None),
            ],
        };
        let all = plan_lookups(active_source(), &review, &[]);
        assert_eq!(all.requests.len(), 2);
        assert_eq!(all.source.name, "pcbparts");
        assert!(all.source.uses_network && !all.source.internal_available);
        assert!(all
            .source
            .if_missing
            .contains("claude mcp add --transport http pcbparts"));
        assert_eq!(all.then["tool"], "sch_set_fields_plan");
        let some = plan_lookups(active_source(), &review, &["C1".to_owned()]);
        assert_eq!(some.requests.len(), 1);
        assert_eq!(some.requests[0].references, ["C1"]);
        assert_eq!(some.requests[0].id, "L1");
        assert!(plan_lookups(active_source(), &review, &["Z9".to_owned()])
            .requests
            .is_empty());
    }
}
