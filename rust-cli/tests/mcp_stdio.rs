//! The real binary, over real pipes, the way an AI client runs it.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

const BINARY: &str = env!("CARGO_BIN_EXE_codebase-context-graph");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A copy of the TypeScript fixture, not yet indexed. The indexers are switched off in its
/// config, so the index comes only from the `.scip` file the real indexer wrote.
fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new().prefix("ccg-stdio-").tempdir().unwrap();
    let root = dir.path().join("fixture-ts");
    copy_tree(&fixtures().join("ts"), &root);
    std::fs::create_dir_all(root.join(".codebase-context")).unwrap();
    std::fs::write(
        root.join(".codebase-context/config.json"),
        r#"{"version":"2.0","project_name":"fixture-ts","ignore_patterns":[],"respect_gitignore":true,
            "indexer_timeout_secs":60,"indexers":[{"name":"rust-analyzer","enabled":false},
            {"name":"scip-typescript","enabled":false},{"name":"scip-python","enabled":false}]}"#,
    )
    .unwrap();
    (dir, root)
}

fn index(root: &Path) {
    let status = Command::new(BINARY)
        .args(["index", "--project-root"])
        .arg(root)
        .arg("--scip")
        .arg(fixtures().join("scip/ts.scip"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "indexing the fixture failed");
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Session {
    fn start(root: &Path) -> Session {
        let mut child = Command::new(BINARY)
            .args(["mcp", "--project-root"])
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Session { child, stdin, lines }
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn send(&mut self, message: &Value) {
        self.send_bytes(message.to_string().as_bytes());
    }

    /// The next line the server wrote, which must be one JSON-RPC message and nothing else.
    fn recv(&mut self) -> Value {
        let line = self.lines.recv_timeout(Duration::from_secs(30)).expect("the server answered");
        let value: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout held something that is not JSON ({e}): {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0", "{line}");
        value
    }

    fn assert_silent(&mut self) {
        match self.lines.recv_timeout(Duration::from_millis(400)) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("the server wrote something it should not have: {other:?}"),
        }
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let reply = self.recv();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    fn call(&mut self, id: i64, tool: &str, arguments: Value) -> Value {
        let reply = self.request(id, "tools/call", json!({"name": tool, "arguments": arguments}));
        reply["result"].clone()
    }

    fn initialize(&mut self) {
        let reply = self.request(1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}));
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// Closes the input, as a client does when it quits, and waits for the server to leave.
    fn finish(mut self) -> String {
        drop(self.stdin.take());
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "the server did not exit when its input closed");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "exit status {status}");
        let mut stderr = String::new();
        self.child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        stderr
    }
}

fn answer(result: &Value) -> Value {
    assert_eq!(result["isError"], json!(false), "{}", result["content"][0]["text"]);
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn a_whole_session_with_the_real_binary() {
    let (_dir, root) = project();
    index(&root);
    let mut session = Session::start(&root);

    // Nothing but ping works before the handshake.
    assert_eq!(session.request(100, "ping", json!({}))["result"], json!({}));
    assert_eq!(session.request(101, "tools/list", json!({}))["error"]["code"], -32002);
    session.initialize();
    session.assert_silent(); // a notification is never answered

    let listed = session.request(2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in ["overview", "find_symbols", "file_outline", "symbol_detail", "trace", "read_source"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    for tool in tools {
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
    }

    // The flow the instructions describe: overview, find, detail, trace, read.
    let overview = answer(&session.call(3, "overview", json!({})));
    assert_eq!(overview["fresh"], true);
    assert_eq!(overview["files"], 12);
    assert_eq!(overview["uncovered"]["rows"][0][0], "scripts/helper.js");

    let found = answer(&session.call(4, "find_symbols", json!({"query": "render", "kind": "METHOD"})));
    let rows = found["symbols"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{found}");
    let chart_render = rows.iter().find(|r| r[3] == "src/shapes.ts" && r[0].as_str().unwrap().contains("Chart#render")).unwrap();
    let id = chart_render[0].as_str().unwrap();
    let (file, start, end) = (chart_render[3].as_str().unwrap(), chart_render[4].as_i64().unwrap(), chart_render[5].as_i64().unwrap());

    let detail = answer(&session.call(5, "symbol_detail", json!({"id": id})));
    assert_eq!(detail["name"], "render");
    assert_eq!(detail["container"], "npm:fixture-ts:src/`shapes.ts`/Chart#");
    assert!(detail["dependents"]["total"].as_i64().unwrap() >= 1);

    let traced = answer(&session.call(6, "trace", json!({"id": id, "direction": "dependents", "depth": 2})));
    let callers: Vec<&str> = traced["nodes"]["rows"].as_array().unwrap().iter().map(|r| r[6].as_str().unwrap()).collect();
    assert!(callers.contains(&"draw"), "{callers:?}");

    let source = answer(&session.call(7, "read_source", json!({"path": file, "start": start, "end": end})));
    let text = source["lines"]["rows"].to_string();
    assert!(text.contains("render(): string") && text.contains(&format!("{start}: ")), "{text}");

    // Mistakes are answered, and the server carries on.
    let refused = session.call(8, "find_symbols", json!({"query": "x", "limit": 5}));
    assert_eq!(refused["isError"], true);
    assert!(refused["content"][0]["text"].as_str().unwrap().contains("unknown parameter `limit`"));
    assert_eq!(session.request(9, "tools/call", json!({"name": "nope"}))["error"]["code"], -32602);
    assert_eq!(session.request(10, "no/such", json!({}))["error"]["code"], -32601);

    session.send_bytes(b"this is not json");
    let broken = session.recv();
    assert_eq!((broken["error"]["code"].as_i64(), &broken["id"]), (Some(-32700), &Value::Null));
    session.send_bytes(&[0xff, 0xfe, 0xfd]);
    assert_eq!(session.recv()["error"]["code"], -32700);
    session.send_bytes(b"");
    session.assert_silent();

    // A line far past the limit is refused without being held in memory, and the next one works.
    let huge = vec![b'a'; 3 * 1024 * 1024];
    session.send_bytes(&huge);
    let too_large = session.recv();
    assert_eq!(too_large["error"]["code"], -32600);
    assert!(too_large["error"]["message"].as_str().unwrap().contains("larger than"));
    assert_eq!(session.request(11, "ping", json!({}))["result"], json!({}));

    let stderr = session.finish();
    assert!(stderr.contains("serving"), "{stderr}");
}

#[test]
fn an_index_that_appears_or_is_rebuilt_while_the_server_runs_is_picked_up() {
    let (_dir, root) = project();
    let mut session = Session::start(&root);
    session.initialize();

    // No index yet: every tool says exactly what to run.
    let result = session.call(2, "overview", json!({}));
    assert_eq!(result["isError"], true);
    let message = result["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("no index at") && message.contains("codebase-context-graph index --project-root"), "{message}");

    // Another process builds it; the next call works with no restart.
    index(&root);
    let first = answer(&session.call(3, "overview", json!({})));
    assert_eq!(first["files"], 12);

    // And again: the index is rebuilt in place by another process while this one is connected.
    std::fs::write(root.join("src/extra.ts"), "export const extra = 1;\n").unwrap();
    assert_eq!(answer(&session.call(4, "overview", json!({})))["changes"]["rows"], json!([["src/extra.ts", "new"]]));
    std::thread::sleep(Duration::from_millis(1100)); // so the two generation times differ
    index(&root);
    let second = answer(&session.call(5, "overview", json!({})));
    assert_eq!(second["files"], 13);
    assert_eq!(second["fresh"], true);
    assert_ne!(first["as_of"], second["as_of"]);
    session.finish();
}

#[test]
fn a_client_that_hangs_up_mid_message_does_not_hang_the_server() {
    let (_dir, root) = project();
    index(&root);
    let mut session = Session::start(&root);
    session.initialize();
    // Half a message and then the end of input, with no newline.
    session.stdin.as_mut().unwrap().write_all(br#"{"jsonrpc":"2.0","id":9,"method":"pi"#).unwrap();
    session.stdin.as_mut().unwrap().flush().unwrap();
    let reply = {
        drop(session.stdin.take());
        session.recv()
    };
    assert_eq!(reply["error"]["code"], -32700);
    let started = std::time::Instant::now();
    while session.child.try_wait().unwrap().is_none() {
        assert!(started.elapsed() < Duration::from_secs(20), "the server did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(session.child.wait().unwrap().success());
}
