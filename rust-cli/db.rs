use crate::graph::{FileRecord, Graph};
use crate::indexers::RunReport;
use rusqlite::{Connection, params};
use std::path::Path;

/// Bump when the table layout changes. The database is derived data, so a mismatch
/// simply rebuilds it (the next `index` run repopulates everything).
pub const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS file_manifest (
    file_path TEXT PRIMARY KEY,
    language TEXT NOT NULL,
    size INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    lines INTEGER NOT NULL,
    indexed_by TEXT
);

CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    name TEXT NOT NULL,
    file_path TEXT,
    start_line INTEGER,
    end_line INTEGER,
    language TEXT,
    metadata TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id TEXT NOT NULL REFERENCES nodes(id),
    target_id TEXT NOT NULL REFERENCES nodes(id),
    type TEXT NOT NULL,
    metadata TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS index_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    indexer TEXT NOT NULL,
    root TEXT NOT NULL,
    status TEXT NOT NULL,
    message TEXT NOT NULL,
    tool TEXT NOT NULL,
    documents INTEGER NOT NULL,
    occurrences INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    output TEXT
);

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_id);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_id);
CREATE INDEX IF NOT EXISTS idx_edges_type ON edges(type);
CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(type);
CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_path);
CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name COLLATE NOCASE);
";

pub fn open_database(db_path: &Path) -> Result<Connection, rusqlite::Error> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let db = Connection::open(db_path)?;
    init_schema(&db)?;
    Ok(db)
}

pub fn init_schema(db: &Connection) -> Result<(), rusqlite::Error> {
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        // Includes the `communities` and `file_hashes` tables of the previous layout.
        db.execute_batch(
            "DROP TABLE IF EXISTS edges;
             DROP TABLE IF EXISTS nodes;
             DROP TABLE IF EXISTS communities;
             DROP TABLE IF EXISTS file_hashes;
             DROP TABLE IF EXISTS file_manifest;
             DROP TABLE IF EXISTS index_runs;
             DROP TABLE IF EXISTS meta;",
        )?;
    }
    db.execute_batch(SCHEMA)?;
    db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Replaces the whole contents in one transaction: either the new graph is fully there
