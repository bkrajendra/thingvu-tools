// SPDX-License-Identifier: GPL-3.0-or-later
//! Drives the real `kicad-ai-mcp` binary over stdio with raw JSON-RPC, as an MCP host would.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    /// The server's own account of what its tools are for, from `initialize`.
    instructions: String,
}

impl Mcp {
    async fn start() -> Self {
        Self::start_with(&[]).await
    }

    async fn start_with(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kicad-ai-mcp"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut mcp = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            instructions: String::new(),
        };
        let init = mcp
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "thingsvu-stdio-test", "version": "0" }
                }),
            )
            .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "kicad-ai-mcp", "{init}");
        mcp.instructions = init["result"]["instructions"].as_str().unwrap_or_default().to_owned();
        mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        mcp
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_string(&message).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Sends a request and returns its response. Every stdout line must be a JSON-RPC message.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(180), self.stdout.next_line())
                .await
                .expect("response timeout")
                .unwrap()
                .expect("server closed stdout");
            let message: Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line}"));
            assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC: {line}");
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let response = self
            .request("tools/call", json!({ "name": tool, "arguments": arguments }))
            .await;
        response["result"]["structuredContent"].clone()
    }
}

#[tokio::test]
async fn lists_tools_and_returns_envelopes() {
    let mut mcp = Mcp::start().await;

    let tools = mcp.request("tools/list", json!({})).await;
    let mut names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "bom_export",
            "bom_get",
            "bom_review",
            "checkpoint_create",
            "checkpoint_diff",
            "checkpoint_list",
            "checkpoint_restore",
            "fp_get",
            "fp_search",
            "parts_lookup",
            "pcb_apply_plan",
            "pcb_autoroute_plan",
            "pcb_close",
            "pcb_export",
            "pcb_fab_package",
            "pcb_footprint_attrs_plan",
            "pcb_get_footprint",
            "pcb_info",
            "pcb_list_drawings",
            "pcb_list_footprints",
            "pcb_list_nets",
            "pcb_list_tracks",
            "pcb_mechanical_info",
            "pcb_move_plan",
            "pcb_open",
            "pcb_outline_plan",
            "pcb_place_plan",
            "pcb_reload",
            "pcb_render",
            "pcb_review",
            "pcb_route_plan",
            "pcb_rules",
            "pcb_rules_plan",
            "pcb_run_drc",
            "pcb_save",
            "pcb_track_edit_plan",
            "pcb_update_from_schematic_plan",
            "pcb_zone_plan",
            "sch_apply_plan",
            "sch_arrange_plan",
            "sch_create_demo_circuit",
            "sch_declutter_plan",
            "sch_describe_layout",
            "sch_draw_plan",
            "sch_edit_items_plan",
            "sch_export_netlist",
            "sch_footprint_plan",
            "sch_get_netlist",
            "sch_get_symbol_pins",
            "sch_label_plan",
            "sch_list_symbols",
            "sch_pin_net_plan",
            "sch_plan_changes",
            "sch_reconnect_plan",
            "sch_render",
            "sch_review",
            "sch_run_erc",
            "sch_save",
            "sch_set_field",
            "sch_set_fields_plan",
            "sch_tidy_plan",
            "session_close",
            "session_list_documents",
            "session_open",
            "session_reload",
            "session_status",
            "sym_get",
            "sym_search"
        ]
    );

    // The instructions say which tool to use for which job: every tool they name exists, and the
    // tools that were hard to find in the ESP32-S3 session (and every plan tool) are named.
    let named: std::collections::BTreeSet<&str> = mcp
        .instructions
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| {
            [
                "pcb_",
                "sch_",
                "sym_",
                "fp_",
                "bom_",
                "parts_",
                "checkpoint_",
                "session_",
            ]
            .iter()
            .any(|p| w.starts_with(p))
        })
        .collect();
    for n in &named {
        assert!(
            names.iter().any(|t| t == n),
            "the instructions name '{n}', which is not a tool"
        );
    }
    for important in [
        "pcb_route_plan",
        "pcb_autoroute_plan",
        "pcb_track_edit_plan",
        "pcb_rules_plan",
        "sch_set_fields_plan",
        "sch_pin_net_plan",
        "bom_review",
        "parts_lookup",
        "pcb_fab_package",
        "pcb_reload",
        "checkpoint_restore",
        "sym_search",
    ] {
        assert!(named.contains(important), "the instructions do not mention {important}");
    }
    for n in names.iter().filter(|n| n.ends_with("_plan")) {
        assert!(
            named.contains(n.as_str()),
            "the plan tool {n} is not in the instructions"
        );
    }
    let described = tools["result"]["tools"].as_array().unwrap();
    assert!(
        described
            .iter()
            .all(|t| t["description"].as_str().is_some_and(|d| d.len() > 40)),
        "every tool has a description that says what it is for"
    );

    let status = mcp.call("session_status", json!({})).await;
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["result_state"], "done");
    assert_eq!(status["result"]["sessions"], json!([]));
    // Whether an autorouter is set up is always reported, with where to look when it is not.
    assert!(status["result"]["autorouter"]["configured"].is_boolean(), "{status}");
    assert_eq!(status["result"]["autorouter"]["tool"], "pcb_autoroute_plan");
    assert!(status["correlation_id"].as_str().unwrap().starts_with("req-"));

    let relative = mcp
        .call("session_open", json!({ "schematic_path": "relative.kicad_sch" }))
        .await;
    assert_eq!(relative["ok"], false);
    assert_eq!(relative["result_state"], "rejected");
    assert_eq!(relative["error"]["code"], "INVALID_ARGUMENT");

    let missing = mcp.call("session_close", json!({ "session_id": "nope" })).await;
    assert_eq!(missing["error"]["code"], "NO_SESSION");

    // Unknown fields are rejected by the input schema, before the tool runs.
    let response = mcp
        .request(
            "tools/call",
            json!({ "name": "session_status", "arguments": { "bogus": 1 } }),
        )
        .await;
    assert!(
        response.get("error").is_some() || response["result"]["isError"] == true,
        "{response}"
    );

    drop(mcp.stdin);
    let _ = tokio::time::timeout(Duration::from_secs(10), mcp.child.wait()).await;
}

#[tokio::test]
async fn live_headless_session_round_trip() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("mcp-new.kicad_sch");

    let mut mcp = Mcp::start().await;
    let status = mcp.call("session_status", json!({})).await;
    let installs = status["result"]["kicad_installs"].as_array().unwrap();
    assert!(installs.iter().any(|i| i["series"] == 11), "{status}");

    let opened = mcp
        .call("session_open", json!({ "schematic_path": schematic, "create": true }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let result = &opened["result"];
    assert_eq!(result["connected"], true);
    assert!(result["kicad_version"].as_str().unwrap().starts_with("10.99"));
    assert_eq!(result["capabilities"]["sch.place_symbol"]["available"], true);
    assert_eq!(result["capabilities"]["sch.erc"]["backend"], "kicad_cli");
    assert_eq!(opened["provenance"][0]["origin"], "kicad_ipc");
    let id = result["session_id"].as_str().unwrap().to_owned();

    let listed = mcp.call("session_status", json!({})).await;
    assert_eq!(listed["result"]["sessions"].as_array().unwrap().len(), 1);

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    let after = mcp.call("session_status", json!({ "session_id": id })).await;
    assert_eq!(after["error"]["code"], "NO_SESSION");
}

#[tokio::test]
async fn live_read_tools_on_the_fixture() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/two-resistor-series-chain");
    for file in ["kicad-demo.kicad_sch", "kicad-demo.kicad_pro"] {
        std::fs::copy(fixture.join(file), dir.join(file)).unwrap();
    }

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": dir.join("kicad-demo.kicad_sch") }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert_eq!(symbols["ok"], true, "{symbols}");
    assert_eq!(symbols["result"]["count"], 2);
    let r1 = &symbols["result"]["symbols"][0];
    assert_eq!(
        (
            r1["reference"].as_str(),
            r1["value"].as_str(),
            r1["library_id"].as_str()
        ),
        (Some("R1"), Some("10k"), Some("Device:R"))
    );
    assert_eq!(r1["position_mm"], json!({ "x": 50.8, "y": 50.8 }));
    assert_eq!(symbols["provenance"][0]["origin"], "kicad_ipc");

    let pins = mcp
        .call("sch_get_symbol_pins", json!({ "session_id": id, "symbol": r1["id"] }))
        .await;
    assert_eq!(
        pins["result"]["pins"][0]["position_mm"],
        json!({ "x": 50.8, "y": 46.99 }),
        "{pins}"
    );
    let ambiguous = mcp
        .call("sch_get_symbol_pins", json!({ "session_id": id, "symbol": "R7" }))
        .await;
    assert_eq!(ambiguous["error"]["code"], "SYMBOL_NOT_FOUND");

    let netlist = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert_eq!(netlist["result"]["count"], 3, "{netlist}");
    assert_eq!(netlist["result"]["unconnected_pins"], 0);
    let mid = netlist["result"]["nets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "/MID")
        .unwrap();
    let mid_pins: Vec<String> = mid["pins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| format!("{}.{}", p["reference"].as_str().unwrap(), p["pin"].as_str().unwrap()))
        .collect();
    assert_eq!(mid_pins, ["R1.2", "R2.1"]);

    let docs = mcp.call("session_list_documents", json!({ "session_id": id })).await;
    assert_eq!(docs["ok"], true, "{docs}");

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
}

#[tokio::test]
async fn live_demo_circuit_end_to_end() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("demo.kicad_sch");

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call("session_open", json!({ "schematic_path": schematic, "create": true }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    // Plan only.
    let plan = mcp.call("sch_create_demo_circuit", json!({ "session_id": id })).await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["can_apply"], true);
    assert_eq!(plan["result"]["counts"]["symbols"], 2);
    assert!(
        mcp.call("sch_list_symbols", json!({ "session_id": id })).await["result"]["count"] == 0,
        "planning changes nothing"
    );

    // Apply without confirm is refused.
    let plan_id = plan["result"]["plan_id"].clone();
    let refused = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": false, "idempotency_key": "demo-key-0001" }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "INVALID_ARGUMENT");

    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "demo-key-0001" }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result_state"], "done", "{applied}");
    assert_eq!(applied["result"]["verified"], true);
    assert_eq!(applied["result"]["undo_steps"], 2);

    // Same key again: replayed, not applied twice.
    let replay = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "demo-key-0001" }),
        )
        .await;
    assert!(replay["warnings"].to_string().contains("replayed"), "{replay}");
    assert_eq!(
        mcp.call("sch_list_symbols", json!({ "session_id": id })).await["result"]["count"],
        2
    );

    // The same plan with a new key is stale.
    let again = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "demo-key-0002" }),
        )
        .await;
    assert_eq!(again["error"]["code"], "STALE_PLAN", "{again}");

    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    assert_eq!(erc["ok"], true, "{erc}");
    assert_eq!(erc["provenance"][0]["origin"], "kicad_cli");
    println!(
        "ERC: {} errors, {} warnings: {}",
        erc["result"]["errors"], erc["result"]["warnings"], erc["result"]["violations"]
    );

    let render = mcp
        .request(
            "tools/call",
            json!({ "name": "sch_render", "arguments": { "session_id": id } }),
        )
        .await;
    let content = render["result"]["content"].as_array().unwrap();
    let image = content
        .iter()
        .find(|c| c["type"] == "image")
        .unwrap_or_else(|| panic!("no image block: {render}"));
    assert_eq!(image["mimeType"], "image/png");
    assert!(image["data"].as_str().unwrap().len() > 1000);

    let net_path = dir.join("demo.net");
    let exported = mcp
        .call(
            "sch_export_netlist",
            json!({ "session_id": id, "output_path": net_path }),
        )
        .await;
    assert_eq!(exported["result"]["nets"], 3, "{exported}");
    let outside = mcp
        .call(
            "sch_export_netlist",
            json!({ "session_id": id, "output_path": std::env::temp_dir().join("x.net") }),
        )
        .await;
    assert_eq!(outside["error"]["code"], "INVALID_ARGUMENT");

    let saved = mcp.call("sch_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");

    // The saved file, checked independently with kicad-cli.
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli = installs.iter().find(|i| i.version.series() >= 11).unwrap();
    let netlist =
        kicad_cli_runner::netlist::export_netlist(cli, &schematic, &dir.join("check.net"), Duration::from_secs(120))
            .await
            .unwrap();
    let topology = netlist.topology();
    for (name, pins) in [
        ("/INPUT", vec!["R1.1"]),
        ("/MID", vec!["R1.2", "R2.1"]),
        ("/OUTPUT", vec!["R2.2"]),
    ] {
        let pins: std::collections::BTreeSet<String> = pins.into_iter().map(str::to_owned).collect();
        assert!(topology.contains(&(name.to_owned(), pins)), "{name} in {topology:?}");
    }
    assert!(
        netlist
            .components
            .iter()
            .all(|c| c.value == "10k" && c.lib_source == "Device:R"),
        "{:?}",
        netlist.components
    );
}

#[tokio::test]
async fn live_file_provider_on_kicad_10() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("demo.kicad_sch");

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "create": true, "provider": "file" }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    assert_eq!(opened["result"]["provider"], "file");
    assert_eq!(opened["provenance"][0]["origin"], "project_file");
    assert!(
        opened["result"]["kicad_version"].as_str().unwrap().starts_with("10.0"),
        "{opened}"
    );
    assert_eq!(
        opened["result"]["capabilities"]["sch.place_symbol"]["backend"],
        "project_file"
    );
    assert_eq!(opened["result"]["capabilities"]["session.commits"]["available"], false);
    let id = opened["result"]["session_id"].clone();
    assert!(schematic.is_file(), "a file session creates the file");

    let plan = mcp.call("sch_create_demo_circuit", json!({ "session_id": id })).await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let plan_id = plan["result"]["plan_id"].clone();

    // An edit made outside the tool after planning makes the plan stale and changes nothing more.
    let original = std::fs::read_to_string(&schematic).unwrap();
    std::fs::write(&schematic, format!("{original}\n")).unwrap();
    let stale = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "file-key-0001" }),
        )
        .await;
    assert_eq!(stale["error"]["code"], "STALE_PLAN", "{stale}");
    std::fs::write(&schematic, &original).unwrap();

    // An editor that opened the file in the meantime blocks the change.
    let lock = dir.join("~demo.kicad_sch.lck");
    std::fs::write(&lock, "{}").unwrap();
    let plan = mcp.call("sch_create_demo_circuit", json!({ "session_id": id })).await;
    let plan_id = plan["result"]["plan_id"].clone();
    let blocked = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "file-key-0002" }),
        )
        .await;
    assert_eq!(blocked["error"]["code"], "DOCUMENT_OPEN_ELSEWHERE", "{blocked}");
    assert_eq!(std::fs::read_to_string(&schematic).unwrap(), original);
    std::fs::remove_file(&lock).unwrap();

    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "file-key-0003" }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result_state"], "done", "{applied}");
    assert_eq!(applied["result"]["verified"], true);
    assert_eq!(applied["result"]["undo_steps"], 0);
    let backup = applied["result"]["backup_path"]
        .as_str()
        .expect("the previous file is kept");
    assert_eq!(std::fs::read_to_string(backup).unwrap(), original);

    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert_eq!(symbols["result"]["count"], 2, "{symbols}");
    assert_eq!(symbols["provenance"][0]["origin"], "project_file");
    let pins = mcp
        .call("sch_get_symbol_pins", json!({ "session_id": id, "symbol": "R1" }))
        .await;
    assert_eq!(pins["result"]["pins"].as_array().unwrap().len(), 2, "{pins}");
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert_eq!(nets["provenance"][0]["origin"], "kicad_cli");
    assert_eq!(nets["result"]["unconnected_pins"], 0, "{nets}");

    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    assert_eq!(erc["ok"], true, "{erc}");
    assert!(
        erc["warnings"].to_string().contains("copy of the schematic file"),
        "{erc}"
    );

    let render = mcp
        .request(
            "tools/call",
            json!({ "name": "sch_render", "arguments": { "session_id": id } }),
        )
        .await;
    let image = render["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "image")
        .unwrap_or_else(|| panic!("no image block: {render}"));
    assert_eq!(image["mimeType"], "image/png");

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");

    // The file as KiCad 10 itself reads it.
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli = installs
        .iter()
        .find(|i| i.version.series() == 10)
        .expect("a KiCad 10 install");
    let netlist =
        kicad_cli_runner::netlist::export_netlist(cli, &schematic, &dir.join("check.net"), Duration::from_secs(120))
            .await
            .unwrap();
    let topology = netlist.topology();
    for (name, pins) in [
        ("/INPUT", vec!["R1.1"]),
        ("/MID", vec!["R1.2", "R2.1"]),
        ("/OUTPUT", vec!["R2.2"]),
    ] {
        let pins: std::collections::BTreeSet<String> = pins.into_iter().map(str::to_owned).collect();
        assert!(topology.contains(&(name.to_owned(), pins)), "{name} in {topology:?}");
    }
    assert!(netlist
        .components
        .iter()
        .all(|c| c.value == "10k" && c.lib_source == "Device:R"));
}

#[tokio::test]
async fn live_review_tidy_and_set_field_through_mcp() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-tidy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": "file" }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    // The review says what is wrong and what a tidy would do, and changes nothing.
    let before = std::fs::read(&schematic).unwrap();
    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    assert_eq!(review["ok"], true, "{review}");
    assert_eq!(review["result"]["parts"], 13, "{review}");
    assert_eq!(review["result"]["tidy"]["possible"], true, "{review}");
    assert!(
        review["result"]["problems"]
            .to_string()
            .contains("D2 and #PWR10 overlap"),
        "{review}"
    );
    assert_eq!(review["result"]["tidy"]["already_tidy"], false);
    assert_eq!(std::fs::read(&schematic).unwrap(), before, "a review changes nothing");

    let plan = mcp.call("sch_tidy_plan", json!({ "session_id": id })).await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    assert!(plan["result"]["moves"].as_array().unwrap().len() >= 10);
    assert!(plan["result"]["removals"].as_array().unwrap().len() >= 30);
    assert!(plan["result"]["readability_before"]["overlapping_symbols"]
        .as_array()
        .is_some());
    assert_eq!(std::fs::read(&schematic).unwrap(), before, "planning changes nothing");
    let plan_id = plan["result"]["plan_id"].clone();

    let refused = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": false, "idempotency_key": "tidy-key-0001" }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "INVALID_ARGUMENT");

    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "tidy-key-0001" }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert!(applied["result"]["moved"].as_u64().unwrap() >= 10);
    assert!(applied["result"]["removed"].as_u64().unwrap() >= 30);

    // Reviewed again, it is tidy. The text KiCad put beside the parts may still run into things.
    let after = mcp.call("sch_review", json!({ "session_id": id })).await;
    assert_eq!(after["result"]["tidy"]["already_tidy"], true, "{after}");
    println!("PROBLEMS after tidy: {}", after["result"]["problems"]);

    // Declutter moves that text; afterwards nothing runs into anything.
    let declutter = mcp.call("sch_declutter_plan", json!({ "session_id": id })).await;
    assert_eq!(declutter["ok"], true, "{declutter}");
    println!("DECLUTTER {}", declutter["result"]["field_changes"]);
    if declutter["result"]["counts"]["field_changes"].as_u64().unwrap_or(0)
        + declutter["result"]["counts"]["labels"].as_u64().unwrap_or(0)
        > 0
    {
        let applied = mcp
            .call(
                "sch_apply_plan",
                json!({ "session_id": id, "plan_id": declutter["result"]["plan_id"], "confirm": true, "idempotency_key": "declutter-key-0001" }),
            )
            .await;
        assert_eq!(applied["ok"], true, "{applied}");
        assert_eq!(applied["result"]["verified"], true, "{applied}");
    }
    let after = mcp.call("sch_review", json!({ "session_id": id })).await;
    assert_eq!(after["result"]["problems"], json!([]), "{after}");
    assert_eq!(after["result"]["tidy"]["already_tidy"], true, "{after}");

    // Change one value. It needs confirm, a real part, and sensible text.
    let no_confirm = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": id, "symbol": "R3", "field": "value", "text": "2k2", "confirm": false }),
        )
        .await;
    assert_eq!(no_confirm["error"]["code"], "INVALID_ARGUMENT");
    let unknown = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": id, "symbol": "R99", "field": "value", "text": "2k2", "confirm": true }),
        )
        .await;
    assert_eq!(unknown["error"]["code"], "SYMBOL_NOT_FOUND", "{unknown}");
    let changed = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": id, "symbol": "R3", "field": "value", "text": "2k2", "confirm": true }),
        )
        .await;
    assert_eq!(changed["ok"], true, "{changed}");
    assert_eq!(changed["result"]["value"], "2k2");
    let footprint = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": id, "symbol": "R3", "field": "footprint", "text": "Resistor_SMD:R_0805_2012Metric", "confirm": true }),
        )
        .await;
    assert_eq!(
        footprint["result"]["footprint"], "Resistor_SMD:R_0805_2012Metric",
        "{footprint}"
    );
    assert_eq!(footprint["result"]["value"], "2k2", "the value is kept");
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    let r3 = symbols["result"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["reference"] == "R3")
        .unwrap();
    assert_eq!(r3["value"], "2k2");

    // The result is still the same circuit, as KiCad reads it.
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert_eq!(nets["result"]["unconnected_pins"], 0, "{nets}");
    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "ERC after tidy: {} errors, {} warnings",
        erc["result"]["errors"], erc["result"]["warnings"]
    );
}

