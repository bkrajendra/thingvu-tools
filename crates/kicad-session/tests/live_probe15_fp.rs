// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 15.2 probe: does `UpdateItems` take a changed pad (drill, size, zone connection) and a
//! changed reference text (layer, size, place, visibility), sent inside the footprint or on its
//! own? Skipped unless `KICAD_LIVE=1`.

use std::path::PathBuf;

use kicad_ipc::proto::kiapi::board::types::{
    BoardLayer, FootprintInstance, Pad, ZoneConnectionSettings, ZoneConnectionStyle,
};
use kicad_ipc::proto::kiapi::common::commands::{GetItems, GetItemsResponse, UpdateItems, UpdateItemsResponse};
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, KiCadObjectType, Vector2};
use kicad_session::{BoardSession, SessionConfig};
use prost::Name;
use prost_types::Any;

async fn footprints(board: &BoardSession) -> Vec<FootprintInstance> {
    let document = board.document_specifier().await.unwrap();
    let r: GetItemsResponse = board
        .raw_call(&GetItems {
            header: Some(ItemHeader {
                document: Some(document),
                ..Default::default()
            }),
            types: vec![KiCadObjectType::KotPcbFootprint as i32],
        })
        .await
        .unwrap();
    r.items
        .iter()
        .filter(|a| a.type_url == FootprintInstance::type_url())
        .map(|a| a.to_msg::<FootprintInstance>().unwrap())
        .collect()
}

fn pads(f: &FootprintInstance) -> Vec<Pad> {
    f.definition
        .iter()
        .flat_map(|d| d.items.iter())
        .filter(|i| i.type_url == Pad::type_url())
        .map(|i| i.to_msg::<Pad>().unwrap())
        .collect()
}

fn describe(label: &str, f: &FootprintInstance) {
    for p in pads(f) {
        let s = p.pad_stack.clone().unwrap_or_default();
        println!(
            "PROBE {label}: pad {} type {} drill {:?} size {:?} zone {:?}",
            p.number,
            p.r#type,
            s.drill.as_ref().and_then(|d| d.diameter),
            s.copper_layers.first().and_then(|l| l.size),
            s.zone_settings
        );
    }
    let t = f.reference_field.as_ref().and_then(|x| x.text.as_ref());
    println!(
        "PROBE {label}: reference layer {:?} visible {:?} text {:?}",
        t.map(|t| t.layer),
        f.reference_field.as_ref().map(|x| x.visible),
        t.and_then(|t| t.text.as_ref().map(|x| x.text.clone()))
    );
}

#[tokio::test]
async fn pad_and_text_edits_through_update_items() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe15f-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-board");
    for f in ["regulator.kicad_pcb", "regulator.kicad_sch"] {
        std::fs::copy(root.join(f), dir.join(f)).unwrap();
    }
    let board = BoardSession::open_headless(&SessionConfig::default(), &installs, &dir.join("regulator.kicad_pcb"))
        .await
        .unwrap();
    let document = board.document_specifier().await.unwrap();
    let header = || {
        Some(ItemHeader {
            document: Some(document.clone()),
            ..Default::default()
        })
    };
    let all = footprints(&board).await;
    let mut target = all
        .iter()
        .find(|f| pads(f).iter().any(|p| p.r#type == 1))
        .cloned()
        .expect("a footprint with a through hole pad");
    let reference = target
        .reference_field
        .as_ref()
        .and_then(|f| f.text.as_ref())
        .and_then(|t| t.text.as_ref().map(|x| x.text.clone()));
    println!("PROBE target {reference:?}");
    describe("before", &target);

    // 1. The footprint sent with a changed pad and a changed reference.
    let mut f = target.clone();
    if let Some(def) = f.definition.as_mut() {
        for item in def.items.iter_mut() {
            if item.type_url == Pad::type_url() {
                let mut p: Pad = item.to_msg().unwrap();
                if p.r#type == 1 {
                    let stack = p.pad_stack.get_or_insert_with(Default::default);
                    if let Some(d) = stack.drill.as_mut() {
                        d.diameter = Some(Vector2 {
                            x_nm: 1_100_000,
                            y_nm: 1_100_000,
                        });
                    }
                    for l in stack.copper_layers.iter_mut() {
                        l.size = Some(Vector2 {
                            x_nm: 2_000_000,
                            y_nm: 2_000_000,
                        });
                    }
                    stack.zone_settings = Some(ZoneConnectionSettings {
                        zone_connection: ZoneConnectionStyle::ZcsFull as i32,
                        thermal_spokes: None,
                    });
                }
                *item = Any::from_msg(&p).unwrap();
            }
        }
    }
    if let Some(field) = f.reference_field.as_mut() {
        field.visible = false;
        if let Some(t) = field.text.as_mut() {
            t.layer = BoardLayer::BlFFab as i32;
            if let Some(x) = t.text.as_mut() {
                if let Some(a) = x.attributes.as_mut() {
                    a.size = Some(Vector2 {
                        x_nm: 2_000_000,
                        y_nm: 2_000_000,
                    });
                }
                if let Some(pos) = x.position.as_mut() {
                    pos.y_nm += 1_000_000;
                }
            }
        }
    }
    let r: Result<UpdateItemsResponse, _> = board
        .raw_call(&UpdateItems {
            header: header(),
            items: vec![Any::from_msg(&f).unwrap()],
        })
        .await;
    println!(
        "PROBE update-footprint: {:?}",
        r.as_ref()
            .map(|r| r.updated_items.iter().map(|u| u.status.clone()).collect::<Vec<_>>())
    );
    let after = footprints(&board).await;
    let id = target.id.as_ref().map(|k| k.value.clone());
    target = after
        .iter()
        .find(|x| x.id.as_ref().map(|k| k.value.clone()) == id)
        .cloned()
        .unwrap();
    describe("after-footprint", &target);

    // 2. A pad sent on its own.
    let items: GetItemsResponse = board
        .raw_call(&GetItems {
            header: header(),
            types: vec![KiCadObjectType::KotPcbPad as i32],
        })
        .await
        .unwrap();
    println!("PROBE pads on the board: {}", items.items.len());
    if let Some(any) = items
        .items
        .iter()
        .find(|a| a.type_url == Pad::type_url() && a.to_msg::<Pad>().map(|p| p.r#type == 1).unwrap_or(false))
    {
        let mut p: Pad = any.to_msg().unwrap();
        let stack = p.pad_stack.get_or_insert_with(Default::default);
        if let Some(d) = stack.drill.as_mut() {
            d.diameter = Some(Vector2 {
                x_nm: 1_300_000,
                y_nm: 1_300_000,
            });
        }
        let r: Result<UpdateItemsResponse, _> = board
            .raw_call(&UpdateItems {
                header: header(),
                items: vec![Any::from_msg(&p).unwrap()],
            })
            .await;
        println!(
            "PROBE update-pad: {:?}",
            r.as_ref()
                .map(|r| r.updated_items.iter().map(|u| u.status.clone()).collect::<Vec<_>>())
        );
        let after = footprints(&board).await;
        let t = after
            .iter()
            .find(|x| x.id.as_ref().map(|k| k.value.clone()) == id)
            .cloned()
            .unwrap();
        describe("after-pad", &t);
    }
}
