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

/// Drain server frames until the `publishDiagnostics` notification whose URI
/// ends with `rel_path` arrives. did_open publishes diagnostics for a file only
/// after its index write lands, so this is the readiness signal that the file's
/// exports (value_set members, enum values, type instances) are queryable.
/// Without it a following completion races the index write — tower-lsp dispatches
/// handlers `buffer_unordered`, so there is no happens-before between a notify
/// handler finishing and the next request's handler running. Matches by path
/// suffix since the server canonicalises the URI (`file://` vs `file:///`).
fn wait_for_diagnostics(reader: &mut BufReader<std::process::ChildStdout>, rel_path: &str) {
    for _ in 0..400 {
        let raw = match read_frame(reader) {
            Ok(r) => r,
            Err(_) => return,
        };
        if raw.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
            && v["method"] == "textDocument/publishDiagnostics"
            && v["params"]["uri"]
                .as_str()
                .is_some_and(|u| u.ends_with(rel_path))
        {
            return;
        }
    }
    panic!("no publishDiagnostics for {rel_path}");
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
    assert!(resp["result"]["capabilities"].is_object());

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
    type[event] = {
        path = "game/events"
    }
}
decision = {
    allowed = {
        alias_name[trigger] = alias_match_left[trigger]
    }
    cost = int
    set_math = {
        value_set[variable] = math_expr
        value_set[variable] = scalar
    }
}
focus = {
    id = scalar
    x = int
    y = int
    cost = float
    completion_reward = {
        alias_name[effect] = alias_match_left[effect]
    }
    available = {
        alias_name[trigger] = alias_match_left[trigger]
    }
}
event = {
    id = scalar
    title = scalar
    trigger = {
        alias_name[trigger] = alias_match_left[trigger]
    }
    immediate = {
        alias_name[effect] = alias_match_left[effect]
    }
    option = {
        name = scalar
    }
}
alias[mathexpr:add] = math_expr
alias[mathexpr:subtract] = math_expr
alias[mathexpr:multiply] = math_expr
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
alias[effect:add_political_power] = int
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
    // Wait for the file's index write to land before requesting completion.
    wait_for_diagnostics(&mut reader, rel_path);

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
    // The sibling decision field `cost` lives one level up, not inside `allowed`.
    // It must not leak in, or context-awareness is broken.
    assert!(
        !labels.iter().any(|l| l == "cost"),
        "out-of-context field `cost` should not appear inside allowed, got: {:?}",
        labels
    );
}

#[test]
fn test_completion_on_blank_line_after_field() {
    // Completing on a fresh line after `cost = 5` must offer the block's other
    // fields (the parser's leaf range absorbs the trailing blank line, which used
    // to resolve to the cost value and return nothing) (cwtools-vscode#20).
    let text = "my_decision = {\n    cost = 5\n    \n}\n";
    // Cursor on the blank line after `cost = 5` (line 2, col 4).
    let labels = completion_labels("common/decisions/test.txt", text, 2, 4);
    assert!(
        labels.iter().any(|l| l == "allowed"),
        "blank line after a field should offer sibling fields, got: {:?}",
        labels
    );
}

#[test]
fn test_completion_response_marks_incomplete() {
    // Context-aware completions must be flagged `is_incomplete` so the client
    // re-queries on every keystroke. Without it, VS Code caches the list and
    // filters client-side — the "popup sticks on a half-typed prefix"
    // symptom when the user returns to a file with stale context.
    let resp = completion_response(
        "common/decisions/test.txt",
        "my_decision = {\n    cost = 5\n}\n",
        1,
        4,
    );
    let is_incomplete = resp["result"]["isIncomplete"]
        .as_bool()
        .or_else(|| resp["result"]["is_incomplete"].as_bool());
    assert_eq!(
        is_incomplete,
        Some(true),
        "context-aware completion must be marked is_incomplete, got: {}",
        resp
    );
}

#[test]
fn test_completion_in_half_typed_state() {
    // User scenario: type a partial block, walk away, come back, start
    // typing more. The parser fails on the partial text, so the last good
    // AST is None and `rules_at_pos` has nothing to walk. Completion must
    // still return SOMETHING (the cached fallback, the loc list, or a
    // re-parsed AST) and flag `is_incomplete` so the popup re-engages on
    // the next keystroke. Regression for the "super unresponsive when you
    // come back to a half-typed file" complaint.
    let text =
        "my_decision = {\n    allowed = {\n        has_completed_focus = \n    }\n    cost = 5\n";
    let resp = completion_response("common/decisions/test.txt", text, 3, 32);
    // Either a context-aware list (from the re-parsed AST) or the fallback
    // list (if even the re-parse failed) — both are valid, but the
    // response must not be empty and must be marked incomplete.
    let items = resp["result"]["items"]
        .as_array()
        .or_else(|| resp["result"].as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !items.is_empty(),
        "half-typed state must return some completions, got: {}",
        resp
    );
    let is_incomplete = resp["result"]["isIncomplete"]
        .as_bool()
        .or_else(|| resp["result"]["is_incomplete"].as_bool());
    assert_eq!(
        is_incomplete,
        Some(true),
        "half-typed completion must be marked is_incomplete, got: {}",
        resp
    );
}

#[test]
fn test_completion_items_carry_text_edit_anchor() {
    // Every item must carry an explicit `textEdit` so the client filters and
    // inserts against the cursor range instead of guessing a word boundary (the
    // guess breaks right after a backspace across `=` / `<` / `>`). Blank-line
    // key position inside the decision block (line 2, col 4): the range anchors
    // at the cursor and `filterText` is pinned to the label. (Non-empty token
    // ranges are covered by paths::test_current_token_range.)
    let text = "my_decision = {\n    cost = 5\n    \n}\n";
    let resp = completion_response("common/decisions/test.txt", text, 2, 4);
    let items = resp["result"]["items"]
        .as_array()
        .or_else(|| resp["result"].as_array())
        .cloned()
        .unwrap_or_default();
    assert!(!items.is_empty(), "expected items, got: {}", resp);
    let allowed = items
        .iter()
        .find(|i| i["label"] == "allowed")
        .unwrap_or_else(|| panic!("`allowed` not offered, got: {}", resp));
    let range = &allowed["textEdit"]["range"];
    assert_eq!(range["start"]["line"], 2, "got: {}", allowed);
    assert_eq!(
        range["start"]["character"], 4,
        "replace range must anchor at the cursor token, got: {}",
        allowed
    );
    assert_eq!(range["end"]["character"], 4, "got: {}", allowed);
    // filterText is pinned to the label so the client never filters a snippet.
    assert_eq!(allowed["filterText"], "allowed", "got: {}", allowed);
}

