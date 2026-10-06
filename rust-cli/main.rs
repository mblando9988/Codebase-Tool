mod config;
mod db;
mod graph;
mod indexer;
mod indexers;
mod mcp;
mod scanner;
mod schema;
mod symbols;
mod tools;

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod mcp_tests;

use std::path::PathBuf;
use std::process;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let command = &args[1];
    let project_root = get_project_root(&args);

    if args.iter().any(|a| a == "--analysis-mode") {
        eprintln!("note: --analysis-mode is ignored; indexing is always semantic now.");
    }

    let result: Result<(), Box<dyn std::error::Error>> = match command.as_str() {
        "init" => indexer::init_project(&project_root),
        "doctor" => indexer::doctor(&project_root),
        "index" => {
            let options = indexer::IndexOptions {
                scip_files: get_flag_values(&args, "--scip")
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
            };
            indexer::index_project(&project_root, &options)
        }
        "watch" => indexer::watch_project(&project_root),
        "smoke" => indexer::smoke_test(&project_root),
        "mcp" => mcp::run(&project_root),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => {
            eprintln!("Unknown command: {}", command);
            print_usage();
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        process::exit(1);
    }

    Ok(())
}

fn print_usage() {
    eprintln!("codebase-context-graph");
    eprintln!("  init   [--project-root <path>]   create .codebase-context/config.json (keeps an existing one)");
    eprintln!("  doctor [--project-root <path>]   check which SCIP indexers are installed");
    eprintln!("  index  [--project-root <path>] [--scip <file>]...   run the indexers and build the graph");
    eprintln!("  watch  [--project-root <path>]   runs one index (file watching is not implemented)");
    eprintln!("  smoke  [--project-root <path>]   check the database");
    eprintln!("  mcp    [--project-root <path>]   serve the graph to AI agents over MCP (stdio)");
    eprintln!("Indexers: rust-analyzer, scip-typescript, scip-python (see `doctor`).");
}

fn get_project_root(args: &[String]) -> PathBuf {
    let root = get_flag_values(args, "--project-root")
        .into_iter()
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    std::fs::canonicalize(&root).unwrap_or(root)
}

/// Every value that follows `flag`, for flags that may be repeated.
fn get_flag_values(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_can_be_repeated_and_are_read_in_order() {
        let a = args(&["x", "index", "--scip", "a.scip", "--project-root", "/p", "--scip", "b.scip"]);
        assert_eq!(get_flag_values(&a, "--scip"), vec!["a.scip", "b.scip"]);
        assert_eq!(get_flag_values(&a, "--project-root"), vec!["/p"]);
        assert!(get_flag_values(&a, "--missing").is_empty());
    }

    #[test]
    fn a_trailing_flag_without_a_value_is_ignored() {
        assert!(get_flag_values(&args(&["x", "index", "--scip"]), "--scip").is_empty());
    }
}
