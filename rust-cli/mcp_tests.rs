//! Tests of the framework itself, using tools built to misbehave in every way a tool could.
//! If the framework did not enforce its rules, these tools would get through.

use crate::config;
use crate::db;
use crate::mcp::*;
use serde_json::{Value, json};
use std::io::Cursor;

// ------------------------------------------------------------------------------------------
// Helpers.
// ------------------------------------------------------------------------------------------

fn request(server: &mut Server, method: &str, params: Value) -> Value {
    server
        .handle_message(json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}))
        .expect("a request gets a reply")
}

fn initialize(server: &mut Server, version: &str) -> Value {
    request(server, "initialize", json!({"protocolVersion": version, "capabilities": {}}))
}

fn is_error(result: &Value) -> bool {
    result["isError"] == json!(true)
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().expect("a result has text")
}

fn answer(result: &Value) -> Value {
    assert!(!is_error(result), "unexpected error: {}", text(result));
    serde_json::from_str(text(result)).expect("the text is JSON")
}

/// A project directory with an empty index, one indexed file (`a.txt`) and a generation time.
fn bare_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let db = db::open_database(&config::database_path(dir.path())).unwrap();
    db.execute("INSERT INTO meta (key, value) VALUES ('generated_at', '2026-01-01T00:00:00Z')", []).unwrap();
    db.execute(
        "INSERT INTO file_manifest (file_path, language, size, content_hash, lines, indexed_by)
         VALUES ('a.txt', 'text', 6, ?1, 1, 'test')",
        [file_hash(b"hello\n")],
    )
    .unwrap();
    dir
}

const NO_HEAD: &[Field] = &[];
const COLUMNS: &[Column] = &[col("id", "string"), col("file", "string"), col("text", "string")];
const ID_FILE: &[Column] = &[col("id", "string"), col("file", "string")];
const PAIR: &[Column] = &[col("id", "string"), col("text", "string")];
const ROWS: &[TableSpec] = &[TableSpec { name: "rows", columns: COLUMNS }];
const TWO: &[TableSpec] = &[TableSpec { name: "left", columns: PAIR }, TableSpec { name: "right", columns: PAIR }];
const NEEDS_FIELD: &[Field] = &[Field { name: "needed", ty: "string" }];
const INTEGER_FIELD: &[Field] = &[Field { name: "count", ty: "integer" }];

type Run = fn(&Ctx, &Args) -> Result<Doc, ToolError>;

fn tool(name: &'static str, paginated: bool, head: &'static [Field], tables: &'static [TableSpec], run: Run) -> ToolDef {
    ToolDef {
        name,
        title: "test tool",
        description: "a tool made for a test",
        params: vec![
            Param::int("count", 0, 5000, "rows that exist").default(100),
            Param::int("width", 0, 5000, "length of the text cells").default(10),
        ],
        paginated,
        head,
        tables,
        run,
    }
}

fn rows_from(offset: i64, window: i64, total: i64, width: usize, file: &str) -> Vec<Vec<Value>> {
    (offset..total)
        .take(window as usize)
        .map(|i| vec![json!(format!("id{i}")), json!(file), json!("x".repeat(width))])
        .collect()
}

fn table(rows: Vec<Vec<Value>>, total: i64) -> Table {
    Table { name: "rows", columns: COLUMNS, rows, total }
}

fn good(ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let (count, width) = (args.int("count"), args.int("width") as usize);
    Ok(Doc { head: vec![], tables: vec![table(rows_from(ctx.offset, ctx.window, count, width, "a.txt"), count)], notes: vec![] })
}

fn undeclared_field(ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let mut doc = good(ctx, args)?;
    doc.head.push(("surprise", json!(1)));
    Ok(doc)
}

fn missing_field(ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    good(ctx, args)
}

fn wrong_columns(ctx: &Ctx, _args: &Args) -> Result<Doc, ToolError> {
    Ok(Doc {
        head: vec![],
        tables: vec![Table { name: "rows", columns: ID_FILE, rows: vec![], total: 0 }],
        notes: vec![ctx.offset.to_string()],
    })
}

fn wrong_width(_ctx: &Ctx, _args: &Args) -> Result<Doc, ToolError> {
    Ok(Doc { head: vec![], tables: vec![table(vec![vec![json!("id"), json!("a.txt")]], 1)], notes: vec![] })
}

fn too_many_rows(_ctx: &Ctx, _args: &Args) -> Result<Doc, ToolError> {
    Ok(Doc { head: vec![], tables: vec![table(rows_from(0, 10_000, 301, 1, "a.txt"), 301)], notes: vec![] })
}

fn total_lies(_ctx: &Ctx, _args: &Args) -> Result<Doc, ToolError> {
    Ok(Doc { head: vec![], tables: vec![table(rows_from(0, 5, 5, 1, "a.txt"), 2)], notes: vec![] })
}

fn panics(_ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let _ = args.int("not_declared"); // the accessor panics for a name nobody declared
    unreachable!()
}

fn wrong_type(_ctx: &Ctx, _args: &Args) -> Result<Doc, ToolError> {
    Ok(Doc { head: vec![("count", json!("many"))], tables: vec![table(vec![], 0)], notes: vec![] })
}

