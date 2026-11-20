// Render PARITY.md from <out>/classified.jsonl.
// Usage: node render.js <out> <PARITY.md>
const fs = require("fs")
const rows = fs.readFileSync(process.argv[2] + "/classified.jsonl", "utf8").trim().split("\n").map(JSON.parse)
const areas = [
  ["Session runner", /^(session-runner|session-run-coordinator|session-execution|session-step|session-error|session-shell|session-skill|session-tool-progress|session-live-tool-metadata|session-generate|generate)\./],
  ["Subagents", /^(subagent-|tool-subagent|session-background)/],
  ["Compaction", /^(session-compaction|session-compact|session-native-compaction|config\/compaction)\./],
  ["Titles", /^session-title\./],
  ["Model requests and transport", /^(session-model-|session-checkpoint-transport|session-compaction-transport|session-native\.)/],
  ["Snapshots and undo", /^(snapshot|session-revert|session-diff)\./],
  ["Sessions and their store", /^(session-|bus-session-routing|plugin\/session-)/],
  ["Tool: read", /^tool-read/],
  ["Tool: edit", /^tool-edit/],
  ["Tool: write", /^(tool-write|file-mutation)\./],
  ["Tool: apply_patch", /^(patch|tool-patch)\./],
  ["Tools: glob and grep", /^(tool-search\.|ripgrep|filesystem\/search)/],
  ["Tool: webfetch", /^(tool-webfetch|tool-html-markdown-budget)/],
  ["Tool: websearch", /^(tool-websearch|websearch|plugin\/websearch)/],
  ["Tools: question, todo, skill", /^(tool-question|tool-todo|tool-skill)/],
  ["Tool output", /^tool-output/],
  ["Tool registry, schemas, placement", /^(tool-registry|tool-schema|tool-model-policy|plugin\/tool-input-repair)/],
  ["Media tools", /^(tool-image-generate|tool-video-generate|video-generation|voice|tool-media-target|image-photon|mime)/],
  ["Tool target environments", /^(tool-.*target-environment|tool-opencode)/],
  ["Shell tool and runtime", /^(tool-shell|shell\.|shell-result|shell-cleanup|shell-retention|shell-daemon|shell-restart|process\/|pty\/|persistent-pty)/],
  ["Shell scan and parse", /^(shell-scan\/|shell-parse)/],
  ["Permission", /^(permission|file-access|plugin\/permission-assert|config\/policy)/],
  ["Agents", /^(agent\.|config\/agent|plugin\/(plan|optimize|verbosity))/],
  ["Skills and commands", /^(skill|config\/skill|config\/markdown|plugin\/skill|command\.|config\/command|plugin\/command)/],
  ["Instructions and references", /^(instruction|instructions\/|reference|repository)/],
  ["Config", /^(config\/(?!provider|warming)|flag\.|variant\.)/],
  ["MCP", /^(mcp|tool-mcp-events|plugin\/mcp-codemode|form\.)/],
  ["Code Mode", /^(codemode|tool-execute)/],
  ["Automations", /^(automation|tool-automation|job\.)/],
  ["Formatters", /^formatter\./],
  ["Worktrees, VCS and projects", /^(worktree|git\.|vcs|project\.|plugin\/worktree|plugin\/vcs-init|util\/git-executable|workspace)/],
  ["Providers, models and credentials", /^(model|models|catalog|provider|aisdk|credential|plugin\/provider|github-copilot|modal\/|usage-limits|config\/(provider|warming)|plugin\/models-dev|plugin\/warming|npm|network-guard|oauth-page|integration)/],
  ["Plugins", /^(plugin\.|plugin-failure|plugin\/(supervisor|module|host-model|identity|internal-flag|promise-tool|goal))/],
  ["Storage, state and bus", /^(database|sqlite|state|storage-failure|kv\.|v1-migration|bus|shared-cache)/],
  ["Runtime, locations and filesystem", /./],
]
const areaOf = (f) => areas.find(([, re]) => re.test(f))[0]
for (const r of rows) r.area = areaOf(r.file)
const sum = (xs) => xs.reduce((a, r) => a + r.count, 0)
const B = { ported: "ported", fixed: "fixed", feature: "feature", design: "design", portable: "portable", bug: "bug" }
const rel = (ref) => ref.replace(/^tests\//, "tests/").replace(/^src\//, "src/")
const out = []
const p = (s = "") => out.push(s)

const totals = { all: sum(rows) }
for (const b of Object.keys(B)) totals[b] = sum(rows.filter((r) => r.bucket === b))
const files = new Set(rows.map((r) => r.file))
const sites = new Set(rows.map((r) => r.file + ":" + r.line))

p("# Core parity with OpenCode")
p()
p("CodeSmith's agent core (`codesmith-core`: the session loop, tools, permissions, shell scanning, config, compaction, MCP, automations, skills and commands) is pinned to OpenCode's `packages/core` by porting its unit tests (MIT) as Rust tests. Existing ports are the `oc_*` tests and those that cite an OpenCode test by name: inline in the modules (`src/tool/*.rs`, `src/permission.rs`, `src/config.rs`, `src/compaction.rs`, `src/retry.rs`, `src/runner.rs`, `src/title.rs`, `src/instructions.rs`, `src/skill.rs`, `src/command.rs`, `src/media.rs`), in [`tests/runner_parity.rs`](tests/runner_parity.rs), [`tests/parity.rs`](tests/parity.rs) and [`tests/websocket_session.rs`](tests/websocket_session.rs), and, added with this inventory, in [`tests/oc_tools.rs`](tests/oc_tools.rs), [`tests/oc_runner.rs`](tests/oc_runner.rs), [`tests/oc_compaction.rs`](tests/oc_compaction.rs) and [`tests/oc_config.rs`](tests/oc_config.rs). The provider protocols have their own matrix in [`codesmith-ai/PARITY.md`](../codesmith-ai/PARITY.md).")
p()
p(`Every test in \`packages/core/test\` was examined: ${files.size} files, ${sites.size} test sites, ${totals.all} test cases. Parametrized tests count once per generated case, as in the provider matrix: a \`test.each\` row, or a pass of a \`for\` loop over a literal list, is one case. (Cases come from the literal tables; a loop over a computed list counts once.) Each case is in one bucket:`)
p()
p("- **ported**: CodeSmith already behaved this way, and a CodeSmith test pins it.")
p("- **ported after a fix**: CodeSmith was wrong in a way that matters; it was fixed and the test ported.")
p("- **gap: missing feature**: the test needs a feature CodeSmith does not have (grouped in the [roadmap](#roadmap)).")
p("- **gap: by design / not applicable**: CodeSmith differs on purpose (a documented deliberate difference), or the test is about OpenCode's own machinery (Effect layers, its SQLite event store, its client/server Locations, hosted services, Windows/PowerShell, npm packages).")
p("- **gap: not yet ported**: CodeSmith has the behaviour (or its equivalent) but no test pins it yet; these need no new feature and are the [porting backlog](#porting-backlog).")
p("- **gap: found bug**: porting showed CodeSmith wrong and it is [recorded](#found-bugs-and-differences) rather than fixed.")
p()
p("How the buckets were decided: a test is **ported** only when a CodeSmith test named for it (or citing it) checks the same behaviour; \"adapted\" notes say where a port asserts CodeSmith's wording or message model instead of OpenCode's. **Not yet ported** is a judgement that CodeSmith has the behaviour, made by reading its code, not by running the test, so porting a backlog test can still turn up a bug (as the ports in this change did). Where one table mixes PowerShell and POSIX rows, the rows were counted from the table and split between the two buckets.")
p()
p("## Summary")
p()
p("| Area | Tests examined | Ported | Ported after a fix | Gap: missing feature | Gap: by design / n.a. | Gap: not yet ported | Gap: found bug |")
p("|---|---:|---:|---:|---:|---:|---:|---:|")
for (const [a] of areas) {
  const rs = rows.filter((r) => r.area === a)
  if (!rs.length) continue
  const c = (b) => sum(rs.filter((r) => r.bucket === b))
  p(`| ${a} | ${sum(rs)} | ${c("ported")} | ${c("fixed")} | ${c("feature")} | ${c("design")} | ${c("portable")} | ${c("bug")} |`)
}
p(`| **Total** | **${totals.all}** | **${totals.ported}** | **${totals.fixed}** | **${totals.feature}** | **${totals.design}** | **${totals.portable}** | **${totals.bug}** |`)
p()
p("OpenCode's `packages/core` has no LSP tests (CodeSmith's language-server client is tested on its own in `tests/lsp.rs`). Shell scan and parse is large because its tables expand into many cases; most of what is left there is PowerShell (by design: CodeSmith runs commands with `<shell> -c` and ports only the POSIX grammar) and Bash/Zsh table rows CodeSmith's scanner should already pass but no test runs yet.")
p()

// fixes
const fixed = rows.filter((r) => r.bucket === "fixed")
p("## Fixes made while porting")
p()
for (const r of fixed) p(`- \`${r.file}\`: "${r.name}" (${r.count} case${r.count > 1 ? "s" : ""}): ${r.note}. Pinned by \`${rel(r.ref)}\`.`)
p()

// bugs
const bugs = rows.filter((r) => r.bucket === "bug")
const differs = rows.filter((r) => r.bucket === "design" && r.ref === "differs")
p("## Found bugs and differences")
p()
if (!bugs.length && !differs.length) p("None open: every bug porting found is fixed above.")
else p("Recorded rather than fixed here; the repro tests are in the files named, `#[ignore]`d where they would fail.")
p()
for (const r of bugs) p(`- **Bug** (\`${r.ref}\`), \`${r.file}\` "${r.name}" (${r.count}): ${r.note}.`)
for (const r of differs) p(`- **Difference to decide**, \`${r.file}\` "${r.name}" (${r.count}): ${r.note}.`)
p()

// roadmap
const feat = {}
for (const r of rows.filter((r) => r.bucket === "feature")) (feat[r.ref] ??= []).push(r)
const groups = Object.entries(feat).sort((a, b) => sum(b[1]) - sum(a[1]))
p("## Roadmap")
p()
p(`Missing features, by how many OpenCode test cases each would unlock (${totals.feature} cases in ${groups.length} groups). Each group lists its tests as \`file\`: name.`)
p()
p("| Feature | Cases | What is missing |")
p("|---|---:|---|")
for (const [g, rs] of groups) {
  const notes = [...new Set(rs.map((r) => r.note))]
  p(`| [${g}](#${slug(g)}) | ${sum(rs)} | ${notes.slice(0, 2).join("; ")}${notes.length > 2 ? "; …" : ""} |`)
}
p()
for (const [g, rs] of groups) {
  p(`### ${g}`)
  p()
  p(`${sum(rs)} cases.`)
  p()
  for (const r of rs) p(`- \`${r.file}\`: ${esc(r.name)}${r.count > 1 ? ` (${r.count} cases)` : ""}`)
  p()
}

// backlog
p("## Porting backlog")
p()
p(`Tests for behaviour CodeSmith already has but no test pins (${totals.portable} cases), by area, with what to drive and assert. Sizes: S a few lines on an existing harness, M a new fixture or scripted model.`)
p()
for (const [a] of areas) {
  const rs = rows.filter((r) => r.area === a && r.bucket === "portable")
  if (!rs.length) continue
  p(`### ${a} (${sum(rs)})`)
  p()
  const byNote = {}
  for (const r of rs) (byNote[r.ref + " | " + r.note] ??= []).push(r)
  for (const [k, xs] of Object.entries(byNote)) {
    const [ref, note] = k.split(" | ")
    p(`- ${note} — \`${ref}\``)
    for (const r of xs) p(`  - \`${r.file}\`: ${esc(r.name)}${r.count > 1 ? ` (${r.count} cases)` : ""}`)
  }
  p()
}

// by design
p("## By design or not applicable")
p()
const dz = {}
for (const r of rows.filter((r) => r.bucket === "design")) (dz[r.ref] ??= []).push(r)
const label = {
  deliberate: "Deliberate differences, documented in the module headers (no steers, queues or session moves; subagents run inside the tool call; no live config reload or v1 config migration; compiled-in plugins; SearXNG-only web search; ...)",
  "effect-internal": "OpenCode internals with no counterpart: Effect layers, services, fibers and schema types",
  "storage-model": "OpenCode's SQLite/drizzle event store, projector rows and bus (CodeSmith has its own session store)",
  "client-server": "OpenCode's client/server architecture: Locations, instances, workspaces, RPC and API shapes",
  cloud: "Hosted OpenCode services: Zen, Console, free models, organizations, integrations, well-known config",
  platform: "Windows, PowerShell, Bun/Node and npm/AI SDK packages",
  differs: "Behaviour that differs and is for the maintainer to decide (see above)",
  other: "Other (each says why)",
}
p("| Reason | Cases |")
p("|---|---:|")
for (const [k, rs] of Object.entries(dz).sort((a, b) => sum(b[1]) - sum(a[1]))) p(`| ${label[k] || k} | ${sum(rs)} |`)
p()

// per-test inventory
p("## Per-test inventory")
p()
p("The tests not listed in the roadmap or the backlog, by file: `P` ported, `F` ported after a fix, `D` by design / n.a., `X` found bug; `×n` is the number of cases. Consecutive by-design tests with the same reason share a line.")
p()
const tag = { ported: "P", fixed: "F", design: "D", bug: "X" }
for (const [a] of areas) {
  const rs = rows.filter((r) => r.area === a && tag[r.bucket])
  if (!rs.length) continue
  p(`<details><summary>${a} (${sum(rs)} of ${sum(rows.filter((r) => r.area === a))})</summary>`)
  p()
  let pf = ""
  let run = null
  const flush = () => {
    if (!run) return
    p(`- D ${run.names.join(" · ")} — ${run.why}`)
    run = null
  }
  for (const r of rs) {
    if (r.file !== pf) { flush(); p(); p(`**\`${r.file}\`**`); p(); pf = r.file }
    const n = `${esc(r.name)}${r.count > 1 ? ` ×${r.count}` : ""}`
    if (r.bucket === "design") {
      const why = `${r.ref}: ${r.note}`
      if (run && run.why === why) run.names.push(n)
      else { flush(); run = { why, names: [n] } }
      continue
    }
    flush()
    const why = r.bucket === "bug" ? r.note : `\`${rel(r.ref)}\`${r.note ? ` (${r.note})` : ""}`
    p(`- ${tag[r.bucket]} ${n} — ${why}`)
  }
  flush()
  p()
  p("</details>")
  p()
}
fs.writeFileSync(process.argv[3], out.join("\n") + "\n")
function slug(s) { return s.toLowerCase().replace(/[^a-z0-9 -]/g, "").replace(/ /g, "-") }
function esc(s) { return s.replace(/\|/g, "\\|").replace(/</g, "&lt;") }