/// Place parts by hand, let the wires break, then wire everything again: the circuit must come
/// out the same, and no part moves in the second step.
#[tokio::test]
async fn live_arrange_then_reconnect_through_mcp() {
    arrange_flow("file").await;
}

#[tokio::test]
async fn live_arrange_then_reconnect_through_the_api() {
    arrange_flow("headless").await;
}

async fn arrange_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-arrange-{provider}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": provider }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    assert_eq!(layout["ok"], true, "{layout}");
    let parts = layout["result"]["parts"].as_array().unwrap();
    assert!(parts.len() >= 10, "{layout}");
    assert!(parts[0]["pins"][0]["x"].is_number(), "{layout}");
    let nets_before = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    let count_before = nets_before["result"]["nets"].as_array().unwrap().len();

    // Move a few parts far away, turn one, and delete nothing yet.
    let key = |n: &str| format!("{n}-{}", std::process::id());
    let arrange = mcp
        .call(
            "sch_arrange_plan",
            json!({ "session_id": id, "edits": [
                { "reference": "C1", "x_mm": 200.0, "y_mm": 40.0 },
                { "reference": "C2", "dx_mm": 30.0, "dy_mm": 20.0, "rotate_deg": 90 },
                { "reference": "R3", "x_mm": 200.0, "y_mm": 90.0, "mirror": "y" },
            ] }),
        )
        .await;
    assert_eq!(arrange["ok"], true, "{arrange}");
    assert_eq!(arrange["result"]["counts"]["moves"], 3, "{arrange}");
    assert!(arrange["result"]["findings"]
        .to_string()
        .contains("ARRANGE_WIRES_LEFT_BEHIND"));
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": arrange["result"]["plan_id"], "confirm": true, "idempotency_key": key("arrange-1") }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["moved"], 3, "{applied}");

    // The circuit is broken now: KiCad no longer sees the nets it had.
    let broken = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    let moved_pins_net = |v: &serde_json::Value| {
        v["result"]["nets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["pins"].to_string().contains("\"C1\""))
            .count()
    };
    println!("nets with C1 before reconnect: {}", moved_pins_net(&broken));

    let reconnect = mcp.call("sch_reconnect_plan", json!({ "session_id": id })).await;
    assert_eq!(reconnect["ok"], true, "{reconnect}");
    assert_eq!(reconnect["result"]["can_apply"], true, "{reconnect}");
    assert_eq!(reconnect["result"]["counts"]["moves"], 0, "{reconnect}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": reconnect["result"]["plan_id"], "confirm": true, "idempotency_key": key("reconnect-1") }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");

    let nets_after = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert_eq!(nets_after["result"]["unconnected_pins"], 0, "{nets_after}");
    assert_eq!(nets_after["result"]["nets"].as_array().unwrap().len(), count_before);
    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "ERC after arrange + reconnect: {} errors, {} warnings",
        erc["result"]["errors"], erc["result"]["warnings"]
    );

    // A tidy starts from a saved document (so that a failure can be reverted).
    let saved = mcp.call("sch_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");

    // Hints: keep C1 where it was put, and the rest is laid out around it.
    let tidy = mcp
        .call(
            "sch_tidy_plan",
            json!({ "session_id": id, "keep": ["C1"], "near": [{ "reference": "C2", "to": "C1", "side": "right", "gap_mm": 10.0 }] }),
        )
        .await;
    assert_eq!(tidy["ok"], true, "{tidy}");
    assert_eq!(tidy["result"]["can_apply"], true, "{tidy}");
    assert!(
        !tidy["result"]["moves"].to_string().contains("\"C1\""),
        "a kept part does not move: {tidy}"
    );
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": tidy["result"]["plan_id"], "confirm": true, "idempotency_key": key("hints-1") }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");

    // Delete a part, then wire the rest again: the other parts keep their nets.
    let saved = mcp.call("sch_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let delete = mcp
        .call("sch_arrange_plan", json!({ "session_id": id, "delete": ["C2"] }))
        .await;
    assert_eq!(delete["result"]["counts"]["removals"], 1, "{delete}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": delete["result"]["plan_id"], "confirm": true, "idempotency_key": key("delete-1") }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    let reconnect = mcp.call("sch_reconnect_plan", json!({ "session_id": id })).await;
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": reconnect["result"]["plan_id"], "confirm": true, "idempotency_key": key("delete-2") }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert!(
        !symbols["result"]["symbols"].to_string().contains("\"C2\""),
        "C2 is gone"
    );

    // A bad request is refused, and an unknown part named.
    let bad = mcp
        .call(
            "sch_arrange_plan",
            json!({ "session_id": id, "edits": [{ "reference": "R99", "dx_mm": 1.0 }] }),
        )
        .await;
    assert_eq!(bad["result"]["can_apply"], false, "{bad}");
}

/// Drawings, labels, and notes: add, rename, turn, move, delete. Every plan leaves the nets as
/// they should be, which applying checks against KiCad.
#[tokio::test]
async fn live_labels_drawings_and_items_through_mcp() {
    text_tools_flow("file").await;
}

#[tokio::test]
async fn live_labels_drawings_and_items_through_the_api() {
    text_tools_flow("headless").await;
}

async fn text_tools_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-text-{provider}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": provider }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let n = std::process::id();
    let mut step = 0;
    let mut apply = |plan: serde_json::Value| {
        step += 1;
        json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                "idempotency_key": format!("text-{provider}-{n}-{step}") })
    };

    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    assert_eq!(layout["ok"], true, "{layout}");
    let drawings_before = layout["result"]["drawings"].as_array().unwrap().len();
    let notes_before = layout["result"]["notes"].as_array().unwrap().len();
    assert!(layout["result"]["labels"][0]["id"].is_string(), "{layout}");
    let label = layout["result"]["labels"][0].clone();

    // Draw: a box around two parts with a title, a note, and a line.
    let draw = mcp
        .call(
            "sch_draw_plan",
            json!({ "session_id": id, "items": [
                { "kind": "box_around", "parts": ["U1", "C1"], "title": "Regulator", "margin_mm": 5.0 },
                { "kind": "text", "x_mm": 30.0, "y_mm": 150.0, "text": "Checked by assistant" },
                { "kind": "line", "x1_mm": 30.0, "y1_mm": 152.0, "x2_mm": 60.0, "y2_mm": 152.0 },
                { "kind": "circle", "x_mm": 80.0, "y_mm": 150.0, "radius_mm": 3.0 },
            ] }),
        )
        .await;
    assert_eq!(draw["ok"], true, "{draw}");
    assert_eq!(draw["result"]["can_apply"], true, "{draw}");
    let applied = mcp.call("sch_apply_plan", apply(draw)).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    assert_eq!(
        layout["result"]["drawings"].as_array().unwrap().len(),
        drawings_before + 3,
        "{layout}"
    );
    assert_eq!(
        layout["result"]["notes"].as_array().unwrap().len(),
        notes_before + 2,
        "{layout}"
    );
    mcp.call("sch_save", json!({ "session_id": id })).await;

    // Labels: a second name for a named net is refused; renaming works and keeps every net.
    let second = mcp
        .call(
            "sch_label_plan",
            json!({ "session_id": id, "add": [{ "text": "SECOND", "x_mm": label["at"]["x"], "y_mm": label["at"]["y"] }] }),
        )
        .await;
    assert_eq!(second["result"]["can_apply"], false, "{second}");
    assert!(
        second["result"]["findings"]
            .to_string()
            .contains("LABEL_NET_ALREADY_NAMED"),
        "{second}"
    );
    let rename = mcp
        .call(
            "sch_label_plan",
            json!({ "session_id": id, "rename": [{ "id": label["id"], "text": "RENAMED" }] }),
        )
        .await;
    assert_eq!(rename["result"]["can_apply"], true, "{rename}");
    let applied = mcp.call("sch_apply_plan", apply(rename)).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert!(nets["result"].to_string().contains("RENAMED"), "{nets}");
    mcp.call("sch_save", json!({ "session_id": id })).await;

    // Turn a label.
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    let renamed = layout["result"]["labels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["text"] == "RENAMED")
        .unwrap()
        .clone();
    let turn = mcp
        .call(
            "sch_label_plan",
            json!({ "session_id": id, "turn": [{ "id": renamed["id"], "direction": "up" }] }),
        )
        .await;
    let applied = mcp.call("sch_apply_plan", apply(turn)).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    mcp.call("sch_save", json!({ "session_id": id })).await;

    // Items: move the new note, delete the line and the circle, refuse to move a wire.
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    let note = layout["result"]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["text"] == "Checked by assistant")
        .unwrap()
        .clone();
    let wire = layout["result"]["wires"][0]["id"].clone();
    let shapes: Vec<serde_json::Value> = layout["result"]["drawings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["shape"].to_string().contains("circle") || d["shape"].to_string().contains("line"))
        .map(|d| d["id"].clone())
        .collect();
    let refused = mcp
        .call(
            "sch_edit_items_plan",
            json!({ "session_id": id, "moves": [{ "id": wire, "dx_mm": 1.0, "dy_mm": 0.0 }] }),
        )
        .await;
    assert_eq!(refused["result"]["can_apply"], false, "{refused}");
    let edit = mcp
        .call(
            "sch_edit_items_plan",
            json!({ "session_id": id, "remove": shapes, "moves": [{ "id": note["id"], "dx_mm": 10.0, "dy_mm": 5.0 }] }),
        )
        .await;
    assert_eq!(edit["result"]["can_apply"], true, "{edit}");
    let applied = mcp.call("sch_apply_plan", apply(edit)).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    assert_eq!(
        layout["result"]["drawings"].as_array().unwrap().len(),
        drawings_before + 1,
        "{layout}"
    );
    let moved = layout["result"]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["text"] == "Checked by assistant")
        .unwrap();
    assert_eq!(
        moved["at"]["x"].as_f64().unwrap(),
        note["at"]["x"].as_f64().unwrap() + 10.16,
        "snapped to the grid: {moved}"
    );
    // Text: after a tidy, move whatever text runs into something. The placements read back.
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let tidy = mcp.call("sch_tidy_plan", json!({ "session_id": id })).await;
    assert_eq!(tidy["result"]["can_apply"], true, "{tidy}");
    let applied = mcp.call("sch_apply_plan", apply(tidy)).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let declutter = mcp.call("sch_declutter_plan", json!({ "session_id": id })).await;
    assert_eq!(declutter["result"]["can_apply"], true, "{declutter}");
    let changes = declutter["result"]["counts"]["field_changes"].as_u64().unwrap_or(0)
        + declutter["result"]["counts"]["labels"].as_u64().unwrap_or(0);
    println!("DECLUTTER ({provider}): {changes} changes");
    if changes > 0 {
        let applied = mcp.call("sch_apply_plan", apply(declutter)).await;
        assert_eq!(applied["ok"], true, "{applied}");
        assert_eq!(applied["result"]["verified"], true, "{applied}");
        assert!(applied["result"]["placements_checked"].as_u64().unwrap_or(0) > 0 || changes > 0);
    }
    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    let problems = review["result"]["problems"].to_string();
    assert!(!problems.contains("runs into"), "text still overlaps: {problems}");
    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!("ERC after text tools ({provider}): {} errors", erc["result"]["errors"]);
}

#[tokio::test]
async fn read_only_mode_refuses_changes() {
    let mut mcp = Mcp::start_with(&["--read-only"]).await;
    for (tool, args) in [
        (
            "sch_apply_plan",
            json!({ "session_id": "s", "plan_id": "p", "confirm": true, "idempotency_key": "key-12345" }),
        ),
        ("sch_save", json!({ "session_id": "s" })),
        (
            "sch_export_netlist",
            json!({ "session_id": "s", "output_path": "C:/x/y.net" }),
        ),
    ] {
        let r = mcp.call(tool, args).await;
        assert_eq!(r["error"]["code"], "READ_ONLY_MODE", "{tool}: {r}");
    }
}

/// Against a real KiCad 11 editor: open a disposable, empty project in the nightly Schematic
/// Editor, press "KiCad AI: connect", then run with THINGSVU_GUI=1.
#[tokio::test]
async fn gui_editor_demo() {
    if std::env::var("THINGSVU_GUI").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let status = mcp.call("session_status", json!({})).await;
    println!("GUI EDITORS {}", status["result"]["gui_editors"]);
    assert!(
        !status["result"]["gui_editors"].as_array().unwrap().is_empty(),
        "press the KiCad AI button first"
    );

    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    println!(
        "GUI SESSION {} document {}",
        opened["result"]["kicad_version"], opened["result"]["document"]
    );
    println!("GUI CAPABILITIES {}", opened["result"]["capabilities"]);
    assert_eq!(opened["provenance"][0]["provider"], "gui");

    let before = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert_eq!(before["result"]["count"], 0, "use an empty project: {before}");

    let applied = mcp
        .call("sch_create_demo_circuit", json!({ "session_id": id, "apply": true, "confirm": true, "idempotency_key": format!("gui-{}", std::process::id()) }))
        .await;
    println!("GUI APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["undo_steps"], 2);

    // Values must persist in the editor (the full-object update did not in Phase 0).
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    let values: Vec<_> = symbols["result"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["reference"].as_str().unwrap().to_owned(),
                s["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        values,
        [("R1".to_owned(), "10k".to_owned()), ("R2".to_owned(), "10k".to_owned())]
    );

    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "GUI ERC ok={} errors={} warnings={}",
        erc["ok"], erc["result"]["errors"], erc["result"]["warnings"]
    );
    let render = mcp
        .request(
            "tools/call",
            json!({ "name": "sch_render", "arguments": { "session_id": id } }),
        )
        .await;
    let has_image = render["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["type"] == "image");
    println!("GUI RENDER image={has_image}");

    // Saving in the GUI: recorded, not asserted (SaveDocument support in the editor is unverified).
    let saved = mcp.call("sch_save", json!({ "session_id": id })).await;
    println!("GUI SAVE {saved}");

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: check the editor (two Ctrl+Z steps should remove the demo)");
}

/// Against a real KiCad 11 editor with a schematic open: review it, plan a tidy, apply it, and
/// check ERC. Open a **copy** of a design in the nightly Schematic Editor, press "KiCad AI:
/// connect", then run with THINGSVU_GUI_TIDY=1. Undo in the editor (Ctrl+Z twice) restores it.
#[tokio::test]
async fn gui_tidy_demo() {
    if std::env::var("THINGSVU_GUI_TIDY").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_TIDY=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    println!("GUI SESSION {}", opened["result"]["document"]);

    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    println!("GUI REVIEW {}", review["result"]);
    assert_eq!(review["ok"], true, "{review}");
    let erc_before = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "GUI ERC BEFORE errors={} warnings={}",
        erc_before["result"]["errors"], erc_before["result"]["warnings"]
    );

    let plan = mcp.call("sch_tidy_plan", json!({ "session_id": id })).await;
    println!(
        "GUI PLAN counts={} findings={}",
        plan["result"]["counts"], plan["result"]["findings"]
    );
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({
                "session_id": id,
                "plan_id": plan["result"]["plan_id"],
                "confirm": true,
                "idempotency_key": format!("gui-tidy-{}", std::process::id())
            }),
        )
        .await;
    println!("GUI APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["undo_steps"], 2);

    let after = mcp.call("sch_review", json!({ "session_id": id })).await;
    println!("GUI REVIEW AFTER {}", after["result"]);
    let again = mcp.call("sch_tidy_plan", json!({ "session_id": id })).await;
    println!("GUI PLAN AGAIN removals={}", again["result"]["removals"]);
    assert_eq!(after["result"]["tidy"]["already_tidy"], true, "{after}");
    let erc_after = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "GUI ERC AFTER errors={} warnings={}",
        erc_after["result"]["errors"], erc_after["result"]["warnings"]
    );
    assert!(erc_after["result"]["errors"].as_u64() <= erc_before["result"]["errors"].as_u64());

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: look at the editor; Ctrl+Z twice undoes the tidy");
}

/// Against a real KiCad 11 editor: move two parts, let their wires break, wire everything again.
/// Open a **copy** of a design, press "KiCad AI: connect", then run with THINGSVU_GUI_ARRANGE=1.
/// Ctrl+Z in the editor undoes each step.
#[tokio::test]
async fn gui_arrange_demo() {
    if std::env::var("THINGSVU_GUI_ARRANGE").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_ARRANGE=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    let parts = layout["result"]["parts"].as_array().unwrap();
    let movable: Vec<&str> = parts
        .iter()
        .filter_map(|p| p["reference"].as_str())
        .filter(|r| !r.starts_with('#'))
        .take(2)
        .collect();
    println!("GUI ARRANGE moving {movable:?}");
    let erc_before = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    let edits: Vec<_> = movable
        .iter()
        .map(|r| json!({ "reference": r, "dx_mm": 25.4, "dy_mm": 12.7 }))
        .collect();
    let plan = mcp
        .call("sch_arrange_plan", json!({ "session_id": id, "edits": edits }))
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("gui-arrange-{}", std::process::id()) }),
        )
        .await;
    println!("GUI ARRANGE APPLY {applied}");
    assert_eq!(applied["ok"], true, "{applied}");

    let reconnect = mcp.call("sch_reconnect_plan", json!({ "session_id": id })).await;
    println!(
        "GUI RECONNECT PLAN counts={} findings={}",
        reconnect["result"]["counts"], reconnect["result"]["findings"]
    );
    assert_eq!(reconnect["result"]["can_apply"], true, "{reconnect}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": reconnect["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("gui-reconnect-{}", std::process::id()) }),
        )
        .await;
    println!("GUI RECONNECT APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let erc_after = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "GUI ERC before={} after={}",
        erc_before["result"]["errors"], erc_after["result"]["errors"]
    );
    assert!(erc_after["result"]["errors"].as_u64() <= erc_before["result"]["errors"].as_u64());
    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: look at the editor; Ctrl+Z undoes the steps");
}

