// SPDX-License-Identifier: GPL-3.0-or-later
//! The bill of materials of a sheet, and a review of it (Phase 10).
//!
//! Parts are grouped the way KiCad groups them (same value and footprint, DNP apart) and also by
//! every custom field, so two capacitors with the same value but different part numbers are two
//! lines. The review lists what an order would need and cannot yet have: footprints, manufacturer
//! and supplier part numbers. It reads only the sheet; part data comes from outside (design plan
//! decision D2).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::schematic::{DesignSymbol, SchematicDesign};
use crate::validation::Severity;

/// Fields every symbol has; anything else is a custom field.
pub const STANDARD_FIELDS: [&str; 5] = ["Reference", "Value", "Footprint", "Datasheet", "Description"];

/// Names under which a manufacturer part number is kept.
pub const MPN_FIELDS: [&str; 4] = [
    "MPN",
    "Manufacturer Part Number",
    "Manufacturer_Part_Number",
    "Mfr. Part #",
];
/// Names under which a manufacturer is kept.
pub const MANUFACTURER_FIELDS: [&str; 3] = ["Manufacturer", "MFR", "Mfr."];
/// Names under which a supplier's part number is kept.
pub const SUPPLIER_FIELDS: [&str; 8] = [
    "LCSC",
    "LCSC Part",
    "LCSC Part #",
    "JLCPCB Part #",
    "Mouser",
    "DigiKey",
    "Supplier Part",
    "Supplier",
];

/// `R10` sorts after `R2`: references by prefix, then number.
pub fn natural(reference: &str) -> (String, u64, String) {
    let prefix: String = reference.chars().take_while(|c| !c.is_ascii_digit()).collect();
    let number = reference[prefix.len()..].parse::<u64>().unwrap_or(u64::MAX);
    (prefix, number, reference.to_owned())
}

/// Fields KiCad's simulator keeps on a part (`Sim.Device`, `Sim.Pins`, ...): not part data.
fn is_simulation_field(name: &str) -> bool {
    name.starts_with("Sim.")
}

/// A real part: not a power symbol or flag.
fn is_part(s: &DesignSymbol) -> bool {
    !s.reference.starts_with('#')
}

fn field<'a>(s: &'a DesignSymbol, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|n| s.properties.get(*n).map(String::as_str))
        .filter(|v| !v.trim().is_empty())
}

