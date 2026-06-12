use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

fn cwtools_server_cmd() -> Command {
    let bin = assert_cmd::cargo::cargo_bin("cwtools-server");
    let mut cmd = Command::new(bin);
    cmd.env("RUST_LOG", "");
    cmd
}

fn write_frame(child: &mut std::process::Child, body: &str) -> std::io::Result<()> {
    let stdin = child.stdin.as_mut().unwrap();
    write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    stdin.flush()?;
    Ok(())
}

fn read_frame(reader: &mut BufReader<std::process::ChildStdout>) -> std::io::Result<String> {
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length: ") {
            content_length = val.parse().unwrap_or(0);
        }
    }
    if content_length == 0 {
        return Ok(String::new());
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    String::from_utf8(body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn read_response(reader: &mut BufReader<std::process::ChildStdout>) -> std::io::Result<String> {
    loop {
        let raw = read_frame(reader)?;
        if raw.is_empty() {
            return Ok(raw);
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
            if val.get("id").is_some() {
                return Ok(raw);
            }
        } else {
            return Ok(raw);
        }
    }
}

fn jsonrpc_request(id: i64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

fn jsonrpc_notification(method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
    .to_string()
}

// ── Help / Version ───────────────────────────────────────────────────────────

#[test]
fn test_lsp_help_exits_zero() {
    assert_cmd::Command::cargo_bin("cwtools-server")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stderr(predicates::str::contains("cwtools-server"))
        .stderr(predicates::str::contains("Language Server Protocol"));
}

#[test]
fn test_lsp_version_exits_zero() {
    assert_cmd::Command::cargo_bin("cwtools-server")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("cwtools-server"));
}

// ── Initialize handshake ─────────────────────────────────────────────────────

#[test]
fn test_lsp_initialize_handshake() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = format!("file://{}", tmp.path().display());

    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let body = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "capabilities": {}
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let resp_str = read_response(&mut reader).expect("no response");
    child.kill().ok();

    let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["capabilities"].is_object());
}

// ── Shutdown without workspace scan ──────────────────────────────────────────

#[test]
fn test_lsp_shutdown_without_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = format!("file://{}", tmp.path().display());

    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let body = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "capabilities": {}
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let resp_str = read_response(&mut reader).expect("no init response");
    let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
    assert_eq!(resp["id"], 1);

    let body = jsonrpc_request(2, "shutdown", serde_json::json!(null));
    write_frame(&mut child, &body).unwrap();
    let resp_str = read_response(&mut reader).expect("no shutdown response");
    child.kill().ok();

    let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
    assert_eq!(resp["id"], 2);
    assert!(resp["result"].is_null());
}

// ── Full lifecycle: initialize → initialized → shutdown ──────────────────────

#[test]
fn test_lsp_full_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = format!("file://{}", tmp.path().display());

    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let body = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "capabilities": {}
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let resp_str = read_response(&mut reader).expect("no init response");
    let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
    assert_eq!(resp["id"], 1);

    let body = jsonrpc_notification("initialized", serde_json::json!({}));
    write_frame(&mut child, &body).unwrap();

    let body = jsonrpc_request(2, "shutdown", serde_json::json!(null));
    write_frame(&mut child, &body).unwrap();
    let resp_str = read_response(&mut reader).expect("no shutdown response");
    child.kill().ok();

    let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
    assert_eq!(resp["id"], 2);
    assert!(resp["result"].is_null());
}

// ── Unknown notification does not crash ──────────────────────────────────────

#[test]
fn test_lsp_unknown_notification_does_not_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = format!("file://{}", tmp.path().display());

    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let body = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "capabilities": {}
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let _ = read_response(&mut reader);

    let body = jsonrpc_notification("nonexistent/method", serde_json::json!({}));
    write_frame(&mut child, &body).unwrap();

    let body = jsonrpc_request(99, "shutdown", serde_json::json!(null));
    write_frame(&mut child, &body).unwrap();
    let resp_str = read_response(&mut reader).expect("server should respond");
    child.kill().ok();

    let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
    assert_eq!(resp["id"], 99);
}

// ── Context-aware completion round-trips ─────────────────────────────────────