fn long_handles(_ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let width = args.int("width") as usize;
    let row = vec![json!("i".repeat(width)), json!("p".repeat(width)), json!("t".repeat(width))];
    Ok(Doc { head: vec![], tables: vec![table(vec![row], 1)], notes: vec![] })
}

fn two_tables(_ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let (count, width) = (args.int("count"), args.int("width") as usize);
    let make = |name: &'static str| Table {
        name,
        columns: PAIR,
        rows: (0..count.min(300)).map(|i| vec![json!(format!("{name}{i}")), json!("x".repeat(width))]).collect(),
        total: count,
    };
    Ok(Doc { head: vec![], tables: vec![make("left"), make("right")], notes: vec![] })
}

fn names_files(ctx: &Ctx, _args: &Args) -> Result<Doc, ToolError> {
    let rows = vec![
        vec![json!("1"), json!("a.txt"), json!("")],
        vec![json!("2"), json!("ghost.txt"), json!("")],
        vec![json!("3"), json!("../../etc/passwd"), json!("")],
    ];
    let _ = ctx;
    Ok(Doc { head: vec![], tables: vec![table(rows, 3)], notes: vec![] })
}

/// Tries every way of changing the index through the connection the server gives a tool.
fn tries_to_write(ctx: &Ctx, _args: &Args) -> Result<Doc, ToolError> {
    let first = ctx.db.execute("INSERT INTO meta (key, value) VALUES ('hacked', '1')", []);
    let _ = ctx.db.execute_batch("PRAGMA query_only = OFF");
    let second = ctx.db.execute("INSERT INTO meta (key, value) VALUES ('hacked', '2')", []);
    let dropped = ctx.db.execute_batch("DROP TABLE nodes");
    let attached = ctx.db.execute_batch("ATTACH DATABASE ':memory:' AS other; CREATE TABLE other.t (a)");
    if first.is_ok() || second.is_ok() || dropped.is_ok() || attached.is_ok() {
        return Err(ToolError::Invalid("a write got through".into()));
    }
    Ok(Doc { head: vec![], tables: vec![table(vec![], 0)], notes: vec![] })
}

fn misbehaving() -> (tempfile::TempDir, Server) {
    let dir = bare_project();
    let tools = vec![
        tool("good", true, NO_HEAD, ROWS, good),
        tool("undeclared_field", true, NO_HEAD, ROWS, undeclared_field),
        tool("missing_field", true, NEEDS_FIELD, ROWS, missing_field),
        tool("wrong_columns", true, NO_HEAD, ROWS, wrong_columns),
        tool("wrong_width", true, NO_HEAD, ROWS, wrong_width),
        tool("too_many_rows", true, NO_HEAD, ROWS, too_many_rows),
        tool("total_lies", true, NO_HEAD, ROWS, total_lies),
        tool("panics", true, NO_HEAD, ROWS, panics),
        tool("wrong_type", true, INTEGER_FIELD, ROWS, wrong_type),
        tool("long_handles", true, NO_HEAD, ROWS, long_handles),
        tool("two_tables", false, NO_HEAD, TWO, two_tables),
        tool("names_files", true, NO_HEAD, ROWS, names_files),
        tool("tries_to_write", true, NO_HEAD, ROWS, tries_to_write),
    ];
    let mut server = Server::new(dir.path(), tools);
    initialize(&mut server, "2025-06-18");
    (dir, server)
}

// ------------------------------------------------------------------------------------------
// Protocol.
// ------------------------------------------------------------------------------------------

#[test]
fn the_protocol_version_is_negotiated_down_never_up() {
    for version in SUPPORTED_VERSIONS {
        let mut server = Server::new(bare_project().path(), vec![]);
        let reply = initialize(&mut server, version);
        assert_eq!(reply["result"]["protocolVersion"], *version);
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(reply["result"]["capabilities"]["tools"]["listChanged"], false);
        assert!(reply["result"].get("resources").is_none());
    }
    let mut server = Server::new(bare_project().path(), vec![]);
    let reply = initialize(&mut server, "2099-01-01");
    assert_eq!(reply["result"]["protocolVersion"], SUPPORTED_VERSIONS[0], "an unknown version gets the newest supported");
    let reply = request(&mut server, "initialize", json!({}));
    assert_eq!(reply["error"]["code"], -32602);
    let reply = request(&mut server, "initialize", json!({"protocolVersion": 20250618}));
    assert_eq!(reply["error"]["code"], -32602, "the version must be a string");

    let mut old = Server::new(bare_project().path(), vec![]);
    assert!(initialize(&mut old, "2024-11-05")["result"].get("instructions").is_none());
    let mut new = Server::new(bare_project().path(), vec![]);
    assert!(new_instructions(&mut new).len() > 100);
}

fn new_instructions(server: &mut Server) -> String {
    initialize(server, "2025-06-18")["result"]["instructions"].as_str().unwrap().to_string()
}

#[test]
fn nothing_but_ping_works_before_initialize() {
    let mut server = Server::new(bare_project().path(), crate::tools::registry());
    assert_eq!(request(&mut server, "ping", json!({}))["result"], json!({}));
    for method in ["tools/list", "tools/call"] {
        let reply = request(&mut server, method, json!({"name": "overview"}));
        assert_eq!(reply["error"]["code"], -32002, "{method}");
    }
    initialize(&mut server, "2025-06-18");
    assert!(request(&mut server, "tools/list", json!({}))["result"]["tools"].is_array());
}