#[test]
fn test_completion_offers_mathexpr_operators_in_math_block() {
    // Inside a `math_expr` block (`set_math = { x = { | } }`), completion must
    // offer `value` and the registered mathexpr operators (add/subtract/…),
    // resolved by the position descent into the synthesized math-clause rules —
    // not the flat fallback.
    let text = "d = {\n    set_math = {\n        x = {\n            \n        }\n    }\n}\n";
    // Blank line inside the innermost math block `x = { ... }` (line 3, col 12).
    let labels = completion_labels("common/decisions/test.txt", text, 3, 12);
    assert!(
        labels.iter().any(|l| l == "add") && labels.iter().any(|l| l == "subtract"),
        "math operators should be offered inside a math block, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|l| l == "value"),
        "`value` base should be offered inside a math block, got: {:?}",
        labels
    );
}

#[test]
fn test_completion_math_block_value_excludes_effects() {
    // Value position after `add = ` inside a math block (line 3, col 18).
    let text = "d = {\n    set_math = {\n        x = {\n            add = \n        }\n    }\n}\n";
    let labels = completion_labels("common/decisions/test.txt", text, 3, 18);
    assert!(
        !labels.iter().any(|l| l == "add_political_power"),
        "effects must not appear at math value position, got: {:?}",
        labels
    );
}

#[test]
fn test_completion_math_leaf_value_excludes_effects() {
    // Value position after `x = ` at the set_variable level (line 2, col 12).
    let text = "d = {\n    set_math = {\n        x = \n    }\n}\n";
    let labels = completion_labels("common/decisions/test.txt", text, 2, 12);
    assert!(
        !labels.iter().any(|l| l == "add_political_power"),
        "effects must not appear at math leaf value position, got: {:?}",
        labels
    );
}

