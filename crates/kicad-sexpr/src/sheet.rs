// SPDX-License-Identifier: GPL-3.0-or-later
//! The page size and title block of a `.kicad_sch` file, read and rewritten as text (Phase 15.1):
//! `(paper "A4")`, `(paper "A4" portrait)`, `(paper "User" 200 150)`, and
//! `(title_block (title "…") (date "…") (rev "…") (company "…") (comment 1 "…"))`. Every byte
//! outside those two nodes stays as it was.

use kicad_domain::sheet::{Page, SheetSettings, TitleBlock};

use crate::node::{self, quote, Kind, Node};

/// What the file says. A file with no `paper` node is A4 landscape, as KiCad reads it.
pub fn read(text: &str) -> Result<SheetSettings, String> {
    let root = node::parse(text).map_err(|e| e.to_string())?;
    let mut settings = SheetSettings {
        page: Page {
            size: "A4".into(),
            ..Default::default()
        },
        title_block: TitleBlock::default(),
    };
    if let Some(paper) = root.child("paper") {
        let mut page = Page {
            size: paper.arg_text(0).unwrap_or("A4").to_owned(),
            ..Default::default()
        };
        if page.size == "User" {
            page.width_mm = paper.arg_f64(1);
            page.height_mm = paper.arg_f64(2);
        }
        page.portrait = paper
            .items()
            .iter()
            .skip(1)
            .any(|n| matches!(&n.kind, Kind::Atom(a) if a == "portrait"));
        settings.page = page;
    }
    if let Some(block) = root.child("title_block") {
        let tb = &mut settings.title_block;
        for (name, field) in [
            ("title", &mut tb.title),
            ("date", &mut tb.date),
            ("rev", &mut tb.revision),
            ("company", &mut tb.company),
        ] {
            if let Some(v) = block.child_text(name) {
                *field = v.to_owned();
            }
        }
        for comment in block.children("comment") {
            if let (Some(n), Some(text)) = (comment.arg_f64(0), comment.arg_text(1)) {
                if (1.0..=9.0).contains(&n) {
                    tb.comments.insert(n as u8, text.to_owned());
                }
            }
        }
    }
    Ok(settings)
}

fn number(v: f64) -> String {
    let s = format!("{v:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn paper_text(p: &Page) -> String {
    if p.size == "User" {
        format!(
            "(paper \"User\" {} {})",
            number(p.width_mm.unwrap_or(297.0)),
            number(p.height_mm.unwrap_or(210.0))
        )
    } else if p.portrait {
        format!("(paper {} portrait)", quote(&p.size))
    } else {
        format!("(paper {})", quote(&p.size))
    }
}

/// The `title_block` node, or nothing when every field is empty.
fn title_block_text(tb: &TitleBlock) -> Option<String> {
    let mut lines = Vec::new();
    for (name, value) in [
        ("title", &tb.title),
        ("date", &tb.date),
        ("rev", &tb.revision),
        ("company", &tb.company),
    ] {
        if !value.is_empty() {
            lines.push(format!("\t\t({name} {})", quote(value)));
        }
    }
    for (n, value) in &tb.comments {
        if !value.is_empty() {
            lines.push(format!("\t\t(comment {n} {})", quote(value)));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(format!("(title_block\n{}\n\t)", lines.join("\n")))
    }
}

/// The file's text with the page and title block set to `settings`.
pub fn write(text: &str, settings: &SheetSettings) -> Result<String, String> {
    let root = node::parse(text).map_err(|e| e.to_string())?;
    let paper: Option<&Node> = root.child("paper");
    let block: Option<&Node> = root.child("title_block");
    let anchor = paper
        .or(block)
        .ok_or_else(|| "the file has no (paper …) node to put the page after".to_owned())?;

    // Edits from the back, so earlier byte ranges stay right.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    match paper {
        Some(p) => edits.push((p.span.clone(), paper_text(&settings.page))),
        None => edits.push((
            anchor.span.start..anchor.span.start,
            format!("{}\n\t", paper_text(&settings.page)),
        )),
    }
    let new_block = title_block_text(&settings.title_block);
    match (block, new_block) {
        (Some(b), Some(t)) => edits.push((b.span.clone(), t)),
        (Some(b), None) => {
            // The line goes, with the whitespace before it.
            let mut start = b.span.start;
            while start > 0 && text.as_bytes()[start - 1].is_ascii_whitespace() {
                start -= 1;
            }
            edits.push((start..b.span.end, String::new()));
        }
        (None, Some(t)) => {
            let after = paper.map_or(anchor.span.end, |p| p.span.end);
            edits.push((after..after, format!("\n\t{t}")));
        }
        (None, None) => {}
    }
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_owned();
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const FILE: &str = "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"abc\")\n\t(paper \"A4\")\n\t(lib_symbols)\n)\n";

    fn settings(size: &str, portrait: bool, title: &str) -> SheetSettings {
        SheetSettings {
            page: Page {
                size: size.into(),
                portrait,
                ..Default::default()
            },
            title_block: TitleBlock {
                title: title.into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn the_defaults_are_read_from_a_plain_file() {
        let s = read(FILE).unwrap();
        assert_eq!(s.page.size, "A4");
        assert!(!s.page.portrait);
        assert_eq!(s.title_block, TitleBlock::default());
        assert_eq!(read("(kicad_sch (version 1))").unwrap().page.size, "A4");
    }

    #[test]
    fn a_page_and_a_title_block_are_written_and_read_back() {
        let mut want = settings("A3", true, "My \"board\"");
        want.title_block.revision = "B".into();
        want.title_block.company = "Thingsvu".into();
        want.title_block.comments = BTreeMap::from([(1, "one".into()), (9, "nine\nlines".into())]);
        let text = write(FILE, &want).unwrap();
        assert!(text.contains("(paper \"A3\" portrait)"), "{text}");
        assert!(
            text.contains("\t(title_block\n\t\t(title \"My \\\"board\\\"\")\n\t\t(rev \"B\")"),
            "{text}"
        );
        assert_eq!(read(&text).unwrap(), want);
        // Everything else is as it was.
        assert!(text.starts_with("(kicad_sch\n\t(version 20260306)"));
        assert!(text.contains("\t(lib_symbols)\n)\n"));
        assert_eq!(text.matches("(paper").count(), 1);
    }

    #[test]
    fn a_custom_page_is_written_in_millimetres() {
        let mut want = settings("User", false, "");
        want.page.width_mm = Some(200.0);
        want.page.height_mm = Some(150.5);
        let text = write(FILE, &want).unwrap();
        assert!(text.contains("(paper \"User\" 200 150.5)"), "{text}");
        let back = read(&text).unwrap();
        assert_eq!((back.page.width_mm, back.page.height_mm), (Some(200.0), Some(150.5)));
    }

    #[test]
    fn a_block_is_replaced_and_removed_in_place() {
        let with = write(FILE, &settings("A4", false, "First")).unwrap();
        let again = write(&with, &settings("A4", false, "Second")).unwrap();
        assert_eq!(again.matches("(title_block").count(), 1);
        assert_eq!(read(&again).unwrap().title_block.title, "Second");
        let cleared = write(&again, &settings("A4", false, "")).unwrap();
        assert_eq!(cleared, FILE, "removing the block gives back the original text");
    }

    #[test]
    fn a_file_without_a_page_node_is_refused() {
        assert!(write("(kicad_sch (version 1))", &settings("A4", false, "x")).is_err());
    }
}
