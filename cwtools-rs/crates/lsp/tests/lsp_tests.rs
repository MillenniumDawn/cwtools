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