fn completion_response(rel_path: &str, text: &str, line0: u32, char0: u32) -> serde_json::Value {
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
    wait_for_diagnostics(&mut reader, rel_path);

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
    serde_json::from_str(&resp_str).unwrap()
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
    // `name` is a top-level mio field, not a modifier; it must not leak into the
    // equipment entry's modifier completions.
    assert!(
        !labels.iter().any(|l| l == "name"),
        "out-of-context field `name` should not appear inside an equipment entry, got: {:?}",
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
    // the async workspace scan). Wait for each file's diagnostics before sending
    // the next message so its index write (value_set members, enum values, type
    // instances) is queryable when the cross-file completion runs.
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
        wait_for_diagnostics(&mut reader, rel);
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
fn test_completion_focus_keys_after_clause_subblock() {
    // Cursor on the blank line AFTER `completion_reward = { ... }` must offer
    // focus-level keys (id, x, y, cost, …), not the effects inside the sub-block.
    // Regression: parser extends completion_reward's range past `}`, causing
    // descend() to enter the sub-block and return effect aliases.
    let text = "my_focus = {\n    completion_reward = {\n        add_political_power = 5\n    }\n    \n}\n";
    // Line 4, col 4: blank line after `}` of completion_reward, still inside focus.
    let labels = completion_labels("common/national_focus/test.txt", text, 4, 4);
    assert!(
        labels.iter().any(|l| l == "id"),
        "focus keys should be offered after a clause sub-block, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|l| l == "cost"),
        "focus keys should be offered after a clause sub-block, got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l == "add_political_power"),
        "effect from sub-block must not leak into focus context, got: {:?}",
        labels
    );
}

#[test]
fn test_completion_focus_effects_inside_clause_subblock() {
    // Cursor on the blank line INSIDE `completion_reward = { }` must offer
    // effects, not focus-level keys.
    let text = "my_focus = {\n    completion_reward = {\n        \n    }\n}\n";
    // Line 2, col 8: blank line inside completion_reward.
    let labels = completion_labels("common/national_focus/test.txt", text, 2, 8);
    assert!(
        labels.iter().any(|l| l == "add_political_power"),
        "effects should be offered inside completion_reward, got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l == "id"),
        "focus key `id` must not appear inside completion_reward, got: {:?}",
        labels
    );
}

#[test]
fn test_completion_event_keys_after_clause_subblock() {
    // Cursor after `trigger = { ... }` inside an event block must offer event-level
    // keys (id, title, immediate, option, …), not trigger aliases.
    let text = "my_event = {\n    trigger = {\n        always = yes\n    }\n    \n}\n";
    // Line 4, col 4: blank line after `}` of trigger, still inside event.
    let labels = completion_labels("events/test.txt", text, 4, 4);
    assert!(
        labels.iter().any(|l| l == "title"),
        "event keys should be offered after a clause sub-block, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|l| l == "immediate"),
        "event keys should be offered after a clause sub-block, got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l == "always"),
        "trigger alias must not leak into event context, got: {:?}",
        labels
    );
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

// ── Issues #64, #65: type-pattern alias and alias_keys_field completions ──────

/// Spawn a server with custom `rules` text, open `extra_files` + the main file,
/// and return the completion labels at `(line0, char0)`.  Mirrors
/// `completion_labels_with_files` but the rules come from the caller.
fn completion_labels_custom_rules(
    rules: &str,
    rel_path: &str,
    text: &str,
    extra_files: &[(&str, &str)],
    line0: u32,
    char0: u32,
) -> Vec<String> {
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    std::fs::write(rules_dir.path().join("test_rules.cwt"), rules).unwrap();

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
        wait_for_diagnostics(&mut reader, rel);
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

/// Rules for the #64 and #66 integration tests.
const SCRIPTED_EFFECT_RULES: &str = r#"
types = {
    type[scripted_effect] = {
        path = "game/common/scripted_effects"
    }
    type[decision] = {
        path = "game/common/decisions"
    }
}
decision = {
    complete_effect = {
        alias_name[effect] = alias_match_left[effect]
    }
}
alias[effect:<scripted_effect>] = yes
scripted_effect = {
    alias_name[effect] = alias_match_left[effect]
}
"#;

#[test]
fn test_completion_scripted_effects_in_effect_block() {
    // #64: `alias[effect:<scripted_effect>] = yes` means every scripted_effect
    // instance must appear as a KEYWORD completion inside effect blocks. The bug
    // was that the raw placeholder `<scripted_effect>` appeared instead of actual
    // instance names.
    let se_file = (
        "common/scripted_effects/my_effects.txt",
        "my_special_effect = {\n}\n",
    );
    // Blank line inside `complete_effect = { }` of the decision.
    let text = "my_dec = {\n    complete_effect = {\n        \n    }\n}\n";
    let labels = completion_labels_custom_rules(
        SCRIPTED_EFFECT_RULES,
        "common/decisions/d.txt",
        text,
        &[se_file],
        2,
        8,
    );
    assert!(
        labels.iter().any(|l| l == "my_special_effect"),
        "scripted_effect instances must be offered in effect blocks, got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l == "<scripted_effect>"),
        "raw placeholder must not appear in labels, got: {:?}",
        labels
    );
}

/// Rules for the #65 integration test: a `dynamic_modifier` type whose body uses
/// `alias_keys_field[modifier]` as the key pattern.
const DYNAMIC_MODIFIER_RULES: &str = r#"
types = {
    type[dynamic_modifier] = {
        path = "game/common/dynamic_modifiers"
    }
}
modifiers = {
    build_cost_ic = economy
    production_speed_factor = economy
}
dynamic_modifier = {
    ## cardinality = 0..inf
    alias_keys_field[modifier] = float
}
"#;

#[test]
fn test_completion_alias_keys_field_in_dynamic_modifier() {
    // #65: a block whose children use `alias_keys_field[modifier]` as their key
    // (common/dynamic_modifiers/*.txt in HOI4) must offer modifier names.
    let text = "my_dmod = {\n    \n}\n";
    let labels = completion_labels_custom_rules(
        DYNAMIC_MODIFIER_RULES,
        "common/dynamic_modifiers/test.txt",
        text,
        &[],
        1,
        4,
    );
    assert!(
        labels.iter().any(|l| l == "build_cost_ic"),
        "modifier names must be offered inside a dynamic_modifier block, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|l| l == "production_speed_factor"),
        "all modifier keys must be offered, got: {:?}",
        labels
    );
}

// ── Backspace robustness: same-context completions must not evaporate when the
//    value is deleted, and the flat variable fallback must NOT be substituted
//    for the context-aware list. The cwt rules define the context; deleting
//    characters in the value doesn't change which block the cursor is in. ─────

/// Like `completion_labels_with_files` but issues a `didChange` to `new_text`
/// (full-sync) before requesting completion, so the test exercises the
/// backspace-into-blank case end to end. `wait_after_change` controls whether
/// the test waits for the debounced validate to republish diagnostics before
/// requesting completion — pass `true` to land on the post-debounce AST,
/// `false` to land on the stale AST (the realistic mid-typing snapshot).
fn completion_labels_after_change(
    rel_path: &str,
    open_text: &str,
    extra_files: &[(&str, &str)],
    new_text: &str,
    line0: u32,
    char0: u32,
    wait_after_change: bool,
) -> Vec<String> {
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    std::fs::write(rules_dir.path().join("test_rules.cwt"), DYNAMIC_RULES).unwrap();

    for (rel, content) in extra_files.iter().chain([&(rel_path, open_text)]) {
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

    for (rel, content) in extra_files.iter().chain([&(rel_path, open_text)]) {
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
        wait_for_diagnostics(&mut reader, rel);
    }

    // didChange to the new (backspaced) text. Bump the version so the LSP
    // accepts it. Full-sync: server ignores the range and replaces the whole
    // document text.
    let doc_uri = format!("file://{}", ws.path().join(rel_path).display());
    let body = jsonrpc_notification(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": &doc_uri, "version": 2 },
            "contentChanges": [{ "text": new_text }],
        }),
    );
    write_frame(&mut child, &body).unwrap();
    if wait_after_change {
        wait_for_diagnostics(&mut reader, rel_path);
    }

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
fn test_completion_value_deleted_then_reoffered_keeps_context() {
    // User scenario: a decision's `allowed` block has `has_country_flag = my_war_flag`
    // with a working completion. They backspace the value, leaving
    // `has_country_flag = ` (or shorter). The same flag set must still be
    // offered — the block context (`allowed = { ... }`) hasn't changed, only
    // the value text has. The flat variable dump must NOT be substituted.
    let setter = (
        "common/decisions/setter.txt",
        "other_decision = {\n    complete_effect = {\n        set_country_flag = my_war_flag\n    }\n    cost = 1\n}\n",
    );
    let open_text = "my_decision = {\n    allowed = {\n        has_country_flag = my_war_flag\n    }\n    cost = 5\n}\n";
    // After backspacing the value, the cursor is right after `= ` on line 2.
    let new_text =
        "my_decision = {\n    allowed = {\n        has_country_flag = \n    }\n    cost = 5\n}\n";

    // Post-debounce (the AST has caught up to the new text): same flag set.
    let labels_post = completion_labels_after_change(
        "common/decisions/test.txt",
        open_text,
        std::slice::from_ref(&setter),
        new_text,
        2,
        27,
        true,
    );
    assert!(
        labels_post.iter().any(|l| l == "my_war_flag"),
        "post-debounce: backspaced value must still offer my_war_flag, got: {:?}",
        labels_post
    );

    // Mid-debounce (the AST is still the open_text one — the user is typing
    // fast and the next completion request arrives before the 250ms validate
    // has fired). Same expected behavior: the context-aware list, not the
    // generic variable dump.
    let labels_mid = completion_labels_after_change(
        "common/decisions/test.txt",
        open_text,
        &[setter],
        new_text,
        2,
        27,
        false,
    );
    assert!(
        labels_mid.iter().any(|l| l == "my_war_flag"),
        "mid-debounce: backspaced value must still offer my_war_flag, got: {:?}",
        labels_mid
    );
}

// ── Hover: localisation display ──────────────────────────────────────────────

/// Spawn a server with DYNAMIC_RULES, write `loc_files` (each: filename under
/// `localisation/`, full text including the `l_xxx:` header; a UTF-8 BOM is
/// prepended) and the one script file, run the workspace scan, then return the
/// hover markdown at (line, character) on the script. `extra_init` is merged
/// into the init options. Polls until the loc map is populated.
fn hover_markdown(
    loc_files: &[(&str, &str)],
    script_rel: &str,
    script_text: &str,
    line: u32,
    character: u32,
    extra_init: serde_json::Value,
) -> String {
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    std::fs::write(rules_dir.path().join("test_rules.cwt"), DYNAMIC_RULES).unwrap();
    // Named scopes so the registry resolves `country` (HOI4 has no hardcoded
    // scope table); lets the hover surface the current scope context.
    std::fs::write(
        rules_dir.path().join("scopes.cwt"),
        "scopes = { country = { } state = { } }\n",
    )
    .unwrap();

    let loc_dir = ws.path().join("localisation");
    std::fs::create_dir_all(&loc_dir).unwrap();
    for (name, content) in loc_files {
        let mut bytes: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(content.as_bytes());
        std::fs::write(loc_dir.join(name), &bytes).unwrap();
    }

    let p = ws.path().join(script_rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, script_text).unwrap();

    // The workspace scan (which builds the loc index) early-returns when there
    // are no game files. Real mods always have some; drop a tiny one so the scan
    // runs even when the opened document is itself a .yml.
    let trigger = ws.path().join("common/_scan_trigger.txt");
    std::fs::create_dir_all(trigger.parent().unwrap()).unwrap();
    std::fs::write(&trigger, "# scan trigger\n").unwrap();

    let ws_uri = format!("file://{}", ws.path().display());
    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let mut init_opts = serde_json::json!({
        "language": "hoi4",
        "rulesCache": rules_dir.path().to_string_lossy(),
    });
    if let Some(obj) = extra_init.as_object() {
        for (k, v) in obj {
            init_opts[k] = v.clone();
        }
    }
    let body = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": ws_uri,
            "capabilities": {},
            "initializationOptions": init_opts,
        }),
    );
    write_frame(&mut child, &body).unwrap();
    let _ = read_response(&mut reader).expect("no init response");

    // initialized — triggers the background workspace scan that rebuilds loc_text
    let body = jsonrpc_notification("initialized", serde_json::json!({}));
    write_frame(&mut child, &body).unwrap();

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
    // read_response only returns id-bearing messages, so send then read.
    let mut hover_value = String::new();
    for attempt in 0..30 {
        let hover_req = jsonrpc_request(
            2 + attempt,
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": doc_uri },
                "position": { "line": line, "character": character },
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
    hover_value
}

#[test]
fn test_hover_shows_current_scope() {
    // Anything hovered inside a scoped block shows the current scope context,
    // independent of whether the rule declares a required scope. The decisions
    // file is country-scoped, so a trigger value there reads as `country`.
    let hover = hover_markdown(
        &[("test_l_english.yml", "l_english:\n my_focus:0 \"Focus\"\n")],
        "common/decisions/d.txt",
        "my_dec = {\n    allowed = {\n        has_completed_focus = my_focus\n    }\n}\n",
        2,
        32,
        serde_json::json!({}),
    );
    assert!(
        hover.contains("**Scope**: country"),
        "hover should surface the current scope, got: {hover}"
    );
}

#[test]
fn test_hover_nested_loc_key_in_yml() {
    // Hovering a `$MY_KEY$` reference inside a .yml loc value resolves to the
    // referenced loc entry's text (nested loc keys / dynamic bindings).
    let hover = hover_markdown(
        &[("test_l_english.yml", "l_english:\n MY_KEY:0 \"My Value\"\n")],
        "localisation/english/ref_l_english.yml",
        "\u{FEFF}l_english:\n OTHER:0 \"see $MY_KEY$\"\n",
        1,
        17,
        serde_json::json!({}),
    );
    assert!(
        hover.contains("My Value"),
        "hover on $MY_KEY$ should resolve to the loc entry text, got: {hover}"
    );
}

#[test]
fn test_hover_shows_localisation() {
    // A reference: the loc key appears as a leaf value (`name = my_idea`).
    let hover = hover_markdown(
        &[(
            "test_l_english.yml",
            "l_english:\n my_idea:0 \"My Awesome Idea\"\n",
        )],
        "common/countries/test.txt",
        "my_country = {\n    name = my_idea\n}\n",
        1,
        14,
        serde_json::json!({}),
    );
    assert!(
        hover.contains("Localisation"),
        "hover should include localisation section, got: {hover}"
    );
    assert!(
        hover.contains("My Awesome Idea"),
        "hover should include loc text, got: {hover}"
    );
    assert!(
        hover.contains("English"),
        "hover should include language label, got: {hover}"
    );
}

#[test]
fn test_hover_idea_definition_shows_name_and_desc() {
    // A definition key: the idea token IS the loc key, with `<key>_desc` for the
    // description. Hover the key itself (not a value reference).
    let hover = hover_markdown(
        &[(
            "test_l_english.yml",
            "l_english:\n my_great_idea:0 \"Great Idea\"\n my_great_idea_desc:0 \"It is great.\"\n",
        )],
        "common/ideas/test.txt",
        "my_great_idea = {\n    cost = 5\n}\n",
        0,
        3,
        serde_json::json!({}),
    );
    assert!(
        hover.contains("Great Idea"),
        "hover on an idea key should show its name loc, got: {hover}"
    );
    assert!(
        hover.contains("It is great."),
        "hover on an idea key should show its _desc loc, got: {hover}"
    );
}

#[test]
fn test_hover_default_hides_other_languages() {
    // Default (hoverShowAllLanguages off): only the primary language is shown.
    let hover = hover_markdown(
        &[
            (
                "test_l_english.yml",
                "l_english:\n my_idea:0 \"English Name\"\n",
            ),
            (
                "test_l_french.yml",
                "l_french:\n my_idea:0 \"Nom Francais\"\n",
            ),
        ],
        "common/countries/test.txt",
        "my_country = {\n    name = my_idea\n}\n",
        1,
        14,
        serde_json::json!({}),
    );
    assert!(
        hover.contains("English Name"),
        "hover should show the primary (English) loc, got: {hover}"
    );
    assert!(
        !hover.contains("Nom Francais"),
        "hover should not show other languages by default, got: {hover}"
    );
}

#[test]
fn test_hover_show_all_languages_flag() {
    // With hoverShowAllLanguages on, every collected language is shown.
    let hover = hover_markdown(
        &[
            (
                "test_l_english.yml",
                "l_english:\n my_idea:0 \"English Name\"\n",
            ),
            (
                "test_l_french.yml",
                "l_french:\n my_idea:0 \"Nom Francais\"\n",
            ),
        ],
        "common/countries/test.txt",
        "my_country = {\n    name = my_idea\n}\n",
        1,
        14,
        serde_json::json!({ "hoverShowAllLanguages": true }),
    );
    assert!(
        hover.contains("English Name"),
        "hover should show English loc, got: {hover}"
    );
    assert!(
        hover.contains("Nom Francais"),
        "hover with the flag on should show French loc too, got: {hover}"
    );
}

// ── Go-to-definition ─────────────────────────────────────────────────────────

/// Rules exercising every navigable reference kind goto must resolve.
const GOTO_RULES: &str = r#"
types = {
    type[focus] = { path = "game/common/national_focus" }
    type[oob] = { path = "game/history/units" }
    type[character] = { path = "game/common/characters" }
    type[special_project] = { path = "game/common/special_projects" }
    type[scripted_effect] = { path = "game/common/scripted_effects" }
    type[decision] = { path = "game/common/decisions" }
    type[on_action] = { path = "game/common/on_actions" }
    ## type_key_filter = on_weekly
    type[on_weekly] = {
        path = "game/common/on_actions"
        skip_root_key = on_actions
    }
}
links = {
    sp = {
        prefix = sp:
        output_scope = special_project
        input_scopes = country
        from_data = yes
        data_source = <special_project>
    }
    character = {
        output_scope = character
        input_scopes = country
        from_data = yes
        data_source = <character>
    }
}
decision = {
    ## cardinality = 0..1
    has_focus = <focus>
    ## cardinality = 0..1
    load_oob = <oob>
    ## cardinality = 0..1
    localization_key = localisation
    ## cardinality = 0..1
    complete_special_project = scope[special_project]
    ## cardinality = 0..1
    available = {
        alias_name[trigger] = alias_match_left[trigger]
    }
    ## cardinality = 0..1
    complete_effect = {
        alias_name[effect] = alias_match_left[effect]
    }
    ## cardinality = 0..inf
    <character> = {
        is_enabled = bool
    }
}
alias[trigger:always] = bool
alias[effect:<scripted_effect>] = yes
focus = { x = bool }
oob = { y = bool }
character = { name = scalar }
special_project = { z = bool }
scripted_effect = { alias_name[effect] = alias_match_left[effect] }
on_action = {
    ## cardinality = 0..inf
    on_weekly = single_alias_right[country_event_effect]
}
single_alias[country_event_effect] = {
    ## cardinality = 0..inf
    effect = {
        alias_name[effect] = alias_match_left[effect]
    }
}
"#;

/// Spawn a server with `rules`, write the loc `.yml` files under `localisation/`
/// and the script files, then resolve textDocument/definition at (line, char) on
/// `doc_rel`. Polls until a non-empty result arrives (the loc index and type
/// index land via the async workspace scan). Returns `(uri, start_line)` pairs.
fn goto_def(
    rules: &str,
    loc_files: &[(&str, &str)],
    files: &[(&str, &str)],
    doc_rel: &str,
    line0: u32,
    char0: u32,
) -> Vec<(String, u32)> {
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    std::fs::write(rules_dir.path().join("test_rules.cwt"), rules).unwrap();

    let loc_dir = ws.path().join("localisation");
    std::fs::create_dir_all(&loc_dir).unwrap();
    for (name, content) in loc_files {
        let mut bytes: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(content.as_bytes());
        std::fs::write(loc_dir.join(name), &bytes).unwrap();
    }

    for (rel, content) in files {
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

    let body = jsonrpc_notification("initialized", serde_json::json!({}));
    write_frame(&mut child, &body).unwrap();

    for (rel, content) in files {
        let uri = format!("file://{}", ws.path().join(rel).display());
        let body = jsonrpc_notification(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri, "languageId": "hoi4", "version": 1, "text": content,
                }
            }),
        );
        write_frame(&mut child, &body).unwrap();
        wait_for_diagnostics(&mut reader, rel);
    }

    let doc_uri = format!("file://{}", ws.path().join(doc_rel).display());
    let mut out: Vec<(String, u32)> = Vec::new();
    // Loc-key goto depends on the async workspace scan populating loc_locations;
    // under parallel test load that can lag, so poll generously.
    for attempt in 0..50 {
        let req = jsonrpc_request(
            100 + attempt,
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": doc_uri },
                "position": { "line": line0, "character": char0 },
            }),
        );
        write_frame(&mut child, &req).unwrap();
        let resp_str = read_response(&mut reader).expect("no definition response");
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
        let arr = resp["result"]
            .as_array()
            .cloned()
            .or_else(|| {
                resp["result"]
                    .as_object()
                    .map(|o| vec![serde_json::Value::Object(o.clone())])
            })
            .unwrap_or_default();
        out = arr
            .iter()
            .filter_map(|l| {
                let uri = l["uri"].as_str()?.to_string();
                let line = l["range"]["start"]["line"].as_u64()? as u32;
                Some((uri, line))
            })
            .collect();
        if !out.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    child.kill().ok();
    out
}