#[test]
fn notifications_never_get_a_reply() {
    let mut server = Server::new(bare_project().path(), crate::tools::registry());
    for method in ["notifications/initialized", "notifications/cancelled", "notifications/whatever", "ping", "tools/list"] {
        assert!(
            server.handle_message(json!({"jsonrpc": "2.0", "method": method})).is_none(),
            "{method} sent as a notification must not be answered"
        );
    }
    // The same method names sent as requests are answered.
    assert!(server.handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})).is_some());
}

#[test]
fn ids_are_echoed_exactly_and_bad_ones_are_refused() {
    let mut server = Server::new(bare_project().path(), vec![]);
    for id in [json!(0), json!(-5), json!("abc"), json!(""), json!(1.5), json!(9_007_199_254_740_993i64)] {
        let reply = server.handle_message(json!({"jsonrpc": "2.0", "id": id, "method": "ping"})).unwrap();
        assert_eq!(reply["id"], id);
    }
    for id in [json!(null), json!(true), json!([1]), json!({"a": 1})] {
        let reply = server.handle_message(json!({"jsonrpc": "2.0", "id": id, "method": "ping"})).unwrap();
        assert_eq!(reply["error"]["code"], -32600, "{id}");
        assert_eq!(reply["id"], Value::Null);
    }
}

#[test]
fn malformed_messages_get_the_right_errors() {
    let mut server = Server::new(bare_project().path(), vec![]);
    let reply = server.handle_line(b"{not json").unwrap();
    assert_eq!((reply["error"]["code"].as_i64(), &reply["id"]), (Some(-32700), &Value::Null));
    let reply = server.handle_line(&[0xff, 0xfe, b'{']).unwrap();
    assert_eq!(reply["error"]["code"], -32700, "invalid UTF-8");
    assert!(server.handle_line(b"   \t ").is_none(), "blank lines are ignored");
    assert!(server.handle_line(b"").is_none());

    for (message, code) in [
        (json!(42), -32600),
        (json!("text"), -32600),
        (json!(null), -32600),
        (json!([]), -32600),
        (json!({"id": 1, "method": "ping"}), -32600),
        (json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}), -32600),
        (json!({"jsonrpc": "2.0", "id": 1}), -32600),
        (json!({"jsonrpc": "2.0", "id": 1, "method": 5}), -32600),
        (json!({"jsonrpc": "2.0", "id": 1, "method": "nope/never"}), -32601),
    ] {
        let reply = server.handle_message(message.clone()).unwrap_or_else(|| panic!("{message} must be answered"));
        assert_eq!(reply["error"]["code"], code, "{message}");
        assert_eq!(reply["jsonrpc"], "2.0");
    }
    // A response to something this server never asked is ignored, not answered.
    assert!(server.handle_message(json!({"jsonrpc": "2.0", "id": 1, "result": {}})).is_none());
    assert!(server.handle_message(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": 1, "message": "x"}})).is_none());
}

#[test]
fn batches_are_answered_in_one_array_without_the_notifications() {
    let mut server = Server::new(bare_project().path(), vec![]);
    let reply = server
        .handle_message(json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": "b", "method": "nope"},
            5
        ]))
        .unwrap();
    let replies = reply.as_array().unwrap();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["error"]["code"], -32601);
    assert_eq!(replies[2]["error"]["code"], -32600);
    assert_eq!(server.handle_message(json!([])).unwrap()["error"]["code"], -32600);
    assert!(server.handle_message(json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}])).is_none());
}

#[test]
fn what_tools_list_publishes_depends_on_the_negotiated_version() {
    let shape = |version: &str| {
        let mut server = Server::new(bare_project().path(), crate::tools::registry());
        initialize(&mut server, version);
        request(&mut server, "tools/list", json!({}))["result"]["tools"].clone()
    };
    let (old, middle, newest) = (shape("2024-11-05"), shape("2025-03-26"), shape("2025-06-18"));
    for tool in old.as_array().unwrap() {
        assert!(tool.get("annotations").is_none() && tool.get("title").is_none() && tool.get("outputSchema").is_none());
    }
    for tool in middle.as_array().unwrap() {
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert!(tool.get("outputSchema").is_none());
    }
    for tool in newest.as_array().unwrap() {
        assert!(tool["title"].is_string());
        assert_eq!(tool["outputSchema"]["additionalProperties"], false);
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }
    let mut server = Server::new(bare_project().path(), vec![]);
    initialize(&mut server, "2025-06-18");
    let reply = request(&mut server, "tools/list", json!({"cursor": "x"}));
    assert_eq!(reply["error"]["code"], -32602, "there is only one page");
}

#[test]
fn tools_call_refuses_malformed_requests_at_the_protocol_level() {
    let (_dir, mut server) = misbehaving();
    for (params, why) in [
        (json!({}), "no name"),
        (json!({"name": 5}), "name is not a string"),
        (json!({"name": "no_such_tool"}), "unknown tool"),
        (json!({"name": "good", "arguments": [1]}), "arguments is an array"),
        (json!({"name": "good", "arguments": "x"}), "arguments is a string"),
        (json!({"name": "good", "arguments": 3}), "arguments is a number"),
    ] {
        let reply = request(&mut server, "tools/call", params);
        assert_eq!(reply["error"]["code"], -32602, "{why}");
    }
    let reply = request(&mut server, "tools/call", json!({"name": "good", "arguments": null}));
    assert!(reply["result"]["isError"] == json!(false), "null arguments mean none");
}