#[derive(Debug, Clone, Default)]
pub struct BomOptions {
    /// Fields that must match for parts to share a line; empty for the default (value, footprint,
    /// and every custom field).
    pub group_by: Vec<String>,
    /// Leave out parts marked do-not-populate.
    pub exclude_dnp: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BomRow {
    pub item: usize,
    pub references: Vec<String>,
    pub quantity: usize,
    pub value: String,
    pub footprint: String,
    pub datasheet: String,
    pub description: String,
    /// Custom fields (manufacturer, part numbers, …) with a value.
    pub fields: BTreeMap<String, String>,
    pub dnp: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bom {
    pub rows: Vec<BomRow>,
    pub total_parts: usize,
    /// Every custom field name used by any part.
    pub custom_fields: Vec<String>,
    /// Parts left out of the BOM by their own flag.
    pub excluded: Vec<String>,
}

/// The BOM of the sheet. A part with several units counts once.
pub fn build(design: &SchematicDesign, options: &BomOptions) -> Bom {
    let mut seen = BTreeSet::new();
    let parts: Vec<&DesignSymbol> = design
        .symbols
        .iter()
        .filter(|s| is_part(s) && seen.insert(s.reference.clone()))
        .collect();
    let custom_fields: BTreeSet<String> = parts
        .iter()
        .flat_map(|s| s.properties.iter())
        .filter(|(k, v)| !STANDARD_FIELDS.contains(&k.as_str()) && !is_simulation_field(k) && !v.trim().is_empty())
        .map(|(k, _)| k.clone())
        .collect();
    let excluded: Vec<String> = parts
        .iter()
        .filter(|s| s.exclude_from_bom)
        .map(|s| s.reference.clone())
        .collect();

    let key_fields: Vec<String> = if options.group_by.is_empty() {
        ["Value", "Footprint"]
            .into_iter()
            .map(str::to_owned)
            .chain(custom_fields.iter().cloned())
            .collect()
    } else {
        options.group_by.clone()
    };
    let prop = |s: &DesignSymbol, name: &str| -> String {
        match name {
            "Value" => s.value.clone(),
            "Footprint" => s.footprint.clone(),
            _ => s.properties.get(name).cloned().unwrap_or_default(),
        }
    };
    let mut groups: BTreeMap<(Vec<String>, bool), Vec<&DesignSymbol>> = BTreeMap::new();
    for s in parts
        .iter()
        .filter(|s| !s.exclude_from_bom && !(options.exclude_dnp && s.dnp))
    {
        let key: Vec<String> = key_fields.iter().map(|k| prop(s, k)).collect();
        groups.entry((key, s.dnp)).or_default().push(s);
    }
    let mut rows: Vec<BomRow> = groups
        .into_values()
        .map(|mut members| {
            members.sort_by_key(|s| natural(&s.reference));
            let first = members[0];
            BomRow {
                item: 0,
                references: members.iter().map(|s| s.reference.clone()).collect(),
                quantity: members.len(),
                value: first.value.clone(),
                footprint: first.footprint.clone(),
                datasheet: prop(first, "Datasheet"),
                description: prop(first, "Description"),
                fields: custom_fields
                    .iter()
                    .filter_map(|k| {
                        let v = prop(first, k);
                        (!v.trim().is_empty()).then(|| (k.clone(), v))
                    })
                    .collect(),
                dnp: first.dnp,
            }
        })
        .collect();
    rows.sort_by_key(|r| natural(&r.references[0]));
    for (i, row) in rows.iter_mut().enumerate() {
        row.item = i + 1;
    }
    Bom {
        total_parts: rows.iter().map(|r| r.quantity).sum(),
        rows,
        custom_fields: custom_fields.into_iter().collect(),
        excluded,
    }
}

// ---------------------------------------------------------------- values

/// A component value as a number in base units, when it is one: `10k`, `4k7`, `0.1uF`, `100nF`,
/// `1R0`, `22 ohm`. None for anything else (`LM358`, `~`).
pub fn parse_value(text: &str) -> Option<f64> {
    let t: String = text
        .trim()
        .replace(['Ω', '\u{2126}'], "R")
        .replace("ohms", "R")
        .replace("ohm", "R")
        .replace('µ', "u")
        .replace(' ', "");
    if t.is_empty() {
        return None;
    }
    // Strip a trailing unit letter after a prefix ("100nF", "10uH", "1MR").
    let mut chars: Vec<char> = t.chars().collect();
    if chars.len() > 1 && matches!(chars.last(), Some('F') | Some('H') | Some('f') | Some('h')) {
        chars.pop();
    }
    // "10kR" (10 kilo-ohm written with the ohm sign): the unit follows a prefix letter.
    if chars.len() > 2 && matches!(chars.last(), Some('R') | Some('r')) && chars[chars.len() - 2].is_ascii_alphabetic()
    {
        chars.pop();
    }
    let multiplier = |c: char| -> Option<f64> {
        Some(match c {
            'p' => 1e-12,
            'n' => 1e-9,
            'u' => 1e-6,
            'm' => 1e-3,
            'R' | 'r' => 1.0,
            'k' | 'K' => 1e3,
            'M' => 1e6,
            'G' => 1e9,
            _ => return None,
        })
    };
    let s: String = chars.iter().collect();
    // RKM code: the letter stands for the decimal point (4k7 = 4.7k, 1R0 = 1.0).
    if let Some(pos) = s.find(|c: char| multiplier(c).is_some() && !c.is_ascii_digit()) {
        let (head, tail) = (&s[..pos], &s[pos + 1..]);
        let letter = s[pos..].chars().next()?;
        if head.chars().all(|c| c.is_ascii_digit() || c == '.')
            && tail.chars().all(|c| c.is_ascii_digit())
            && !head.is_empty()
        {
            let number: f64 = if tail.is_empty() || head.contains('.') {
                // "10k", "0.1u": the letter is only the unit prefix.
                if !tail.is_empty() {
                    return None;
                }
                head.parse().ok()?
            } else {
                format!("{head}.{tail}").parse().ok()?
            };
            return Some(number * multiplier(letter)?);
        }
        return None;
    }
    s.parse::<f64>().ok()
}

/// Do two spellings of a value mean the same number?
fn same_number(a: f64, b: f64) -> bool {
    (a - b).abs() <= a.abs().max(b.abs()) * 1e-9
}

// ---------------------------------------------------------------- review

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BomIssue {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub references: Vec<String>,
}

/// A line the assistant should look up, with a search hint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Lookup {
    pub references: Vec<String>,
    pub quantity: usize,
    pub value: String,
    pub footprint: String,
    pub description: String,
    /// What to search for.
    pub query: String,
    /// What is missing: `mpn`, `supplier`.
    pub needs: Vec<&'static str>,
    /// The manufacturer part number the line already has, when only the supplier's is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mpn: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BomReview {
    pub parts: usize,
    pub lines: usize,
    pub issues: Vec<BomIssue>,
    pub lookups: Vec<Lookup>,
}

fn issue(code: &'static str, severity: Severity, message: String, references: Vec<String>) -> BomIssue {
    BomIssue {
        code,
        severity,
        message,
        references,
    }
}

pub fn review(design: &SchematicDesign) -> BomReview {
    let bom = build(design, &BomOptions::default());
    let mut issues = Vec::new();
    let mut seen = BTreeSet::new();
    let parts: Vec<&DesignSymbol> = design
        .symbols
        .iter()
        .filter(|s| is_part(s) && seen.insert(s.reference.clone()))
        .collect();

    // Two parts with the same reference (KiCad would renumber one).
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for s in design.symbols.iter().filter(|s| is_part(s) && s.unit <= 1) {
        *counts.entry(s.reference.as_str()).or_default() += 1;
    }
    for (reference, n) in counts.iter().filter(|(_, n)| **n > 1) {
        issues.push(issue(
            "DUPLICATE_REFERENCE",
            Severity::Error,
            format!("{n} parts are called {reference}"),
            vec![(*reference).to_owned()],
        ));
    }

    let missing_fp: Vec<String> = parts
        .iter()
        .filter(|s| s.footprint.trim().is_empty() && !s.exclude_from_board)
        .map(|s| s.reference.clone())
        .collect();
    if !missing_fp.is_empty() {
        issues.push(issue(
            "MISSING_FOOTPRINT",
            Severity::Error,
            format!(
                "{} part(s) have no footprint, so the board cannot be updated from this schematic",
                missing_fp.len()
            ),
            missing_fp,
        ));
    }

    let placeholder: Vec<String> = parts
        .iter()
        .filter(|s| {
            let v = s.value.trim();
            v.is_empty()
                || v == "~"
                || s.library_id.rsplit(':').next() == Some(v) && matches!(v, "R" | "C" | "L" | "LED" | "D")
        })
        .map(|s| s.reference.clone())
        .collect();
    if !placeholder.is_empty() {
        issues.push(issue(
            "PLACEHOLDER_VALUE",
            Severity::Warning,
            "these parts still carry the library's placeholder value".into(),
            placeholder,
        ));
    }

    // One value written two ways among parts of one kind (R: "10k" and "10K0").
    let mut by_kind: BTreeMap<String, Vec<(&DesignSymbol, f64)>> = BTreeMap::new();
    for s in &parts {
        if let Some(n) = parse_value(&s.value) {
            let kind: String = s.reference.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
            by_kind.entry(kind).or_default().push((s, n));
        }
    }
    for (kind, list) in &by_kind {
        let mut reported: BTreeSet<String> = BTreeSet::new();
        for (i, (a, na)) in list.iter().enumerate() {
            for (b, nb) in &list[i + 1..] {
                if a.value != b.value && same_number(*na, *nb) && reported.insert(format!("{}|{}", a.value, b.value)) {
                    let refs: Vec<String> = list
                        .iter()
                        .filter(|(s, n)| (s.value == a.value || s.value == b.value) && same_number(*n, *na))
                        .map(|(s, _)| s.reference.clone())
                        .collect();
                    issues.push(issue(
                        "VALUE_SPELLINGS",
                        Severity::Warning,
                        format!("{kind} parts write the same value as '{}' and as '{}'; use one spelling so they share a BOM line", a.value, b.value),
                        refs,
                    ));
                }
            }
        }
    }

    // Parts on the board but out of the BOM.
    let off_bom: Vec<String> = parts
        .iter()
        .filter(|s| s.exclude_from_bom && !s.exclude_from_board)
        .map(|s| s.reference.clone())
        .collect();
    if !off_bom.is_empty() {
        issues.push(issue(
            "ON_BOARD_NOT_IN_BOM",
            Severity::Warning,
            "these parts go on the board but are left out of the BOM".into(),
            off_bom,
        ));
    }

    // Stacked parts: two of the same kind drawn on one spot.
    let mut spots: BTreeMap<(i64, i64, String), Vec<String>> = BTreeMap::new();
    for s in &parts {
        spots
            .entry((s.position.x.0, s.position.y.0, s.library_id.clone()))
            .or_default()
            .push(s.reference.clone());
    }
    for refs in spots.into_values().filter(|r| r.len() > 1) {
        issues.push(issue(
            "STACKED_PARTS",
            Severity::Warning,
            "these parts are drawn on top of each other and look like a duplicate".into(),
            refs,
        ));
    }

    // What an order needs, per BOM line.
    let mut lookups = Vec::new();
    let (mut no_mpn, mut no_supplier, mut no_maker) = (Vec::new(), Vec::new(), Vec::new());
    for row in bom.rows.iter().filter(|r| !r.dnp) {
        let sample = parts.iter().find(|s| s.reference == row.references[0]).copied();
        let Some(sample) = sample else { continue };
        let mut needs = Vec::new();
        if field(sample, &MPN_FIELDS).is_none() {
            needs.push("mpn");
            no_mpn.extend(row.references.iter().cloned());
        } else if field(sample, &MANUFACTURER_FIELDS).is_none() {
            no_maker.extend(row.references.iter().cloned());
        }
        if field(sample, &SUPPLIER_FIELDS).is_none() {
            needs.push("supplier");
            no_supplier.extend(row.references.iter().cloned());
        }
        if !needs.is_empty() {
            let footprint_hint = row.footprint.rsplit(':').next().unwrap_or_default();
            lookups.push(Lookup {
                references: row.references.clone(),
                quantity: row.quantity,
                value: row.value.clone(),
                footprint: row.footprint.clone(),
                description: row.description.clone(),
                query: [row.value.as_str(), footprint_hint, row.description.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" "),
                needs,
                mpn: field(sample, &MPN_FIELDS).map(str::to_owned),
            });
        }
    }
    for (code, list, text) in [
        ("MISSING_MPN", no_mpn, "have no manufacturer part number (field MPN)"),
        (
            "MISSING_MANUFACTURER",
            no_maker,
            "have a part number but no manufacturer (field Manufacturer)",
        ),
        (
            "MISSING_SUPPLIER_PART",
            no_supplier,
            "have no supplier part number (for example field LCSC)",
        ),
    ] {
        if !list.is_empty() {
            issues.push(issue(
                code,
                Severity::Warning,
                format!("{} part(s) {text}", list.len()),
                list,
            ));
        }
    }

    BomReview {
        parts: bom.total_parts,
        lines: bom.rows.len(),
        issues,
        lookups,
    }
}

// ---------------------------------------------------------------- setting fields

/// One part's fields to set: (name, text) pairs.
#[derive(Debug, Clone, Default)]
pub struct FieldEdit {
    pub reference: String,
    pub fields: Vec<(String, String)>,
}

/// Plans setting fields on parts: any text field except the reference, and the flags.
pub fn plan_set_fields(
    design: &SchematicDesign,
    nets: &[crate::schematic::NetInfo],
    edits: &[FieldEdit],
) -> crate::plan::OperationPlan {
    use crate::plan::{error, OperationPlan, PropertyChange, FLAG_FIELDS};
    let mut plan = OperationPlan {
        circuit: "fields".into(),
        ..Default::default()
    };
    let mut findings = Vec::new();
    let mut seen = BTreeSet::new();
    for edit in edits {
        if !seen.insert(edit.reference.as_str()) {
            findings.push(error(
                "FIELDS_TWICE",
                format!("{} is named twice; give all its fields in one entry", edit.reference),
                vec![edit.reference.clone()],
            ));
            continue;
        }
        let Some(part) = design
            .symbols
            .iter()
            .find(|s| is_part(s) && s.reference == edit.reference)
        else {
            findings.push(error(
                "FIELDS_UNKNOWN_PART",
                format!("there is no part {}", edit.reference),
                vec![edit.reference.clone()],
            ));
            continue;
        };
        let mut names = BTreeSet::new();
        for (name, value) in &edit.fields {
            let bad = |why: &str| {
                error(
                    "FIELDS_BAD_FIELD",
                    format!("{}: field '{name}': {why}", edit.reference),
                    vec![edit.reference.clone()],
                )
            };
            if name.trim().is_empty() || name.chars().count() > 64 || name.chars().any(|c| c.is_control() || c == '"') {
                findings.push(bad("a name is 1 to 64 characters, with no quote or control character"));
                continue;
            }
            if name == "Reference" {
                findings.push(bad("the reference is set by annotation, not here"));
                continue;
            }
            if !names.insert(name.as_str()) {
                findings.push(bad("named twice"));
                continue;
            }
            if value.chars().count() > 256 || value.chars().any(|c| c.is_control()) {
                findings.push(bad("a value is up to 256 characters with no control character"));
                continue;
            }
            let is_flag = FLAG_FIELDS.contains(&name.as_str());
            if is_flag && value != "yes" && value != "no" {
                findings.push(bad("a flag is set to yes or no"));
                continue;
            }
            let now = match name.as_str() {
                "DNP" => Some(if part.dnp { "yes" } else { "no" }.to_owned()),
                "Exclude from BOM" => Some(if part.exclude_from_bom { "yes" } else { "no" }.to_owned()),
                "Exclude from board" => Some(if part.exclude_from_board { "yes" } else { "no" }.to_owned()),
                "Value" => Some(part.value.clone()),
                "Footprint" => Some(part.footprint.clone()),
                other => part.properties.get(other).cloned(),
            };
            if now.as_deref() == Some(value.as_str()) {
                continue;
            }
            plan.property_changes.push(PropertyChange {
                id: part.id.clone(),
                reference: part.reference.clone(),
                name: name.clone(),
                from: now,
                to: value.clone(),
            });
        }
    }
    if plan.property_changes.is_empty() && !crate::validation::has_errors(&findings) {
        findings.push(crate::validation::Finding {
            severity: Severity::Warning,
            code: "NOTHING_TO_CHANGE",
            message: "every field already has the text asked for".into(),
            objects: vec![],
        });
    }
    plan.expected_nets = crate::edits::expected_from(nets);
    plan.findings = findings;
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(reference: &str, value: &str, footprint: &str, extra: &[(&str, &str)]) -> DesignSymbol {
        let mut properties: BTreeMap<String, String> =
            extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        properties.insert("Value".into(), value.into());
        properties.insert("Footprint".into(), footprint.into());
        DesignSymbol {
            id: reference.into(),
            reference: reference.into(),
            value: value.into(),
            footprint: footprint.into(),
            library_id: "Device:R".into(),
            unit: 1,
            properties,
            position: crate::geometry::Point::new(
                crate::geometry::Nm(reference.len() as i64 * 1_270_000),
                crate::geometry::Nm(reference.bytes().map(i64::from).sum::<i64>() * 1_270_000),
            ),
            ..Default::default()
        }
    }

    fn design(parts: Vec<DesignSymbol>) -> SchematicDesign {
        SchematicDesign {
            symbols: parts,
            ..Default::default()
        }
    }

    #[test]
    fn parts_with_the_same_value_and_footprint_share_a_line() {
        let bom = build(
            &design(vec![
                part("R2", "10k", "Resistor_SMD:R_0603_1608Metric", &[]),
                part("R10", "10k", "Resistor_SMD:R_0603_1608Metric", &[]),
                part("R1", "10k", "Resistor_SMD:R_0603_1608Metric", &[]),
                part("R3", "1k", "Resistor_SMD:R_0603_1608Metric", &[]),
                part("#PWR01", "GND", "", &[]),
            ]),
            &BomOptions::default(),
        );
        assert_eq!(bom.rows.len(), 2);
        assert_eq!(bom.rows[0].references, ["R1", "R2", "R10"]);
        assert_eq!(bom.rows[0].quantity, 3);
        assert_eq!(bom.total_parts, 4);
    }

    #[test]
    fn a_different_part_number_makes_a_different_line_and_dnp_stands_apart() {
        let mut dnp = part("R3", "10k", "F", &[]);
        dnp.dnp = true;
        let mut off = part("R4", "10k", "F", &[]);
        off.exclude_from_bom = true;
        let bom = build(
            &design(vec![
                part("R1", "10k", "F", &[("MPN", "A")]),
                part("R2", "10k", "F", &[("MPN", "B")]),
                dnp,
                off,
            ]),
            &BomOptions::default(),
        );
        assert_eq!(bom.rows.len(), 3);
        assert_eq!(bom.excluded, ["R4"]);
        assert!(bom.rows.iter().any(|r| r.dnp && r.references == ["R3"]));
        let no_dnp = build(
            &design(vec![part("R1", "10k", "F", &[]), {
                let mut d = part("R2", "10k", "F", &[]);
                d.dnp = true;
                d
            }]),
            &BomOptions {
                exclude_dnp: true,
                ..Default::default()
            },
        );
        assert_eq!(no_dnp.total_parts, 1);
    }

    #[test]
    fn values_read_the_way_people_write_them() {
        for (a, b) in [
            ("10k", "10K"),
            ("4k7", "4.7k"),
            ("10k", "10000"),
            ("100nF", "0.1uF"),
            ("1R0", "1"),
            ("22 ohm", "22R"),
            ("10kΩ", "10k"),
        ] {
            let (x, y) = (parse_value(a).unwrap(), parse_value(b).unwrap());
            assert!(same_number(x, y), "{a} = {x}, {b} = {y}");
        }
        assert!(parse_value("LM358").is_none());
        assert!(parse_value("~").is_none());
        assert!(!same_number(parse_value("10k").unwrap(), parse_value("1k").unwrap()));
    }

    #[test]
    fn the_review_finds_what_an_order_lacks() {
        let r = review(&design(vec![
            part(
                "R1",
                "10k",
                "Resistor_SMD:R_0603_1608Metric",
                &[
                    ("MPN", "RC0603FR-0710KL"),
                    ("Manufacturer", "Yageo"),
                    ("LCSC", "C25804"),
                ],
            ),
            part("R2", "10K", "Resistor_SMD:R_0603_1608Metric", &[]),
            part("R3", "R", "", &[]),
        ]));
        let codes: Vec<&str> = r.issues.iter().map(|i| i.code).collect();
        for want in [
            "MISSING_FOOTPRINT",
            "PLACEHOLDER_VALUE",
            "VALUE_SPELLINGS",
            "MISSING_MPN",
            "MISSING_SUPPLIER_PART",
        ] {
            assert!(codes.contains(&want), "{want} in {codes:?}");
        }
        // R1 is complete; the other lines need a lookup.
        assert!(r.lookups.iter().all(|l| !l.references.contains(&"R1".to_owned())));
        assert!(r
            .lookups
            .iter()
            .any(|l| l.references.contains(&"R2".to_owned()) && l.needs.contains(&"mpn")));
    }

    #[test]
    fn fields_are_planned_checked_and_unchanged_ones_skipped() {
        let d = design(vec![part("R1", "10k", "F", &[("MPN", "A")])]);
        let ok = plan_set_fields(
            &d,
            &[],
            &[FieldEdit {
                reference: "R1".into(),
                fields: vec![
                    ("MPN".into(), "A".into()),
                    ("Manufacturer".into(), "Yageo".into()),
                    ("DNP".into(), "yes".into()),
                ],
            }],
        );
        assert!(!crate::validation::has_errors(&ok.findings), "{:?}", ok.findings);
        assert_eq!(ok.property_changes.len(), 2, "the MPN is already A");
        let bad = plan_set_fields(
            &d,
            &[],
            &[
                FieldEdit {
                    reference: "R1".into(),
                    fields: vec![
                        ("Reference".into(), "R9".into()),
                        ("DNP".into(), "maybe".into()),
                        ("".into(), "x".into()),
                    ],
                },
                FieldEdit {
                    reference: "R9".into(),
                    fields: vec![],
                },
            ],
        );
        let codes: Vec<_> = bad.findings.iter().map(|f| f.code).collect();
        assert_eq!(
            codes.iter().filter(|c| **c == "FIELDS_BAD_FIELD").count(),
            3,
            "{codes:?}"
        );
        assert!(codes.contains(&"FIELDS_UNKNOWN_PART"));
    }

    #[test]
    fn a_complete_sheet_has_no_issues() {
        let r = review(&design(vec![part(
            "R1",
            "10k",
            "Resistor_SMD:R_0603_1608Metric",
            &[("MPN", "X"), ("Manufacturer", "Y"), ("LCSC", "C1")],
        )]));
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        assert!(r.lookups.is_empty());
    }

    #[test]
    fn stacked_and_duplicate_parts_are_noticed() {
        let mut a = part("R1", "10k", "F", &[]);
        let mut b = part("R2", "10k", "F", &[]);
        b.position = a.position;
        a.properties.insert("MPN".into(), "X".into());
        let r = review(&design(vec![a.clone(), b, a]));
        let codes: Vec<&str> = r.issues.iter().map(|i| i.code).collect();
        assert!(codes.contains(&"STACKED_PARTS"), "{codes:?}");
        assert!(codes.contains(&"DUPLICATE_REFERENCE"), "{codes:?}");
    }
}
