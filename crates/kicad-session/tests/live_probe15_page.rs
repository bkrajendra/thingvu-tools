// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 15.1 probe: on a schematic in a headless KiCad, are `GetTitleBlockInfo`,
//! `SetTitleBlockInfo`, `GetPageSettings`, and `SetPageSettings` handled, does a change of size
//! take, and what does the saved file say? Skipped unless `KICAD_LIVE=1`.

use std::path::PathBuf;

use kicad_ipc::proto::kiapi::common::commands::{
    GetPageSettings, GetTitleBlockInfo, SetPageSettings, SetTitleBlockInfo,
};
use kicad_ipc::proto::kiapi::common::types::{PageOrientation, PageSettings, PageSize, TitleBlockInfo, Vector2};
use kicad_session::{OpenTarget, Session, SessionConfig};

fn show<T: std::fmt::Debug, E: std::fmt::Debug>(what: &str, r: &Result<T, E>) {
    match r {
        Ok(v) => println!("PROBE {what}: ok {v:?}"),
        Err(e) => println!("PROBE {what}: ERR {e:?}"),
    }
}

#[tokio::test]
async fn page_and_title_block_on_a_schematic() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe15p-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-board");
    std::fs::copy(root.join("regulator.kicad_sch"), dir.join("regulator.kicad_sch")).unwrap();
    let path = dir.join("regulator.kicad_sch");
    let session = Session::open_headless(&SessionConfig::default(), &installs, OpenTarget::Existing(path.clone()))
        .await
        .unwrap();
    let document = session.document_specifier().clone();

    let r: Result<TitleBlockInfo, _> = session
        .raw_call(&GetTitleBlockInfo {
            document: Some(document.clone()),
        })
        .await;
    show("GetTitleBlockInfo", &r);
    let r: Result<PageSettings, _> = session
        .raw_call(&GetPageSettings {
            document: Some(document.clone()),
        })
        .await;
    show("GetPageSettings", &r);

    let r: Result<(), _> = session
        .raw_call(&SetTitleBlockInfo {
            document: Some(document.clone()),
            title_block: Some(TitleBlockInfo {
                title: "Probe board".into(),
                date: "2026-11-01".into(),
                revision: "B".into(),
                company: "Thingsvu".into(),
                comment1: "first".into(),
                comment9: "ninth".into(),
                ..Default::default()
            }),
        })
        .await;
    show("SetTitleBlockInfo", &r);
    let r: Result<TitleBlockInfo, _> = session
        .raw_call(&GetTitleBlockInfo {
            document: Some(document.clone()),
        })
        .await;
    show("GetTitleBlockInfo-after", &r);

    for (name, page) in [
        (
            "A3 portrait",
            PageSettings {
                page_size: PageSize::PsA3 as i32,
                orientation: PageOrientation::PoPortrait as i32,
                ..Default::default()
            },
        ),
        (
            "User 200x150",
            PageSettings {
                page_size: PageSize::PsUser as i32,
                user_page_size: Some(Vector2 {
                    x_nm: 200_000_000,
                    y_nm: 150_000_000,
                }),
                orientation: PageOrientation::PoLandscape as i32,
                ..Default::default()
            },
        ),
        (
            "US Letter",
            PageSettings {
                page_size: PageSize::PsUsLetter as i32,
                orientation: PageOrientation::PoLandscape as i32,
                ..Default::default()
            },
        ),
    ] {
        let r: Result<PageSettings, _> = session
            .raw_call(&SetPageSettings {
                document: Some(document.clone()),
                page_settings: Some(page),
            })
            .await;
        show(&format!("SetPageSettings {name}"), &r);
        let r: Result<PageSettings, _> = session
            .raw_call(&GetPageSettings {
                document: Some(document.clone()),
            })
            .await;
        show(&format!("GetPageSettings-after {name}"), &r);
    }
    session.save().await.unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    for line in text.lines().filter(|l| {
        let t = l.trim_start();
        t.starts_with("(paper")
            || t.starts_with("(title")
            || t.starts_with("(date")
            || t.starts_with("(rev")
            || t.starts_with("(company")
            || t.starts_with("(comment")
    }) {
        println!("PROBE FILE {line}");
    }
}