// ------------------------------------------------------------------------------------------
// Input validation.
// ------------------------------------------------------------------------------------------

#[test]
fn every_argument_is_checked_against_the_published_schema() {
    let (_dir, mut server) = misbehaving();
    let budget = json!(DEFAULT_BUDGET_TOKENS);
    let cases: Vec<(Value, &str)> = vec![
        (json!({"count": "5"}), "count"),
        (json!({"count": 5.5}), "count"),
        (json!({"count": 5.0}), "count"),
        (json!({"count": null}), "count"),
        (json!({"count": true}), "count"),
        (json!({"count": [5]}), "count"),
        (json!({"count": -1}), "count"),
        (json!({"count": 5001}), "count"),
        (json!({"count": 9223372036854775807i64}), "count"),
        (json!({"extra": 1}), "unknown parameter `extra`"),
        (json!({"offset": -1}), "offset"),
        (json!({"offset": 10_000_001}), "offset"),
        (json!({"offset": "0"}), "offset"),
        (json!({"budget_tokens": MIN_BUDGET_TOKENS - 1}), "budget_tokens"),
        (json!({"budget_tokens": MAX_BUDGET_TOKENS + 1}), "budget_tokens"),
        (json!({"budget_tokens": 0}), "budget_tokens"),
        (json!({"budget_tokens": "big"}), "budget_tokens"),
        (json!({"budget_tokens": budget, "Count": 1}), "unknown parameter `Count`"),
    ];
    for (arguments, expected) in cases {
        let result = server.call_tool("good", &arguments);
        assert!(is_error(&result), "{arguments} must be refused");
        assert!(text(&result).contains(expected), "{arguments}: {}", text(&result));
        assert!(text(&result).contains("usage: good("), "the error shows how to call the tool: {}", text(&result));
        assert!(text(&result).len() < 1500);
    }
    // Both ends of every range are accepted.
    for arguments in [
        json!({"count": 0, "width": 0, "offset": 0, "budget_tokens": MIN_BUDGET_TOKENS}),
        json!({"count": 5000, "width": 5000, "offset": 10_000_000, "budget_tokens": MAX_BUDGET_TOKENS}),
    ] {
        let result = server.call_tool("good", &arguments);
        assert!(!is_error(&result), "{arguments}: {}", text(&result));
    }
}

#[test]
fn defaults_are_applied_and_a_nul_character_is_refused() {
    let (_dir, mut server) = misbehaving();
    let a = answer(&server.call_tool("good", &json!({})));
    assert_eq!(a["rows"]["total"], 100, "count defaults to 100");
    assert_eq!(a["rows"]["rows"][0][2], "xxxxxxxxxx", "width defaults to 10");
    assert!(!is_error(&server.call_tool("long_handles", &json!({"count": 1, "width": 1, "offset": 0}))));

    // A tool with a text parameter: the real ones are covered by the conformance tests; this
    // checks the framework rule itself.
    let mut tools = crate::tools::registry();
    tools.truncate(2);
    let dir = bare_project();
    let mut real = Server::new(dir.path(), tools);
    initialize(&mut real, "2025-06-18");
    let result = real.call_tool("find_symbols", &json!({"query": "a\u{0}b"}));
    assert!(is_error(&result) && text(&result).contains("NUL"), "{}", text(&result));
}

// ------------------------------------------------------------------------------------------
// The budget.
// ------------------------------------------------------------------------------------------

#[test]
fn answers_are_cut_to_the_budget_and_say_how_to_continue() {
    let (_dir, mut server) = misbehaving();
    for budget in [MIN_BUDGET_TOKENS, DEFAULT_BUDGET_TOKENS, MAX_BUDGET_TOKENS] {
        let (mut seen, mut offset, mut pages) = (Vec::new(), 0i64, 0);
        loop {
            let result = server.call_tool(
                "good",
                &json!({"count": 1000, "width": 40, "offset": offset, "budget_tokens": budget}),
            );
            assert!(text(&result).len() <= budget as usize * BYTES_PER_TOKEN, "page {pages} of budget {budget}");
            let a = answer(&result);
            let t = &a["rows"];
            assert_eq!(t["total"], 1000);
            let rows = t["rows"].as_array().unwrap();
            assert_eq!(t["shown"], rows.len());
            seen.extend(rows.iter().map(|r| r[0].as_str().unwrap().to_string()));
            pages += 1;
            match t["next_offset"].as_i64() {
                Some(next) => {
                    assert!(!rows.is_empty(), "a page with a continuation must make progress");
                    assert_eq!(next, offset + rows.len() as i64);
                    offset = next;
                }
                None => {
                    assert_eq!(offset + rows.len() as i64, 1000, "the last page ends at the total");
                    break;
                }
            }
            assert!(pages < 2000);
        }
        assert!(pages > 1, "budget {budget} should need several pages for 1000 rows");
        let expected: Vec<String> = (0..1000).map(|i| format!("id{i}")).collect();
        assert_eq!(seen, expected, "pages must join up with no gap and no repeat (budget {budget})");
    }
    // A larger budget needs fewer pages.
    let pages_for = |server: &mut Server, budget: i64| {
        let (mut offset, mut pages) = (0, 0);
        loop {
            let a = answer(&server.call_tool("good", &json!({"count": 1000, "width": 40, "offset": offset, "budget_tokens": budget})));
            pages += 1;
            match a["rows"]["next_offset"].as_i64() {
                Some(n) => offset = n,
                None => return pages,
            }
        }
    };
    assert!(pages_for(&mut server, MAX_BUDGET_TOKENS) < pages_for(&mut server, MIN_BUDGET_TOKENS));
}

