// Extract every test case from OpenCode's packages/core/test into
// <out>/core_tests.jsonl, one JSON object per test site:
// {file, describe, name, callee, mult, skip, line}; `mult` is the number of
// generated cases (`.each` rows, passes of `for` loops over literal lists).
// Usage: node extract.js <packages/core/test> <out>
const ts = require("typescript")
const fs = require("fs")
const path = require("path")
const root = process.argv[2]
const outDir = process.argv[3]
const lines = []
const files = []
const walk = (d) => {
  for (const e of fs.readdirSync(d, { withFileTypes: true })) {
    const p = path.join(d, e.name)
    if (e.isDirectory()) { if (e.name !== "node_modules") walk(p) }
    else if (e.name.endsWith(".test.ts")) files.push(p)
  }
}
walk(root)
files.sort()
const isFn = (n) => n && (ts.isArrowFunction(n) || ts.isFunctionExpression(n))
const strOf = (n) => {
  if (!n) return null
  if (ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n)) return n.text
  if (ts.isTemplateExpression(n)) return n.getText().slice(1, -1)
  return null
}
const calleeName = (c) => c.getText().replace(/\s+/g, "")
for (const file of files) {
  const src = ts.createSourceFile(file, fs.readFileSync(file, "utf8"), ts.ScriptTarget.Latest, true)
  // const arrays at any scope by name (best effort)
  const arrays = {}
  const collect = (n) => {
    if (ts.isVariableDeclaration(n) && ts.isIdentifier(n.name) && n.initializer) {
      let init = n.initializer
      while (ts.isAsExpression(init) || ts.isSatisfiesExpression?.(init)) init = init.expression
      if (ts.isArrayLiteralExpression(init)) arrays[n.name.text] = init.elements.length
      if (ts.isObjectLiteralExpression(init)) arrays[n.name.text + "#obj"] = init.properties.length
    }
    ts.forEachChild(n, collect)
  }
  collect(src)
  const lenOf = (e) => {
    if (!e) return 1
    while (ts.isAsExpression(e) || ts.isParenthesizedExpression(e)) e = e.expression
    if (ts.isArrayLiteralExpression(e)) return e.elements.length
    if (ts.isIdentifier(e) && arrays[e.text] != null) return arrays[e.text]
    if (ts.isCallExpression(e) && ts.isPropertyAccessExpression(e.expression)) {
      const m = e.expression.name.text
      const o = e.expression.expression
      if (["entries", "keys", "values"].includes(m) && ts.isIdentifier(o) && ts.isIdentifier(o) && ["Object"].includes(o.text)) {
        const a = e.arguments[0]
        if (a && ts.isIdentifier(a) && arrays[a.text + "#obj"] != null) return arrays[a.text + "#obj"]
        if (a && ts.isObjectLiteralExpression(a)) return a.properties.length
      }
      if (["entries", "map", "filter", "flatMap"].includes(m)) return lenOf(o)
    }
    return 1
  }
  const visit = (n, desc, mult) => {
    if (ts.isForOfStatement(n) || ts.isForInStatement(n)) {
      const m = ts.isForOfStatement(n) ? lenOf(n.expression) : 1
      ts.forEachChild(n, (c) => visit(c, desc, mult * m))
      return
    }
    if (ts.isCallExpression(n)) {
      const callee = n.expression
      // [..].forEach(x => ...) / .map
      if (ts.isPropertyAccessExpression(callee) && ["forEach", "map", "flatMap"].includes(callee.name.text)) {
        const m = lenOf(callee.expression)
        ts.forEachChild(n, (c) => visit(c, desc, mult * (c === callee ? 1 : m)))
        return
      }
      let name = strOf(n.arguments[0])
      if (name == null && mult > 1 && n.arguments[0] && /^(it|test)(\.|$)/.test(calleeName(callee)) && n.arguments.slice(1).some(isFn))
        name = "{" + n.arguments[0].getText() + "}"
      const hasFn = n.arguments.slice(1).some((a) => isFn(a) || (ts.isCallExpression(a) && /^Effect\./.test(a.expression.getText())))
      const cname = calleeName(callee)
      if (name != null && /^describe(\.|$)/.test(cname)) {
        n.arguments.slice(1).forEach((a) => visit(a, [...desc, name], mult))
        return
      }
      const parent = n.parent
      const stmt = ts.isExpressionStatement(parent) || (ts.isArrowFunction(parent) && parent.body === n)
      if (name != null && hasFn && stmt && !/^(Effect|yield|editor|TestLLM)/.test(cname)) {
        let m = mult
        let each = false
        if (ts.isCallExpression(callee) && /\.each$/.test(calleeName(callee.expression))) {
          m *= lenOf(callee.arguments[0]); each = true
        }
        const line = src.getLineAndCharacterOfPosition(n.getStart()).line + 1
        lines.push(JSON.stringify({ file: path.relative(root, file), describe: desc.join(" > "), name, callee: each ? calleeName(callee.expression) : cname, mult: m, skip: /skip|todo/.test(cname), line }))
        return
      }
    }
    ts.forEachChild(n, (c) => visit(c, desc, mult))
  }
  visit(src, [], 1)
}
// Calls that look like tests but are callbacks inside test bodies.
const kept = lines.filter((l) => !/"callee":"[^"]*(setRequestHandler|addEventListener|\.on)"/.test(l))
fs.writeFileSync(path.join(outDir, "core_tests.jsonl"), kept.join("\n") + "\n")
console.log(`${kept.length} test sites`)
