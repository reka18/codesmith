// Apply rules/*.txt (and the name matches in ocmatch.tsv) to every test in
// <out>/core_tests.jsonl; write <out>/classified.jsonl and print the totals.
// A rule line is `selector :: bucket :: ref :: note` under an `@file` header;
// the selector is `*`, a /regex/ on the test name, or line numbers
// (`730:72` takes 72 of that site's cases). Explicit lines win, and a later
// rules file overrides an earlier one for the same line.
// Usage: node classify.js <out> [file regex to list unclassified]
const fs = require("fs")
const outDir = process.argv[2]
const dir = __dirname
const tests = fs.readFileSync(outDir + "/core_tests.jsonl", "utf8").trim().split("\n").map(JSON.parse)
const auto = {}
for (const l of fs.readFileSync(outDir + "/ocmatch.tsv", "utf8").trim().split("\n")) {
  const [file, fn, score, loc] = l.split("\t")
  if (Number(score) >= 0.3 && loc) (auto[loc] ??= []).push(file + "::" + fn)
}
const rules = {} // file -> [{sel, bucket, ref, note}]
for (const f of fs.readdirSync(dir + "/rules").sort()) {
  let cur = null
  for (const raw of fs.readFileSync(dir + "/rules/" + f, "utf8").split("\n")) {
    const line = raw.trim()
    if (!line || line.startsWith("#")) continue
    if (line.startsWith("@")) { cur = line.slice(1).trim(); (rules[cur] ??= []); continue }
    const [sel, bucket, ref = "", note = ""] = line.split(" :: ").map((s) => s.trim())
    if (!["ported", "fixed", "portable", "feature", "design", "bug"].includes(bucket)) throw new Error(`bad bucket in ${f}: ${line}`)
    rules[cur].push({ sel, bucket, ref, note, src: f })
  }
}
const out = []
const problems = []
for (const t of tests) {
  const rs = rules[t.file] || []
  const rows = []
  let left = t.mult
  // explicit line rules (possibly with counts)
  const hits = rs.filter((r) => /^[0-9:,]+$/.test(r.sel) && r.sel.split(",").some((p) => Number(p.split(":")[0]) === t.line))
  const lastSrc = hits.length ? hits[hits.length - 1].src : null
  for (const r of rs) {
    if (/^[0-9:,]+$/.test(r.sel) && r.src === lastSrc) {
      for (const part of r.sel.split(",")) {
        const [ln, cnt] = part.split(":")
        if (Number(ln) === t.line) {
          const c = cnt ? Number(cnt) : left
          rows.push({ ...r, count: c }); left -= c
        }
      }
    }
  }
  if (rows.length && left !== 0) problems.push(`count mismatch ${t.file}:${t.line} left=${left}`)
  if (!rows.length && auto[t.file + ":" + t.line]) {
    rows.push({ bucket: "ported", ref: auto[t.file + ":" + t.line].join(", "), note: "", count: t.mult, src: "auto" })
  }
  if (!rows.length) {
    const r = rs.find((r) => r.sel.startsWith("/") && new RegExp(r.sel.slice(1, -1), "i").test(t.name))
      || rs.find((r) => r.sel === "*")
    if (r) rows.push({ ...r, count: t.mult })
  }
  if (!rows.length) rows.push({ bucket: "unclassified", ref: "", note: "", count: t.mult })
  for (const r of rows) out.push({ ...t, bucket: r.bucket, ref: r.ref, note: r.note, count: r.count })
}
fs.writeFileSync(outDir + "/classified.jsonl", out.map((x) => JSON.stringify(x)).join("\n") + "\n")
// check explicit line numbers exist
const have = new Set(tests.map((t) => t.file + ":" + t.line))
for (const [file, rs] of Object.entries(rules)) for (const r of rs) if (/^[0-9:,]+$/.test(r.sel))
  for (const part of r.sel.split(",")) if (!have.has(file + ":" + part.split(":")[0])) problems.push(`no such test ${file}:${part}`)
for (const p of problems) console.log("PROBLEM", p)
const by = {}
for (const r of out) by[r.bucket] = (by[r.bucket] || 0) + r.count
console.log(by)
const unc = {}
for (const r of out) if (r.bucket === "unclassified" || r.note === "unclassified") unc[r.file] = (unc[r.file] || 0) + r.count
if (Object.keys(unc).length) process.exitCode = 1
console.log("unclassified files:", Object.keys(unc).length, Object.values(unc).reduce((a, b) => a + b, 0))
if (process.argv[3]) for (const [f, c] of Object.entries(unc)) if (f.match(process.argv[3])) console.log(" ", f, c)
