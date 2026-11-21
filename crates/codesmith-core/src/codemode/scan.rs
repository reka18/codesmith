//! The tools a Code Mode program calls, read from its source as OpenCode's
//! `CodeMode.scanTools` does: every `tools.<path>(...)` call, in the order
//! first seen. A program that reaches `tools` any other way (a dynamic
//! `tools[name]`, an alias such as `const t = tools.slack`, passing `tools`
//! along, or `import()`) is refused, since the calls it makes could not be
//! read. An automation records what this finds when it is created, and its
//! runs are offered only those tools.
//!
//! Unlike OpenCode, a local binding named `tools` that shadows the global is
//! not told apart: using it as anything but a static call is refused too.

use boa_engine::ast::expression::access::{PropertyAccess, PropertyAccessField};
use boa_engine::ast::expression::literal::{LiteralKind, PropertyDefinition};
use boa_engine::ast::expression::Expression;
use boa_engine::ast::scope::Scope;
use boa_engine::ast::visitor::{VisitWith, Visitor};
use boa_engine::interner::{Interner, Sym};
use boa_engine::parser::{Parser, Source};
use std::ops::ControlFlow;

const DYNAMIC: &str = "Automation code must reference tools statically (e.g. tools.slack.read_thread), not with dynamic bracket expressions.";
const ALIAS: &str = "Automation code must call tools directly as tools.<name>(...), not alias or pass the tools object.";
const IMPORT: &str =
    "Automation code must call tools directly as tools.<name>(...), not import or require them.";

/// The `tools.<path>` each call in `code` names (`tools.slack.post(...)` is
/// `slack.post`, `tools["hex-meetings"].get(...)` is `hex-meetings.get`),
/// once each in first-seen order; or why the calls cannot be read.
pub fn scan_tools(code: &str) -> Result<Vec<String>, String> {
    if code.trim().is_empty() {
        return Err("Code cannot be empty.".into());
    }
    // Parsed as it runs: the body of a strict async function, or a sloppy
    // one when strict mode refuses repeated parameter names.
    let body = super::program(code);
    let mut interner = Interner::default();
    let mut parse = |wrapped: String| {
        Parser::new(Source::from_bytes(wrapped.as_bytes()))
            .parse_script(&Scope::new_global(), &mut interner)
            .map_err(|e| format!("SyntaxError: {e}"))
    };
    let script = match parse(format!(
        "(async function () {{ \"use strict\";\n{body}\n}})"
    )) {
        Err(e) if e.contains("Duplicate parameter name") => {
            parse(format!("(async () => {{\n{body}\n}})"))?
        }
        r => r?,
    };
    let tools = interner.get_or_intern("tools");
    let mut scan = Scan {
        interner: &interner,
        tools,
        found: Vec::new(),
    };
    match script.visit_with(&mut scan) {
        ControlFlow::Break(e) => Err(e),
        ControlFlow::Continue(()) => Ok(scan.found),
    }
}

struct Scan<'a> {
    interner: &'a Interner,
    tools: Sym,
    found: Vec<String>,
}

impl Scan<'_> {
    /// The path of `e` when it is a static `tools.<path>` chain (`Some` of
    /// no segments for `tools` itself), `None` when it is not rooted at
    /// `tools`, or a break when it is but a segment is computed.
    fn chain(&self, e: &Expression) -> ControlFlow<String, Option<Vec<String>>> {
        match e {
            Expression::Identifier(id) if id.sym() == self.tools => {
                ControlFlow::Continue(Some(vec![]))
            }
            Expression::Parenthesized(p) => self.chain(p.expression()),
            Expression::PropertyAccess(PropertyAccess::Simple(a)) => {
                let Some(mut path) = self.chain(a.target())? else {
                    return ControlFlow::Continue(None);
                };
                let segment = match a.field() {
                    PropertyAccessField::Const(id) => {
                        self.interner.resolve_expect(id.sym()).to_string()
                    }
                    PropertyAccessField::Expr(x) => match x.as_ref() {
                        Expression::Literal(l) => match l.kind() {
                            LiteralKind::String(s) => self.interner.resolve_expect(*s).to_string(),
                            _ => return ControlFlow::Break(DYNAMIC.into()),
                        },
                        _ => return ControlFlow::Break(DYNAMIC.into()),
                    },
                };
                path.push(segment);
                ControlFlow::Continue(Some(path))
            }
            _ => ControlFlow::Continue(None),
        }
    }
}