#[test]
fn an_offset_past_the_end_is_an_empty_page_that_says_so() {
    let (_dir, mut server) = misbehaving();
    let a = answer(&server.call_tool("good", &json!({"count": 10, "offset": 50})));
    assert_eq!((a["rows"]["shown"].as_i64(), a["rows"]["total"].as_i64()), (Some(0), Some(10)));
    assert_eq!(a["rows"]["next_offset"], Value::Null);
    assert!(a["notes"][0].as_str().unwrap().contains("offset 50 is past the last row"));
    let a = answer(&server.call_tool("good", &json!({"count": 10, "offset": 10})));
    assert_eq!(a["rows"]["next_offset"], Value::Null, "an offset exactly at the end is also empty");
    let a = answer(&server.call_tool("good", &json!({"count": 0})));
    assert_eq!((a["rows"]["shown"].as_i64(), a["rows"]["total"].as_i64()), (Some(0), Some(0)));
}

#[test]
fn long_values_are_clipped_and_counted_never_dropped_silently() {
    let (_dir, mut server) = misbehaving();
    let a = answer(&server.call_tool("good", &json!({"count": 5, "width": 5000})));
    assert_eq!(a["clipped"], 5, "five text cells were clipped");
    for row in a["rows"]["rows"].as_array().unwrap() {
        let cell = row[2].as_str().unwrap();
        assert!(cell.ends_with('…'));
        assert!(serde_json::to_string(cell).unwrap().len() <= CELL_CLIP);
    }
    // Ids and paths have larger limits, and the answer says that they can no longer be used.
    let a = answer(&server.call_tool("long_handles", &json!({"width": 5000})));
    let row = &a["rows"]["rows"][0];
    assert!(serde_json::to_string(row[0].as_str().unwrap()).unwrap().len() <= ID_CLIP);
    assert!(serde_json::to_string(row[1].as_str().unwrap()).unwrap().len() <= PATH_CLIP);
    assert_eq!(a["clipped"], 3);
    assert!(a["notes"][0].as_str().unwrap().contains("cannot be passed to other tools"), "{}", a["notes"]);
    // Short values are left alone, and nothing is reported.
    let a = answer(&server.call_tool("long_handles", &json!({"width": 20})));
    assert!(a.get("clipped").is_none() && a.get("notes").is_none());
}

#[test]
fn a_tool_cannot_return_more_than_it_declared_or_was_allowed() {
    let (_dir, mut server) = misbehaving();
    for (name, expected) in [
        ("undeclared_field", "undeclared field `surprise`"),
        ("missing_field", "declared field `needed` is missing"),
        ("wrong_columns", "does not match its declaration"),
        ("wrong_width", "wrong width"),
        ("too_many_rows", "more than 300 rows"),
        ("total_lies", "fewer rows in total than it returned"),
        ("wrong_type", "breaks its own output schema"),
    ] {
        let result = server.call_tool(name, &json!({}));
        assert!(is_error(&result), "{name} must not get through");
        assert!(text(&result).starts_with("internal error:"), "{name}: {}", text(&result));
        assert!(text(&result).contains(expected), "{name}: {}", text(&result));
    }
}

#[test]
fn a_panicking_tool_is_an_error_and_the_server_carries_on() {
    let (_dir, mut server) = misbehaving();
    let result = server.call_tool("panics", &json!({}));
    assert!(is_error(&result) && text(&result).contains("panicked"), "{}", text(&result));
    assert!(!is_error(&server.call_tool("good", &json!({}))));
}

#[test]
fn a_row_that_cannot_fit_is_an_error_not_an_overflow() {
    let (_dir, mut server) = misbehaving();
    let args = json!({"count": 5, "width": 1000});
    let result = server.call_tool_with_budget_bytes("good", &args, 300);
    assert!(is_error(&result) && text(&result).contains("could not make progress"), "{}", text(&result));
    let result = server.call_tool_with_budget_bytes("good", &args, 20);
    assert!(is_error(&result) && text(&result).contains("does not fit the budget"), "{}", text(&result));
    // Just enough for the fixed part and one row.
    let result = server.call_tool_with_budget_bytes("good", &args, 600);
    let a = answer(&result);
    assert!(text(&result).len() <= 600 && a["rows"]["shown"].as_i64().unwrap() >= 1);
}

