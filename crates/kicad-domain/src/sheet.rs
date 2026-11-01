// SPDX-License-Identifier: GPL-3.0-or-later
//! The page and title block of a schematic (Phase 15.1): size and orientation, and the text of
//! the title block (title, date, revision, company, and nine comments). A change is a plan like
//! any other; it touches nothing else on the sheet.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::plan::OperationPlan;
use crate::validation::{Finding, Severity};

/// The sizes KiCad knows, under the names the schematic file uses.
pub const SIZES: [&str; 15] = [
    "A5", "A4", "A3", "A2", "A1", "A0", "A", "B", "C", "D", "E", "GERBER", "USLetter", "USLegal", "USLedger",
];

/// The largest and smallest custom page, in millimetres (KiCad limits a page to 48 inches).
pub const MIN_USER_MM: f64 = 50.0;
pub const MAX_USER_MM: f64 = 1219.2;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Page {
    /// A name from [`SIZES`], or `User`.
    pub size: String,
    /// The custom size, for `User` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height_mm: Option<f64>,
    pub portrait: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TitleBlock {
    pub title: String,
    pub date: String,
    pub revision: String,
    pub company: String,
    /// Comments 1 to 9; one that is not there is empty.
    pub comments: BTreeMap<u8, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SheetSettings {
    pub page: Page,
    pub title_block: TitleBlock,
}

impl SheetSettings {
    /// The same page and title block. A custom size is compared to 0.1 mm: KiCad keeps it in
    /// thousandths of an inch.
    pub fn same_as(&self, other: &SheetSettings) -> bool {
        let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => (a - b).abs() < 0.1,
            (None, None) => true,
            _ => false,
        };
        self.page.size == other.page.size
            && self.page.portrait == other.page.portrait
            && close(self.page.width_mm, other.page.width_mm)
            && close(self.page.height_mm, other.page.height_mm)
            && self.title_block == other.title_block
    }
}

/// What a plan asks for. Anything left out stays as it is; an empty text clears a field.
#[derive(Debug, Clone, Default)]
pub struct SheetRequest {
    /// A standard size (case does not matter; `letter`, `legal`, `ledger`, `ansi_a` and so on are understood).
    pub size: Option<String>,
    /// A custom size: both are needed.
    pub width_mm: Option<f64>,
    pub height_mm: Option<f64>,
    /// `landscape` or `portrait`.
    pub orientation: Option<String>,
    pub title: Option<String>,
    pub date: Option<String>,
    pub revision: Option<String>,
    pub company: Option<String>,
    pub comments: BTreeMap<u8, String>,
}

/// The page and title block before and after a plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SheetChange {
    pub from: SheetSettings,
    pub to: SheetSettings,
}

impl SheetChange {
    /// One line per thing that changes, for the person reviewing the plan.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        let (a, b) = (&self.from.page, &self.to.page);
        if a != b {
            out.push(format!("page: {} -> {}", page_text(a), page_text(b)));
        }
        let (x, y) = (&self.from.title_block, &self.to.title_block);
        for (name, from, to) in [
            ("title", &x.title, &y.title),
            ("date", &x.date, &y.date),
            ("revision", &x.revision, &y.revision),
            ("company", &x.company, &y.company),
        ] {
            if from != to {
                out.push(format!("{name}: '{from}' -> '{to}'"));
            }
        }
        for n in 1..=9u8 {
            let (from, to) = (
                x.comments.get(&n).cloned().unwrap_or_default(),
                y.comments.get(&n).cloned().unwrap_or_default(),
            );
            if from != to {
                out.push(format!("comment {n}: '{from}' -> '{to}'"));
            }
        }
        out
    }
}