impl<'ast> Visitor<'ast> for Scan<'_> {
    type BreakTy = String;

    fn visit_expression(&mut self, e: &'ast Expression) -> ControlFlow<String> {
        match e {
            Expression::Call(c) => match self.chain(c.function())? {
                Some(path) if !path.is_empty() => {
                    let path = path.join(".");
                    if !self.found.contains(&path) {
                        self.found.push(path);
                    }
                    for a in c.args() {
                        self.visit_expression(a)?;
                    }
                    ControlFlow::Continue(())
                }
                Some(_) => ControlFlow::Break(ALIAS.into()),
                None => e.visit_with(self),
            },
            Expression::ImportCall(_) => ControlFlow::Break(IMPORT.into()),
            Expression::Identifier(_) | Expression::PropertyAccess(_) => match self.chain(e)? {
                // `tools` reached other than as a call.
                Some(_) => ControlFlow::Break(ALIAS.into()),
                None => e.visit_with(self),
            },
            _ => e.visit_with(self),
        }
    }

    fn visit_property_definition(&mut self, p: &'ast PropertyDefinition) -> ControlFlow<String> {
        // `{ tools }` passes the object along.
        if let PropertyDefinition::IdentifierReference(id) = p {
            if id.sym() == self.tools {
                return ControlFlow::Break(ALIAS.into());
            }
        }
        p.visit_with(self)
    }
}

#[cfg(test)]
mod tests {
    use super::scan_tools;

    // OpenCode codemode/test/scan.test.ts (MIT): "extracts dotted, bracketed and nested
    // static tool calls in first-seen order"
    #[test]
    fn scan_extracts_dotted_bracketed_and_nested_static_tool_calls_in_first_seen_order() {
        let scanned = scan_tools(
            r#"
      const chapters = await tools["hex-meetings"].get_chapters({ meeting_id: "m1" })
      const thread = await tools.slack.read_thread({ ts: await tools.slack.latest_ts({}) })
      await tools.slack.read_thread({ ts: "again" })
      const ignored = { tools: 1 }.tools
      return { wake: true, state: chapters }
    "#,
        );
        assert_eq!(
            scanned.unwrap(),
            [
                "hex-meetings.get_chapters",
                "slack.read_thread",
                "slack.latest_ts"
            ]
        );
    }

    // "rejects empty code, syntax errors, dynamic bracket access, and
    // aliasing tools"
    #[test]
    fn scan_rejects_empty_code_syntax_errors_dynamic_access_and_aliasing() {
        for code in [
            "   ",
            "const x = {",
            "return await tools.slack[name]({})",
            "const s = tools.slack; return await s.read_thread({})",
            "return Object.keys(tools)",
        ] {
            assert!(scan_tools(code).is_err(), "{code}");
        }
        assert!(scan_tools("return await tools.slack[name]({})")
            .unwrap_err()
            .contains("statically"));
        assert!(scan_tools("const s = tools.slack")
            .unwrap_err()
            .contains("not alias"));
    }

    // "rejects loading tools as a module, which would hide the calls from
    // the scan" (adapted: programs have no require; a dynamic import is
    // refused whatever it loads)
    #[test]
    fn scan_rejects_loading_tools_as_a_module() {
        let e =
            scan_tools(r#"const t = await import("tools"); await t.slack.post({})"#).unwrap_err();
        assert!(e.contains("not import"), "{e}");
    }

    #[test]
    fn scan_edge_cases() {
        let ok = |code: &str| scan_tools(code).unwrap();
        // Strings, comments and templates that mention tools are not calls.
        assert!(
            ok("// tools.slack.post()\nconst s = 'tools.x.y()'; return `tools.z()`").is_empty()
        );
        // A program without a return, a parenthesized callee, nested
        // namespaces, a call inside a callback and a nested function.
        assert_eq!(ok("await (tools.a.b)({})"), ["a.b"]);
        assert_eq!(ok("await tools.gh.issues.list({})"), ["gh.issues.list"]);
        assert_eq!(
            ok("for (const c of args) await tools.slack.post({ c }); [1].map(async () => tools.x.y())"),
            ["slack.post", "x.y"]
        );
        assert_eq!(
            ok("async function f() { return tools.a.b() } return f()"),
            ["a.b"]
        );
        // Calls that are not tool calls are fine.
        assert!(ok("return JSON.stringify({ a: 1 }).length").is_empty());
        assert!(ok("return input.state").is_empty());
        // Every other way to reach the object is refused.
        for code in [
            "return tools()",
            "const t = tools",
            "return { tools }",
            "return typeof tools.slack.post",
            "const f = tools.slack.post; await f({})",
            "await tools.slack[`post`]({})",
            "await tools.slack[1]({})",
            "await tools?.slack.post({})",
            "with (tools) { slack.post({}) }",
            "function f(a, a) { return tools }",
        ] {
            assert!(scan_tools(code).is_err(), "{code}");
        }
    }
}