#[test]
fn tables_share_the_budget_and_truncation_is_stated() {
    let (_dir, mut server) = misbehaving();
    let result = server.call_tool("two_tables", &json!({"count": 300, "width": 60, "budget_tokens": MIN_BUDGET_TOKENS}));
    let a = answer(&result);
    assert!(text(&result).len() <= MIN_BUDGET_TOKENS as usize * BYTES_PER_TOKEN);
    let (left, right) = (a["left"]["shown"].as_i64().unwrap(), a["right"]["shown"].as_i64().unwrap());
    assert!(left > 10 && right > 10 && (left - right).abs() <= 2, "a fair split: {left} and {right}");
    assert_eq!(a["left"]["next_offset"], Value::Null, "a table that is not paginated cannot be continued");
    let notes = a["notes"].to_string();
    assert!(notes.contains("`left` shows") && notes.contains("`right` shows") && notes.contains("raise budget_tokens"), "{notes}");

    // When everything fits there is nothing to report.
    let a = answer(&server.call_tool("two_tables", &json!({"count": 3, "width": 5})));
    assert_eq!((a["left"]["shown"].as_i64(), a["right"]["shown"].as_i64()), (Some(3), Some(3)));
    assert!(a.get("notes").is_none());

    // One table that needs little leaves its share to the other.
    let a = answer(&server.call_tool("two_tables", &json!({"count": 300, "width": 60, "budget_tokens": MAX_BUDGET_TOKENS})));
    assert!(a["left"]["shown"].as_i64().unwrap() > 60);
}

#[test]
fn clipping_by_json_size_holds_for_any_characters() {
    // A small deterministic generator, so the cases are the same on every run.
    let mut state = 0x2545F4914F6CDD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let alphabet: Vec<char> = "ab\"\\\n\t\r\u{1}\u{8}\u{c}\u{1f}\u{7f}é日👋\u{2028}\u{0}".chars().collect();
    for _ in 0..4000 {
        let length = (next() % 300) as usize;
        let original: String = (0..length).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
        let limit = 5 + (next() % 300) as usize;
        let json_size = |s: &str| serde_json::to_string(s).unwrap().len();
        match clip_json(&original, limit) {
            None => assert!(json_size(&original) <= limit, "left alone but too big"),
            Some(clipped) => {
                assert!(json_size(&original) > limit, "clipped although it fitted");
                assert!(json_size(&clipped) <= limit, "{} > {limit}", json_size(&clipped));
                let body = clipped.strip_suffix('…').expect("a clipped value ends in an ellipsis");
                assert!(original.starts_with(body), "what is kept is a prefix");
                if let Some(next_char) = original[body.len()..].chars().next() {
                    let longer = format!("{body}{next_char}…");
                    assert!(json_size(&longer) > limit, "clipped more than necessary");
                }
            }
        }
    }
    assert_eq!(clip_json("abc", 5), None);
    assert_eq!(clip_json("abcd", 5).as_deref(), Some("…"));
}

// ------------------------------------------------------------------------------------------
// Freshness, the index and the transport.
// ------------------------------------------------------------------------------------------

#[test]
fn files_changed_since_indexing_are_reported_in_every_answer() {
    let dir = bare_project();
    let mut server = Server::new(dir.path(), vec![tool("names_files", true, NO_HEAD, ROWS, names_files)]);
    initialize(&mut server, "2025-06-18");
    let a = answer(&server.call_tool("names_files", &json!({})));
    assert!(a.get("stale_files").is_none() && a.get("notes").is_none(), "unchanged and unknown files are not reported");

    std::fs::write(dir.path().join("a.txt"), "hello, changed\n").unwrap();
    let a = answer(&server.call_tool("names_files", &json!({})));
    assert_eq!(a["stale_files"], json!(["a.txt"]));
    assert!(a["notes"][0].as_str().unwrap().contains("1 file(s) changed and 0 missing"), "{}", a["notes"]);

    std::fs::remove_file(dir.path().join("a.txt")).unwrap();
    let a = answer(&server.call_tool("names_files", &json!({})));
    assert_eq!(a["stale_files"], json!(["a.txt"]));
    assert!(a["notes"][0].as_str().unwrap().contains("0 file(s) changed and 1 missing"), "{}", a["notes"]);

    // The same content again is fresh again, whatever happened in between.
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    assert!(answer(&server.call_tool("names_files", &json!({}))).get("stale_files").is_none());
}

#[test]
fn file_states_only_ever_reads_files_that_are_in_the_index() {
    let dir = bare_project();
    std::fs::write(dir.path().join("secret.txt"), "not indexed").unwrap();
    let db = open_read_only(&config::database_path(dir.path())).unwrap();
    let paths: Vec<String> = ["a.txt", "secret.txt", "../outside", "/etc/passwd", ""].iter().map(|s| s.to_string()).collect();
    assert!(file_states(&db, dir.path(), &paths).is_empty());
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    assert_eq!(file_states(&db, dir.path(), &paths), vec![("a.txt".to_string(), "changed")]);
}

#[test]
fn the_index_is_open_read_only_whatever_a_tool_tries() {
    let dir = bare_project();
    let path = config::database_path(dir.path());
    let before = file_hash(&std::fs::read(&path).unwrap());
    let db = open_read_only(&path).unwrap();
    for sql in [
        "INSERT INTO meta (key, value) VALUES ('x', 'y')",
        "DELETE FROM meta",
        "UPDATE meta SET value = 'z'",
        "CREATE TABLE t (a)",
        "DROP TABLE meta",
        "PRAGMA user_version = 99",
    ] {
        assert!(db.execute_batch(sql).is_err(), "{sql} must fail");
    }
    // Even after switching the pragma off, the connection itself is read-only.
    db.execute_batch("PRAGMA query_only = OFF").unwrap();
    assert!(db.execute_batch("INSERT INTO meta (key, value) VALUES ('x', 'y')").is_err());
    drop(db);
    assert_eq!(file_hash(&std::fs::read(&path).unwrap()), before, "the index file is unchanged");
}