pub fn page_text(p: &Page) -> String {
    let mut s = if p.size == "User" {
        format!(
            "custom {} x {} mm",
            p.width_mm.map_or(0.0, round1),
            p.height_mm.map_or(0.0, round1)
        )
    } else {
        p.size.clone()
    };
    s.push_str(if p.portrait { ", portrait" } else { ", landscape" });
    s
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The page's width and height in millimetres, turned as the page is.
pub fn size_mm(p: &Page) -> Option<(f64, f64)> {
    let (w, h) = match p.size.as_str() {
        "A5" => (210.0, 148.0),
        "A4" => (297.0, 210.0),
        "A3" => (420.0, 297.0),
        "A2" => (594.0, 420.0),
        "A1" => (841.0, 594.0),
        "A0" => (1189.0, 841.0),
        "A" => (279.4, 215.9),
        "B" => (431.8, 279.4),
        "C" => (558.8, 431.8),
        "D" => (863.6, 558.8),
        "E" => (1117.6, 863.6),
        "GERBER" => (812.8, 812.8),
        "USLetter" => (279.4, 215.9),
        "USLegal" => (355.6, 215.9),
        "USLedger" => (431.8, 279.4),
        "User" => (p.width_mm?, p.height_mm?),
        _ => return None,
    };
    Some(if p.portrait && p.size != "User" { (h, w) } else { (w, h) })
}

/// The references of the parts whose place is outside a page of `width` x `height` mm (the page
/// starts at the sheet's origin).
pub fn parts_off_page(design: &crate::schematic::SchematicDesign, width: f64, height: f64) -> Vec<String> {
    let mut out: Vec<String> = design
        .symbols
        .iter()
        .filter(|s| {
            let (x, y) = (s.position.x.to_mm(), s.position.y.to_mm());
            x < 0.0 || y < 0.0 || x > width || y > height
        })
        .map(|s| s.reference.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The file's name for a size the user wrote, or `None`.
pub fn canonical_size(text: &str) -> Option<&'static str> {
    let key: String = text
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect();
    let key = key.strip_prefix("ansi").unwrap_or(&key).to_owned();
    Some(match key.as_str() {
        "a5" => "A5",
        "a4" => "A4",
        "a3" => "A3",
        "a2" => "A2",
        "a1" => "A1",
        "a0" => "A0",
        "a" => "A",
        "b" => "B",
        "c" => "C",
        "d" => "D",
        "e" => "E",
        "gerber" => "GERBER",
        "letter" | "usletter" => "USLetter",
        "legal" | "uslegal" => "USLegal",
        "ledger" | "usledger" | "tabloid" => "USLedger",
        _ => return None,
    })
}

fn finding(code: &'static str, message: String) -> Finding {
    Finding {
        severity: Severity::Error,
        code,
        message,
        objects: vec![],
    }
}

/// The settings after the request, or why it cannot be done.
pub fn apply_request(current: &SheetSettings, req: &SheetRequest) -> Result<SheetSettings, Vec<Finding>> {
    let mut problems = Vec::new();
    let mut to = current.clone();

    if req.size.is_some() && (req.width_mm.is_some() || req.height_mm.is_some()) {
        problems.push(finding(
            "SIZE_AND_CUSTOM",
            "give either a standard size or width_mm and height_mm, not both".into(),
        ));
    }
    if let Some(size) = &req.size {
        match canonical_size(size) {
            Some(name) => {
                to.page.size = name.to_owned();
                to.page.width_mm = None;
                to.page.height_mm = None;
            }
            None => problems.push(finding(
                "UNKNOWN_PAGE_SIZE",
                format!(
                    "'{size}' is not a page size; use one of {} (or width_mm and height_mm)",
                    SIZES.join(", ")
                ),
            )),
        }
    }
    match (req.width_mm, req.height_mm) {
        (Some(w), Some(h)) => {
            if [w, h]
                .iter()
                .any(|v| !v.is_finite() || !(MIN_USER_MM..=MAX_USER_MM).contains(v))
            {
                problems.push(finding(
                    "PAGE_SIZE_OUT_OF_RANGE",
                    format!("a custom page is {MIN_USER_MM} to {MAX_USER_MM} mm on each side"),
                ));
            } else {
                to.page.size = "User".into();
                to.page.width_mm = Some(w);
                to.page.height_mm = Some(h);
                to.page.portrait = false;
            }
        }
        (None, None) => {}
        _ => problems.push(finding(
            "CUSTOM_PAGE_NEEDS_BOTH",
            "a custom page needs both width_mm and height_mm".into(),
        )),
    }
    if let Some(o) = &req.orientation {
        match o.trim().to_lowercase().as_str() {
            "landscape" | "portrait" if to.page.size == "User" => problems.push(finding(
                "ORIENTATION_ON_CUSTOM_PAGE",
                "a custom page has no orientation; swap width_mm and height_mm instead".into(),
            )),
            "landscape" => to.page.portrait = false,
            "portrait" => to.page.portrait = true,
            other => problems.push(finding(
                "UNKNOWN_ORIENTATION",
                format!("orientation is landscape or portrait, not '{other}'"),
            )),
        }
    }

    let tb = &mut to.title_block;
    for (field, value) in [
        (&mut tb.title, &req.title),
        (&mut tb.date, &req.date),
        (&mut tb.revision, &req.revision),
        (&mut tb.company, &req.company),
    ] {
        if let Some(v) = value {
            *field = v.clone();
        }
    }
    for (n, text) in &req.comments {
        if !(1..=9).contains(n) {
            problems.push(finding(
                "COMMENT_NUMBER",
                format!("comments are numbered 1 to 9, not {n}"),
            ));
        } else if text.is_empty() {
            tb.comments.remove(n);
        } else {
            tb.comments.insert(*n, text.clone());
        }
    }
    // A title block holds single lines of text.
    let mut all: Vec<(&str, &String)> = vec![
        ("title", &tb.title),
        ("date", &tb.date),
        ("revision", &tb.revision),
        ("company", &tb.company),
    ];
    all.extend(tb.comments.values().map(|c| ("comment", c)));
    for (name, text) in all {
        if text.len() > 500 {
            problems.push(finding(
                "TITLE_BLOCK_TEXT_TOO_LONG",
                format!("the {name} is {} characters; keep it under 500", text.len()),
            ));
        }
    }
    if problems.is_empty() {
        Ok(to)
    } else {
        Err(problems)
    }
}

/// Plans a change of page and title block. A plan that changes nothing says so and cannot be
/// applied (as for the other field plans).
pub fn plan_sheet(current: &SheetSettings, req: &SheetRequest) -> OperationPlan {
    let mut plan = OperationPlan {
        circuit: "page".into(),
        ..Default::default()
    };
    match apply_request(current, req) {
        Err(problems) => plan.findings = problems,
        Ok(to) => {
            let change = SheetChange {
                from: current.clone(),
                to,
            };
            if change.describe().is_empty() {
                plan.findings.push(Finding {
                    severity: Severity::Error,
                    code: "NOTHING_TO_CHANGE",
                    message: "the page and title block are already as asked".into(),
                    objects: vec![],
                });
            } else {
                plan.sheet = Some(change);
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a4() -> SheetSettings {
        SheetSettings {
            page: Page {
                size: "A4".into(),
                ..Default::default()
            },
            title_block: TitleBlock::default(),
        }
    }

    #[test]
    fn sizes_are_understood_in_the_ways_people_write_them() {
        for (said, file) in [
            ("a3", "A3"),
            ("A4", "A4"),
            ("US Letter", "USLetter"),
            ("letter", "USLetter"),
            ("ANSI B", "B"),
            ("ansi_d", "D"),
            ("tabloid", "USLedger"),
            ("Gerber", "GERBER"),
        ] {
            assert_eq!(canonical_size(said), Some(file), "{said}");
        }
        assert_eq!(canonical_size("A6"), None);
        assert_eq!(canonical_size(""), None);
    }

    #[test]
    fn a_size_and_a_turn_change_the_page_only() {
        let plan = plan_sheet(
            &a4(),
            &SheetRequest {
                size: Some("a3".into()),
                orientation: Some("Portrait".into()),
                ..Default::default()
            },
        );
        assert!(plan.findings.is_empty(), "{:?}", plan.findings);
        let change = plan.sheet.unwrap();
        assert_eq!(change.to.page.size, "A3");
        assert!(change.to.page.portrait);
        assert_eq!(change.describe(), ["page: A4, landscape -> A3, portrait"]);
        assert_eq!(change.to.title_block, change.from.title_block);
    }

    #[test]
    fn a_custom_page_takes_both_sides_and_no_orientation() {
        let plan = plan_sheet(
            &a4(),
            &SheetRequest {
                width_mm: Some(200.0),
                height_mm: Some(150.0),
                ..Default::default()
            },
        );
        let page = plan.sheet.unwrap().to.page;
        assert_eq!(
            (page.size.as_str(), page.width_mm, page.height_mm),
            ("User", Some(200.0), Some(150.0))
        );
        for (req, code) in [
            (
                SheetRequest {
                    width_mm: Some(200.0),
                    ..Default::default()
                },
                "CUSTOM_PAGE_NEEDS_BOTH",
            ),
            (
                SheetRequest {
                    width_mm: Some(10.0),
                    height_mm: Some(150.0),
                    ..Default::default()
                },
                "PAGE_SIZE_OUT_OF_RANGE",
            ),
            (
                SheetRequest {
                    width_mm: Some(2000.0),
                    height_mm: Some(150.0),
                    ..Default::default()
                },
                "PAGE_SIZE_OUT_OF_RANGE",
            ),
            (
                SheetRequest {
                    size: Some("A4".into()),
                    width_mm: Some(200.0),
                    height_mm: Some(150.0),
                    ..Default::default()
                },
                "SIZE_AND_CUSTOM",
            ),
            (
                SheetRequest {
                    width_mm: Some(200.0),
                    height_mm: Some(150.0),
                    orientation: Some("portrait".into()),
                    ..Default::default()
                },
                "ORIENTATION_ON_CUSTOM_PAGE",
            ),
            (
                SheetRequest {
                    size: Some("A9".into()),
                    ..Default::default()
                },
                "UNKNOWN_PAGE_SIZE",
            ),
            (
                SheetRequest {
                    orientation: Some("sideways".into()),
                    ..Default::default()
                },
                "UNKNOWN_ORIENTATION",
            ),
        ] {
            let plan = plan_sheet(&a4(), &req);
            assert!(plan.sheet.is_none(), "{code}");
            assert!(
                plan.findings.iter().any(|f| f.code == code),
                "{code}: {:?}",
                plan.findings
            );
        }
    }

    #[test]
    fn title_block_text_is_set_and_cleared() {
        let mut current = a4();
        current.title_block.title = "Old".into();
        current.title_block.comments.insert(2, "keep".into());
        current.title_block.comments.insert(3, "drop".into());
        let plan = plan_sheet(
            &current,
            &SheetRequest {
                title: Some("Regulator".into()),
                revision: Some("B".into()),
                comments: BTreeMap::from([(1, "first".into()), (3, String::new())]),
                ..Default::default()
            },
        );
        let change = plan.sheet.unwrap();
        assert_eq!(change.to.title_block.title, "Regulator");
        assert_eq!(change.to.title_block.revision, "B");
        assert_eq!(
            change.to.title_block.comments,
            BTreeMap::from([(1, "first".to_owned()), (2, "keep".to_owned())])
        );
        assert_eq!(
            change.describe(),
            [
                "title: 'Old' -> 'Regulator'",
                "revision: '' -> 'B'",
                "comment 1: '' -> 'first'",
                "comment 3: 'drop' -> ''"
            ]
        );
    }

    #[test]
    fn page_sizes_are_in_millimetres_and_turn_with_the_page() {
        let a4 = Page {
            size: "A4".into(),
            ..Default::default()
        };
        assert_eq!(size_mm(&a4), Some((297.0, 210.0)));
        assert_eq!(size_mm(&Page { portrait: true, ..a4 }), Some((210.0, 297.0)));
        let user = Page {
            size: "User".into(),
            width_mm: Some(200.0),
            height_mm: Some(150.0),
            portrait: false,
        };
        assert_eq!(size_mm(&user), Some((200.0, 150.0)));
        assert_eq!(size_mm(&Page::default()), None);
    }

    #[test]
    fn a_custom_size_is_compared_to_a_tenth_of_a_millimetre() {
        let mut a = a4();
        a.page = Page {
            size: "User".into(),
            width_mm: Some(200.0),
            height_mm: Some(150.0),
            portrait: false,
        };
        let mut b = a.clone();
        b.page.width_mm = Some(199.9996);
        b.page.height_mm = Some(150.0124);
        assert!(a.same_as(&b));
        b.page.width_mm = Some(199.0);
        assert!(!a.same_as(&b));
    }

    #[test]
    fn a_comment_outside_one_to_nine_and_a_change_of_nothing_are_refused() {
        let plan = plan_sheet(
            &a4(),
            &SheetRequest {
                comments: BTreeMap::from([(10, "x".into())]),
                ..Default::default()
            },
        );
        assert!(plan.findings.iter().any(|f| f.code == "COMMENT_NUMBER"));
        let same = plan_sheet(
            &a4(),
            &SheetRequest {
                size: Some("A4".into()),
                orientation: Some("landscape".into()),
                ..Default::default()
            },
        );
        assert!(same.sheet.is_none());
        assert!(same.findings.iter().any(|f| f.code == "NOTHING_TO_CHANGE"));
    }
}