/// Against a real KiCad 11 editor: report text that runs into things, move it, draw a titled box
/// around the first two parts, and report again. Open a **copy**, press "KiCad AI: connect", then
/// run with THINGSVU_GUI_TEXT=1. Ctrl+Z in the editor undoes each step.
#[tokio::test]
async fn gui_text_demo() {
    if std::env::var("THINGSVU_GUI_TEXT").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_TEXT=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    println!("GUI TEXT PROBLEMS BEFORE {}", review["result"]["problems"]);

    let declutter = mcp.call("sch_declutter_plan", json!({ "session_id": id })).await;
    println!(
        "GUI DECLUTTER counts={} changes={} findings={}",
        declutter["result"]["counts"], declutter["result"]["field_changes"], declutter["result"]["findings"]
    );
    assert_eq!(declutter["result"]["can_apply"], true, "{declutter}");
    let changes = declutter["result"]["counts"]["field_changes"].as_u64().unwrap_or(0)
        + declutter["result"]["counts"]["labels"].as_u64().unwrap_or(0);
    if changes > 0 {
        let applied = mcp
            .call(
                "sch_apply_plan",
                json!({ "session_id": id, "plan_id": declutter["result"]["plan_id"], "confirm": true,
                        "idempotency_key": format!("gui-declutter-{}", std::process::id()) }),
            )
            .await;
        println!("GUI DECLUTTER APPLY {applied}");
        assert_eq!(applied["result"]["verified"], true, "{applied}");
    }

    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    let parts: Vec<&str> = layout["result"]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["reference"].as_str())
        .filter(|r| !r.starts_with('#'))
        .take(2)
        .collect();
    let draw = mcp
        .call(
            "sch_draw_plan",
            json!({ "session_id": id, "items": [{ "kind": "box_around", "parts": parts, "title": "Assistant box" }] }),
        )
        .await;
    assert_eq!(draw["result"]["can_apply"], true, "{draw}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": draw["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("gui-draw-{}", std::process::id()) }),
        )
        .await;
    println!("GUI DRAW APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");

    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    println!("GUI TEXT PROBLEMS AFTER {}", review["result"]["problems"]);
    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: look at the editor; Ctrl+Z undoes the steps");
}

/// Parts and BOM: review, set fields, check and assign footprints, export. Every plan leaves the
/// nets as they were, and each field is read back.
#[tokio::test]
async fn live_bom_and_footprints_through_mcp() {
    bom_flow("file").await;
}

#[tokio::test]
async fn live_bom_and_footprints_through_the_api() {
    bom_flow("headless").await;
}

async fn bom_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-bom-{provider}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": provider }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let n = std::process::id();
    let mut step = 0;
    let mut apply = |plan: serde_json::Value| {
        step += 1;
        json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                "idempotency_key": format!("bom-{provider}-{n}-{step}") })
    };

    // The BOM and its review, before any part data exists.
    let bom = mcp.call("bom_get", json!({ "session_id": id })).await;
    assert_eq!(bom["ok"], true, "{bom}");
    let lines_before = bom["result"]["rows"].as_array().unwrap().len();
    assert!(lines_before >= 5, "{bom}");
    assert_eq!(bom["result"]["custom_fields"], json!([]), "{bom}");
    let review = mcp.call("bom_review", json!({ "session_id": id })).await;
    let codes = review["result"]["issues"].to_string();
    assert!(
        codes.contains("MISSING_MPN") && codes.contains("MISSING_SUPPLIER_PART"),
        "{review}"
    );
    assert!(!review["result"]["lookups"].as_array().unwrap().is_empty(), "{review}");

    // The same lines as calls to make on the parts service; the server looks nothing up.
    let parts = mcp.call("parts_lookup", json!({ "session_id": id })).await;
    assert_eq!(parts["ok"], true, "{parts}");
    assert_eq!(parts["result"]["source"]["name"], "pcbparts", "{parts}");
    assert_eq!(parts["result"]["source"]["kind"], "external", "{parts}");
    let requests = parts["result"]["requests"].as_array().unwrap();
    assert_eq!(
        requests.len(),
        review["result"]["lookups"].as_array().unwrap().len(),
        "{parts}"
    );
    assert!(
        requests
            .iter()
            .all(|r| r["status"] == "delegated" && !r["steps"].as_array().unwrap().is_empty()),
        "{parts}"
    );
    assert!(
        requests
            .iter()
            .any(|r| r["steps"][0]["tool"] == "jlc_search" && r["steps"][0]["arguments"]["query"].is_string()),
        "{parts}"
    );
    let only = mcp
        .call("parts_lookup", json!({ "session_id": id, "references": ["C1"] }))
        .await;
    assert!(
        only["result"]["requests"].as_array().unwrap().len() < requests.len(),
        "{only}"
    );
    let status = mcp.call("session_status", json!({})).await;
    assert_eq!(
        status["result"]["parts_lookup"]["source"]["name"], "pcbparts",
        "{status}"
    );

    // Fields: part numbers on the capacitors, and one resistor out of the BOM run.
    let plan = mcp
        .call(
            "sch_set_fields_plan",
            json!({ "session_id": id, "edits": [
                { "reference": "C1", "fields": { "MPN": "GRM21BR61A106KE19", "Manufacturer": "Murata", "LCSC": "C440198" } },
                { "reference": "C2", "fields": { "MPN": "CL10B104KB8NNNC", "Manufacturer": "Samsung", "LCSC": "C1591" } },
                { "reference": "R3", "fields": { "DNP": "yes" } },
            ] }),
        )
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    assert_eq!(plan["result"]["counts"]["property_changes"], 7, "{plan}");
    let applied = mcp.call("sch_apply_plan", apply(plan)).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert!(
        applied["result"]["placements_checked"].as_u64().unwrap() >= 7,
        "{applied}"
    );
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let bom = mcp.call("bom_get", json!({ "session_id": id })).await;
    let fields = bom["result"]["custom_fields"].to_string();
    assert!(
        fields.contains("MPN") && fields.contains("LCSC") && fields.contains("Manufacturer"),
        "{bom}"
    );
    assert!(
        bom["result"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["dnp"] == true && r["references"] == json!(["R3"])),
        "{bom}"
    );
    let reviewed = mcp.call("bom_review", json!({ "session_id": id })).await;
    assert!(
        !reviewed["result"]["lookups"].to_string().contains("\"C1\""),
        "C1 is complete now: {reviewed}"
    );
    let after = mcp
        .call("parts_lookup", json!({ "session_id": id, "references": ["C1"] }))
        .await;
    assert_eq!(
        after["result"]["requests"],
        json!([]),
        "nothing is left to look up for C1: {after}"
    );

    // A flag or field that makes no change, and bad requests, are refused or skipped.
    let same = mcp
        .call(
            "sch_set_fields_plan",
            json!({ "session_id": id, "edits": [{ "reference": "C1", "fields": { "MPN": "GRM21BR61A106KE19" } }] }),
        )
        .await;
    assert!(
        same["result"]["findings"].to_string().contains("NOTHING_TO_CHANGE"),
        "{same}"
    );
    let bad = mcp
        .call("sch_set_fields_plan", json!({ "session_id": id, "edits": [{ "reference": "C1", "fields": { "Reference": "C9", "DNP": "maybe" } }] }))
        .await;
    assert_eq!(bad["result"]["can_apply"], false, "{bad}");

    // Footprints: search, read, check, assign.
    let search = mcp
        .call(
            "fp_search",
            json!({ "query": "r 0805", "library": "Resistor_SMD", "session_id": id }),
        )
        .await;
    assert_eq!(search["ok"], true, "{search}");
    assert!(
        search["result"]["footprints"]
            .to_string()
            .contains("Resistor_SMD:R_0805_2012Metric"),
        "{search}"
    );
    let footprint = mcp
        .call("fp_get", json!({ "id": "Resistor_SMD:R_0805_2012Metric" }))
        .await;
    assert_eq!(footprint["result"]["pad_count"], 2, "{footprint}");
    assert!(
        footprint["result"]["courtyard"]["width"].as_f64().unwrap() > 2.0,
        "{footprint}"
    );
    let missing = mcp.call("fp_get", json!({ "id": "Nope:Nothing" })).await;
    assert_eq!(missing["ok"], false, "{missing}");

    let check = mcp.call("sch_footprint_plan", json!({ "session_id": id })).await;
    assert_eq!(check["ok"], true, "{check}");
    println!("FOOTPRINT CHECK ({provider}): {}", check["result"]["findings"]);
    let unknown = mcp
        .call(
            "sch_footprint_plan",
            json!({ "session_id": id, "assign": [{ "reference": "R3", "footprint": "Nope:Nothing" }] }),
        )
        .await;
    assert_eq!(unknown["result"]["can_apply"], false, "{unknown}");
    let assign = mcp
        .call(
            "sch_footprint_plan",
            json!({ "session_id": id, "assign": [{ "reference": "R3", "footprint": "Resistor_SMD:R_0805_2012Metric" }] }),
        )
        .await;
    assert_eq!(assign["result"]["can_apply"], true, "{assign}");
    assert_eq!(assign["result"]["counts"]["property_changes"], 1, "{assign}");
    let applied = mcp.call("sch_apply_plan", apply(assign)).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    let r3 = symbols["result"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["reference"] == "R3")
        .unwrap();
    assert_eq!(r3["footprint"], "Resistor_SMD:R_0805_2012Metric", "{r3}");
    mcp.call("sch_save", json!({ "session_id": id })).await;

    // Export: the generic CSV has the custom fields; the JLCPCB one leaves the DNP part out.
    let generic = dir.join("bom.csv");
    let exported = mcp
        .call(
            "bom_export",
            json!({ "session_id": id, "output_path": generic, "preset": "generic" }),
        )
        .await;
    assert_eq!(exported["ok"], true, "{exported}");
    let text = std::fs::read_to_string(&generic).unwrap();
    assert!(text.contains("MPN") && text.contains("GRM21BR61A106KE19"), "{text}");
    let jlc = dir.join("jlc.csv");
    let exported = mcp
        .call(
            "bom_export",
            json!({ "session_id": id, "output_path": jlc, "preset": "jlcpcb" }),
        )
        .await;
    assert_eq!(exported["ok"], true, "{exported}");
    let text = std::fs::read_to_string(&jlc).unwrap();
    assert!(
        text.contains("Designator") && text.contains("LCSC Part #") && text.contains("C440198"),
        "{text}"
    );
    assert!(!text.contains("R3"), "the DNP part is left out: {text}");
    let outside = mcp
        .call(
            "bom_export",
            json!({ "session_id": id, "output_path": std::env::temp_dir().join("elsewhere.csv") }),
        )
        .await;
    assert_eq!(outside["error"]["code"], "INVALID_ARGUMENT", "{outside}");

    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!("ERC after BOM tools ({provider}): {} errors", erc["result"]["errors"]);
}

/// Against a real KiCad 11 editor: review the BOM, set a part-number field on the first part and
/// read it back, then check the footprints. Open a **copy**, press "KiCad AI: connect", then run
/// with THINGSVU_GUI_BOM=1. Ctrl+Z in the editor undoes the field change.
#[tokio::test]
async fn gui_bom_demo() {
    if std::env::var("THINGSVU_GUI_BOM").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_BOM=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let review = mcp.call("bom_review", json!({ "session_id": id })).await;
    println!("GUI BOM REVIEW {}", review["result"]["issues"]);
    let bom = mcp.call("bom_get", json!({ "session_id": id })).await;
    let first = bom["result"]["rows"][0]["references"][0].as_str().unwrap().to_owned();
    println!("GUI BOM first part {first}");
    let plan = mcp
        .call(
            "sch_set_fields_plan",
            json!({ "session_id": id, "edits": [{ "reference": first, "fields": { "MPN": "TEST-PART-123" } }] }),
        )
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("gui-bom-{}", std::process::id()) }),
        )
        .await;
    println!("GUI BOM APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let check = mcp.call("sch_footprint_plan", json!({ "session_id": id })).await;
    println!("GUI FOOTPRINT CHECK {}", check["result"]["findings"]);
    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: look at part {first} (field MPN is hidden); Ctrl+Z undoes it");
}

fn board_fixture(name: &str, empty_board: bool) -> std::path::PathBuf {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-board");
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-board-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(source.join("regulator.kicad_sch"), dir.join("regulator.kicad_sch")).unwrap();
    if empty_board {
        std::fs::write(
            dir.join("regulator.kicad_pcb"),
            "(kicad_pcb (version 20261018) (generator \"pcbnew\") (generator_version \"10.99\")\n)\n",
        )
        .unwrap();
    } else {
        std::fs::copy(source.join("regulator.kicad_pcb"), dir.join("regulator.kicad_pcb")).unwrap();
    }
    dir.join("regulator.kicad_pcb")
}

/// Read a board, run DRC, render it, and review it.
#[tokio::test]
async fn live_board_read_drc_render_and_review_through_mcp() {
    board_read_flow("file").await;
}

#[tokio::test]
async fn live_board_read_drc_render_and_review_through_the_api() {
    board_read_flow("headless").await;
}

async fn board_read_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture(&format!("read-{provider}"), false);
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": provider }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(info["ok"], true, "{info}");
    assert_eq!(info["result"]["counts"]["footprints"], 11, "{info}");
    assert!(info["result"]["counts"]["pads"].as_u64().unwrap() >= 20, "{info}");
    assert!(info["result"]["counts"]["nets"].as_u64().unwrap() >= 6, "{info}");
    assert!(info["result"]["outline"].is_null(), "{info}");
    assert_eq!(info["result"]["copper_layers"], json!(["F.Cu", "B.Cu"]), "{info}");

    let list = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
    assert_eq!(list["result"]["count"], 11, "{list}");
    let j1 = list["result"]["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["reference"] == "J1")
        .unwrap();
    assert_eq!(j1["side"], "front");
    assert!((j1["at"]["x"].as_f64().unwrap() - 15.66).abs() < 0.02, "{j1}");
    assert!(
        j1["courtyard"]["width"].as_f64().unwrap() > 2.0,
        "courtyards come from the libraries: {j1}"
    );

    let u1 = mcp
        .call("pcb_get_footprint", json!({ "session_id": id, "reference": "U1" }))
        .await;
    assert_eq!(u1["result"]["pad_count"], 4, "{u1}");
    assert!(u1["result"]["pads"].to_string().contains("+3V3"), "{u1}");
    let missing = mcp
        .call("pcb_get_footprint", json!({ "session_id": id, "reference": "U9" }))
        .await;
    assert_eq!(missing["ok"], false, "{missing}");

    // Phase 14.4: the full geometry. Every pad has a shape and a size; the through-hole connector's
    // pads have a drill; the courtyard is an outline; the reference is a visible text; the place of
    // each pad agrees with the summary listing.
    let full = mcp
        .call(
            "pcb_list_footprints",
            json!({ "session_id": id, "detail": "full", "include_pads": true, "references": ["J1", "U1"] }),
        )
        .await;
    assert_eq!(full["ok"], true, "{full}");
    assert_eq!(full["result"]["count"], 2, "only the named references: {full}");
    for f in full["result"]["footprints"].as_array().unwrap() {
        let detail = &f["detail"];
        let summary_pads = f["pads"].as_array().unwrap();
        let pads = detail["pads"].as_array().unwrap();
        assert_eq!(pads.len(), summary_pads.len(), "{f}");
        for pad in pads {
            assert!(
                pad["shape"].is_string() && pad["size"]["x"].as_f64().unwrap() > 0.0,
                "{pad}"
            );
            // (A number can be on two pads: the tab of a SOT-223 is a second pad 2.)
            let near = |p: &Value| {
                p["number"] == pad["number"]
                    && (p["at"]["x"].as_f64().unwrap() - pad["at"]["x"].as_f64().unwrap()).abs() < 0.01
                    && (p["at"]["y"].as_f64().unwrap() - pad["at"]["y"].as_f64().unwrap()).abs() < 0.01
            };
            assert!(
                summary_pads.iter().any(near),
                "{provider}: detail and summary agree on where pad {} is: {pad}",
                pad["number"]
            );
        }
        assert!(!detail["courtyard"].as_array().unwrap().is_empty(), "{f}");
        let reference = detail["texts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "Reference")
            .unwrap_or_else(|| panic!("{provider}: no reference text in {detail}"));
        assert_eq!(reference["text"], f["reference"], "{reference}");
        assert!(reference["height"].as_f64().unwrap() > 0.0, "{reference}");
    }
    let j1_full = full["result"]["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["reference"] == "J1")
        .unwrap();
    assert!(
        j1_full["detail"]["pads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["kind"] == "thru_hole" && p["drill"]["x"].as_f64().unwrap_or(0.0) > 0.3),
        "{provider}: the connector's pads are drilled: {j1_full}"
    );
    println!("DETAIL ({provider}) J1 pad {}", j1_full["detail"]["pads"][0]);
    println!("DETAIL ({provider}) J1 reference {}", j1_full["detail"]["texts"][0]);
    let one = mcp
        .call(
            "pcb_get_footprint",
            json!({ "session_id": id, "reference": "U1", "detail": "full" }),
        )
        .await;
    assert_eq!(one["result"]["detail"]["pads"].as_array().unwrap().len(), 4, "{one}");
    let bad = mcp
        .call(
            "pcb_list_footprints",
            json!({ "session_id": id, "detail": "everything" }),
        )
        .await;
    assert_eq!(bad["ok"], false, "{bad}");
    // A board with no outline and no drawings has none to list.
    let drawings = mcp.call("pcb_list_drawings", json!({ "session_id": id })).await;
    assert_eq!(drawings["ok"], true, "{drawings}");
    assert_eq!(drawings["result"]["shapes_matching"], 0, "{drawings}");

    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let gnd = nets["result"]["nets"]["GND"]["pads"].as_array().unwrap();
    assert!(gnd.len() >= 6, "{nets}");
    let tracks = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    assert_eq!(tracks["result"]["track_count"], 0, "{tracks}");

    let drc = mcp.call("pcb_run_drc", json!({ "session_id": id })).await;
    assert_eq!(drc["ok"], true, "{drc}");
    assert_eq!(drc["result"]["errors"], 1, "the missing outline: {drc}");
    assert!(drc["result"]["by_type"]["invalid_outline"] == 1, "{drc}");
    assert!(
        drc["result"]["unrouted"].as_u64().unwrap() >= 10,
        "nothing is routed yet: {drc}"
    );

    let render = mcp
        .call("pcb_render", json!({ "session_id": id, "width": 800, "height": 500 }))
        .await;
    assert_eq!(render["ok"], true, "{render}");

    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    assert_eq!(review["ok"], true, "{review}");
    let codes = review["result"]["issues"].to_string();
    assert!(codes.contains("NO_OUTLINE"), "{review}");
    assert!(
        !codes.contains("NO_FOOTPRINT_FOR_PART"),
        "every part has its footprint: {review}"
    );
    assert!(review["result"]["schematic_compared"].is_string(), "{review}");
    assert_eq!(review["result"]["drc"]["errors"], 1, "{review}");
    println!("BOARD REVIEW ({provider}): {}", review["result"]["issues"]);

    // A file session cannot update from the schematic.
    let update = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    if provider == "file" {
        assert_eq!(update["ok"], false, "{update}");
    } else {
        assert_eq!(
            update["result"]["new_footprints"], 0,
            "the board already has everything: {update}"
        );
    }
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
}

/// An empty board is updated from its schematic: planned (nothing changes), applied, verified
/// against the netlist, saved, and read back.
#[tokio::test]
async fn live_board_is_updated_from_the_schematic() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("update", true);
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let before = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(before["result"]["counts"]["footprints"], 0, "{before}");
    let early = mcp.call("pcb_review", json!({ "session_id": id })).await;
    assert!(
        early["result"]["issues"].to_string().contains("NO_FOOTPRINTS"),
        "{early}"
    );

    let plan = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    assert_eq!(plan["result"]["new_footprints"], 11, "{plan}");
    assert!(plan["result"]["report"].as_str().unwrap().contains("Add R3"), "{plan}");
    // Planning changed nothing.
    let still = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(still["result"]["counts"]["footprints"], 0, "{still}");

    let key = format!("pcb-update-{}", std::process::id());
    let refused = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": false, "idempotency_key": key }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "INVALID_ARGUMENT", "{refused}");
    let applied = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true, "idempotency_key": key }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["footprints_on_board"], 11, "{applied}");
    let again = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true, "idempotency_key": format!("{key}-2") }),
        )
        .await;
    assert_eq!(again["ok"], false, "a plan is applied once: {again}");

    // The live board has them; the file does not until it is saved.
    let after = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(after["result"]["counts"]["footprints"], 11, "{after}");
    assert_eq!(after["result"]["status"]["unsaved_changes"], true, "{after}");
    let close = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(close["ok"], false, "unsaved changes refuse a close: {close}");
    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let replanned = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    assert_eq!(
        replanned["result"]["new_footprints"], 0,
        "nothing is left to add: {replanned}"
    );
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");

    // Read back from the file by a second session.
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let fid = file["result"]["session_id"].clone();
    let info = mcp.call("pcb_info", json!({ "session_id": fid })).await;
    assert_eq!(info["result"]["counts"]["footprints"], 11, "{info}");
    let review = mcp.call("pcb_review", json!({ "session_id": fid })).await;
    assert!(
        !review["result"]["issues"].to_string().contains("NO_FOOTPRINT_FOR_PART"),
        "{review}"
    );
    assert!(
        !review["result"]["issues"].to_string().contains("FOOTPRINT_DIFFERS"),
        "{review}"
    );
}

