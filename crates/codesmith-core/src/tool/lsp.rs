//! `lsp`: code intelligence from the language servers (definition,
//! references, hover, symbols, call hierarchy). Ported from OpenCode's lsp
//! tool; offered only when language servers are on.

use super::{opt_str, opt_u64, str_arg, Ctx, Outcome, Output};
use crate::lsp::{path_to_uri, Lsp};
use crate::permission::Request;
use serde_json::{json, Value};
use std::path::Path;

const OPERATIONS: [&str; 9] = [
    "goToDefinition",
    "findReferences",
    "hover",
    "documentSymbol",
    "workspaceSymbol",
    "goToImplementation",
    "prepareCallHierarchy",
    "incomingCalls",
    "outgoingCalls",
];

/// Symbol kinds `workspaceSymbol` keeps: class, method, enum, interface,
/// function, variable, constant, struct.
const KINDS: [u64; 8] = [5, 6, 10, 11, 12, 13, 14, 23];

pub fn spec() -> (String, Value) {
    (
        "Interact with Language Server Protocol (LSP) servers to get code intelligence features.\n\nSupported operations:\n- goToDefinition: Find where a symbol is defined\n- findReferences: Find all references to a symbol\n- hover: Get hover information (documentation, type info) for a symbol\n- documentSymbol: Get all symbols (functions, classes, variables) in a document\n- workspaceSymbol: Search for symbols across the entire workspace\n- goToImplementation: Find implementations of an interface or abstract method\n- prepareCallHierarchy: Get call hierarchy item at a position (functions/methods)\n- incomingCalls: Find all functions/methods that call the function at a position\n- outgoingCalls: Find all functions/methods called by the function at a position\n\nAll operations require:\n- path: The file to operate on\n- line: The line number (1-based, as shown in editors)\n- character: The character offset (1-based, as shown in editors)\n\nNote: LSP servers must be configured for the file type. If no server is available, an error will be returned.".into(),
        json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "enum": OPERATIONS, "description": "The LSP operation to perform"},
                "path": {"type": "string", "description": "The file to operate on"},
                "line": {"type": "integer", "minimum": 1, "description": "The line number (1-based, as shown in editors)"},
                "character": {"type": "integer", "minimum": 1, "description": "The character offset (1-based, as shown in editors)"},
                "query": {"type": "string", "description": "Symbol name to search for (workspaceSymbol only; defaults to all)"}
            },
            "required": ["operation", "path", "line", "character"]
        }),
    )
}

pub async fn run(ctx: &Ctx<'_>, input: &Value) -> Outcome {
    let op = str_arg(input, "operation")?;
    if !OPERATIONS.contains(&op) {
        return Err(format!(
            "Unknown operation {op:?}. Use one of: {}",
            OPERATIONS.join(", ")
        ));
    }
    let path = str_arg(input, "path")?;
    let line = opt_u64(input, "line").ok_or("Missing required integer parameter `line`")?;
    let character =
        opt_u64(input, "character").ok_or("Missing required integer parameter `character`")?;
    let Some(lsp) = &ctx.lsp else {
        return Err("Language servers are off; set \"lsp\": true in codesmith.json.".into());
    };
    let target = ctx.resolve(path);
    ctx.authorize_external(&target, false, super::Access::Read)?;
    ctx.ask(Request {
        action: "lsp".into(),
        resources: target.resources.clone(),
        save: vec!["*".into()],
        title: format!("Query language servers: {op} in {}", target.resource),
        access: None,
    })?;
    if !target.absolute.is_file() {
        return Err(format!("File not found: {path}"));
    }
    let file = target.absolute.as_path();
    if !lsp.has_clients(file).await {
        return Err("No LSP server available for this file type.".into());
    }
    lsp.touch(file, true).await;
    let results = query(
        lsp,
        file,
        op,
        line.saturating_sub(1),
        character.saturating_sub(1),
        opt_str(input, "query").unwrap_or(""),
    )
    .await;
    if results.is_empty() {
        return Ok(Output::text(format!("No results found for {op}")));
    }
    Ok(Output::text(
        serde_json::to_string_pretty(&results).unwrap_or_default(),
    ))
}

/// Flatten one result per server into a list.
fn flatten(values: Vec<Value>) -> Vec<Value> {
    values
        .into_iter()
        .flat_map(|v| match v {
            Value::Array(a) => a,
            other => vec![other],
        })
        .filter(|v| !v.is_null())
        .collect()
}

async fn query(lsp: &Lsp, file: &Path, op: &str, line: u64, character: u64, q: &str) -> Vec<Value> {
    let doc = json!({"uri": path_to_uri(file)});
    let at = json!({"textDocument": doc, "position": {"line": line, "character": character}});
    let f = Some(file);
    match op {
        "goToDefinition" => flatten(lsp.request(f, "textDocument/definition", at).await),
        "findReferences" => {
            let mut p = at;
            p["context"] = json!({"includeDeclaration": true});
            flatten(lsp.request(f, "textDocument/references", p).await)
        }
        "hover" => flatten(lsp.request(f, "textDocument/hover", at).await),
        "documentSymbol" => flatten(
            lsp.request(
                f,
                "textDocument/documentSymbol",
                json!({"textDocument": doc}),
            )
            .await,
        ),
        "workspaceSymbol" => lsp
            .request(None, "workspace/symbol", json!({"query": q}))
            .await
            .into_iter()
            .flat_map(|v| {
                let list = v.as_array().cloned().unwrap_or_default();
                list.into_iter()
                    .filter(|s| s["kind"].as_u64().is_some_and(|k| KINDS.contains(&k)))
                    .take(10)
            })
            .collect(),
        "goToImplementation" => flatten(lsp.request(f, "textDocument/implementation", at).await),
        "prepareCallHierarchy" => flatten(
            lsp.request(f, "textDocument/prepareCallHierarchy", at)
                .await,
        ),
        _ => {
            let method = if op == "incomingCalls" {
                "callHierarchy/incomingCalls"
            } else {
                "callHierarchy/outgoingCalls"
            };
            let items = flatten(
                lsp.request(f, "textDocument/prepareCallHierarchy", at)
                    .await,
            );
            let Some(item) = items.into_iter().next() else {
                return vec![];
            };
            flatten(lsp.request(f, method, json!({"item": item})).await)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::Env;
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn off_and_bad_input() {
        let env = Env::new();
        let ctx = env.ctx();
        let e = run(
            &ctx,
            &json!({"operation": "hover", "path": "a.rs", "line": 1, "character": 1}),
        )
        .await
        .unwrap_err();
        assert!(e.contains("off"), "{e}");
        let e = run(&ctx, &json!({"operation": "rename", "path": "a.rs"}))
            .await
            .unwrap_err();
        assert!(e.contains("Unknown operation"), "{e}");
        assert_eq!(
            flatten(vec![json!([1, 2]), json!({"a": 1}), Value::Null]),
            vec![json!(1), json!(2), json!({"a": 1})]
        );
    }
}