#[test]
fn the_connection_a_tool_receives_cannot_write() {
    let (dir, mut server) = misbehaving();
    let path = config::database_path(dir.path());
    let before = file_hash(&std::fs::read(&path).unwrap());
    let result = server.call_tool("tries_to_write", &json!({}));
    assert!(!is_error(&result), "a write got through, or the tool failed: {}", text(&result));
    assert_eq!(file_hash(&std::fs::read(&path).unwrap()), before);
}

#[test]
fn a_missing_or_old_index_gives_an_error_that_says_what_to_run() {
    let empty = tempfile::tempdir().unwrap();
    let mut server = Server::new(empty.path(), crate::tools::registry());
    initialize(&mut server, "2025-06-18");
    for name in ["overview", "find_symbols", "file_outline", "symbol_detail", "trace", "read_source"] {
        let arguments = match name {
            "find_symbols" => json!({"query": "x"}),
            "file_outline" => json!({"path": "a"}),
            "symbol_detail" => json!({"id": "a"}),
            "trace" => json!({"id": "a", "direction": "dependents"}),
            "read_source" => json!({"path": "a", "start": 1, "end": 1}),
            _ => json!({}),
        };
        let result = server.call_tool(name, &arguments);
        assert!(is_error(&result), "{name}");
        let message = text(&result);
        assert!(message.contains("no index at") && message.contains("codebase-context-graph index --project-root"), "{name}: {message}");
    }
    // The tool list works without an index, so a client can still connect.
    assert_eq!(request(&mut server, "tools/list", json!({}))["result"]["tools"].as_array().unwrap().len(), 6);

    let dir = bare_project();
    let db = db::open_database(&config::database_path(dir.path())).unwrap();
    db.pragma_update(None, "user_version", 1).unwrap();
    drop(db);
    let mut server = Server::new(dir.path(), crate::tools::registry());
    initialize(&mut server, "2025-06-18");
    let result = server.call_tool("overview", &json!({}));
    assert!(is_error(&result) && text(&result).contains("layout version 1"), "{}", text(&result));
}

#[test]
fn an_index_that_is_deleted_and_rebuilt_is_picked_up_without_a_restart() {
    let dir = bare_project();
    let mut server = Server::new(dir.path(), vec![tool("good", true, NO_HEAD, ROWS, good)]);
    initialize(&mut server, "2025-06-18");
    assert_eq!(answer(&server.call_tool("good", &json!({})))["as_of"], "2026-01-01T00:00:00Z");

    let path = config::database_path(dir.path());
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let db = db::open_database(&path).unwrap();
    db.execute("INSERT INTO meta (key, value) VALUES ('generated_at', '2027-07-07T07:07:07Z')", []).unwrap();
    drop(db);
    assert_eq!(answer(&server.call_tool("good", &json!({})))["as_of"], "2027-07-07T07:07:07Z");

    std::fs::remove_file(&path).unwrap();
    let result = server.call_tool("good", &json!({}));
    assert!(is_error(&result) && text(&result).contains("no index at"), "a deleted index is not served from memory");
}

#[test]
fn read_line_never_holds_more_than_its_limit_and_recovers() {
    use crate::mcp::Line::{Eof, Message, TooLarge};
    let read_all = |bytes: &[u8], max: usize| {
        let mut input = Cursor::new(bytes.to_vec());
        let mut out = Vec::new();
        loop {
            match read_line(&mut input, max).unwrap() {
                Eof => return out,
                Message(m) => out.push(Some(String::from_utf8_lossy(&m).into_owned())),
                TooLarge => out.push(None),
            }
        }
    };
    let some = |s: &str| Some(s.to_string());
    assert_eq!(read_all(b"abc\ndef", 10), vec![some("abc"), some("def")]);
    assert_eq!(read_all(b"abc\r\ndef\r\n", 10), vec![some("abc"), some("def")]);
    assert_eq!(read_all(b"", 10), Vec::<Option<String>>::new());
    assert_eq!(read_all(b"\n\n", 10), vec![some(""), some("")]);
    assert_eq!(read_all(b"0123456789\nnext\n", 10), vec![some("0123456789"), some("next")], "exactly at the limit");
    assert_eq!(read_all(b"0123456789A\nnext\n", 10), vec![None, some("next")], "one byte over");
    assert_eq!(read_all(b"0123456789ABCDEF", 10), vec![None], "too long and never ends");
    let mut big = vec![b'a'; 5_000_000];
    big.extend_from_slice(b"\nafter\n");
    assert_eq!(read_all(&big, MAX_REQUEST_BYTES), vec![None, some("after")]);
}

