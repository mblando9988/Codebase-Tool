# Test fixtures

Three tiny projects (`ts`, `py`, `rust`) and the SCIP indexes the real indexers produced for them
(`scip/*.scip`). The tests index these without running any indexer, so they work on a machine that
has none installed.

The projects contain the cases a happy-path test would miss: mutually recursive functions, a
function called from many places, a five-step call chain, a duplicate `main`, a file no indexer
covers (`ts/scripts/helper.js`), non-ASCII identifiers, CRLF line endings, a 600-character line,
a file without a trailing newline, and documentation and signatures long enough to be clipped.

If you change a source file here, run `./regenerate.sh` so the `.scip` files match it. The tests
compare every location the tools report with the source text, so a change that moves a definition
fails them until the `.scip` files are regenerated.
