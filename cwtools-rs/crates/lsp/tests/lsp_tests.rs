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

// ── Help ─────────────────────────────────────────────────────────────────────

#[test]
fn test_lsp_help_exits_zero() {
    assert_cmd::Command::cargo_bin("cwtools-server")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
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
