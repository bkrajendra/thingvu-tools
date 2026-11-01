// SPDX-License-Identifier: GPL-3.0-or-later
//! The page and title block of a schematic through KiCad's API (Phase 15.1): read with
//! `GetPageSettings` and `GetTitleBlockInfo`, written with `SetPageSettings` and
//! `SetTitleBlockInfo`, and read again to see that KiCad took them.

use std::collections::BTreeMap;

use kicad_domain::sheet::{Page, SheetChange, SheetSettings, TitleBlock};
use kicad_domain::{ErrorCode, MutationState, ToolError};
use kicad_ipc::proto::kiapi::common::commands::{
    GetPageSettings, GetTitleBlockInfo, SetPageSettings, SetTitleBlockInfo,
};
use kicad_ipc::proto::kiapi::common::types::{
    DocumentSpecifier, PageOrientation, PageSettings, PageSize, TitleBlockInfo, Vector2,
};
use kicad_ipc::Client;

use crate::errors::from_ipc_error;

const NM_PER_MM: f64 = 1_000_000.0;

const NAMES: [(PageSize, &str); 16] = [
    (PageSize::PsA5, "A5"),
    (PageSize::PsA4, "A4"),
    (PageSize::PsA3, "A3"),
    (PageSize::PsA2, "A2"),
    (PageSize::PsA1, "A1"),
    (PageSize::PsA0, "A0"),
    (PageSize::PsAnsiA, "A"),
    (PageSize::PsAnsiB, "B"),
    (PageSize::PsAnsiC, "C"),
    (PageSize::PsAnsiD, "D"),
    (PageSize::PsAnsiE, "E"),
    (PageSize::PsGerber, "GERBER"),
    (PageSize::PsUsLetter, "USLetter"),
    (PageSize::PsUsLegal, "USLegal"),
    (PageSize::PsUsLedger, "USLedger"),
    (PageSize::PsUser, "User"),
];

fn page_from(settings: &PageSettings) -> Page {
    let size = NAMES
        .iter()
        .find(|(s, _)| *s as i32 == settings.page_size)
        .map_or("A4", |(_, n)| *n);
    let user = settings.user_page_size.as_ref();
    Page {
        size: size.to_owned(),
        width_mm: (size == "User").then(|| user.map_or(0.0, |v| v.x_nm as f64 / NM_PER_MM)),
        height_mm: (size == "User").then(|| user.map_or(0.0, |v| v.y_nm as f64 / NM_PER_MM)),
        portrait: settings.orientation == PageOrientation::PoPortrait as i32,
    }
}

fn title_block_from(info: &TitleBlockInfo) -> TitleBlock {
    let comments: BTreeMap<u8, String> = [
        &info.comment1,
        &info.comment2,
        &info.comment3,
        &info.comment4,
        &info.comment5,
        &info.comment6,
        &info.comment7,
        &info.comment8,
        &info.comment9,
    ]
    .iter()
    .enumerate()
    .filter(|(_, c)| !c.is_empty())
    .map(|(i, c)| (i as u8 + 1, (*c).clone()))
    .collect();
    TitleBlock {
        title: info.title.clone(),
        date: info.date.clone(),
        revision: info.revision.clone(),
        company: info.company.clone(),
        comments,
    }
}

fn info_from(tb: &TitleBlock) -> TitleBlockInfo {
    let c = |n: u8| tb.comments.get(&n).cloned().unwrap_or_default();
    TitleBlockInfo {
        title: tb.title.clone(),
        date: tb.date.clone(),
        revision: tb.revision.clone(),
        company: tb.company.clone(),
        comment1: c(1),
        comment2: c(2),
        comment3: c(3),
        comment4: c(4),
        comment5: c(5),
        comment6: c(6),
        comment7: c(7),
        comment8: c(8),
        comment9: c(9),
    }
}

/// The page and title block of the open document.
pub(crate) async fn read(client: &mut Client, document: &DocumentSpecifier) -> Result<SheetSettings, ToolError> {
    let page: PageSettings = client
        .call(&GetPageSettings {
            document: Some(document.clone()),
        })
        .await
        .map_err(|e| from_ipc_error(&e, false))?;
    let info: TitleBlockInfo = client
        .call(&GetTitleBlockInfo {
            document: Some(document.clone()),
        })
        .await
        .map_err(|e| from_ipc_error(&e, false))?;
    Ok(SheetSettings {
        page: page_from(&page),
        title_block: title_block_from(&info),
    })
}

/// Sets the page and title block, then reads them back. `from` must still be what the document
/// has (a plan made before someone else changed the page is stale).
pub(crate) async fn apply(
    client: &mut Client,
    document: &DocumentSpecifier,
    change: &SheetChange,
) -> Result<SheetSettings, ToolError> {
    let now = read(client, document).await?;
    if !now.same_as(&change.from) {
        return Err(ToolError::new(
            ErrorCode::StalePlan,
            "the page or title block changed since the plan was made; make a new plan",
        ));
    }
    // The page keeps its drawing sheet file: only the size and the turn change.
    let mut page: PageSettings = client
        .call(&GetPageSettings {
            document: Some(document.clone()),
        })
        .await
        .map_err(|e| from_ipc_error(&e, false))?;
    let to = &change.to.page;
    page.page_size = NAMES
        .iter()
        .find(|(_, n)| *n == to.size)
        .map_or(PageSize::PsA4, |(s, _)| *s) as i32;
    page.orientation = if to.portrait {
        PageOrientation::PoPortrait
    } else {
        PageOrientation::PoLandscape
    } as i32;
    page.user_page_size = (to.size == "User").then(|| Vector2 {
        x_nm: (to.width_mm.unwrap_or(0.0) * NM_PER_MM).round() as i64,
        y_nm: (to.height_mm.unwrap_or(0.0) * NM_PER_MM).round() as i64,
    });
    let not_applied = |e: kicad_ipc::Error| from_ipc_error(&e, false).with_mutation_state(MutationState::NotApplied);
    client
        .call_any(&SetPageSettings {
            document: Some(document.clone()),
            page_settings: Some(page),
        })
        .await
        .map_err(not_applied)?;
    client
        .call_any(&SetTitleBlockInfo {
            document: Some(document.clone()),
            title_block: Some(info_from(&change.to.title_block)),
        })
        .await
        .map_err(|e| from_ipc_error(&e, true))?;
    read(client, document).await
}