/// Rules covering both regressions from cwtools-vscode#11: trigger aliases
/// (`has_completed_focus`) and the MIO `equipment_bonus` typed-key descent
/// into `alias_name[modifier]`.
const COMPLETION_RULES: &str = r#"
types = {
    type[focus] = {
        path = "game/common/national_focus"
    }
    type[decision] = {
        path = "game/common/decisions"
    }
    type[mio] = {
        path = "game/common/military_industrial_organization/organizations"
    }
}
decision = {
    allowed = {
        alias_name[trigger] = alias_match_left[trigger]
    }
    cost = int
}
mio = {
    name = scalar
    equipment_bonus = {
        <equipment> = {
            alias_name[modifier] = alias_match_left[modifier]
        }
    }
}
alias[trigger:has_completed_focus] = <focus>
alias[trigger:always] = bool
modifiers = {
    build_cost_ic = economy
    production_speed_factor = economy
}
"#;

/// Spawn a server with COMPLETION_RULES loaded, open `rel_path` with `text`,
/// request completion at (line0, char0), and return the completion labels.
fn completion_labels(rel_path: &str, text: &str, line0: u32, char0: u32) -> Vec<String> {
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    std::fs::write(rules_dir.path().join("test_rules.cwt"), COMPLETION_RULES).unwrap();

    let file_path = ws.path().join(rel_path);
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, text).unwrap();

    let ws_uri = format!("file://{}", ws.path().display());
    let doc_uri = format!("file://{}", file_path.display());

    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let body = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": ws_uri,
            "capabilities": {},
            "initializationOptions": {
                "language": "hoi4",
                "rulesCache": rules_dir.path().to_string_lossy(),
            }
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let _ = read_response(&mut reader).expect("no init response");

    let body = jsonrpc_notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": doc_uri,
                "languageId": "hoi4",
                "version": 1,
                "text": text,
            }
        }),
    );
    write_frame(&mut child, &body).unwrap();

    let body = jsonrpc_request(
        2,
        "textDocument/completion",
        serde_json::json!({
            "textDocument": { "uri": doc_uri },
            "position": { "line": line0, "character": char0 },
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let resp_str = read_response(&mut reader).expect("no completion response");
    child.kill().ok();

    let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
    assert_eq!(resp["id"], 2, "got: {}", resp_str);
    let items = resp["result"]
        .as_array()
        .cloned()
        .or_else(|| resp["result"]["items"].as_array().cloned())
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|i| i["label"].as_str().map(|s| s.to_string()))
        .collect()
}

#[test]
fn test_completion_trigger_alias_in_allowed_block() {
    let text = "my_decision = {\n    allowed = {\n        \n    }\n    cost = 5\n}\n";
    // Cursor on the blank line inside `allowed = { ... }` (line 2, col 8).
    let labels = completion_labels("common/decisions/test.txt", text, 2, 8);
    assert!(
        labels.iter().any(|l| l == "has_completed_focus"),
        "trigger aliases should be offered inside allowed, got: {:?}",
        labels
    );
    assert!(labels.iter().any(|l| l == "always"), "got: {:?}", labels);
}

#[test]
fn test_completion_modifiers_in_mio_equipment_bonus() {
    let text = "my_org = {\n    name = org\n    equipment_bonus = {\n        some_equipment = {\n            \n        }\n    }\n}\n";
    // Cursor on the blank line inside the equipment block (line 4, col 12).
    let labels = completion_labels(
        "common/military_industrial_organization/organizations/test.txt",
        text,
        4,
        12,
    );
    assert!(
        labels.iter().any(|l| l == "build_cost_ic"),
        "modifier names should be offered inside an equipment_bonus entry, got: {:?}",
        labels
    );
}

// ── Dynamic value completion round-trips (real MIO/trigger shapes) ───────────

/// Rules mirroring the REAL HOI4 config shapes: MIO trait equipment_bonus is
/// keyed by enum[equipment_stat] (a complex enum collected from
/// common/script_enums.txt), has_idea reads enum[idea_name], and
/// has_country_flag reads value[country_flag] (members written by
/// set_country_flag).
const DYNAMIC_RULES: &str = r#"
types = {
    type[mio] = {
        path = "game/common/military_industrial_organization/organizations"
    }
    type[decision] = {
        path = "game/common/decisions"
    }
}
enums = {
    complex_enum[equipment_stat] = {
        path = "game/common"
        path_file = "script_enums.txt"
        start_from_root = yes
        name = {
            script_enum_equipment_stat = {
                enum_name
            }
        }
    }
    complex_enum[idea_name] = {
        path = "game/common/ideas"
        name = {
            scalar = {
                enum_name = {
                }
            }
        }
    }
}
mio = {
    name = scalar
    trait = {
        token = scalar
        equipment_bonus = {
            ## cardinality = ~1..inf
            enum[equipment_stat] = variable_field
            ## cardinality = 0..1
            instant = bool
        }
    }
}
decision = {
    allowed = {
        alias_name[trigger] = alias_match_left[trigger]
    }
    complete_effect = {
        alias_name[effect] = alias_match_left[effect]
    }
    cost = int
}
### Does the country have this idea
alias[trigger:has_idea] = enum[idea_name]
### Has the country flag been set
alias[trigger:has_country_flag] = value[country_flag]
alias[effect:set_country_flag] = value_set[country_flag]
"#;

/// Open `extra_files` (indexed on didOpen) then request completion in `text`
/// at (line0, char0); returns the labels.
fn completion_labels_with_files(
    rel_path: &str,
    text: &str,
    extra_files: &[(&str, &str)],
    line0: u32,
    char0: u32,
) -> Vec<String> {
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    std::fs::write(rules_dir.path().join("test_rules.cwt"), DYNAMIC_RULES).unwrap();

    for (rel, content) in extra_files.iter().chain([&(rel_path, text)]) {
        let p = ws.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
    }

    let ws_uri = format!("file://{}", ws.path().display());
    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let body = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": ws_uri,
            "capabilities": {},
            "initializationOptions": {
                "language": "hoi4",
                "rulesCache": rules_dir.path().to_string_lossy(),
            }
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let _ = read_response(&mut reader).expect("no init response");

    // didOpen every file so each is indexed deterministically (no reliance on
    // the async workspace scan).
    for (rel, content) in extra_files.iter().chain([&(rel_path, text)]) {
        let uri = format!("file://{}", ws.path().join(rel).display());
        let body = jsonrpc_notification(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "hoi4",
                    "version": 1,
                    "text": content,
                }
            }),
        );
        write_frame(&mut child, &body).unwrap();
    }

    let doc_uri = format!("file://{}", ws.path().join(rel_path).display());
    let body = jsonrpc_request(
        2,
        "textDocument/completion",
        serde_json::json!({
            "textDocument": { "uri": doc_uri },
            "position": { "line": line0, "character": char0 },
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let resp_str = read_response(&mut reader).expect("no completion response");
    child.kill().ok();

    let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
    assert_eq!(resp["id"], 2, "got: {}", resp_str);
    let items = resp["result"]
        .as_array()
        .cloned()
        .or_else(|| resp["result"]["items"].as_array().cloned())
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|i| i["label"].as_str().map(|s| s.to_string()))
        .collect()
}

const SCRIPT_ENUMS: (&str, &str) = (
    "common/script_enums.txt",
    "script_enum_equipment_stat = {\n\tbuild_cost_ic\n\treliability\n\tsoft_attack\n}\n",
);

#[test]
fn test_completion_equipment_stats_in_mio_trait_bonus() {
    // The real MIO shape: equipment_bonus keyed by the equipment_stat complex
    // enum, collected from common/script_enums.txt.
    let text = "my_org = {\n    name = org\n    trait = {\n        token = t1\n        equipment_bonus = {\n            \n        }\n    }\n}\n";
    // Cursor on the blank line inside equipment_bonus (line 5, col 12).
    let labels = completion_labels_with_files(
        "common/military_industrial_organization/organizations/test.txt",
        text,
        &[SCRIPT_ENUMS],
        5,
        12,
    );
    assert!(
        labels.iter().any(|l| l == "build_cost_ic"),
        "equipment stats should be offered, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|l| l == "soft_attack"),
        "got: {:?}",
        labels
    );
    assert!(labels.iter().any(|l| l == "instant"), "got: {:?}", labels);
}

#[test]
fn test_completion_idea_names_for_has_idea() {
    // has_idea = | offers idea names collected via the idea_name complex enum.
    let ideas = (
        "common/ideas/test_ideas.txt",
        "ideas = {\n\tcountry = {\n\t\tmy_test_idea = {\n\t\t\tcost = 1\n\t\t}\n\t}\n}\n",
    );
    let text = "my_decision = {\n    allowed = {\n        has_idea = \n    }\n    cost = 5\n}\n";
    // Cursor right after `has_idea = ` (line 2, col 19).
    let labels = completion_labels_with_files("common/decisions/test.txt", text, &[ideas], 2, 19);
    assert!(
        labels.iter().any(|l| l == "my_test_idea"),
        "idea names should be offered for has_idea, got: {:?}",
        labels
    );
}

#[test]
fn test_completion_country_flags_for_has_country_flag() {
    // Flags written by set_country_flag anywhere in the workspace are offered
    // for has_country_flag = |.
    let setter = (
        "common/decisions/setter.txt",
        "other_decision = {\n    complete_effect = {\n        set_country_flag = my_war_flag\n    }\n    cost = 1\n}\n",
    );
    let text =
        "my_decision = {\n    allowed = {\n        has_country_flag = \n    }\n    cost = 5\n}\n";
    // Cursor right after `has_country_flag = ` (line 2, col 27).
    let labels = completion_labels_with_files("common/decisions/test.txt", text, &[setter], 2, 27);
    assert!(
        labels.iter().any(|l| l == "my_war_flag"),
        "collected country flags should be offered, got: {:?}",
        labels
    );
}

// ── Hover: localisation display ──────────────────────────────────────────────

#[test]
fn test_hover_shows_localisation() {
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    std::fs::write(rules_dir.path().join("test_rules.cwt"), DYNAMIC_RULES).unwrap();

    // Create a loc file (needs UTF-8 BOM and l_english header).
    let loc_dir = ws.path().join("localisation");
    std::fs::create_dir_all(&loc_dir).unwrap();
    let mut loc_content: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
    loc_content.extend_from_slice(b"l_english:\n");
    loc_content.extend_from_slice(b" my_idea:0 \"My Awesome Idea\"\n");
    std::fs::write(loc_dir.join("test_l_english.yml"), &loc_content).unwrap();

    // Script file that references the loc key as a value.
    let script_text = "my_country = {\n    name = my_idea\n}\n";
    let script_path = "common/countries/test.txt";
    let p = ws.path().join(script_path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, script_text).unwrap();

    let ws_uri = format!("file://{}", ws.path().display());
    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    // initialize
    let body = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": ws_uri,
            "capabilities": {},
            "initializationOptions": {
                "language": "hoi4",
                "rulesCache": rules_dir.path().to_string_lossy(),
            }
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let _ = read_response(&mut reader).expect("no init response");

    // initialized — triggers the background workspace scan that rebuilds loc_text
    let body = jsonrpc_notification("initialized", serde_json::json!({}));
    write_frame(&mut child, &body).unwrap();

    // didOpen the script file
    let doc_uri = format!("file://{}", p.display());
    let body = jsonrpc_notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": doc_uri,
                "languageId": "hoi4",
                "version": 1,
                "text": script_text,
            }
        }),
    );
    write_frame(&mut child, &body).unwrap();

    // Poll hover until loc_text is populated (workspace scan completes).
    let mut hover_value = String::new();
    for attempt in 0..30 {
        // Drain any pending notifications before sending the request.
        // read_response only returns messages with an `id`, so we need to
        // send the hover request first, then read.
        let hover_req = jsonrpc_request(
            2 + attempt,
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": doc_uri },
                "position": { "line": 1, "character": 14 },
            }),
        );
        write_frame(&mut child, &hover_req).unwrap();
        let resp_str = read_response(&mut reader).expect("no hover response");
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
        hover_value = resp["result"]["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if hover_value.contains("Localisation") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    child.kill().ok();

    assert!(
        hover_value.contains("Localisation"),
        "hover should include localisation section, got: {}",
        hover_value
    );
    assert!(
        hover_value.contains("My Awesome Idea"),
        "hover should include loc text, got: {}",
        hover_value
    );
    assert!(
        hover_value.contains("English"),
        "hover should include language label, got: {}",
        hover_value
    );
}