/// Against a real KiCad 11 PCB editor: read the board, run DRC, review it, and plan an update from
/// the schematic (a dry run). Open a **copy** of a project in the PCB Editor, press "KiCad AI:
/// connect" there, then run with THINGSVU_GUI_PCB=1.
#[tokio::test]
async fn gui_pcb_demo() {
    if std::env::var("THINGSVU_GUI_PCB").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    println!("GUI PCB {}", opened["result"]["document"]);
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    println!("GUI PCB INFO {}", info["result"]);
    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    println!("GUI PCB REVIEW {}", review["result"]["issues"]);
    println!("GUI PCB DRC {}", review["result"]["drc"]);
    let plan = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    println!("GUI PCB UPDATE PLAN {plan}");
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: nothing was changed");
}

/// Applies a board plan and returns the result.
async fn apply_board(mcp: &mut Mcp, id: &Value, plan: &Value, key: &str) -> Value {
    mcp.call(
        "pcb_apply_plan",
        json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true, "idempotency_key": key }),
    )
    .await
}

fn footprint<'a>(list: &'a Value, reference: &str) -> &'a Value {
    list["result"]["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["reference"] == reference)
        .unwrap_or_else(|| panic!("no {reference} in {list}"))
}

/// An outline with mounting holes, then footprints moved, turned, flipped, and locked; each plan
/// applied, read back, saved, and read again from the file.
#[tokio::test]
async fn live_board_outline_and_moves() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("edit", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-edit-{n}-{}", std::process::id());

    // A file session only reads.
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let refused = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": file["result"]["session_id"] }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "FEATURE_UNAVAILABLE", "{refused}");
    mcp.call("pcb_close", json!({ "session_id": file["result"]["session_id"] }))
        .await;

    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    // 40 x 30 mm, rounded, centred on the parts, a hole in each corner.
    let plan = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 40, "height_mm": 30, "corner_radius_mm": 2,
                    "mounting_holes": "corners", "hole_inset_mm": 3.5 }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    println!("OUTLINE PLAN {}", plan["result"]);
    let ops = plan["result"]["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 5, "an outline and four holes: {plan}");
    assert_eq!(ops[0]["outline"]["width"], 40.0, "{plan}");
    assert_eq!(plan["result"]["after"]["outside_outline"], json!([]), "{plan}");
    let outline = ops[0]["outline"].clone();
    let applied = apply_board(&mut mcp, &id, &plan, &key("outline")).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(
        applied["result"]["report"]["placed"],
        json!(["H1", "H2", "H3", "H4"]),
        "{applied}"
    );
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert!(
        (info["result"]["outline"]["width"].as_f64().unwrap() - 40.05).abs() < 0.02,
        "{info}"
    );
    assert_eq!(info["result"]["counts"]["footprints"], 15, "{info}");

    // A second outline is refused unless it replaces the first.
    let again = mcp.call("pcb_outline_plan", json!({ "session_id": id })).await;
    assert_eq!(again["ok"], false, "{again}");
    let replace = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "replace": true, "margin_mm": 3 }),
        )
        .await;
    assert_eq!(replace["ok"], true, "{replace}");
    assert_eq!(
        replace["result"]["operations"][0]["op"], "remove_outline_drawing",
        "{replace}"
    );

    // Moves: R3 turned and placed, C1 to the back, U1 shifted and locked.
    let before = mcp
        .call("pcb_list_footprints", json!({ "session_id": id, "include_pads": true }))
        .await;
    let u1_before = footprint(&before, "U1")["at"].clone();
    let plan = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [
                { "reference": "R3", "x_mm": 22, "y_mm": 4, "rotation_deg": 90 },
                { "reference": "C1", "side": "back" },
                { "reference": "U1", "dx_mm": 1, "dy_mm": 0, "lock": true }
            ] }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    println!("MOVE PLAN {}", plan["result"]);
    let applied = apply_board(&mut mcp, &id, &plan, &key("move")).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["report"]["flipped"], json!(["C1"]), "{applied}");
    let after = mcp
        .call("pcb_list_footprints", json!({ "session_id": id, "include_pads": true }))
        .await;
    let r3 = footprint(&after, "R3");
    assert_eq!(r3["at"], json!({ "x": 22.0, "y": 4.0 }), "{r3}");
    assert_eq!(r3["rotation_deg"], 90.0, "{r3}");
    // Turned 90°: the pads are one above the other, about the new place.
    let pads = r3["pads"].as_array().unwrap();
    assert!((pads[0]["at"]["x"].as_f64().unwrap() - 22.0).abs() < 0.01, "{r3}");
    assert!((pads[1]["at"]["x"].as_f64().unwrap() - 22.0).abs() < 0.01, "{r3}");
    assert!(
        (pads[0]["at"]["y"].as_f64().unwrap() - pads[1]["at"]["y"].as_f64().unwrap()).abs() > 1.0,
        "{r3}"
    );
    assert_eq!(footprint(&after, "C1")["side"], "back");
    let u1 = footprint(&after, "U1");
    assert_eq!(u1["locked"], true, "{u1}");
    assert!(
        (u1["at"]["x"].as_f64().unwrap() - u1_before["x"].as_f64().unwrap() - 1.0).abs() < 0.011,
        "{u1}"
    );

    // A locked footprint stays unless the move allows it.
    let locked = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": "U1", "dx_mm": 2, "dy_mm": 0 }] }),
        )
        .await;
    assert_eq!(locked["ok"], false, "{locked}");
    let unknown = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": "U9", "x_mm": 2, "y_mm": 0 }] }),
        )
        .await;
    assert_eq!(unknown["ok"], false, "{unknown}");
    // A plan made before another change is stale.
    let stale = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": "D1", "dx_mm": 1, "dy_mm": 0 }] }),
        )
        .await;
    let other = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": "D2", "dx_mm": 1, "dy_mm": 0 }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &other, &key("d2")).await["result"]["verified"],
        true
    );
    let late = apply_board(&mut mcp, &id, &stale, &key("d1")).await;
    assert_eq!(late["error"]["code"], "STALE_PLAN", "{late}");

    let drc = mcp.call("pcb_run_drc", json!({ "session_id": id })).await;
    assert_eq!(
        drc["result"]["by_type"]["invalid_outline"],
        Value::Null,
        "the outline is closed: {drc}"
    );
    println!("DRC AFTER EDITS {}", drc["result"]["by_type"]);
    let render = mcp
        .call("pcb_render", json!({ "session_id": id, "width": 800, "height": 500 }))
        .await;
    assert_eq!(render["ok"], true, "{render}");

    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    mcp.call("pcb_close", json!({ "session_id": id })).await;

    // The file says the same.
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let fid = file["result"]["session_id"].clone();
    let info = mcp.call("pcb_info", json!({ "session_id": fid })).await;
    assert_eq!(info["result"]["outline"]["width"], outline["width"], "{info}");
    let list = mcp
        .call(
            "pcb_list_footprints",
            json!({ "session_id": fid, "include_pads": true }),
        )
        .await;
    assert_eq!(footprint(&list, "R3")["pads"], r3["pads"], "{list}");
    // The pads' copper is known on both sides and turned with the part: an 0603 pad is 0.8 mm wide
    // and 0.95 mm tall at 0°, so 0.95 x 0.8 at 90°.
    let copper = &r3["pads"][0]["copper"];
    assert_eq!(
        (copper["width"].as_f64(), copper["height"].as_f64()),
        (Some(0.95), Some(0.8)),
        "{r3}"
    );
    assert_eq!(r3["pads"][0]["layers"], json!(["F.Cu"]), "{r3}");
    assert_eq!(
        footprint(&list, "H4")["library_id"],
        "MountingHole:MountingHole_3.2mm_M3"
    );
    assert_eq!(footprint(&list, "U1")["locked"], true);
}

/// Against a real KiCad 11 PCB editor: an outline around the parts and one part turned, each one
/// undo step. Open a **copy** of a project in the PCB Editor, press "KiCad AI: connect" there,
/// then run with THINGSVU_GUI_PCB_EDIT=1.
#[tokio::test]
async fn gui_pcb_edit_demo() {
    if std::env::var("THINGSVU_GUI_PCB_EDIT").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_EDIT=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let key = |n: &str| format!("gui-pcb-edit-{n}-{}", std::process::id());
    let plan = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "margin_mm": 3, "corner_radius_mm": 1, "replace": true }),
        )
        .await;
    println!("GUI OUTLINE PLAN {plan}");
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("outline")).await;
    println!("GUI OUTLINE APPLY {applied}");
    let list = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
    let first = list["result"]["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["locked"] == false)
        .expect("an unlocked footprint")["reference"]
        .clone();
    let plan = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": first, "rotate_by_deg": 90 }] }),
        )
        .await;
    println!("GUI MOVE PLAN {plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("move")).await;
    println!("GUI MOVE APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: {first} is turned and the board has an outline; Ctrl+Z twice undoes both");
}

/// The regulator board as KiCad leaves it after an update from the schematic (footprints stacked
/// near the origin): an outline with holes, then every footprint placed, applied, read back,
/// reviewed, and saved.
#[tokio::test]
async fn live_board_is_placed() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("place", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-place-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    let early = mcp.call("pcb_place_plan", json!({ "session_id": id })).await;
    assert_eq!(early["ok"], false, "no outline yet: {early}");

    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "corner_radius_mm": 1.5,
                    "origin": { "x_mm": 100, "y_mm": 80 }, "mounting_holes": "corners", "hole_inset_mm": 3.5 }),
        )
        .await;
    assert_eq!(outline["ok"], true, "{outline}");
    let applied = apply_board(&mut mcp, &id, &outline, &key("outline")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");

    let plan = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    let measured = &plan["result"]["measured"];
    println!("PLACE MEASURED {measured}");
    println!("PLACE WARNINGS {}", plan["result"]["warnings"]);
    assert_eq!(measured["main_ic"], "U1", "{measured}");
    assert_eq!(measured["after"]["overlapping_pairs"], json!([]), "{measured}");
    assert_eq!(measured["after"]["outside_outline"], json!([]), "{measured}");
    assert_eq!(
        measured["before"]["outside_outline"].as_array().unwrap().len(),
        11,
        "everything starts clustered at the origin, away from the outline: {measured}"
    );
    for h in ["H1", "H2", "H3", "H4"] {
        assert!(measured["fixed"].to_string().contains(h), "holes stay: {measured}");
    }
    assert_eq!(
        plan["result"]["operations"].as_array().unwrap().len(),
        11,
        "every part moves: {plan}"
    );
    let applied = apply_board(&mut mcp, &id, &plan, &key("place")).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(
        applied["result"]["measured_after"]["overlapping_pairs"],
        json!([]),
        "{applied}"
    );

    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    let codes = review["result"]["issues"].to_string();
    println!("REVIEW AFTER PLACING {}", review["result"]["issues"]);
    for code in [
        "FOOTPRINTS_OVERLAP",
        "OUTSIDE_OUTLINE",
        "DECOUPLING_FAR",
        "CONNECTOR_FAR_FROM_EDGE",
        "NO_OUTLINE",
    ] {
        assert!(!codes.contains(code), "{code}: {review}");
    }
    let drc = &review["result"]["drc"];
    println!("DRC AFTER PLACING {}", drc["by_type"]);
    assert_eq!(drc["by_type"]["courtyards_overlap"], Value::Null, "{drc}");
    assert_eq!(drc["by_type"]["invalid_outline"], Value::Null, "{drc}");
    let list = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
    let j1 = footprint(&list, "J1");
    assert!(
        j1["courtyard"]["min"]["x"].as_f64().unwrap() < 101.0,
        "J1 on the left edge: {j1}"
    );

    let render = mcp
        .call("pcb_render", json!({ "session_id": id, "width": 800, "height": 600 }))
        .await;
    assert_eq!(render["ok"], true, "{render}");
    if let Ok(dir) = std::env::var("THINGSVU_SAVE_RENDER") {
        use base64::Engine;
        let response = mcp
            .request(
                "tools/call",
                json!({ "name": "pcb_render", "arguments": { "session_id": id, "width": 1200, "height": 900 } }),
            )
            .await;
        let data = response["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == "image")
            .unwrap()["data"]
            .as_str()
            .unwrap()
            .to_owned();
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        std::fs::write(std::path::Path::new(&dir).join("placed-board.png"), bytes).unwrap();
    }

    // Placing again changes little: the board is already placed.
    let again = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    println!(
        "PLACE AGAIN {} operations",
        again["result"]["operations"].as_array().map(|a| a.len()).unwrap_or(0)
    );

    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    mcp.call("pcb_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let fid = file["result"]["session_id"].clone();
    let review = mcp.call("pcb_review", json!({ "session_id": fid })).await;
    assert!(
        !review["result"]["issues"].to_string().contains("FOOTPRINTS_OVERLAP"),
        "{review}"
    );
}

/// Against a real KiCad 11 PCB editor: every unlocked footprint placed inside the outline, one undo
/// step. Open a **copy** of a project that has an outline in the PCB Editor, press "KiCad AI:
/// connect" there, then run with THINGSVU_GUI_PCB_PLACE=1.
#[tokio::test]
async fn gui_pcb_place_demo() {
    if std::env::var("THINGSVU_GUI_PCB_PLACE").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_PLACE=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    // A board whose outline was deleted (select all, delete) gets one sized from the parts: about
    // three times their courtyard area, 4:3, centred on them.
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    if info["result"]["outline"].is_null() {
        let list = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
        let area: f64 = list["result"]["footprints"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| Some(f["courtyard"]["width"].as_f64()? * f["courtyard"]["height"].as_f64()?))
            .sum();
        let width = ((area * 3.0 * 4.0 / 3.0).sqrt().max(20.0) / 5.0).ceil() * 5.0;
        let height = ((width * 3.0 / 4.0) / 5.0).ceil() * 5.0;
        let outline = mcp
            .call(
                "pcb_outline_plan",
                json!({ "session_id": id, "width_mm": width, "height_mm": height, "corner_radius_mm": 1 }),
            )
            .await;
        println!(
            "GUI NO OUTLINE: adding {width} x {height} mm: {}",
            outline["result"]["operations"]
        );
        assert_eq!(outline["ok"], true, "{outline}");
        let applied = apply_board(
            &mut mcp,
            &id,
            &outline,
            &format!("gui-pcb-outline-{}", std::process::id()),
        )
        .await;
        assert_eq!(applied["result"]["verified"], true, "{applied}");
    }
    let plan = mcp.call("pcb_place_plan", json!({ "session_id": id })).await;
    println!("GUI PLACE PLAN {}", plan["result"]["measured"]);
    println!("GUI PLACE WARNINGS {}", plan["result"]["warnings"]);
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &format!("gui-pcb-place-{}", std::process::id())).await;
    println!("GUI PLACE APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    println!("GUI REVIEW AFTER PLACING {}", review["result"]["issues"]);
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: the footprints are placed; Ctrl+Z undoes it (and once more the outline, if one was added)");
}

/// The placed regulator board gets tracks: every two-pad net routed pad to pad where a simple path
/// is clear, and one route through a via; DRC before and after, read back, saved.
#[tokio::test]
async fn live_board_is_routed() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("route", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-route-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );

    // Refusals: two nets, an unknown pad, no layer in common.
    let short = mcp
        .call(
            "pcb_route_plan",
            json!({ "session_id": id, "routes": [{ "points": [{ "pad": "U1.1" }, { "pad": "U1.2" }] }] }),
        )
        .await;
    assert_eq!(short["ok"], false, "{short}");
    assert!(short["error"]["message"].as_str().unwrap().contains("short"), "{short}");
    let unknown = mcp
        .call(
            "pcb_route_plan",
            json!({ "session_id": id, "routes": [{ "points": [{ "pad": "U1.1" }, { "pad": "U9.2" }] }] }),
        )
        .await;
    assert_eq!(unknown["ok"], false, "{unknown}");

    // Every net with two pads, pad to pad; keep the ones a simple path can do.
    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let mut routable = Vec::new();
    for (name, net) in nets["result"]["nets"].as_object().unwrap() {
        let pads = net["pads"].as_array().unwrap();
        if pads.len() != 2 {
            continue;
        }
        let route = json!({ "points": [{ "pad": pads[0] }, { "pad": pads[1] }] });
        let one = mcp
            .call("pcb_route_plan", json!({ "session_id": id, "routes": [route.clone()] }))
            .await;
        println!(
            "ROUTE {name}: {}",
            if one["ok"] == true {
                one["result"]["measured"]["routes"][0].to_string()
            } else {
                one["error"]["message"].to_string()
            }
        );
        if one["ok"] == true {
            routable.push(route);
        }
    }
    assert!(!routable.is_empty(), "at least one two-pad net has a simple path");

    // One route through a via: from a pad of the first routable net down on the back and back up.
    let all = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": routable }))
        .await;
    assert_eq!(all["ok"], true, "{all}");
    let tracks: usize = all["result"]["measured"]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["segments"].as_u64().unwrap() as usize)
        .sum();
    let applied = apply_board(&mut mcp, &id, &all, &key("routes")).await;
    println!("ROUTE APPLY {}", applied["result"]["drc"]);
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let drc = &applied["result"]["drc"];
    assert!(
        drc["unrouted_after"].as_u64().unwrap() < drc["unrouted_before"].as_u64().unwrap(),
        "{drc}"
    );
    assert!(
        drc["errors_after"].as_u64().unwrap() <= drc["errors_before"].as_u64().unwrap(),
        "{drc}"
    );

    // A via route on GND: a free spot on the front, down to the back, along, and back up.
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    let o = &info["result"]["outline"];
    let (x0, y0) = (o["min"]["x"].as_f64().unwrap(), o["min"]["y"].as_f64().unwrap());
    let (x1, y1) = (o["max"]["x"].as_f64().unwrap(), o["max"]["y"].as_f64().unwrap());
    let via_route = json!({ "net": "GND", "points": [
        { "x_mm": x1 - 4.0, "y_mm": y0 + 4.0, "layer": "F.Cu" },
        { "x_mm": x1 - 4.0, "y_mm": y0 + 7.0, "layer": "B.Cu" },
        { "x_mm": x1 - 8.0, "y_mm": y0 + 7.0, "layer": "F.Cu" },
        { "x_mm": x1 - 8.0, "y_mm": y0 + 4.0 }
    ] });
    let _ = (x0, y1);
    let plan = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": [via_route] }))
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["measured"]["routes"][0]["vias"], 2, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("via")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let listed = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    let vias = listed["result"]["vias"].as_array().unwrap();
    assert_eq!(vias.len(), 2, "{listed}");
    assert!(
        vias.iter().all(|v| v["diameter"] == 0.6 && v["drill"] == 0.3),
        "{listed}"
    );

    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    mcp.call("pcb_close", json!({ "session_id": id })).await;
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let fid = file["result"]["session_id"].clone();
    let listed = mcp.call("pcb_list_tracks", json!({ "session_id": fid })).await;
    assert_eq!(
        listed["result"]["track_count"].as_u64().unwrap() as usize,
        tracks + 3,
        "{listed}"
    );
}