#[test]
fn test_goto_focus_value() {
    // has_focus = MY_FOCUS — goto on the value jumps to the focus definition.
    let files = &[
        ("common/national_focus/f.txt", "MY_FOCUS = { x = yes }\n"),
        (
            "common/decisions/d.txt",
            "my_dec = {\n    has_focus = MY_FOCUS\n}\n",
        ),
    ];
    // Cursor on MY_FOCUS (line 1, col ~16).
    let locs = goto_def(GOTO_RULES, &[], files, "common/decisions/d.txt", 1, 16);
    assert!(
        locs.iter()
            .any(|(u, _)| u.ends_with("national_focus/f.txt")),
        "goto should resolve focus def, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_quoted_oob_value() {
    // load_oob = "MY_OOB" — the quoted value must be unquoted before the index
    // lookup, else nothing resolves.
    let files = &[
        ("history/units/o.txt", "MY_OOB = { y = yes }\n"),
        (
            "common/decisions/d.txt",
            "my_dec = {\n    load_oob = \"MY_OOB\"\n}\n",
        ),
    ];
    // Cursor inside the quoted value (line 1, col ~17).
    let locs = goto_def(GOTO_RULES, &[], files, "common/decisions/d.txt", 1, 17);
    assert!(
        locs.iter().any(|(u, _)| u.ends_with("units/o.txt")),
        "goto should resolve quoted oob def, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_nested_loc_key_in_yml() {
    // Goto on a `$MY_KEY$` reference inside a .yml jumps to the loc entry it
    // names. A game file is present so the workspace scan (which builds
    // loc_locations) runs.
    let loc = &[("def_l_english.yml", "l_english:\n MY_KEY:0 \"My Value\"\n")];
    let files = &[
        ("common/scan_trigger.txt", "# trigger\n"),
        (
            "localisation/english/use_l_english.yml",
            "\u{FEFF}l_english:\n OTHER:0 \"$MY_KEY$\"\n",
        ),
    ];
    // Cursor inside `$MY_KEY$` on line 1 (col 12).
    let locs = goto_def(
        GOTO_RULES,
        loc,
        files,
        "localisation/english/use_l_english.yml",
        1,
        12,
    );
    assert!(
        locs.iter().any(|(u, _)| u.ends_with("def_l_english.yml")),
        "goto on $MY_KEY$ should resolve to its loc definition, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_character_key() {
    // MY_CHAR = { ... } used as a <character> key — the reference is on the key,
    // which only resolves with the key-side classifier.
    let files = &[
        ("common/characters/c.txt", "MY_CHAR = { name = bob }\n"),
        (
            "common/decisions/d.txt",
            "my_dec = {\n    MY_CHAR = { is_enabled = yes }\n}\n",
        ),
    ];
    // Cursor on the MY_CHAR key (line 1, col 6).
    let locs = goto_def(GOTO_RULES, &[], files, "common/decisions/d.txt", 1, 6);
    assert!(
        locs.iter().any(|(u, _)| u.ends_with("characters/c.txt")),
        "goto should resolve character key def, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_localisation_key() {
    // localization_key = MY_KEY — goto jumps to the .yml entry.
    let loc = &[("test_l_english.yml", "l_english:\n MY_KEY:0 \"Text\"\n")];
    let files = &[(
        "common/decisions/d.txt",
        "my_dec = {\n    localization_key = MY_KEY\n}\n",
    )];
    // Cursor on MY_KEY (line 1, col ~25).
    let locs = goto_def(GOTO_RULES, loc, files, "common/decisions/d.txt", 1, 25);
    assert!(
        locs.iter().any(|(u, _)| u.ends_with("test_l_english.yml")),
        "goto should resolve loc key to the yml, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_special_project_sp_prefix() {
    // complete_special_project = sp:MY_PROJ — the sp: prefix resolves through the
    // matching link's data_source <special_project>.
    let files = &[
        ("common/special_projects/p.txt", "MY_PROJ = { z = yes }\n"),
        (
            "common/decisions/d.txt",
            "my_dec = {\n    complete_special_project = sp:MY_PROJ\n}\n",
        ),
    ];
    // Cursor inside the value after the sp: prefix (line 1, col ~34).
    let locs = goto_def(GOTO_RULES, &[], files, "common/decisions/d.txt", 1, 34);
    assert!(
        locs.iter()
            .any(|(u, _)| u.ends_with("special_projects/p.txt")),
        "goto should resolve sp: special_project, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_loc_key_prefers_english() {
    // The key exists in both English and Brazilian Portuguese; goto must land on
    // the English (primary) entry, not whichever was scanned first.
    let loc = &[
        ("test_l_braz_por.yml", "l_braz_por:\n MY_KEY:0 \"Texto\"\n"),
        ("test_l_english.yml", "l_english:\n MY_KEY:0 \"Text\"\n"),
    ];
    let files = &[(
        "common/decisions/d.txt",
        "my_dec = {\n    localization_key = MY_KEY\n}\n",
    )];
    let locs = goto_def(GOTO_RULES, loc, files, "common/decisions/d.txt", 1, 25);
    assert!(
        locs.iter().any(|(u, _)| u.ends_with("test_l_english.yml")),
        "goto should prefer the English loc file, got: {:?}",
        locs
    );
    assert!(
        !locs.iter().any(|(u, _)| u.ends_with("braz_por.yml")),
        "goto must not land on braz_por, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_character_scope_link_key() {
    // A character used as a scope key inside a trigger block matches no rule
    // (value_rules is empty); it resolves via the `character` link's data_source
    // <character>. This is the real MD case the rule-based path missed.
    let files = &[
        ("common/characters/c.txt", "MY_CHAR = { name = bob }\n"),
        (
            "common/decisions/d.txt",
            "my_dec = {\n    available = {\n        MY_CHAR = { always = yes }\n    }\n}\n",
        ),
    ];
    // Cursor on the MY_CHAR key (line 2, col 8).
    let locs = goto_def(GOTO_RULES, &[], files, "common/decisions/d.txt", 2, 8);
    assert!(
        locs.iter().any(|(u, _)| u.ends_with("characters/c.txt")),
        "goto should resolve scope-link character key, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_scripted_effect_call() {
    // A scripted_effect call (`my_se = yes`) resolves through the
    // `alias[effect:<scripted_effect>]` rule whose left field names the type.
    let files = &[
        (
            "common/scripted_effects/e.txt",
            "my_se = { log = \"hi\" }\n",
        ),
        (
            "common/decisions/d.txt",
            "my_dec = {\n    complete_effect = {\n        my_se = yes\n    }\n}\n",
        ),
    ];
    // Cursor on the my_se call key (line 2, col 8).
    let locs = goto_def(GOTO_RULES, &[], files, "common/decisions/d.txt", 2, 8);
    assert!(
        locs.iter()
            .any(|(u, _)| u.ends_with("scripted_effects/e.txt")),
        "goto should resolve scripted_effect call, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_scripted_effect_in_on_actions() {
    // A `*_on_actions`-style scripted_effect call inside an on_actions effect
    // block. The call site sits behind skip_root_key=on_actions + an inlined
    // single_alias_right effect block — a far deeper path than the decision case.
    let files = &[
        (
            "common/scripted_effects/e.txt",
            "my_se = { log = \"hi\" }\n",
        ),
        (
            "common/on_actions/x.txt",
            "on_actions = {\n    on_weekly = {\n        effect = {\n            my_se = yes\n        }\n    }\n}\n",
        ),
    ];
    // Cursor on the my_se call key (line 3, col 12).
    let locs = goto_def(GOTO_RULES, &[], files, "common/on_actions/x.txt", 3, 12);
    assert!(
        locs.iter()
            .any(|(u, _)| u.ends_with("scripted_effects/e.txt")),
        "goto should resolve scripted_effect call inside on_actions, got: {:?}",
        locs
    );
}

#[test]
fn test_goto_scripted_effect_in_scripted_effect_body() {
    // A scripted_effect call nested inside another scripted_effect's body
    // (common/scripted_effects/), not behind a decision/event effect block.
    let files = &[
        (
            "common/scripted_effects/e.txt",
            "my_se = { log = \"hi\" }\n",
        ),
        (
            "common/scripted_effects/caller.txt",
            "my_caller = {\n    my_se = yes\n}\n",
        ),
    ];
    // Cursor on the my_se call key (line 1, col 4).
    let locs = goto_def(
        GOTO_RULES,
        &[],
        files,
        "common/scripted_effects/caller.txt",
        1,
        4,
    );
    assert!(
        locs.iter()
            .any(|(u, _)| u.ends_with("scripted_effects/e.txt")),
        "goto should resolve scripted_effect call inside a scripted_effect body, got: {:?}",
        locs
    );
}

// ── did_open re-validates open dependents (stale scripted_effect bug) ─────────

/// Read frames until a publishDiagnostics for a URI ending in `suffix` arrives
/// (after at least `min_skips` matching ones already seen), returning its codes.
/// Returns None on timeout.
fn diags_for(
    reader: &mut BufReader<std::process::ChildStdout>,
    suffix: &str,
    occurrence: usize,
) -> Option<Vec<String>> {
    let mut seen = 0usize;
    for _ in 0..2000 {
        let raw = read_frame(reader).ok()?;
        if raw.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if v["method"] == "textDocument/publishDiagnostics"
            && v["params"]["uri"]
                .as_str()
                .is_some_and(|u| u.ends_with(suffix))
        {
            seen += 1;
            if seen >= occurrence {
                return Some(
                    v["params"]["diagnostics"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|d| d["code"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
        }
    }
    None
}

/// Read frames until the `loadingBar` notification with `enable=false` arrives,
/// i.e. the workspace scan finished (index_ready is now set).
fn wait_for_scan_done(reader: &mut BufReader<std::process::ChildStdout>) {
    for _ in 0..5000 {
        let Ok(raw) = read_frame(reader) else { return };
        if raw.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
            && v["method"] == "loadingBar"
            && v["params"]["enable"] == serde_json::Value::Bool(false)
        {
            return;
        }
    }
}

#[test]
fn test_did_open_definition_clears_open_caller_stale_error() {
    // Caller B references scripted_effect my_se; the defining file A is opened
    // afterwards. Opening A must re-validate B so its "undefined" diagnostic
    // (CW263 — the call matches no `<scripted_effect>` alias until my_se is
    // indexed) clears without a manual re-save.
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    let vanilla = tempfile::tempdir().unwrap(); // empty dir → index marked complete
    std::fs::write(rules_dir.path().join("r.cwt"), GOTO_RULES).unwrap();

    // Only the caller exists on disk at first; the definition is added later.
    let b_rel = "common/decisions/b.txt";
    let b_path = ws.path().join(b_rel);
    std::fs::create_dir_all(b_path.parent().unwrap()).unwrap();
    std::fs::write(
        &b_path,
        "my_dec = {\n    complete_effect = {\n        my_se = yes\n    }\n}\n",
    )
    .unwrap();

    let ws_uri = format!("file://{}", ws.path().display());
    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let init = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": ws_uri,
            "capabilities": {},
            "initializationOptions": {
                "language": "hoi4",
                "rulesCache": rules_dir.path().to_string_lossy(),
                "vanilla": vanilla.path().to_string_lossy(),
            }
        }),
    );
    write_frame(&mut child, &init).unwrap();
    let _ = read_response(&mut reader);
    write_frame(
        &mut child,
        &jsonrpc_notification("initialized", serde_json::json!({})),
    )
    .unwrap();
    // Wait until the scan finishes so diagnostics are no longer deferred.
    wait_for_scan_done(&mut reader);

    // Open the caller; the definition is absent, so B shows CW263.
    let b_uri = format!("file://{}", b_path.display());
    write_frame(
        &mut child,
        &jsonrpc_notification(
            "textDocument/didOpen",
            serde_json::json!({"textDocument":{"uri":b_uri,"languageId":"hoi4","version":1,
                "text":"my_dec = {\n    complete_effect = {\n        my_se = yes\n    }\n}\n"}}),
        ),
    )
    .unwrap();
    let before = diags_for(&mut reader, "b.txt", 1).expect("B diagnostics");
    assert!(
        before.contains(&"CW263".to_string()),
        "expected CW263 before the definition is opened, got: {:?}",
        before
    );

    // Now create + open the defining scripted_effect file.
    let a_rel = "common/scripted_effects/a.txt";
    let a_path = ws.path().join(a_rel);
    std::fs::create_dir_all(a_path.parent().unwrap()).unwrap();
    std::fs::write(&a_path, "my_se = { log = \"hi\" }\n").unwrap();
    let a_uri = format!("file://{}", a_path.display());
    write_frame(
        &mut child,
        &jsonrpc_notification(
            "textDocument/didOpen",
            serde_json::json!({"textDocument":{"uri":a_uri,"languageId":"hoi4","version":1,
                "text":"my_se = { log = \"hi\" }\n"}}),
        ),
    )
    .unwrap();

    // The did_open dependent sweep must re-publish B without the CW263.
    let after = diags_for(&mut reader, "b.txt", 1).expect("B re-validated");
    child.kill().ok();
    assert!(
        !after.contains(&"CW263".to_string()),
        "opening the definition file should clear B's stale CW263, got: {:?}",
        after
    );
}

#[test]
fn test_rescan_prunes_deleted_file_from_index() {
    // Definition file A holds scripted_effect my_se; caller B references it.
    // A is deleted from disk (no watcher event — e.g. deleted while the server
    // wasn't watching, or the file was never open) and a rescan is forced via
    // `clearAllCaches`. The rescan re-indexes what's still on disk (just B) but
    // must also PRUNE A's now-stale entries, or my_se keeps "resolving" forever
    // and B's CW263 never comes back.
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    let vanilla = tempfile::tempdir().unwrap(); // empty dir → index marked complete
    let cache_dir = tempfile::tempdir().unwrap(); // isolate clearAllCaches from the real cache
    std::fs::write(rules_dir.path().join("r.cwt"), GOTO_RULES).unwrap();

    let a_rel = "common/scripted_effects/a.txt";
    let a_path = ws.path().join(a_rel);
    std::fs::create_dir_all(a_path.parent().unwrap()).unwrap();
    std::fs::write(&a_path, "my_se = { log = \"hi\" }\n").unwrap();

    let b_rel = "common/decisions/b.txt";
    let b_path = ws.path().join(b_rel);
    std::fs::create_dir_all(b_path.parent().unwrap()).unwrap();
    std::fs::write(
        &b_path,
        "my_dec = {\n    complete_effect = {\n        my_se = yes\n    }\n}\n",
    )
    .unwrap();

    let ws_uri = format!("file://{}", ws.path().display());
    let mut child = cwtools_server_cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let init = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": ws_uri,
            "capabilities": {},
            "initializationOptions": {
                "language": "hoi4",
                "rulesCache": rules_dir.path().to_string_lossy(),
                "vanilla": vanilla.path().to_string_lossy(),
                "cacheDir": cache_dir.path().to_string_lossy(),
            }
        }),
    );
    write_frame(&mut child, &init).unwrap();
    let _ = read_response(&mut reader);
    write_frame(
        &mut child,
        &jsonrpc_notification("initialized", serde_json::json!({})),
    )
    .unwrap();

    // Both files exist on disk for the initial scan, so my_se resolves.
    let before = diags_for(&mut reader, "b.txt", 1).expect("B diagnostics before delete");
    assert!(
        !before.contains(&"CW263".to_string()),
        "expected my_se to resolve while A exists, got: {:?}",
        before
    );

    // Delete the definition, then force a rescan (no file watcher in this test).
    std::fs::remove_file(&a_path).unwrap();
    write_frame(
        &mut child,
        &jsonrpc_request(
            2,
            "workspace/executeCommand",
            serde_json::json!({"command": "clearAllCaches", "arguments": []}),
        ),
    )
    .unwrap();

    let after = diags_for(&mut reader, "b.txt", 1).expect("B diagnostics after rescan");
    child.kill().ok();
    assert!(
        after.contains(&"CW263".to_string()),
        "deleting A should resurrect B's CW263 once the rescan prunes it, got: {:?}",
        after
    );
}

// ── Periodic background reindex ───────────────────────────────────────────

/// Read frames until a `publishDiagnostics` for a URI ending in `suffix`
/// arrives whose codes no longer include `missing_code`. Fails immediately if
/// a `loadingBar` notification is observed along the way — a quiet background
/// pass must never touch the status bar, unlike the startup scan or
/// `clearAllCaches`. Returns `Err` on a stray loadingBar or on timeout.
fn wait_for_cleared_diag_quiet(
    reader: &mut BufReader<std::process::ChildStdout>,
    suffix: &str,
    missing_code: &str,
) -> Result<(), String> {
    for _ in 0..10_000 {
        let raw = read_frame(reader).map_err(|e| format!("read error: {e}"))?;
        if raw.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if v["method"] == "loadingBar" {
            return Err(format!(
                "unexpected loadingBar notification during quiet background pass: {v}"
            ));
        }
        if v["method"] == "textDocument/publishDiagnostics"
            && v["params"]["uri"]
                .as_str()
                .is_some_and(|u| u.ends_with(suffix))
        {
            let codes: Vec<String> = v["params"]["diagnostics"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d["code"].as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if !codes.iter().any(|c| c == missing_code) {
                return Ok(());
            }
        }
    }
    Err(format!(
        "timed out waiting for {suffix} diagnostics without {missing_code}"
    ))
}

#[test]
fn test_background_reindex_picks_up_new_file_quietly() {
    // The periodic background pass (CWTOOLS_REINDEX_INTERVAL_SECS=1,
    // CWTOOLS_REINDEX_IDLE_SECS=0 so it fires almost immediately once the
    // interval elapses) must discover a file created directly on disk — no
    // didOpen, no didChangeWatchedFiles notification over stdio — the same
    // way a real file-watcher gap would (a git checkout that raced the
    // watcher, or a file appearing while the window had no focus). It must
    // also run quiet: unlike the startup scan or `clearAllCaches`, no
    // loadingBar notification should reach the client.
    let ws = tempfile::tempdir().unwrap();
    let rules_dir = tempfile::tempdir().unwrap();
    let vanilla = tempfile::tempdir().unwrap(); // empty dir → index marked complete
    std::fs::write(rules_dir.path().join("r.cwt"), GOTO_RULES).unwrap();

    // Only the caller exists on disk at first; the definition is added later,
    // directly on disk, simulating a watcher-missed change.
    let b_rel = "common/decisions/b.txt";
    let b_path = ws.path().join(b_rel);
    std::fs::create_dir_all(b_path.parent().unwrap()).unwrap();
    std::fs::write(
        &b_path,
        "my_dec = {\n    complete_effect = {\n        my_se = yes\n    }\n}\n",
    )
    .unwrap();

    let ws_uri = format!("file://{}", ws.path().display());
    let mut child = cwtools_server_cmd()
        .env("CWTOOLS_REINDEX_INTERVAL_SECS", "1")
        .env("CWTOOLS_REINDEX_IDLE_SECS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let init = jsonrpc_request(
        1,
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": ws_uri,
            "capabilities": {},
            "initializationOptions": {
                "language": "hoi4",
                "rulesCache": rules_dir.path().to_string_lossy(),
                "vanilla": vanilla.path().to_string_lossy(),
            }
        }),
    );
    write_frame(&mut child, &init).unwrap();
    let _ = read_response(&mut reader);
    write_frame(
        &mut child,
        &jsonrpc_notification("initialized", serde_json::json!({})),
    )
    .unwrap();
    // The startup scan runs non-quiet; drain its loadingBar traffic before the
    // quiet-observation window starts.
    wait_for_scan_done(&mut reader);

    // Open the caller; the definition is absent, so B shows CW263.
    let b_uri = format!("file://{}", b_path.display());
    write_frame(
        &mut child,
        &jsonrpc_notification(
            "textDocument/didOpen",
            serde_json::json!({"textDocument":{"uri":b_uri,"languageId":"hoi4","version":1,
                "text":"my_dec = {\n    complete_effect = {\n        my_se = yes\n    }\n}\n"}}),
        ),
    )
    .unwrap();
    let before = diags_for(&mut reader, "b.txt", 1).expect("B diagnostics before background pass");
    assert!(
        before.contains(&"CW263".to_string()),
        "expected CW263 before the definition exists, got: {:?}",
        before
    );

    // Create the defining file directly on disk — no didOpen, no
    // didChangeWatchedFiles notification. Only the periodic background
    // pass's own filesystem walk can find it.
    let a_rel = "common/scripted_effects/a.txt";
    let a_path = ws.path().join(a_rel);
    std::fs::create_dir_all(a_path.parent().unwrap()).unwrap();
    std::fs::write(&a_path, "my_se = { log = \"hi\" }\n").unwrap();

    let result = wait_for_cleared_diag_quiet(&mut reader, "b.txt", "CW263");
    child.kill().ok();
    if let Err(e) = result {
        panic!("{e}");
    }
}
