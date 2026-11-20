# Core parity inventory generator

Builds [`crates/codesmith-core/PARITY.md`](../../../crates/codesmith-core/PARITY.md):
every test in OpenCode's `packages/core/test`, bucketed as ported, ported
after a fix, missing feature, by design, not yet ported or found bug.

```sh
# once: the TypeScript compiler, for parsing the test files (nothing else)
npm install --prefix /tmp/parity-ts typescript@5.6.3
NODE_PATH=/tmp/parity-ts/node_modules scripts/parity/core/run.sh ~/src/opencode
```

The OpenCode checkout should be the revision the inventory was made
against; intermediate files go to `target/parity-core/`.

The steps (`run.sh` runs them in order):

1. `extract.js` parses each `*.test.ts` with the TypeScript compiler and
   lists every test site with its case count (`.each` rows and passes of
   `for` loops over literal lists count once each).
2. `match.js` matches the crate's existing `oc_*` tests to OpenCode tests by
   name (a score of 0.3 or more counts). Tests in `tests/oc_*.rs` are not
   matched; `rules/zz_ports.txt` maps them.
3. `classify.js` applies `rules/*.txt` and exits non-zero if any test is
   left unclassified (`node classify.js target/parity-core 'regex'` lists
   them for files matching `regex`).
4. `render.js` writes PARITY.md.

## Rules

One file per area. Under an `@<test file>` header, each line is
`selector :: bucket :: ref :: note`:

- selector: `*` (the file's default), `/regex/` on the test name, or line
  numbers of test sites, `730:72` taking 72 of that site's cases (the counts
  of one site must add up);
- bucket: `ported` or `fixed` (ref: `<file>::<test fn>`), `portable` (not
  yet ported; ref: what to drive), `feature` (ref: the roadmap group),
  `design` (ref: `deliberate`, `effect-internal`, `storage-model`,
  `client-server`, `cloud`, `platform`, `differs` or `other`) or `bug`.

Line rules beat name matches, which beat regex and `*` rules; for the same
line, a later file (by name) overrides an earlier one. When you port a test,
add its line to `rules/zz_ports.txt` and rerun.