/// Against a real KiCad 11 PCB editor: every two-pad net that a simple path can reach is routed,
/// one undo step, with DRC before and after. Open a **copy** of a placed board in the PCB Editor,
/// press "KiCad AI: connect" there, then run with THINGSVU_GUI_PCB_ROUTE=1.
#[tokio::test]
async fn gui_pcb_route_demo() {
    if std::env::var("THINGSVU_GUI_PCB_ROUTE").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_ROUTE=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let mut routes = Vec::new();
    for (name, net) in nets["result"]["nets"].as_object().unwrap() {
        let pads = net["pads"].as_array().unwrap();
        if pads.len() != 2 || net["tracks"] != 0 {
            continue;
        }
        let route = json!({ "points": [{ "pad": pads[0] }, { "pad": pads[1] }] });
        let one = mcp
            .call("pcb_route_plan", json!({ "session_id": id, "routes": [route.clone()] }))
            .await;
        println!(
            "GUI ROUTE {name}: {}",
            if one["ok"] == true {
                "clear"
            } else {
                one["error"]["message"].as_str().unwrap_or("")
            }
        );
        if one["ok"] == true {
            routes.push(route);
        }
    }
    assert!(
        !routes.is_empty(),
        "no two-pad net has a clear simple path; place the parts first"
    );
    let plan = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": routes }))
        .await;
    let applied = apply_board(&mut mcp, &id, &plan, &format!("gui-pcb-route-{}", std::process::id())).await;
    println!(
        "GUI ROUTE APPLY verified {} unrouted {} -> {}, errors {} -> {}",
        applied["result"]["verified"],
        applied["result"]["drc"]["unrouted_before"],
        applied["result"]["drc"]["unrouted_after"],
        applied["result"]["drc"]["errors_before"],
        applied["result"]["drc"]["errors_after"]
    );
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: the tracks are on the board; Ctrl+Z undoes them");
}

/// The placed and partly routed regulator board gets a ground pour on both layers: planned,
/// created, refilled, read back, and checked with DRC; a second pour is refused unless it replaces.
#[tokio::test]
async fn live_board_gets_a_ground_pour() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("zone", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-zone-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "corner_radius_mm": 1,
                    "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );

    let plan = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "GND" }] }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    println!("ZONE PLAN {}", plan["result"]["measured"]);
    let zone = &plan["result"]["measured"]["zones"][0];
    assert_eq!(zone["layers"], json!(["F.Cu", "B.Cu"]), "{plan}");
    assert!(
        zone["pads_inside"].as_array().unwrap().len() >= 6,
        "every GND pad is inside: {plan}"
    );
    assert_eq!(plan["result"]["operations"][0]["clearance"], 0.2, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("gnd")).await;
    println!(
        "ZONE APPLY {} {}",
        applied["result"]["report"], applied["result"]["drc"]["after"]["by_type"]
    );
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["report"]["refilled"], true, "{applied}");
    let drc = &applied["result"]["drc"];
    assert!(
        drc["unrouted_after"].as_u64().unwrap() < drc["unrouted_before"].as_u64().unwrap(),
        "the pour connects GND: {drc}"
    );
    let unrouted_gnd = drc["after"]["unrouted_items"].to_string().matches("[GND]").count();
    assert_eq!(unrouted_gnd, 0, "no GND pad is left unconnected: {drc}");

    // A second GND pour is refused unless it replaces the first.
    let again = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "GND", "layers": ["B.Cu"] }] }),
        )
        .await;
    assert_eq!(again["ok"], false, "{again}");
    let replace = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "GND", "layers": ["B.Cu"], "replace": true, "name": "GND back" }] }),
        )
        .await;
    assert_eq!(replace["ok"], true, "{replace}");
    assert_eq!(replace["result"]["operations"][0]["op"], "remove_zone", "{replace}");
    let applied = apply_board(&mut mcp, &id, &replace, &key("replace")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let listed = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    let zones = listed["result"]["zones"].as_array().unwrap();
    assert_eq!(zones.len(), 1, "{listed}");
    assert_eq!(zones[0]["name"], "GND back", "{listed}");
    let bad = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "NOPE" }] }),
        )
        .await;
    assert_eq!(bad["ok"], false, "{bad}");

    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    mcp.call("pcb_close", json!({ "session_id": id })).await;
    let text = std::fs::read_to_string(&pcb).unwrap();
    assert!(
        text.contains("\"GND back\"") && text.contains("filled_polygon"),
        "the pour is saved filled"
    );
    assert!(text.contains("(thermal_gap 0.5)") && text.contains("(min_thickness 0.25)"));
}

/// Against a real KiCad 11 PCB editor: a ground pour over the whole board on every copper layer.
/// Open a **copy** of a placed board in the PCB Editor, press "KiCad AI: connect" there, then run
/// with THINGSVU_GUI_PCB_ZONE=1.
#[tokio::test]
async fn gui_pcb_zone_demo() {
    if std::env::var("THINGSVU_GUI_PCB_ZONE").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_ZONE=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let plan = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "GND", "replace": true }] }),
        )
        .await;
    println!("GUI ZONE PLAN {plan}");
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &format!("gui-pcb-zone-{}", std::process::id())).await;
    println!(
        "GUI ZONE APPLY verified {} unrouted {} -> {}, errors {} -> {}",
        applied["result"]["verified"],
        applied["result"]["drc"]["unrouted_before"],
        applied["result"]["drc"]["unrouted_after"],
        applied["result"]["drc"]["errors_before"],
        applied["result"]["drc"]["errors_after"]
    );
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: GND is poured; Ctrl+Z undoes the fill, a second Ctrl+Z the zone");
}

/// Against a real KiCad 11 PCB editor: the pre-export checks and the exports, from the live board.
/// Open a **copy** of a project in the PCB Editor (a routed board gets its files in `fab/` next to
/// it), press "KiCad AI: connect" there, then run with THINGSVU_GUI_PCB_EXPORT=1. Nothing in the
/// board changes.
#[tokio::test]
async fn gui_pcb_export_demo() {
    if std::env::var("THINGSVU_GUI_PCB_EXPORT").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_EXPORT=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let all = json!(["gerbers", "drill", "positions", "step", "pdf", "svg", "mechanical"]);
    let exported = mcp
        .call("pcb_export", json!({ "session_id": id, "outputs": all }))
        .await;
    println!("GUI EXPORT {}", exported["result_state"]);
    for check in exported["result"]["checks"].as_array().into_iter().flatten() {
        println!("  check {} {}: {}", check["id"], check["status"], check["message"]);
    }
    match exported["result_state"].as_str() {
        Some("done") => {
            println!("GUI EXPORT verification {}", exported["result"]["verification"]);
            assert!(exported["result"]["verification"]
                .as_array()
                .is_some_and(|v| !v.is_empty()));
        }
        Some("rejected") => println!(
            "GUI EXPORT refused by a check; nothing was written: {}",
            exported["result"]["blocking"]
        ),
        other => panic!("unexpected result {other:?}: {exported}"),
    }
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
}

/// The placed regulator board routed by Freerouting: planned (export, route, summary; nothing
/// changes), imported, read back, and checked with DRC. Needs KICAD_LIVE=1 and
/// THINGSVU_FREEROUTING (and THINGSVU_JAVA unless java is on the PATH).
#[tokio::test]
async fn live_board_is_autorouted() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1")
        || (std::env::var("THINGSVU_FREEROUTING").is_err() && std::env::var("THINGSVU_CONFIG").is_err())
    {
        eprintln!("skipped: set KICAD_LIVE=1 and THINGSVU_FREEROUTING (or THINGSVU_CONFIG naming a config.json)");
        return;
    }
    let pcb = board_fixture("autoroute", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-autoroute-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );
    // One track by hand, which the import will replace.
    let route = mcp
        .call(
            "pcb_route_plan",
            json!({ "session_id": id, "routes": [{ "points": [{ "pad": "J1.1" }, { "pad": "F1.1" }] }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &route, &key("route")).await["result"]["verified"],
        true
    );
    let before = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;

    let bad = mcp
        .call("pcb_autoroute_plan", json!({ "session_id": id, "passes": 0 }))
        .await;
    assert_eq!(bad["ok"], false, "{bad}");
    let plan = mcp
        .call(
            "pcb_autoroute_plan",
            json!({ "session_id": id, "passes": 20, "time_limit_s": 180 }),
        )
        .await;
    println!(
        "AUTOROUTE PLAN router {} session {} replaces {} warnings {}",
        plan["result"]["router"],
        plan["result"]["session"]["segments"],
        plan["result"]["replaces"],
        plan["result"]["warnings"]
    );
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["router"]["unrouted"], 0, "{plan}");
    assert!(plan["result"]["session"]["segments"].as_u64().unwrap() > 10, "{plan}");
    // The track drawn by hand is protected in the router's file and written again as it is.
    assert_eq!(
        plan["result"]["replaces"]["tracks"], 0,
        "the hand-made track stays: {plan}"
    );
    assert_eq!(plan["result"]["replaces"]["unlocked_tracks_identical"], 1, "{plan}");
    // The board's minimum goes to the router, so nothing comes back narrower.
    let minimum = &plan["result"]["minimum_width"];
    assert!(minimum["mm"].as_f64().unwrap() > 0.0, "{plan}");
    println!("AUTOROUTE MINIMUM WIDTH {minimum}");
    assert!(
        plan["result"]["session"]["nets"].as_array().unwrap().iter().all(
            |n| n["segments"] == 0 || n["min_width_mm"].as_f64().unwrap() + 1e-6 >= minimum["mm"].as_f64().unwrap()
        ),
        "{plan}"
    );
    // Planning changed nothing.
    let tracks = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    assert_eq!(tracks["result"]["track_count"], 1, "{tracks}");

    let applied = apply_board(&mut mcp, &id, &plan, &key("import")).await;
    println!(
        "AUTOROUTE APPLY verified {} drc {} problems {}",
        applied["result"]["verified"],
        json!({ "unrouted": [applied["result"]["drc"]["unrouted_before"], applied["result"]["drc"]["unrouted_after"]], "errors": [applied["result"]["drc"]["errors_before"], applied["result"]["drc"]["errors_after"]], "by_type": applied["result"]["drc"]["after"]["by_type"] }),
        applied["result"]["problems"]
    );
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["drc"]["unrouted_after"], 0, "{applied}");
    // Counts by type before and after, and only what is new is listed.
    assert!(applied["result"]["drc"]["by_type_after"].is_object(), "{applied}");
    assert!(applied["result"]["drc"]["new_violations_total"].is_u64(), "{applied}");
    assert!(
        applied["result"]["drc"]["after"].get("violations").is_none(),
        "{applied}"
    );
    assert!(applied["result"]["dangling_vias_removed"].is_u64(), "{applied}");
    println!(
        "AUTOROUTE NEW VIOLATIONS {} dangling vias removed {}",
        applied["result"]["drc"]["new_violations_total"], applied["result"]["dangling_vias_removed"]
    );
    let after = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
    assert_eq!(
        before["result"]["footprints"], after["result"]["footprints"],
        "no footprint moved"
    );
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let again = apply_board(&mut mcp, &id, &plan, &key("again")).await;
    assert_eq!(again["ok"], false, "a plan is applied once: {again}");
    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let text = std::fs::read_to_string(&pcb).unwrap();
    assert!(text.matches("(segment").count() > 10, "the routes are saved");
}