#[test]
fn a_whole_session_over_the_transport() {
    let dir = bare_project();
    let mut server = Server::new(dir.path(), vec![tool("good", true, NO_HEAD, ROWS, good)]);
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"good","arguments":{"count":3}}}"#,
        "garbage",
        r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#,
    ]
    .join("\n");
    let mut output = Vec::new();
    serve(&mut server, Cursor::new(input.into_bytes()), &mut output).unwrap();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("every output line is one JSON message"))
        .collect();
    assert_eq!(lines.len(), 5, "initialize, tools/list, tools/call, the parse error, ping");
    assert_eq!(lines[0]["id"], 1);
    assert_eq!(lines[1]["result"]["tools"][0]["name"], "good");
    assert_eq!(answer(&lines[2]["result"])["rows"]["total"], 3);
    assert_eq!(lines[3]["error"]["code"], -32700);
    assert_eq!(lines[4]["id"], 4);
    for line in &lines {
        assert_eq!(line["jsonrpc"], "2.0");
    }
}

// ------------------------------------------------------------------------------------------
// Declarations, and the proof that the smallest budget is always enough.
// ------------------------------------------------------------------------------------------

#[test]
fn mistakes_in_a_tool_declaration_are_caught() {
    let mut bad = tool("has space", true, NO_HEAD, TWO, good);
    bad.params.push(Param::int("offset", 0, 5, "reserved"));
    bad.params.push(Param::int("silent", 0, 5, ""));
    bad.params.push(Param::int("defaulted", 0, 5, "help").default(9));
    let problems = bad.declaration_problems().join("\n");
    for expected in [
        "not a valid tool name",
        "exactly one table",
        "`offset` is added by the framework",
        "parameter `silent` has no help text",
        "default of `defaulted` breaks its own schema",
    ] {
        assert!(problems.contains(expected), "missing {expected:?} in:\n{problems}");
    }
    const CLASH: &[Field] = &[Field { name: "notes", ty: "string" }];
    let reserved = tool("ok", false, CLASH, ROWS, good).declaration_problems().join("\n");
    assert!(reserved.contains("`notes` is reserved or declared twice"), "{reserved}");
    let doubled: &'static [Field] = &[Field { name: "a", ty: "string" }, Field { name: "a", ty: "string" }];
    assert!(!tool("ok", false, doubled, ROWS, good).declaration_problems().is_empty());
    assert!(tool("fine", true, NO_HEAD, ROWS, good).declaration_problems().is_empty());
}

#[test]
fn the_registered_tools_are_declared_correctly_and_cost_little_context() {
    let tools = crate::tools::registry();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name).collect();
    for t in &tools {
        let problems = t.declaration_problems();
        assert!(problems.is_empty(), "{}: {problems:?}", t.name);
    }
    names.sort();
    names.dedup();
    assert_eq!(names.len(), tools.len(), "tool names are unique");
    // What a client keeps in its context for the whole session just to know the tools exist:
    // the names, descriptions and input schemas. The output schemas are for programs, and
    // most clients never show them to the model.
    let mut server = Server::new(bare_project().path(), tools);
    initialize(&mut server, "2025-06-18");
    let listing = request(&mut server, "tools/list", json!({}))["result"].clone();
    let model_facing: usize = listing["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].to_string().len() + t["description"].to_string().len() + t["inputSchema"].to_string().len())
        .sum();
    println!("tools/list: {} bytes in all, {model_facing} bytes of names, descriptions and inputs", listing.to_string().len());
    assert!(model_facing <= 7_500, "the tools cost the model {model_facing} bytes of context");
    assert!(listing.to_string().len() <= 20_000, "the whole listing is {} bytes", listing.to_string().len());
}

/// The largest content each declared type can hold: text far past every limit and made of
/// characters that double in size when escaped, and numbers as wide as numbers get.
fn worst_value(ty: &str) -> Value {
    let nasty = "\"".repeat(5000);
    match ty.split('|').next().unwrap() {
        "integer" => json!(i64::MAX),
        "boolean" => json!(true),
        "array" => json!([i64::MAX, i64::MAX, i64::MAX, i64::MAX, i64::MAX]),
        _ => json!(nasty),
    }
}

fn worst_case(def: &ToolDef) -> Doc {
    Doc {
        head: def.head.iter().map(|f| (f.name, worst_value(f.ty))).collect(),
        tables: def
            .tables
            .iter()
            .map(|t| Table {
                name: t.name,
                columns: t.columns,
                rows: (0..3).map(|_| t.columns.iter().map(|c| worst_value(c.ty)).collect()).collect(),
                total: 1_000_000,
            })
            .collect(),
        notes: vec!["\"".repeat(5000); 10],
    }
}

#[test]
fn even_the_worst_possible_content_fits_the_smallest_budget() {
    for def in crate::tools::registry() {
        let stale: Vec<(String, &'static str)> = (0..8).map(|_| ("\"".repeat(5000), "changed")).collect();
        let value = render(
            &def,
            worst_case(&def),
            0,
            MIN_BUDGET_TOKENS as usize * BYTES_PER_TOKEN,
            &"2026-01-01T00:00:00.000000000+00:00".repeat(2),
            &stale,
            vec!["\"".repeat(5000)],
        )
        .unwrap_or_else(|e| panic!("{}: {}", def.name, e.text()));
        assert!(value.to_string().len() <= MIN_BUDGET_TOKENS as usize * BYTES_PER_TOKEN, "{}", def.name);
        if def.paginated {
            assert!(value[def.tables[0].name]["shown"].as_i64().unwrap() >= 1, "{}: paging must make progress", def.name);
        }
    }
}