/// or the previous one is untouched.
pub fn replace_all(
    db: &Connection,
    graph: &Graph,
    files: &[FileRecord],
    runs: &[RunReport],
    meta: &[(&str, String)],
) -> Result<(), rusqlite::Error> {
    let tx = db.unchecked_transaction()?;
    tx.execute_batch(
        "DELETE FROM edges; DELETE FROM nodes; DELETE FROM file_manifest;
         DELETE FROM index_runs; DELETE FROM meta;",
    )?;

    {
        let mut insert = tx.prepare(
            "INSERT INTO nodes (id, type, name, file_path, start_line, end_line, language, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for n in &graph.nodes {
            insert.execute(params![
                n.id,
                n.node_type,
                n.name,
                n.file_path,
                n.start_line,
                n.end_line,
                n.language,
                n.metadata.to_string()
            ])?;
        }
    }
    {
        let mut insert = tx.prepare(
            "INSERT INTO edges (source_id, target_id, type, metadata) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for e in &graph.edges {
            insert.execute(params![e.source_id, e.target_id, e.edge_type, e.metadata.to_string()])?;
        }
    }
    {
        let mut insert = tx.prepare(
            "INSERT INTO file_manifest (file_path, language, size, content_hash, lines, indexed_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for f in files {
            insert.execute(params![
                f.file.path,
                f.file.language,
                f.file.size as i64,
                f.file.hash,
                f.file.lines,
                f.indexed_by
            ])?;
        }
    }
    {
        let mut insert = tx.prepare(
            "INSERT INTO index_runs
                 (indexer, root, status, message, tool, documents, occurrences, duration_ms, output)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for r in runs {
            insert.execute(params![
                r.indexer,
                r.root,
                r.status.as_str(),
                r.message,
                r.tool,
                r.documents as i64,
                r.occurrences as i64,
                r.duration_ms as i64,
                r.output
            ])?;
        }
    }
    for (key, value) in meta {
        tx.execute("INSERT INTO meta (key, value) VALUES (?1, ?2)", params![key, value])?;
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Node};
    use crate::indexers::RunStatus;
    use crate::scanner::SourceFile;
    use serde_json::json;

    fn node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            node_type: "FUNCTION".to_string(),
            name: id.to_string(),
            file_path: None,
            start_line: None,
            end_line: None,
            language: None,
            metadata: json!({}),
        }
    }

    fn edge(source: &str, target: &str) -> Edge {
        Edge {
            source_id: source.to_string(),
            target_id: target.to_string(),
            edge_type: "CALLS".to_string(),
            metadata: json!({"count": 1}),
        }
    }

    fn memory_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        init_schema(&db).unwrap();
        db
    }

    fn count(db: &Connection, table: &str) -> i64 {
        db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn replace_all_stores_everything_and_replaces_on_the_next_run() {
        let db = memory_db();
        let graph = Graph {
            nodes: vec![node("a"), node("b")],
            edges: vec![edge("a", "b")],
        };
        let files = vec![FileRecord {
            file: SourceFile {
                path: "a.rs".into(),
                language: "rust".into(),
                size: 3,
                hash: "h".into(),
                lines: 1,
            },
            indexed_by: None,
        }];
        let runs = vec![RunReport {
            indexer: "rust-analyzer".into(),
            root: "".into(),
            status: RunStatus::Ok,
            message: String::new(),
            tool: "rust-analyzer 1".into(),
            documents: 1,
            occurrences: 2,
            duration_ms: 5,
            output: None,
        }];
        replace_all(&db, &graph, &files, &runs, &[("generated_at", "now".into())]).unwrap();
        assert_eq!(
            (count(&db, "nodes"), count(&db, "edges"), count(&db, "file_manifest"), count(&db, "index_runs"), count(&db, "meta")),
            (2, 1, 1, 1, 1)
        );

        let smaller = Graph { nodes: vec![node("c")], edges: vec![] };
        replace_all(&db, &smaller, &[], &[], &[]).unwrap();
        assert_eq!((count(&db, "nodes"), count(&db, "edges"), count(&db, "file_manifest")), (1, 0, 0));
    }

    #[test]
    fn a_failed_replace_leaves_the_previous_graph_untouched() {
        let db = memory_db();
        let good = Graph { nodes: vec![node("a")], edges: vec![] };
        replace_all(&db, &good, &[], &[], &[]).unwrap();

        // The edge points at a node that does not exist: the foreign key rejects it.
        let broken = Graph { nodes: vec![node("x")], edges: vec![edge("x", "missing")] };
        assert!(replace_all(&db, &broken, &[], &[], &[]).is_err());

        let ids: Vec<String> = db
            .prepare("SELECT id FROM nodes")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn a_database_from_the_previous_layout_is_rebuilt_instead_of_misread() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE nodes (id TEXT PRIMARY KEY, type TEXT, name TEXT);
             CREATE TABLE communities (id INTEGER PRIMARY KEY, label TEXT);
             CREATE TABLE file_hashes (file_path TEXT PRIMARY KEY);
             INSERT INTO nodes VALUES ('old', 'FUNCTION', 'old');",
        )
        .unwrap();
        init_schema(&db).unwrap();

        assert_eq!(count(&db, "nodes"), 0, "old rows are gone");
        let has = |table: &str| -> bool {
            db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [table], |r| r.get::<_, i64>(0)).unwrap() > 0
        };
        assert!(!has("communities") && !has("file_hashes"));
        assert!(has("index_runs") && has("meta"));
        db.execute("INSERT INTO nodes (id, type, name, metadata) VALUES ('n', 'FILE', 'n', '{}')", []).unwrap();
    }

    #[test]
    fn opening_the_same_database_twice_keeps_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.db");
        {
            let db = open_database(&path).unwrap();
            replace_all(&db, &Graph { nodes: vec![node("kept")], edges: vec![] }, &[], &[], &[]).unwrap();
        }
        let db = open_database(&path).unwrap();
        assert_eq!(count(&db, "nodes"), 1);
    }
}
