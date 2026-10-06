#!/bin/sh
# Rebuilds fixtures/scip/*.scip with the real indexers. Needs rust-analyzer, scip-typescript and
# scip-python on PATH and a built CLI (cargo build --release). Run it from rust-cli/.
# The tests only read the .scip files, so they do not need the indexers.
set -e
here=$(cd "$(dirname "$0")" && pwd)
cli=${CLI:-"$here/../target/release/codebase-context-graph"}
work=$(mktemp -d)
for pair in ts:fixture-ts:scip-typescript py:fixture-py:scip-python rust:fixture-rust:rust-analyzer; do
  dir=${pair%%:*}; rest=${pair#*:}; name=${rest%%:*}; indexer=${rest#*:}
  cp -r "$here/$dir" "$work/$name"
  "$cli" index --project-root "$work/$name"
  cp "$work/$name/.codebase-context/scip/$indexer.scip" "$here/scip/$dir.scip"
done
rm -rf "$work"