/// Phase 14.2: routes can be made locked; a rip-up skips locked tracks unless they are named; a
/// width change and a lock change are read back; a clean-up removes what is left over.
#[tokio::test]
async fn live_tracks_are_edited() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("trackedit", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-trackedit-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );

    // Route every two-pad net a simple path can do; the first one locked.
    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let mut routes = Vec::new();
    for (_, net) in nets["result"]["nets"].as_object().unwrap() {
        let pads = net["pads"].as_array().unwrap();
        if pads.len() != 2 {
            continue;
        }
        let route = json!({ "points": [{ "pad": pads[0] }, { "pad": pads[1] }] });
        let one = mcp
            .call("pcb_route_plan", json!({ "session_id": id, "routes": [route.clone()] }))
            .await;
        if one["ok"] == true {
            routes.push(route);
        }
    }
    assert!(routes.len() >= 2, "two simple routes are needed");
    routes[0]["locked"] = json!(true);
    let plan = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": routes }))
        .await;
    let applied = apply_board(&mut mcp, &id, &plan, &key("route")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let listing = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    let tracks = listing["result"]["tracks"].as_array().unwrap().clone();
    let locked: Vec<&Value> = tracks.iter().filter(|t| t["locked"] == true).collect();
    assert!(!locked.is_empty() && locked.len() < tracks.len(), "{listing}");
    let locked_ids: Vec<Value> = locked.iter().map(|t| t["id"].clone()).collect();

    // Refusals.
    for bad in [
        json!({ "session_id": id }),
        json!({ "session_id": id, "action": "delete" }),
        json!({ "session_id": id, "action": "delete", "select": { "nets": ["NOPE"] } }),
        json!({ "session_id": id, "action": "set_width", "select": { "all": true } }),
        json!({ "session_id": id, "action": "explode", "select": { "all": true } }),
    ] {
        let r = mcp.call("pcb_track_edit_plan", bad.clone()).await;
        assert_eq!(r["ok"], false, "{bad}: {r}");
    }

    // A rip-up of everything skips the locked tracks and says so.
    let rip = mcp
        .call(
            "pcb_track_edit_plan",
            json!({ "session_id": id, "action": "delete", "select": { "all": true } }),
        )
        .await;
    assert_eq!(rip["ok"], true, "{rip}");
    let skipped = rip["result"]["skipped_locked"].as_array().unwrap().len();
    assert_eq!(skipped, locked_ids.len(), "{rip}");
    assert!(!rip["result"]["per_net"].as_object().unwrap().is_empty(), "{rip}");
    let applied = apply_board(&mut mcp, &id, &rip, &key("rip")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert!(
        applied["result"]["drc"]["unrouted_after"].as_u64().unwrap() > 0,
        "{applied}"
    );
    let left = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    let left_ids: Vec<Value> = left["result"]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].clone())
        .collect();
    assert_eq!(left_ids, locked_ids, "only the locked tracks are left: {left}");

    // A width that breaks DRC is rolled back, and the tracks are as they were (the restore puts
    // back an edit made in place, not only what was created or deleted).
    let before_widths = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await["result"]["tracks"].clone();
    let huge = mcp
        .call(
            "pcb_track_edit_plan",
            json!({ "session_id": id, "action": "set_width", "width_mm": 6.0,
                    "select": { "ids": locked_ids, "tracks": true, "vias": false } }),
        )
        .await;
    assert_eq!(huge["ok"], true, "{huge}");
    let rolled = apply_board(&mut mcp, &id, &huge, &key("huge")).await;
    println!(
        "TRACKS ROLLBACK {} {}",
        rolled["result_state"], rolled["result"]["problems"]
    );
    assert_eq!(rolled["result"]["rollback"]["restored"], true, "{rolled}");
    let restored = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await["result"]["tracks"].clone();
    assert_eq!(restored, before_widths, "the widths are back");

    // New width, then unlock, then delete by name.
    let wide = mcp
        .call(
            "pcb_track_edit_plan",
            json!({ "session_id": id, "action": "set_width", "width_mm": 0.4,
                    "select": { "ids": locked_ids, "tracks": true, "vias": false } }),
        )
        .await;
    assert_eq!(wide["ok"], true, "named locked tracks may be re-widened: {wide}");
    assert_eq!(
        apply_board(&mut mcp, &id, &wide, &key("wide")).await["result"]["verified"],
        true
    );
    let after = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    assert!(
        after["result"]["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["width"] == 0.4 && t["locked"] == true),
        "{after}"
    );
    let unlock = mcp
        .call(
            "pcb_track_edit_plan",
            json!({ "session_id": id, "action": "unlock", "select": { "all": true } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &unlock, &key("unlock")).await["result"]["verified"],
        true
    );
    let after = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    assert!(
        after["result"]["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["locked"] == false),
        "{after}"
    );
    // Nothing left to clean: refused with a reason.
    let clean = mcp
        .call(
            "pcb_track_edit_plan",
            json!({ "session_id": id, "cleanup": ["dangling_vias", "zero_length", "duplicates"] }),
        )
        .await;
    assert_eq!(clean["ok"], false, "{clean}");
    assert!(
        clean["error"]["message"].as_str().unwrap().contains("nothing"),
        "{clean}"
    );
    // Everything goes now.
    let rip = mcp
        .call(
            "pcb_track_edit_plan",
            json!({ "session_id": id, "action": "delete", "select": { "all": true } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &rip, &key("rip2")).await["result"]["verified"],
        true
    );
    let none = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    assert_eq!(none["result"]["track_count"], 0, "{none}");
    mcp.call("pcb_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
}

/// Phase 14.3: net classes, assignments, board minimums, and mask/paste margins are read and
/// changed without a file edit: through the API in a live session (saved with `pcb_save`), and in
/// the project file by a file session.
#[tokio::test]
async fn live_design_rules_are_planned_applied_and_read_back() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("rules", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-rules-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();

    let before = mcp.call("pcb_rules", json!({ "session_id": id })).await;
    assert_eq!(before["ok"], true, "{before}");
    assert_eq!(before["result"]["net_classes"][0]["name"], "Default", "{before}");
    assert!(
        before["result"]["stackup"]["layers"].as_array().unwrap().len() >= 4,
        "{before}"
    );

    // Refusals.
    for bad in [
        json!({ "session_id": id }),
        json!({ "session_id": id, "remove_net_classes": ["Default"] }),
        json!({ "session_id": id, "net_classes": [{ "name": "Empty" }] }),
        json!({ "session_id": id, "assign": [{ "net": "NOPE", "class": "Default" }] }),
        json!({ "session_id": id, "assign": [{ "net": "GND", "class": "Ghost" }] }),
        json!({ "session_id": id, "net_classes": [{ "name": "X", "via_diameter_mm": 0.4, "via_drill_mm": 0.5 }] }),
    ] {
        let r = mcp.call("pcb_rules_plan", bad.clone()).await;
        assert_eq!(r["ok"], false, "{bad}: {r}");
    }

    let plan = mcp
        .call(
            "pcb_rules_plan",
            json!({ "session_id": id,
                "net_classes": [{ "name": "Power", "track_width_mm": 0.5, "clearance_mm": 0.25,
                                  "via_diameter_mm": 0.8, "via_drill_mm": 0.4 }],
                "assign": [{ "net": "GND", "class": "Power" }, { "pattern": "/VBUS*", "class": "Power" }],
                "minimums": { "track_width_mm": 0.15, "hole_to_hole_mm": 0.25 },
                "mask_paste": { "mask_expansion_mm": 0.05, "paste_margin_ratio": -0.1 } }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["kind"], "rules");
    assert!(plan["result"]["changes"].as_array().unwrap().len() >= 10, "{plan}");
    // Planning changed nothing.
    let same = mcp.call("pcb_rules", json!({ "session_id": id })).await;
    assert_eq!(same["result"]["net_classes"], before["result"]["net_classes"]);

    let applied = apply_board(&mut mcp, &id, &plan, &key("apply")).await;
    println!("RULES APPLY {}", applied["result"]);
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let after = mcp.call("pcb_rules", json!({ "session_id": id })).await;
    let power = after["result"]["net_classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "Power")
        .expect("the new class")
        .clone();
    assert_eq!(power["values_mm"]["track_width"], 0.5, "{after}");
    assert_eq!(power["values_mm"]["via_diameter"], 0.8, "{after}");
    assert_eq!(power["values_mm"]["via_drill"], 0.4, "{after}");
    assert!(power["nets"].to_string().contains("GND"), "{power}");
    assert!(power["nets"].to_string().contains("/VBUS"), "pattern: {power}");
    assert_eq!(after["result"]["minimums_mm"]["track_width"], 0.15, "{after}");
    assert_eq!(after["result"]["mask_paste_mm"]["paste_margin_ratio"], -0.1, "{after}");

    // Saved with the board.
    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let project = std::fs::read_to_string(std::path::Path::new(&pcb).with_extension("kicad_pro")).unwrap();
    assert!(
        project.contains("\"Power\"") && project.contains("\"min_track_width\": 0.15"),
        "{project}"
    );
    mcp.call("pcb_close", json!({ "session_id": id })).await;

    // A file session changes the project file: a class, an assignment, a minimum; with a backup.
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let fid = opened["result"]["session_id"].clone();
    let read = mcp.call("pcb_rules", json!({ "session_id": fid })).await;
    assert_eq!(read["ok"], true, "{read}");
    assert!(read["result"]["net_classes"].to_string().contains("Power"), "{read}");
    assert!(read["result"]["mask_paste_mm"].is_null(), "{read}");
    let mask = mcp
        .call(
            "pcb_rules_plan",
            json!({ "session_id": fid, "mask_paste": { "mask_expansion_mm": 0.1 } }),
        )
        .await;
    assert_eq!(mask["ok"], false, "mask margins are not in the project file: {mask}");
    let plan = mcp
        .call(
            "pcb_rules_plan",
            json!({ "session_id": fid,
                "net_classes": [{ "name": "Power", "track_width_mm": 0.6 }, { "name": "Signal", "track_width_mm": 0.2 }],
                "assign": [{ "net": "+3V3", "class": "Signal" }],
                "unassign": ["GND"],
                "minimums": { "copper_edge_mm": 0.4 } }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(&mut mcp, &fid, &plan, &key("file")).await;
    println!("RULES FILE APPLY {}", applied["result"]);
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let pro_path = std::path::Path::new(&pcb).with_extension("kicad_pro");
    assert!(
        pro_path.with_extension("kicad_pro.thingsvu-bak").is_file(),
        "a backup is kept"
    );
    let project = std::fs::read_to_string(&pro_path).unwrap();
    assert!(
        project.contains("\"Signal\"") && project.contains("\"min_copper_edge_clearance\": 0.4"),
        "{project}"
    );
    let read = mcp.call("pcb_rules", json!({ "session_id": fid })).await;
    let signal = read["result"]["net_classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "Signal")
        .unwrap()
        .clone();
    assert!(signal["nets"].to_string().contains("+3V3"), "{read}");
    assert!(!read["result"]["assignments"].to_string().contains("GND"), "{read}");

    // An editor that has the board open blocks the change.
    let lock = std::path::Path::new(&pcb).with_file_name(format!(
        "~{}.lck",
        std::path::Path::new(&pcb).file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&lock, "{}").unwrap();
    let plan = mcp
        .call(
            "pcb_rules_plan",
            json!({ "session_id": fid, "minimums": { "copper_edge_mm": 0.3 } }),
        )
        .await;
    let blocked = apply_board(&mut mcp, &fid, &plan, &key("locked")).await;
    assert_eq!(blocked["error"]["code"], "DOCUMENT_OPEN_ELSEWHERE", "{blocked}");
    std::fs::remove_file(&lock).unwrap();
    mcp.call("pcb_close", json!({ "session_id": fid })).await;
}

/// Phase 14.5: a session says when the file on disk changed under it and when it holds unsaved
/// changes; a reload takes the file's version, refusing to drop changes unless told, and listing
/// them; a file session can be opened read-only on a file an editor has open.
#[tokio::test]
async fn live_board_freshness_and_reload() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("fresh", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-fresh-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    assert_eq!(opened["result"]["file_changed_on_disk"], false, "{opened}");
    assert_eq!(opened["result"]["unsaved_changes"], false, "{opened}");
    assert_eq!(opened["result"]["read_only"], false);

    // Another program changes the file: a line on Edge.Cuts.
    let text = std::fs::read_to_string(&pcb).unwrap();
    let cut = text.rfind(')').unwrap();
    let edited = format!(
        "{}\t(gr_line (start 1 1) (end 9 9) (stroke (width 0.1) (type solid)) (layer \"Edge.Cuts\") (uuid \"11111111-1111-4111-8111-111111111111\"))\n)\n",
        &text[..cut]
    );
    std::fs::write(&pcb, edited).unwrap();
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(info["result"]["status"]["file_changed_on_disk"], true, "{info}");
    assert!(
        info["result"]["status"]["freshness_note"]
            .as_str()
            .unwrap()
            .contains("pcb_reload"),
        "{info}"
    );
    let status = mcp.call("session_status", json!({})).await;
    assert_eq!(status["result"]["boards"][0]["file_changed_on_disk"], true, "{status}");
    let stale = mcp.call("pcb_list_drawings", json!({ "session_id": id })).await;
    assert_eq!(
        stale["result"]["shapes_matching"], 0,
        "the session still has the old board: {stale}"
    );

    // A plan made now is dropped by the reload.
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 30, "height_mm": 20, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(outline["ok"], true, "{outline}");
    let reloaded = mcp.call("pcb_reload", json!({ "session_id": id })).await;
    assert_eq!(reloaded["ok"], true, "{reloaded}");
    assert_eq!(reloaded["result"]["file_changed_on_disk"], true, "{reloaded}");
    assert_eq!(reloaded["result"]["had_unsaved_changes"], false, "{reloaded}");
    assert_eq!(reloaded["result"]["counts_after"]["drawings"], 1, "{reloaded}");
    let fresh = mcp.call("pcb_list_drawings", json!({ "session_id": id })).await;
    assert_eq!(fresh["result"]["shapes_matching"], 1, "the file's board: {fresh}");
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(info["result"]["status"]["file_changed_on_disk"], false, "{info}");
    let gone = apply_board(&mut mcp, &id, &outline, &key("old-plan")).await;
    assert_eq!(gone["ok"], false, "a plan from before the reload is gone: {gone}");

    // An unsaved change: the reload refuses and says what it would lose.
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 30, "height_mm": 20, "origin": { "x_mm": 100, "y_mm": 80 },
                    "replace": true, "mounting_holes": "corners" }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(info["result"]["status"]["unsaved_changes"], true, "{info}");
    let refused = mcp.call("pcb_reload", json!({ "session_id": id })).await;
    assert_eq!(refused["ok"], false, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    println!("RELOAD REFUSED: {message}");
    assert!(
        message.contains("footprint") && message.contains("discard_unsaved"),
        "{refused}"
    );
    let drawings_before =
        mcp.call("pcb_list_drawings", json!({ "session_id": id })).await["result"]["shapes_matching"].clone();
    let dropped = mcp
        .call("pcb_reload", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
    assert_eq!(dropped["ok"], true, "{dropped}");
    assert_eq!(dropped["result"]["had_unsaved_changes"], true, "{dropped}");
    assert!(
        !dropped["result"]["dropped"].as_array().unwrap().is_empty(),
        "{dropped}"
    );
    let after = mcp.call("pcb_list_drawings", json!({ "session_id": id })).await;
    assert_eq!(
        drawings_before, 1,
        "the outline replaced the file's line: {drawings_before}"
    );
    assert_eq!(
        after["result"]["shapes"][0]["kind"], "line",
        "the file's line is back, the unsaved outline is gone: {after}"
    );
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(info["result"]["status"]["unsaved_changes"], false, "{info}");
    mcp.call("pcb_close", json!({ "session_id": id })).await;

    // A file session opened read-only works while an editor has the board open, and writes nothing.
    let lock = std::path::Path::new(&pcb).with_file_name(format!(
        "~{}.lck",
        std::path::Path::new(&pcb).file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&lock, "{}").unwrap();
    // A project file for the rules plan below to refuse to change.
    std::fs::write(
        std::path::Path::new(&pcb).with_extension("kicad_pro"),
        r#"{"board":{"design_settings":{"rules":{"min_clearance":0.0}}},"net_settings":{"classes":[{"name":"Default","clearance":0.2,"track_width":0.2,"priority":2147483647}],"meta":{"version":5},"netclass_assignments":{},"netclass_patterns":[]}}"#,
    )
    .unwrap();
    let bad = mcp
        .call(
            "pcb_open",
            json!({ "pcb_path": pcb, "provider": "headless", "read_only": true }),
        )
        .await;
    assert_eq!(bad["ok"], false, "read_only is for the file provider: {bad}");
    let ro = mcp
        .call(
            "pcb_open",
            json!({ "pcb_path": pcb, "provider": "file", "read_only": true }),
        )
        .await;
    assert_eq!(ro["ok"], true, "{ro}");
    assert_eq!(ro["result"]["read_only"], true, "{ro}");
    let rid = ro["result"]["session_id"].clone();
    let read = mcp.call("pcb_list_footprints", json!({ "session_id": rid })).await;
    assert_eq!(read["result"]["count"], 11, "{read}");
    let plan = mcp
        .call(
            "pcb_rules_plan",
            json!({ "session_id": rid, "minimums": { "copper_edge_mm": 0.3 } }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    std::fs::remove_file(&lock).unwrap();
    let blocked = apply_board(&mut mcp, &rid, &plan, &key("ro")).await;
    assert_eq!(blocked["error"]["code"], "READ_ONLY_MODE", "{blocked}");
    let reload = mcp.call("pcb_reload", json!({ "session_id": rid })).await;
    assert_eq!(reload["ok"], true, "a file session reloads trivially: {reload}");
    mcp.call("pcb_close", json!({ "session_id": rid })).await;
}

/// The same for a schematic: a headless session sees the file change and reloads it; a read-only
/// file session opens a schematic an editor has locked and refuses every change.
#[tokio::test]
async fn live_schematic_freshness_reload_and_read_only() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-fresh-sch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": "headless" }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    assert_eq!(opened["result"]["file_changed_on_disk"], false, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let value_of = |list: &Value, reference: &str| -> String {
        list["result"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["reference"] == reference)
            .map(|s| s["value"].as_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let list = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    let c = list["result"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["value"] == "10uF")
        .expect("a 10uF part")["reference"]
        .as_str()
        .unwrap()
        .to_owned();

    // Another program changes the value in the file.
    let text = std::fs::read_to_string(&schematic).unwrap();
    std::fs::write(
        &schematic,
        text.replacen("(property \"Value\" \"10uF\"", "(property \"Value\" \"22uF\"", 1),
    )
    .unwrap();
    let status = mcp.call("session_status", json!({ "session_id": id })).await;
    assert_eq!(status["result"]["file_changed_on_disk"], true, "{status}");
    assert!(
        status["result"]["freshness_note"]
            .as_str()
            .unwrap()
            .contains("session_reload"),
        "{status}"
    );
    let stale = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert_eq!(value_of(&stale, &c), "10uF", "the session still has the old document");

    let reloaded = mcp.call("session_reload", json!({ "session_id": id })).await;
    assert_eq!(reloaded["ok"], true, "{reloaded}");
    assert_eq!(reloaded["result"]["file_changed_on_disk"], true, "{reloaded}");
    let list = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert_eq!(value_of(&list, &c), "22uF", "the file's value: {list}");
    let status = mcp.call("session_status", json!({ "session_id": id })).await;
    assert_eq!(status["result"]["file_changed_on_disk"], false, "{status}");

    // An unsaved change is not dropped without being told.
    let set = mcp
        .call(
            "sch_set_fields_plan",
            json!({ "session_id": id, "edits": [{ "reference": c, "fields": { "MPN": "X1" } }] }),
        )
        .await;
    assert_eq!(set["ok"], true, "{set}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": set["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("fresh-sch-{}", std::process::id()) }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    let refused = mcp.call("session_reload", json!({ "session_id": id })).await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("discard_unsaved"),
        "{refused}"
    );
    let dropped = mcp
        .call("session_reload", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
    assert_eq!(dropped["ok"], true, "{dropped}");
    assert_eq!(dropped["result"]["had_unsaved_changes"], true, "{dropped}");
    mcp.call("session_close", json!({ "session_id": id })).await;

    // Read-only on a locked file.
    let lock = dir.join("~regulator.kicad_sch.lck");
    std::fs::write(&lock, "{}").unwrap();
    let locked = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": "file" }),
        )
        .await;
    assert_eq!(locked["error"]["code"], "DOCUMENT_OPEN_ELSEWHERE", "{locked}");
    assert!(
        locked["error"]["message"].as_str().unwrap().contains("read_only"),
        "{locked}"
    );
    let ro = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": "file", "read_only": true }),
        )
        .await;
    assert_eq!(ro["ok"], true, "{ro}");
    assert_eq!(ro["result"]["read_only"], true, "{ro}");
    let rid = ro["result"]["session_id"].clone();
    let list = mcp.call("sch_list_symbols", json!({ "session_id": rid })).await;
    assert_eq!(value_of(&list, &c), "22uF", "{list}");
    let before = std::fs::read_to_string(&schematic).unwrap();
    let set = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": rid, "symbol": c, "field": "value", "text": "33uF", "confirm": true }),
        )
        .await;
    assert_eq!(set["error"]["code"], "READ_ONLY_MODE", "{set}");
    assert_eq!(
        std::fs::read_to_string(&schematic).unwrap(),
        before,
        "nothing was written"
    );
    let bad = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": "headless", "read_only": true }),
        )
        .await;
    assert_eq!(bad["ok"], false, "{bad}");
    std::fs::remove_file(&lock).unwrap();
    mcp.call("session_close", json!({ "session_id": rid })).await;
}

/// Phase 14.6: a checkpoint keeps the project (including a live board's unsaved changes); a
/// rip-up applied with `checkpoint` can be taken back; the diff says what changed in parts, nets,
/// tracks, and rules; a restore refuses over unsaved changes, takes its own checkpoint first, and
/// reloads the sessions.
#[tokio::test]
async fn live_checkpoints_keep_diff_and_restore() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("checkpoint", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-checkpoint-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );
    // Net rules and a few tracks, saved: the project as the user knows it.
    let rules = mcp
        .call(
            "pcb_rules_plan",
            json!({ "session_id": id, "net_classes": [{ "name": "Power", "track_width_mm": 0.5 }],
                    "assign": [{ "net": "GND", "class": "Power" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &rules, &key("rules")).await["result"]["verified"],
        true
    );
    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let mut routes = Vec::new();
    for (_, net) in nets["result"]["nets"].as_object().unwrap() {
        let pads = net["pads"].as_array().unwrap();
        if pads.len() == 2 {
            let route = json!({ "points": [{ "pad": pads[0] }, { "pad": pads[1] }] });
            let one = mcp
                .call("pcb_route_plan", json!({ "session_id": id, "routes": [route.clone()] }))
                .await;
            if one["ok"] == true {
                routes.push(route);
            }
        }
    }
    let plan = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": routes }))
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &plan, &key("route")).await["result"]["verified"],
        true
    );
    assert_eq!(mcp.call("pcb_save", json!({ "session_id": id })).await["ok"], true);
    let tracks_before = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await["result"]["track_count"]
        .as_u64()
        .unwrap();
    assert!(tracks_before > 2);

    // A checkpoint by hand, and the list.
    let first = mcp
        .call(
            "checkpoint_create",
            json!({ "session_id": id, "label": "routed and saved" }),
        )
        .await;
    assert_eq!(first["ok"], true, "{first}");
    let first_id = first["result"]["checkpoint"]["id"].as_str().unwrap().to_owned();
    assert_eq!(first["result"]["checkpoint"]["unsaved_included"], false, "{first}");
    let files = first["result"]["checkpoint"]["files"].to_string();
    assert!(files.contains("kicad_pcb") && files.contains("kicad_pro"), "{first}");
    let none = mcp.call("checkpoint_create", json!({ "label": "no target" })).await;
    assert_eq!(none["ok"], false, "{none}");
    let bad_label = mcp
        .call("checkpoint_create", json!({ "session_id": id, "label": "x\ny" }))
        .await;
    assert_eq!(bad_label["ok"], false, "{bad_label}");

    // Identical to the project now.
    let same = mcp
        .call("checkpoint_diff", json!({ "session_id": id, "checkpoint": first_id }))
        .await;
    assert_eq!(same["result"]["identical"], true, "{same}");

    // A rip-up with a checkpoint taken by the apply.
    let rip = mcp
        .call(
            "pcb_track_edit_plan",
            json!({ "session_id": id, "action": "delete", "select": { "all": true } }),
        )
        .await;
    assert!(
        rip["result"]["checkpoint"].as_str().unwrap().contains("checkpoint"),
        "{rip}"
    );
    let applied = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": rip["result"]["plan_id"], "confirm": true,
                    "idempotency_key": key("rip"), "checkpoint": "before the rip-up" }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["checkpoint"]["unsaved_included"], false, "{applied}");
    assert_eq!(
        mcp.call("pcb_list_tracks", json!({ "session_id": id })).await["result"]["track_count"],
        0
    );

    // The diff sees the unsaved rip-up (the file on disk still has the tracks); `latest` is the
    // one the apply took, with the tracks in it.
    let latest = mcp
        .call("checkpoint_diff", json!({ "session_id": id, "checkpoint": "latest" }))
        .await;
    assert_eq!(latest["result"]["identical"], false, "{latest}");
    println!("DIFF {}", latest["result"]["differences"]);
    let diff = mcp
        .call("checkpoint_diff", json!({ "session_id": id, "checkpoint": first_id }))
        .await;
    assert_eq!(diff["result"]["identical"], false, "{diff}");
    let board_lines = diff["result"]["differences"]["board"].to_string();
    assert!(board_lines.contains("tracks removed"), "{diff}");
    assert!(
        diff["result"]["differences"]["nets"]
            .to_string()
            .contains("track(s), was"),
        "{diff}"
    );

    // Restore refuses over the unsaved rip-up, and for a ghost id or without confirm.
    let refused = mcp
        .call(
            "checkpoint_restore",
            json!({ "session_id": id, "checkpoint": first_id, "confirm": true }),
        )
        .await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"]["message"].as_str().unwrap().contains("unsaved"),
        "{refused}"
    );
    let unconfirmed = mcp
        .call(
            "checkpoint_restore",
            json!({ "session_id": id, "checkpoint": first_id, "confirm": false }),
        )
        .await;
    assert_eq!(unconfirmed["ok"], false, "{unconfirmed}");
    // Saved, then restored: the tracks are back, the session is reloaded, and the state before
    // the restore is itself a checkpoint.
    assert_eq!(mcp.call("pcb_save", json!({ "session_id": id })).await["ok"], true);
    let ghost = mcp
        .call(
            "checkpoint_restore",
            json!({ "session_id": id, "checkpoint": "nope", "confirm": true }),
        )
        .await;
    assert_eq!(ghost["ok"], false, "{ghost}");
    let restored = mcp
        .call(
            "checkpoint_restore",
            json!({ "session_id": id, "checkpoint": first_id, "confirm": true }),
        )
        .await;
    assert_eq!(restored["ok"], true, "{restored}");
    assert!(
        restored["result"]["files_written"].to_string().contains("kicad_pcb"),
        "{restored}"
    );
    assert_eq!(restored["result"]["sessions_reloaded"], json!([id]), "{restored}");
    let before_id = restored["result"]["checkpoint_of_the_state_before"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        mcp.call("pcb_list_tracks", json!({ "session_id": id })).await["result"]["track_count"],
        tracks_before,
        "the tracks are back in the open session"
    );
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(info["result"]["status"]["unsaved_changes"], false, "{info}");
    let rules = mcp.call("pcb_rules", json!({ "session_id": id })).await;
    assert!(
        rules["result"]["net_classes"].to_string().contains("Power"),
        "the rules are the checkpoint's: {rules}"
    );

    // And the restore can be undone.
    let listing = mcp.call("checkpoint_list", json!({ "session_id": id })).await;
    assert_eq!(listing["result"]["count"], 3, "{listing}");
    let undo = mcp
        .call(
            "checkpoint_restore",
            json!({ "session_id": id, "checkpoint": before_id, "confirm": true }),
        )
        .await;
    assert_eq!(undo["ok"], true, "{undo}");
    assert_eq!(
        mcp.call("pcb_list_tracks", json!({ "session_id": id })).await["result"]["track_count"],
        0,
        "the rip-up is back"
    );

    // An editor that has the files open blocks a restore; with a project path and no session.
    mcp.call("pcb_close", json!({ "session_id": id })).await;
    let lock = std::path::Path::new(&pcb).with_file_name(format!(
        "~{}.lck",
        std::path::Path::new(&pcb).file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&lock, "{}").unwrap();
    let blocked = mcp
        .call(
            "checkpoint_restore",
            json!({ "project_path": pcb, "checkpoint": first_id, "confirm": true }),
        )
        .await;
    assert_eq!(blocked["error"]["code"], "DOCUMENT_OPEN_ELSEWHERE", "{blocked}");
    std::fs::remove_file(&lock).unwrap();
    let by_path = mcp
        .call(
            "checkpoint_diff",
            json!({ "project_path": pcb, "checkpoint": first_id, "against": before_id }),
        )
        .await;
    assert_eq!(by_path["ok"], true, "{by_path}");
    assert_eq!(by_path["result"]["identical"], false, "{by_path}");
}

/// Phase 14.7: symbols are found and described without placing them.
#[tokio::test]
async fn live_symbols_are_searched_and_described() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let mut mcp = Mcp::start().await;
    let started = std::time::Instant::now();
    let found = mcp.call("sym_search", json!({ "query": "ams1117 3.3" })).await;
    println!(
        "SYM SEARCH (cold) {:?}: {}",
        started.elapsed(),
        found["result"]["symbols"][0]
    );
    assert_eq!(found["ok"], true, "{found}");
    let first = &found["result"]["symbols"][0];
    assert_eq!(first["id"], "Regulator_Linear:AMS1117-3.3", "{found}");
    assert_eq!(first["pins"], 3, "{found}");
    assert!(first["footprint"].as_str().unwrap().contains("SOT-223"), "{found}");
    assert!(first["footprint_filters"].as_str().unwrap().contains("SOT"), "{found}");
    // By description and keyword, and narrowed to a library.
    let ldo = mcp
        .call(
            "sym_search",
            json!({ "query": "ldo fixed positive", "library": "Regulator_Linear", "limit": 5 }),
        )
        .await;
    assert!(ldo["result"]["count"].as_u64().unwrap() >= 3, "{ldo}");
    assert!(
        ldo["result"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"].as_str().unwrap().starts_with("Regulator_Linear:")),
        "{ldo}"
    );
    let again = std::time::Instant::now();
    let _ = mcp.call("sym_search", json!({ "query": "usb c receptacle" })).await;
    println!("SYM SEARCH (warm) {:?}", again.elapsed());
    let power = mcp
        .call("sym_search", json!({ "query": "pwr_flag", "library": "power" }))
        .await;
    assert_eq!(power["result"]["symbols"][0]["power"], true, "{power}");
    let none = mcp.call("sym_search", json!({ "query": "zzzzqqqq" })).await;
    assert_eq!(none["result"]["count"], 0, "{none}");

    // One symbol with its pins: a derived symbol has its parent's.
    let ams = mcp
        .call("sym_get", json!({ "id": "Regulator_Linear:AMS1117-3.3" }))
        .await;
    assert_eq!(ams["ok"], true, "{ams}");
    assert_eq!(ams["result"]["pin_count"], 3, "{ams}");
    assert_eq!(ams["result"]["reference_prefix"], "U", "{ams}");
    let names: Vec<&str> = ams["result"]["pins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"VI") && names.contains(&"VO") && names.contains(&"GND"),
        "{ams}"
    );
    assert!(ams["result"]["pins"][0]["electrical_type"].is_string());
    let r = mcp.call("sym_get", json!({ "id": "Device:R" })).await;
    assert_eq!(r["result"]["pin_count"], 2, "{r}");
    assert_eq!(r["result"]["units"], 1);
    let missing = mcp.call("sym_get", json!({ "id": "Device:NoSuchPart" })).await;
    assert_eq!(missing["ok"], false, "{missing}");
    let bad = mcp.call("sym_get", json!({ "id": "R" })).await;
    assert_eq!(bad["ok"], false, "{bad}");
}

/// Phase 14.7: a circuit with a pin left open and a power flag on a supply net; the no-connect flag
/// and the flag part are drawn, read back, and ERC has nothing to say about the open pin.
#[tokio::test]
async fn live_circuit_no_connects_and_power_flags_through_the_file_provider() {
    circuit_flags_flow("file").await;
}

#[tokio::test]
async fn live_circuit_no_connects_and_power_flags_through_the_api() {
    circuit_flags_flow("headless").await;
}

async fn circuit_flags_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-ncflags-{provider}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("flags.kicad_sch");
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "create": true, "provider": provider }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let circuit = json!({ "schema_version": 1, "name": "flags",
        "components": [
            { "id": "r1", "library_id": "Device:R", "value": "1k", "reference_preference": "R1" },
            { "id": "r2", "library_id": "Device:R", "value": "2k", "reference_preference": "R2" } ],
        "nets": [
            { "name": "+5V", "pins": [ { "component_id": "r1", "pin_number": "1" } ] },
            { "name": "MID", "pins": [ { "component_id": "r1", "pin_number": "2" }, { "component_id": "r2", "pin_number": "1" } ] },
            { "name": "GND", "pins": [ { "component_id": "r2", "pin_number": "2" } ] } ],
        "power_flags": [ "+5V", "GND" ] });
    let with_open_pin = |mut c: Value| {
        // R2's pin 2 is left open instead of on GND.
        c["nets"][2]["pins"] = json!([]);
        c["nets"].as_array_mut().unwrap().pop();
        c["no_connects"] = json!([{ "component_id": "r2", "pin_number": "2" }]);
        c["power_flags"] = json!(["+5V"]);
        c
    };
    // Mistakes are refused with the reason.
    let mut bad = with_open_pin(circuit.clone());
    bad["no_connects"] = json!([{ "component_id": "r1", "pin_number": "1" }]);
    let refused = mcp
        .call("sch_plan_changes", json!({ "session_id": id, "circuit": bad }))
        .await;
    assert!(refused.to_string().contains("NO_CONNECT_PIN_IN_NET"), "{refused}");
    let mut ghost = circuit.clone();
    ghost["power_flags"] = json!(["+9V"]);
    let refused = mcp
        .call("sch_plan_changes", json!({ "session_id": id, "circuit": ghost }))
        .await;
    assert!(refused.to_string().contains("POWER_FLAG_UNKNOWN_NET"), "{refused}");

    let plan = mcp
        .call(
            "sch_plan_changes",
            json!({ "session_id": id, "circuit": with_open_pin(circuit) }),
        )
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    assert_eq!(plan["result"]["counts"]["no_connects"], 1, "{plan}");
    assert!(plan.to_string().contains("add_no_connect"), "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("ncflags-{provider}-{}", std::process::id()) }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert!(
        symbols.to_string().contains("PWR_FLAG"),
        "the flag is a part: {symbols}"
    );
    let text = std::fs::read_to_string(&schematic).unwrap();
    assert_eq!(
        text.matches("(no_connect").count(),
        1,
        "one no-connect flag in the file"
    );
    // ERC: the open pin is marked, so it is not reported as unconnected.
    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    // The open pin is marked, and the flag is joined to its net (a junction where the flag sits
    // on the wire: without it KiCad 10 reads the flag as unconnected and the supply as undriven).
    println!("ERC ({provider}) {}", erc["result"]["violations"]);
    assert_eq!(erc["ok"], true, "{erc}");
    assert_eq!(erc["result"]["violations"], json!([]), "a clean sheet: {erc}");
    mcp.call("session_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
}

/// Phase 14.7: a pin moves to another net and two pins swap nets on a drawn sheet, changing only
/// those pins' wiring, and the nets are read back from KiCad.
#[tokio::test]
async fn live_pins_change_nets_through_the_file_provider() {
    pin_net_flow("file").await;
}

#[tokio::test]
async fn live_pins_change_nets_through_the_api() {
    pin_net_flow("headless").await;
}

async fn pin_net_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-pinnet-{provider}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "create": true, "provider": provider }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let key = |n: &str| format!("pinnet-{provider}-{n}-{}", std::process::id());
    let circuit: Value = serde_json::from_str(include_str!("../../../docs/examples/regulator-3v3.json")).unwrap();
    let plan = mcp
        .call("sch_plan_changes", json!({ "session_id": id, "circuit": circuit }))
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true, "idempotency_key": key("build") }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    // Changes that remove drawing items need a saved document (so a failed one can be reverted).
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let net_of = |nets: &Value, pin: &str| -> String {
        let (reference, number) = pin.split_once('.').unwrap();
        nets["result"]["nets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| {
                n["pins"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["reference"] == reference && p["pin"] == number)
            })
            .map(|n| n["name"].as_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert!(net_of(&nets, "C5.1").contains("3V3"), "{nets}");
    let wires_before = std::fs::read_to_string(&schematic)
        .map(|t| t.matches("(wire").count())
        .unwrap_or(0);

    // Refusals.
    for bad in [
        json!({ "session_id": id }),
        json!({ "session_id": id, "move": { "pin": "C5.1", "to_net": "+3V3" } }),
        json!({ "session_id": id, "move": { "pin": "C9.1", "to_net": "GND" } }),
        json!({ "session_id": id, "move": { "pin": "C5.9", "to_net": "GND" } }),
        json!({ "session_id": id, "swap": ["C3.1", "C4.1"] }),
        json!({ "session_id": id, "swap": ["C3.1"] }),
    ] {
        let r = mcp.call("sch_pin_net_plan", bad.clone()).await;
        assert_eq!(r["ok"], false, "{bad}: {r}");
    }

    // C5.1 moves from +3V3 to +5V.
    let moved = mcp
        .call(
            "sch_pin_net_plan",
            json!({ "session_id": id, "move": { "pin": "C5.1", "to_net": "+5V" } }),
        )
        .await;
    assert_eq!(moved["ok"], true, "{moved}");
    let (removals, ops) = (
        moved["result"]["removals"].as_array().map_or(0, Vec::len),
        moved["result"]["operations"].as_array().map_or(0, Vec::len),
    );
    println!("PIN MOVE ({provider}): {removals} removals, {ops} additions");
    assert!(removals + ops <= 8, "only that pin's drawing changes: {moved}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": moved["result"]["plan_id"], "confirm": true, "idempotency_key": key("move") }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(
        applied["result"]["verified"], true,
        "KiCad reads the nets as planned: {applied}"
    );
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert!(net_of(&nets, "C5.1").contains("5V"), "{nets}");
    assert!(
        net_of(&nets, "C3.1").contains("3V3"),
        "the rest of +3V3 is as it was: {nets}"
    );

    // Two pins swap: C5.1 (now +5V) and C3.1 (+3V3).
    let swapped = mcp
        .call(
            "sch_pin_net_plan",
            json!({ "session_id": id, "swap": ["C5.1", "C3.1"] }),
        )
        .await;
    assert_eq!(swapped["ok"], true, "{swapped}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": swapped["result"]["plan_id"], "confirm": true, "idempotency_key": key("swap") }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert!(net_of(&nets, "C5.1").contains("3V3"), "{nets}");
    assert!(net_of(&nets, "C3.1").contains("5V"), "{nets}");
    if provider == "file" {
        let wires_after = std::fs::read_to_string(&schematic).unwrap().matches("(wire").count();
        println!("WIRES {wires_before} -> {wires_after}");
        assert!(
            wires_after.abs_diff(wires_before) <= 6,
            "the sheet was not redrawn: {wires_before} -> {wires_after}"
        );
    }
    mcp.call("session_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
}

/// Phase 14.8: the autorouter is told which nets to route and what it may rip up; tracks already on
/// the board stay (a locked one stays locked and is not doubled); a net class gives a net its width.
/// Needs Freerouting.
#[tokio::test]
async fn live_board_autoroute_is_controlled() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") || !freerouting_configured() {
        eprintln!("skipped: set KICAD_LIVE=1 and set up Freerouting");
        return;
    }
    let pcb = board_fixture("autoctl", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-autoctl-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );
    // One track by hand, locked: it must stay, and stay locked, and not be drawn twice.
    let route = mcp
        .call(
            "pcb_route_plan",
            json!({ "session_id": id, "routes": [{ "points": [{ "pad": "J1.1" }, { "pad": "F1.1" }], "locked": true }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &route, &key("route")).await["result"]["verified"],
        true
    );
    let tracks_of = |list: &Value| -> Vec<Value> { list["result"]["tracks"].as_array().unwrap().clone() };
    let nets_with_tracks = |tracks: &[Value]| -> std::collections::BTreeSet<String> {
        tracks.iter().map(|t| t["net"].as_str().unwrap().to_owned()).collect()
    };
    let list = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    let hand = tracks_of(&list);
    assert_eq!(hand.len(), 1, "{list}");
    assert_eq!(hand[0]["locked"], true);

    // Refusals.
    for bad in [
        json!({ "session_id": id, "nets": ["NOPE"] }),
        json!({ "session_id": id, "exclude_nets": ["NOPE"] }),
        json!({ "session_id": id, "nets": ["GND"], "exclude_nets": ["GND"] }),
    ] {
        let r = mcp.call("pcb_autoroute_plan", bad.clone()).await;
        assert_eq!(r["ok"], false, "{bad}: {r}");
    }

    // 1. Only +3V3.
    let plan = mcp
        .call(
            "pcb_autoroute_plan",
            json!({ "session_id": id, "nets": ["+3V3"], "passes": 20, "time_limit_s": 180 }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["routes"]["nets"], json!(["+3V3"]), "{plan}");
    assert!(
        plan["result"]["routes"]["left_alone"].to_string().contains("GND"),
        "{plan}"
    );
    assert_eq!(plan["result"]["routes"]["keep_existing"], true);
    assert_eq!(
        plan["result"]["replaces"]["tracks"], 0,
        "the hand track is written again as it is: {plan}"
    );
    assert_eq!(plan["result"]["replaces"]["locked_tracks_kept"], 1, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("only-3v3")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let after = tracks_of(&mcp.call("pcb_list_tracks", json!({ "session_id": id })).await);
    let nets = nets_with_tracks(&after);
    println!("ONLY +3V3: tracks on {nets:?}");
    assert_eq!(
        nets,
        ["+3V3".to_owned(), "/VBUS".to_owned()].into(),
        "only the asked net and the hand track"
    );
    // The locked hand track is still there, still locked, and there is one of it.
    let vbus: Vec<&Value> = after.iter().filter(|t| t["net"] == "/VBUS").collect();
    assert_eq!(vbus.len(), 1, "not doubled: {after:?}");
    assert_eq!(vbus[0]["locked"], true);
    let tracks_3v3 = after.iter().filter(|t| t["net"] == "+3V3").count();

    // 2. Everything but GND: the +3V3 tracks are kept, GND stays bare.
    let plan = mcp
        .call(
            "pcb_autoroute_plan",
            json!({ "session_id": id, "exclude_nets": ["GND"], "passes": 20, "time_limit_s": 180 }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert!(!plan["result"]["routes"]["nets"].to_string().contains("GND"), "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("not-gnd")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let after = tracks_of(&mcp.call("pcb_list_tracks", json!({ "session_id": id })).await);
    assert!(!nets_with_tracks(&after).contains("GND"), "GND was left alone");
    assert_eq!(
        after.iter().filter(|t| t["net"] == "+3V3").count(),
        tracks_3v3,
        "the +3V3 tracks were kept"
    );
    assert!(
        applied["result"]["drc"]["unrouted_after"].as_u64().unwrap() > 0,
        "GND is not connected: {applied}"
    );

    // 3. Ripping up +3V3 and routing it again: the plan says what it rips up.
    let plan = mcp
        .call(
            "pcb_autoroute_plan",
            json!({ "session_id": id, "nets": ["+3V3"], "keep_existing": false, "passes": 20, "time_limit_s": 180 }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["routes"]["keep_existing"], false);
    assert_eq!(plan["result"]["routes"]["ripped_up"]["tracks"], tracks_3v3, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("rip-3v3")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let after = tracks_of(&mcp.call("pcb_list_tracks", json!({ "session_id": id })).await);
    assert!(nets_with_tracks(&after).contains("+3V3"));
    assert_eq!(
        after
            .iter()
            .filter(|t| t["net"] == "/VBUS" && t["locked"] == true)
            .count(),
        1,
        "the locked track stayed"
    );

    // 4. A net class gives GND its width, and the router uses it.
    let rules = mcp
        .call(
            "pcb_rules_plan",
            json!({ "session_id": id, "net_classes": [{ "name": "Power", "track_width_mm": 0.5 }],
                    "assign": [{ "net": "GND", "class": "Power" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &rules, &key("rules")).await["result"]["verified"],
        true
    );
    let plan = mcp
        .call(
            "pcb_autoroute_plan",
            json!({ "session_id": id, "nets": ["GND"], "passes": 20, "time_limit_s": 180 }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    let gnd = plan["result"]["session"]["nets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["net"] == "GND")
        .expect("GND in the session")
        .clone();
    println!("GND in the session: {gnd}");
    assert!(
        gnd["min_width_mm"].as_f64().unwrap() >= 0.5 - 1e-6,
        "every GND track is as wide as its net class says: {gnd}"
    );
    let applied = apply_board(&mut mcp, &id, &plan, &key("gnd")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    mcp.call("pcb_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
}

/// Against a real KiCad 11 PCB editor: the board routed by Freerouting, after a plan that shows
/// the router's result. Open a **copy** of a placed board in the PCB Editor, press "KiCad AI:
/// connect" there, then run with THINGSVU_GUI_PCB_AUTOROUTE=1 and THINGSVU_FREEROUTING.
#[tokio::test]
async fn gui_pcb_autoroute_demo() {
    if std::env::var("THINGSVU_GUI_PCB_AUTOROUTE").as_deref() != Ok("1") {
        eprintln!(
            "skipped: set THINGSVU_GUI_PCB_AUTOROUTE=1 and THINGSVU_FREEROUTING with a registered KiCad PCB editor"
        );
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let plan = mcp.call("pcb_autoroute_plan", json!({ "session_id": id })).await;
    println!(
        "GUI AUTOROUTE PLAN router {} session {} replaces {} warnings {}",
        plan["result"]["router"]["unrouted"],
        plan["result"]["session"]["segments"],
        plan["result"]["replaces"],
        plan["result"]["warnings"]
    );
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(
        &mut mcp,
        &id,
        &plan,
        &format!("gui-pcb-autoroute-{}", std::process::id()),
    )
    .await;
    println!(
        "GUI AUTOROUTE APPLY verified {} unrouted {} -> {}, errors {} -> {}, problems {}",
        applied["result"]["verified"],
        applied["result"]["drc"]["unrouted_before"],
        applied["result"]["drc"]["unrouted_after"],
        applied["result"]["drc"]["errors_before"],
        applied["result"]["drc"]["errors_after"],
        applied["result"]["problems"]
    );
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: the board is routed; Ctrl+Z undoes KiCad's import");
}

/// A change that makes DRC worse is rolled back, and the board read again is as it was; with
/// keep_if_unverified it stays. Also: a track that would pass inside the board's hole clearance is
/// refused at plan time.
#[tokio::test]
async fn live_board_change_that_fails_drc_is_rolled_back() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("rollback", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-rollback-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    // An outline fitted around the parts, with a hole beside them.
    let fitted = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "margin_mm": 8, "hole_positions": [{ "x_mm": -4.5, "y_mm": 7 }] }),
        )
        .await;
    assert_eq!(fitted["ok"], true, "{fitted}");
    let applied = apply_board(&mut mcp, &id, &fitted, &key("fitted")).await;
    assert_eq!(applied["result"]["verified"], true, "{}", applied["result"]["problems"]);

    // 0.21 mm from the hole's edge: clear of the 0.2 mm net class, inside the 0.25 mm hole clearance.
    let list = mcp
        .call("pcb_list_footprints", json!({ "session_id": id, "include_pads": true }))
        .await;
    let edge = footprint(&list, "H1")["pads"][0]["copper"]["max"]["y"]
        .as_f64()
        .unwrap();
    let y = ((edge + 0.21 + 0.1) * 1000.0).round() / 1000.0;
    let near_hole = mcp
        .call(
            "pcb_route_plan",
            json!({ "session_id": id, "routes": [{ "net": "GND", "points": [{ "x_mm": -6.5, "y_mm": y, "layer": "B.Cu" }, { "x_mm": -2.5, "y_mm": y }] }] }),
        )
        .await;
    assert_eq!(near_hole["ok"], false, "{near_hole}");
    assert!(
        near_hole["error"]["message"].as_str().unwrap().contains("H1 (hole)"),
        "{near_hole}"
    );

    // An outline far too small: its plan warns, DRC after it is worse, so it is rolled back.
    let before = mcp.call("pcb_info", json!({ "session_id": id })).await["result"]["outline"].clone();
    let small = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "replace": true, "width_mm": 8, "height_mm": 6 }),
        )
        .await;
    assert_eq!(small["ok"], true, "{small}");
    assert!(
        small["result"]["warnings"]
            .to_string()
            .contains("outside the new outline"),
        "{small}"
    );
    let applied = apply_board(&mut mcp, &id, &small, &key("small")).await;
    println!("ROLLBACK {} {}", applied["result_state"], applied["result"]["rollback"]);
    println!("ROLLBACK PROBLEMS {}", applied["result"]["problems"]);
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result_state"], "rejected", "{applied}");
    assert!(applied["result"]["problems"].to_string().contains("DRC:"), "{applied}");
    assert_eq!(applied["result"]["rollback"]["restored"], true, "{applied}");
    assert_eq!(
        applied["result"]["rollback"]["restore"]["recreated"], 1,
        "the old outline: {applied}"
    );
    assert_eq!(applied["result"]["applied"], false, "{applied}");
    let after = mcp.call("pcb_info", json!({ "session_id": id })).await["result"]["outline"].clone();
    assert_eq!(after, before, "the fitted outline is back");

    // Kept on request.
    let small = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "replace": true, "width_mm": 8, "height_mm": 6 }),
        )
        .await;
    let kept = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": small["result"]["plan_id"], "confirm": true,
                    "idempotency_key": key("keep"), "keep_if_unverified": true }),
        )
        .await;
    assert_eq!(kept["result_state"], "partial", "{kept}");
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert!(
        (info["result"]["outline"]["width"].as_f64().unwrap() - 8.05).abs() < 0.02,
        "{info}"
    );
}

/// Whether an autorouter is set up for the server (an environment variable or the installer's
/// settings file).
fn freerouting_configured() -> bool {
    std::env::var_os("THINGSVU_FREEROUTING").is_some()
        || std::env::var_os("THINGSVU_CONFIG").is_some()
        || kicad_session::registry::config_file().is_file()
}

/// Phase 14.1: the `fields` of a planned part are written to the placed symbol (hidden) and read
/// back; a field that cannot be written is refused, never dropped. Both providers.
#[tokio::test]
async fn live_circuit_plan_fields_are_written_through_the_file_provider() {
    plan_fields_flow("file").await;
}

#[tokio::test]
async fn live_circuit_plan_fields_are_written_through_the_api() {
    plan_fields_flow("headless").await;
}

async fn plan_fields_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-fields-{provider}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("fields.kicad_sch");
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "create": true, "provider": provider }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let circuit = |fields: Value| {
        json!({ "schema_version": 1, "name": "fields",
            "components": [
                { "id": "r1", "library_id": "Device:R", "value": "10k", "reference_preference": "R1", "fields": fields },
                { "id": "r2", "library_id": "Device:R", "value": "4k7", "reference_preference": "R2" } ],
            "nets": [ { "name": "MID", "pins": [
                { "component_id": "r1", "pin_number": "2" }, { "component_id": "r2", "pin_number": "1" } ] } ] })
    };
    // A field that cannot be written is refused.
    for name in ["Reference", "Value", "DNP"] {
        let refused = mcp
            .call(
                "sch_plan_changes",
                json!({ "session_id": id, "circuit": circuit(json!({ name: "x" })) }),
            )
            .await;
        assert_eq!(refused["result"]["can_apply"], false, "{name}: {refused}");
        assert!(refused.to_string().contains("UNWRITABLE_FIELD"), "{name}: {refused}");
    }
    let plan = mcp
        .call(
            "sch_plan_changes",
            json!({ "session_id": id, "circuit": circuit(json!({ "MPN": "RC0603FR-0710KL", "Manufacturer": "Yageo" })) }),
        )
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    assert!(
        plan.to_string().contains("RC0603FR-0710KL"),
        "the plan shows the field: {plan}"
    );
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("fields-{provider}-{}", std::process::id()) }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let bom = mcp.call("bom_get", json!({ "session_id": id })).await;
    let custom = bom["result"]["custom_fields"].to_string();
    assert!(custom.contains("MPN") && custom.contains("Manufacturer"), "{bom}");
    assert!(bom.to_string().contains("RC0603FR-0710KL"), "{bom}");
    mcp.call("session_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
}

/// Holes made by v0.7.0 had no attributes: the review and the export checks warn, and
/// `pcb_footprint_attrs_plan` repairs them, read back from the board.
#[tokio::test]
async fn live_board_old_holes_are_repaired() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("holes", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-holes-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 44, "height_mm": 34, "origin": { "x_mm": 100, "y_mm": 80 },
                    "mounting_holes": "corners", "hole_inset_mm": 3.5 }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    // The outline is a drawing on Edge.Cuts: a rectangle 44 x 34 mm at the origin asked for.
    let drawings = mcp
        .call(
            "pcb_list_drawings",
            json!({ "session_id": id, "layers": ["Edge.Cuts"], "kinds": ["rect"] }),
        )
        .await;
    assert_eq!(drawings["result"]["shapes_matching"], 1, "{drawings}");
    let edge = &drawings["result"]["shapes"][0];
    let (a, b) = (&edge["points"][0], &edge["points"][1]);
    let (w, h) = (
        (b["x"].as_f64().unwrap() - a["x"].as_f64().unwrap()).abs(),
        (b["y"].as_f64().unwrap() - a["y"].as_f64().unwrap()).abs(),
    );
    assert!((w - 44.0).abs() < 0.2 && (h - 34.0).abs() < 0.2, "{edge}");
    assert!(
        drawings["result"]["counts_by_layer"]["Edge.Cuts"].as_u64().unwrap() >= 1,
        "{drawings}"
    );
    let none = mcp
        .call(
            "pcb_list_drawings",
            json!({ "session_id": id, "layers": ["F.SilkS"], "kinds": ["text"] }),
        )
        .await;
    assert_eq!(none["result"]["texts_matching"], 0, "{none}");
    let codes = |review: &Value| -> Vec<String> {
        review["result"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["code"].as_str().unwrap().to_owned())
            .collect()
    };
    // New holes carry the attributes, so nothing is reported.
    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    assert!(!codes(&review).contains(&"HOLE_LISTED_AS_PART".to_owned()), "{review}");

    // Unrouted connections come with the pointer to the autorouter.
    let drc = mcp.call("pcb_run_drc", json!({ "session_id": id })).await;
    assert!(drc["result"]["unrouted"].as_u64().unwrap() > 0, "{drc}");
    let hint = drc["result"]["hint"].as_str().unwrap();
    assert!(hint.contains("pcb_autoroute_plan"), "{drc}");
    // A summary has the counts and no list; types and a limit narrow the list (the counts stay).
    let summary = mcp
        .call("pcb_run_drc", json!({ "session_id": id, "summary_only": true }))
        .await;
    assert!(summary["result"].get("violations").is_none(), "{summary}");
    assert_eq!(summary["result"]["unrouted"], drc["result"]["unrouted"], "{summary}");
    assert_eq!(summary["result"]["by_type"], drc["result"]["by_type"], "{summary}");
    let narrow = mcp
        .call(
            "pcb_run_drc",
            json!({ "session_id": id, "types": ["no_such_type"], "limit": 1, "refill_zones": false }),
        )
        .await;
    assert_eq!(narrow["result"]["violations"], json!([]), "{narrow}");
    assert_eq!(narrow["result"]["by_type"], drc["result"]["by_type"], "{narrow}");

    // Make them like v0.7.0 made them.
    let old: Vec<Value> = ["H1", "H2", "H3", "H4"]
        .iter()
        .map(|r| json!({ "reference": r, "exclude_from_bom": false, "exclude_from_pos": false, "board_only": false }))
        .collect();
    let undo = mcp
        .call(
            "pcb_footprint_attrs_plan",
            json!({ "session_id": id, "footprints": old }),
        )
        .await;
    assert_eq!(undo["result"]["kind"], "attributes", "{undo}");
    assert_eq!(
        apply_board(&mut mcp, &id, &undo, &key("old")).await["result"]["verified"],
        true
    );
    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    assert!(codes(&review).contains(&"HOLE_LISTED_AS_PART".to_owned()), "{review}");
    let exported = mcp
        .call("pcb_export", json!({ "session_id": id, "outputs": ["positions"] }))
        .await;
    let checks = exported["result"]["checks"].as_array().unwrap();
    let holes = checks.iter().find(|c| c["id"] == "holes_marked").unwrap();
    assert_eq!(holes["status"], "warning", "{exported}");
    assert_eq!(holes["items"], json!(["H1", "H2", "H3", "H4"]));

    // The repair: every hole at once; a second plan has nothing to do.
    let fix = mcp
        .call(
            "pcb_footprint_attrs_plan",
            json!({ "session_id": id, "mounting_holes": true }),
        )
        .await;
    assert_eq!(fix["result"]["operations"].as_array().unwrap().len(), 4, "{fix}");
    let applied = apply_board(&mut mcp, &id, &fix, &key("fix")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(
        applied["result"]["report"]["attributes_set"].as_array().unwrap().len(),
        4
    );
    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    assert!(!codes(&review).contains(&"HOLE_LISTED_AS_PART".to_owned()), "{review}");
    let again = mcp
        .call(
            "pcb_footprint_attrs_plan",
            json!({ "session_id": id, "mounting_holes": true }),
        )
        .await;
    assert_eq!(again["ok"], false, "{again}");
    mcp.call("pcb_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
}

/// Phase 13: an outlined, placed board is refused for export while it is unrouted, exported when
/// the user accepts that, read as a mechanical model, and, once autorouted (when Freerouting is
/// set up), packaged for JLCPCB with every check passing.
#[tokio::test]
async fn live_board_is_exported() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("export", false);
    let fab = pcb.parent().unwrap().join("fab");
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-export-{n}-{}", std::process::id());

    // The file as it is (no outline, nothing routed), through the file provider: refused.
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let refused = mcp
        .call("pcb_export", json!({ "session_id": file["result"]["session_id"] }))
        .await;
    assert_eq!(refused["result_state"], "rejected", "{refused}");
    assert_eq!(
        refused["result"]["blocking"],
        json!(["outline_closed", "drc"]),
        "{refused}"
    );
    mcp.call("pcb_close", json!({ "session_id": file["result"]["session_id"] }))
        .await;

    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 44, "height_mm": 34, "origin": { "x_mm": 100, "y_mm": 80 },
                    "mounting_holes": "corners", "hole_inset_mm": 3.5 }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );

    // Unrouted: refused, nothing written.
    let refused = mcp.call("pcb_export", json!({ "session_id": id })).await;
    println!("EXPORT REFUSED {}", refused["result"]["checks"]);
    assert_eq!(refused["result_state"], "rejected", "{refused}");
    assert_eq!(refused["result"]["blocking"], json!(["drc"]), "{refused}");
    let checks = refused["result"]["checks"].as_array().unwrap();
    let status = |id: &str| checks.iter().find(|c| c["id"] == id).unwrap()["status"].clone();
    assert_eq!(status("outline_closed"), "pass");
    assert_eq!(status("bom_lines"), "pass", "the holes need no BOM line: {refused}");
    assert!(!fab.exists());

    // Accepted by the user: everything written and read back.
    let all = json!(["gerbers", "drill", "positions", "step", "pdf", "svg", "mechanical"]);
    let exported = mcp
        .call(
            "pcb_export",
            json!({ "session_id": id, "outputs": all, "accept_problems": true,
                    "acceptance_note": "test: exporting before routing to look at the plots" }),
        )
        .await;
    println!("EXPORT {}", exported["result"]["verification"]);
    assert_eq!(exported["result_state"], "done", "{exported}");
    assert_eq!(exported["result"]["accepted_problems"], true);
    let verification = exported["result"]["verification"].as_array().unwrap();
    assert!(verification.iter().all(|v| v["ok"] == true), "{exported}");
    let drill = verification.iter().find(|v| v["what"] == "drill holes").unwrap();
    assert!(
        drill["detail"].as_str().unwrap().starts_with("6 holes"),
        "J1's two pins and four mounting holes: {drill}"
    );
    let positions = verification.iter().find(|v| v["what"] == "positions").unwrap();
    assert!(
        positions["detail"].as_str().unwrap().starts_with("11 parts"),
        "the mounting holes are not placed by the assembler: {positions}"
    );
    let gerbers: Vec<String> = std::fs::read_dir(fab.join("gerbers"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        gerbers.len(),
        12,
        "nine layers, the job file, two drill files: {gerbers:?}"
    );
    for f in [
        "assembly/regulator-pos.csv",
        "3d/regulator.step",
        "3d/regulator-mechanical.json",
        "plots/regulator.pdf",
    ] {
        assert!(fab.join(f).is_file(), "{f}");
    }
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(fab.join("regulator-checks.json")).unwrap()).unwrap();
    assert_eq!(
        report["accepted_by_user"]["note"],
        "test: exporting before routing to look at the plots"
    );

    let mech = mcp.call("pcb_mechanical_info", json!({ "session_id": id })).await;
    println!("MECHANICAL {}", mech["result"]["connectors"]);
    assert_eq!(mech["ok"], true, "{mech}");
    let m = &mech["result"];
    assert_eq!(m["format"], "thingsvu.mechanical/1");
    assert_eq!(m["board"]["thickness_mm"], 1.6);
    assert_eq!(m["board"]["outline_closed"], true);
    assert_eq!(m["mounting_holes"].as_array().unwrap().len(), 4, "{m}");
    assert_eq!(m["connectors"][0]["reference"], "J1");
    assert_eq!(m["connectors"][0]["edge"], "left", "{m}");
    assert!(
        m["step_path"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("fab/3d/regulator.step"),
        "{m}"
    );
    assert_eq!(m["components"].as_array().unwrap().len(), 11);

    if !freerouting_configured() {
        eprintln!("the package part needs Freerouting (install.ps1 -FreeroutingOnly)");
        return;
    }
    let plan = mcp
        .call(
            "pcb_autoroute_plan",
            json!({ "session_id": id, "passes": 20, "time_limit_s": 180 }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    println!(
        "EXPORT AUTOROUTE router {} minimum {} warnings {}",
        plan["result"]["router"], plan["result"]["minimum_width"], plan["result"]["warnings"]
    );
    let routed = apply_board(&mut mcp, &id, &plan, &key("route")).await;
    assert_eq!(routed["result"]["drc"]["unrouted_after"], 0, "{routed}");

    // Routed: every blocking check passes, and the JLCPCB package is made without accepting anything.
    let package = mcp.call("pcb_fab_package", json!({ "session_id": id })).await;
    println!(
        "PACKAGE {} {}",
        package["result"]["checks"], package["result"]["verification"]
    );
    assert_eq!(package["result_state"], "done", "{package}");
    assert_eq!(package["result"]["accepted_problems"], false);
    let zip = fab.join("regulator-fab.zip");
    assert!(zip.is_file());
    let cpl = std::fs::read_to_string(fab.join("regulator-cpl.csv")).unwrap();
    assert!(
        cpl.starts_with("Designator,Val,Package,Mid X,Mid Y,Rotation,Layer"),
        "{cpl}"
    );
    let bom = std::fs::read_to_string(fab.join("regulator-bom.csv")).unwrap();
    assert!(
        bom.starts_with("\"Comment\",\"Designator\",\"Footprint\",\"LCSC Part #\""),
        "{bom}"
    );
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(fab.join("regulator-checks.json")).unwrap()).unwrap();
    assert_eq!(report["accepted_by_user"], Value::Null);
}

/// Phase 14.9: the steps of the ESP32-S3 session that were once done by editing `.kicad_*` files,
/// each done with a tool on one project: part data in the schematic, the board brought in from it,
/// net classes, routes (one locked), a checkpoint, a rip-up that spares the locked track, the diff,
/// a restore, and DRC. The test opens no project file itself.
#[tokio::test]
async fn live_the_file_edit_steps_of_a_real_session_are_tool_calls() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("replay", true);
    let schematic = pcb.with_extension("kicad_sch");
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("replay-{n}-{}", std::process::id());

    // 1. Schematic: what the order still needs, recorded as fields; ends saved.
    let sch = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": "headless" }),
        )
        .await;
    assert_eq!(sch["ok"], true, "{sch}");
    let sid = sch["result"]["session_id"].clone();
    let review = mcp.call("bom_review", json!({ "session_id": sid })).await;
    assert!(
        review["result"]["issues"].to_string().contains("MISSING_MPN"),
        "{review}"
    );
    let plan = mcp
        .call(
            "sch_set_fields_plan",
            json!({ "session_id": sid, "edits": [
                { "reference": "C1", "fields": { "MPN": "GRM21BR61A106KE19", "Manufacturer": "Murata" } },
                { "reference": "R3", "fields": { "DNP": "yes" } },
            ] }),
        )
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": sid, "plan_id": plan["result"]["plan_id"], "confirm": true, "idempotency_key": key("fields") }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(mcp.call("sch_save", json!({ "session_id": sid })).await["ok"], true);
    let found = mcp.call("sym_search", json!({ "query": "ldo 3.3v", "limit": 3 })).await;
    assert_eq!(found["ok"], true, "{found}");
    mcp.call("session_close", json!({ "session_id": sid })).await;

    // 2. Board: brought in from the schematic, outlined, placed.
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let update = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    assert_eq!(update["result"]["can_apply"], true, "{update}");
    assert_eq!(
        apply_board(&mut mcp, &id, &update, &key("update")).await["result"]["verified"],
        true
    );
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );

    // 3. Net classes, then tracks (the first locked).
    let rules = mcp
        .call(
            "pcb_rules_plan",
            json!({ "session_id": id,
                    "net_classes": [{ "name": "Power", "track_width_mm": 0.5, "clearance_mm": 0.25 }],
                    "assign": [{ "net": "GND", "class": "Power" }] }),
        )
        .await;
    assert_eq!(rules["ok"], true, "{rules}");
    assert_eq!(
        apply_board(&mut mcp, &id, &rules, &key("rules")).await["result"]["verified"],
        true
    );
    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let mut routes = Vec::new();
    for (_, net) in nets["result"]["nets"].as_object().unwrap() {
        let pads = net["pads"].as_array().unwrap();
        if pads.len() == 2 {
            let route = json!({ "points": [{ "pad": pads[0] }, { "pad": pads[1] }] });
            let one = mcp
                .call("pcb_route_plan", json!({ "session_id": id, "routes": [route.clone()] }))
                .await;
            if one["ok"] == true {
                routes.push(route);
            }
        }
    }
    assert!(routes.len() >= 2, "two simple routes are needed");
    routes[0]["locked"] = json!(true);
    let route = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": routes }))
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &route, &key("route")).await["result"]["verified"],
        true
    );
    assert_eq!(mcp.call("pcb_save", json!({ "session_id": id })).await["ok"], true);
    let routed = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    let tracks_before = routed["result"]["track_count"].as_u64().unwrap();
    let locked_before = routed["result"]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["locked"] == true)
        .count();
    assert!(locked_before >= 1 && (locked_before as u64) < tracks_before, "{routed}");

    // 4. A rip-up with a copy taken first; the locked track stays.
    let rip = mcp
        .call(
            "pcb_track_edit_plan",
            json!({ "session_id": id, "action": "delete", "select": { "all": true } }),
        )
        .await;
    assert_eq!(rip["ok"], true, "{rip}");
    let applied = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": rip["result"]["plan_id"], "confirm": true,
                    "idempotency_key": key("rip"), "checkpoint": "before the rip-up" }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let after_rip = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    assert_eq!(after_rip["result"]["track_count"], locked_before, "{after_rip}");

    // 5. What changed since the copy, then back to it; the open session is reloaded.
    let diff = mcp
        .call("checkpoint_diff", json!({ "session_id": id, "checkpoint": "latest" }))
        .await;
    assert_eq!(diff["ok"], true, "{diff}");
    assert_eq!(mcp.call("pcb_save", json!({ "session_id": id })).await["ok"], true);
    let restored = mcp
        .call(
            "checkpoint_restore",
            json!({ "session_id": id, "checkpoint": "latest", "confirm": true }),
        )
        .await;
    assert_eq!(restored["ok"], true, "{restored}");
    assert_eq!(
        mcp.call("pcb_list_tracks", json!({ "session_id": id })).await["result"]["track_count"],
        tracks_before,
        "the tracks are back"
    );
    let reloaded = mcp.call("pcb_reload", json!({ "session_id": id })).await;
    assert_eq!(reloaded["ok"], true, "{reloaded}");
    let rules_now = mcp.call("pcb_rules", json!({ "session_id": id })).await;
    assert!(
        rules_now["result"]["net_classes"].to_string().contains("Power"),
        "{rules_now}"
    );

    // 6. DRC says how many problems there are, without the list.
    let drc = mcp
        .call("pcb_run_drc", json!({ "session_id": id, "summary_only": true }))
        .await;
    assert_eq!(drc["ok"], true, "{drc}");
    assert!(
        pcb.parent().unwrap().join(".thingsvu").join("checkpoints").is_dir(),
        "the copies are kept next to the project"
    );
    mcp.call("pcb_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
}
