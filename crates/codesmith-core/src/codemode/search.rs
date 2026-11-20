//! The Code Mode tool tree and catalog search, as in OpenCode
//! (`packages/codemode/src/codemode/tool-runtime.ts`): dots in a tool's
//! name nest it (`issues.list` under `tools.github` is
//! `tools.github.issues.list`), unknown paths fail with the closest match,
//! and the global `search(...)` ranks tools by field-weighted BM25 over
//! their namespace, name, description and parameter names.

use super::{signature, Entry};
use serde_json::{json, Value};
use std::collections::HashMap;

/// `tools.a.b` with bracket notation for segments that need it.
pub fn expression(path: &str) -> String {
    let mut s = String::from("tools");
    for seg in path.split('.') {
        if signature::is_identifier(seg) {
            s.push('.');
            s.push_str(seg);
        } else {
            s.push_str(&format!(
                "[{}]",
                serde_json::to_string(seg).unwrap_or_default()
            ));
        }
    }
    s
}

/// A tool's canonical path: its namespace and name, dots splitting both.
pub fn canonical(e: &Entry) -> String {
    // A built-in placed in Code Mode sits at the top level (`tools.read`).
    if e.namespace.is_empty() {
        return e.name.clone();
    }
    format!("{}.{}", e.namespace, e.name)
}

const FIELDS: [(&str, f64); 4] = [
    ("name", 3.0),
    ("namespace", 2.0),
    ("description", 1.0),
    ("parameters", 0.3),
];
const K1: f64 = 1.2;
const B: f64 = 0.75;
const BREVITY: f64 = 0.5;
const PREFIX: f64 = 0.3;
const LIMIT: usize = 10;

/// Words split at case changes and non-alphanumerics.
fn split_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut spaced = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i > 0
            && c.is_ascii_uppercase()
            && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit())
        {
            spaced.push(' ');
        }
        spaced.push(*c);
    }
    spaced
        .to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Plural endings only.
fn stem(w: &str) -> String {
    if w.len() > 4 && w.ends_with("ies") {
        return format!("{}y", &w[..w.len() - 3]);
    }
    if w.len() > 4
        && ["sses", "shes", "ches", "xes", "zes", "uses"]
            .iter()
            .any(|e| w.ends_with(e))
    {
        return w[..w.len() - 2].to_string();
    }
    if w.len() > 2 && w.ends_with('s') && !["ss", "us", "is"].iter().any(|e| w.ends_with(e)) {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

/// A text's split words plus each camelCase word whole, stemmed.
fn search_words(text: &str) -> Vec<String> {
    let mut words = split_words(text);
    // `[A-Za-z0-9]*[a-z0-9][A-Z][A-Za-z0-9]*`: alphanumeric runs with a case change.
    for run in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let b = run.as_bytes();
        let camel = (1..b.len()).any(|i| {
            b[i].is_ascii_uppercase()
                && (b[i - 1].is_ascii_lowercase() || b[i - 1].is_ascii_digit())
        });
        if camel {
            words.push(run.to_lowercase());
        }
    }
    words.iter().map(|w| stem(w)).collect()
}

struct Item {
    path: String,
    description: String,
    signature: String,
    name_length: usize,
}

/// The catalog's search index, built once per execution.
pub struct Index {
    items: Vec<Item>,
    postings: HashMap<String, Vec<(usize, f64)>>,
    words: Vec<String>,
}

impl Index {
    pub fn new(entries: &[Entry]) -> Index {
        let mut list: Vec<&Entry> = entries.iter().collect();
        list.sort_by_key(|e| canonical(e));
        let mut items = vec![];
        let mut fields: Vec<[Vec<String>; 4]> = vec![];
        for e in list {
            let path = canonical(e);
            let segments: Vec<&str> = path.split('.').collect();
            let name = segments.last().copied().unwrap_or("");
            let params: Vec<String> = e.schema["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            let words = |texts: &[&str]| texts.iter().flat_map(|t| search_words(t)).collect();
            fields.push([
                words(&[name]),
                words(&segments[..segments.len().saturating_sub(1)]),
                words(&[e.description.as_str()]),
                words(&params.iter().map(String::as_str).collect::<Vec<_>>()),
            ]);
            items.push(Item {
                signature: super::signature(e),
                description: e.description.clone(),
                name_length: split_words(name).len(),
                path,
            });
        }
        let n = items.len().max(1) as f64;
        let average: Vec<f64> = (0..4)
            .map(|f| {
                let a = fields.iter().map(|x| x[f].len()).sum::<usize>() as f64 / n;
                if a == 0.0 {
                    1.0
                } else {
                    a
                }
            })
            .collect();
        let mut postings: HashMap<String, Vec<(usize, f64)>> = HashMap::new();
        for (position, entry) in fields.iter().enumerate() {
            // A map, not a list: a long description must not make this quadratic.
            let mut weights: HashMap<&str, f64> = HashMap::new();
            for (f, (_, weight)) in FIELDS.iter().enumerate() {
                let w = weight / (1.0 - B + B * entry[f].len() as f64 / average[f]);
                for word in &entry[f] {
                    *weights.entry(word.as_str()).or_default() += w;
                }
            }
            for (word, w) in weights {
                postings
                    .entry(word.to_string())
                    .or_default()
                    .push((position, w));
            }
        }
        let mut words: Vec<String> = postings.keys().cloned().collect();
        words.sort();
        Index {
            items,
            postings,
            words,
        }
    }

    /// Positions of `allowed` items, best match for `query` first.
    fn rank(&self, allowed: &[usize], query: &str) -> Vec<usize> {
        let terms = search_words(query);
        if terms.is_empty() {
            let mut out = allowed.to_vec();
            out.sort_by(|a, b| self.items[*a].path.cmp(&self.items[*b].path));
            return out;
        }
        let docs = self.items.len() as f64;
        let mut scores: HashMap<usize, f64> = HashMap::new();
        for term in &terms {
            let mut best: HashMap<usize, f64> = HashMap::new();
            let mut matches = vec![(term.clone(), 1.0)];
            if term.len() >= 4 {
                let start = self.words.partition_point(|w| w < term);
                for w in self.words[start..]
                    .iter()
                    .take_while(|w| w.starts_with(term.as_str()))
                {
                    if w != term {
                        matches.push((w.clone(), PREFIX));
                    }
                }
            }
            for (word, factor) in matches {
                let Some(posting) = self.postings.get(&word) else {
                    continue;
                };
                let df = posting.len() as f64;
                let idf = (1.0 + (docs - df + 0.5) / (df + 0.5)).ln();
                for (position, weight) in posting {
                    if !allowed.contains(position) {
                        continue;
                    }
                    let score = factor * idf * weight * (K1 + 1.0) / (weight + K1);
                    let b = best.entry(*position).or_insert(0.0);
                    if score > *b {
                        *b = score;
                    }
                }
            }
            for (p, s) in best {
                *scores.entry(p).or_insert(0.0) += s;
            }
        }
        let mut ranked: Vec<(usize, f64)> = scores
            .into_iter()
            .map(|(p, s)| {
                (
                    p,
                    s * (1.0 + BREVITY / self.items[p].name_length.max(1) as f64),
                )
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.items[a.0].path.cmp(&self.items[b.0].path))
        });
        ranked.into_iter().map(|(p, _)| p).collect()
    }

    fn under(&self, prefix: &str) -> Vec<usize> {
        (0..self.items.len())
            .filter(|&i| {
                prefix.is_empty()
                    || self.items[i].path == prefix
                    || self.items[i].path.starts_with(&format!("{prefix}."))
            })
            .collect()
    }

    /// Paths ranked for `query` among the tools under `prefix` (all when empty).
    pub fn closest(&self, prefix: &str, query: &str) -> Vec<String> {
        let allowed: Vec<usize> = self
            .under(prefix)
            .into_iter()
            .filter(|&i| prefix.is_empty() || self.items[i].path != prefix)
            .collect();
        self.rank(&allowed, query)
            .into_iter()
            .map(|i| self.items[i].path.clone())
            .collect()
    }

    /// Every tool path under `prefix` (excluding a tool at `prefix` itself), in path order.
    pub fn within(&self, prefix: &str) -> Vec<String> {
        self.under(prefix)
            .into_iter()
            .map(|i| self.items[i].path.clone())
            .filter(|p| p != prefix)
            .collect()
    }

    /// The `search(...)` global: a query string, or
    /// `{ query?, namespace?, limit?, offset? }`.
    pub fn search(&self, input: &Value) -> Result<Value, String> {
        let invalid = |why: &str| {
            Err(format!(
                "Invalid input for tool 'search': {why}. Pass search a query string or an object, such as search(\"issues\") or search({{ namespace: \"github\" }})."
            ))
        };
        let (query, namespace, limit, offset) = match input {
            Value::String(q) => (q.clone(), None, LIMIT, 0),
            Value::Null => (String::new(), None, LIMIT, 0),
            Value::Object(o) => {
                let text = |k: &str| match o.get(k) {
                    None => Ok(None),
                    Some(Value::String(s)) => Ok(Some(s.clone())),
                    Some(_) => Err(format!("`{k}` must be a string")),
                };
                let int = |k: &str, min: u64| match o.get(k) {
                    None => Ok(None),
                    Some(v) => match v.as_f64() {
                        Some(f) if f.fract() == 0.0 && f >= min as f64 => Ok(Some(f as usize)),
                        _ => Err(format!("`{k}` must be an integer of at least {min}")),
                    },
                };
                let parsed = (|| {
                    Ok::<_, String>((
                        text("query")?.unwrap_or_default(),
                        text("namespace")?,
                        int("limit", 1)?.unwrap_or(LIMIT),
                        int("offset", 0)?.unwrap_or(0),
                    ))
                })();
                match parsed {
                    Ok(p) => p,
                    Err(e) => return invalid(&e),
                }
            }
            _ => return invalid("expected a string or an object"),
        };
        let namespace = namespace.and_then(|ns| {
            if ns == "tools" {
                None
            } else {
                Some(ns.strip_prefix("tools.").unwrap_or(&ns).to_string())
            }
        });
        let scoped = self.under(namespace.as_deref().unwrap_or(""));
        let trimmed = query.trim();
        let path_query = trimmed.strip_prefix("tools.").unwrap_or(trimmed);
        let exact = (!path_query.is_empty())
            .then(|| {
                scoped.iter().copied().find(|&i| {
                    self.items[i].path == path_query || expression(&self.items[i].path) == trimmed
                })
            })
            .flatten();
        let ranked = match exact {
            Some(i) => vec![i],
            None => self.rank(&scoped, path_query),
        };
        let items: Vec<Value> = ranked
            .iter()
            .skip(offset)
            .take(limit)
            .map(|&i| {
                let it = &self.items[i];
                json!({"path": expression(&it.path), "description": it.description, "signature": it.signature})
            })
            .collect();
        let remaining = ranked.len().saturating_sub(offset + items.len());
        let next = if remaining > 0 {
            json!({"offset": offset + items.len()})
        } else {
            Value::Null
        };
        Ok(json!({"items": items, "remaining": remaining, "next": next}))
    }
}

/// Define `search` and a global `__tools(list)` that builds the `tools`
/// tree from `[[tool, canonical path], ...]`, calling tools through the
/// global `__call`.
pub fn install(ctx: &mut boa_engine::Context, entries: &[Entry]) -> boa_engine::JsResult<()> {
    use boa_engine::object::builtins::JsArray;
    use boa_engine::object::FunctionObjectBuilder;
    use boa_engine::{
        js_string, JsNativeError, JsObject, JsString, JsValue, NativeFunction, Source,
    };
    use std::rc::Rc;

    // Built on the first search or unknown path: most programs need neither.
    struct Lazy(Vec<Entry>, std::cell::OnceCell<Index>);
    impl Lazy {
        fn get(&self) -> &Index {
            self.1.get_or_init(|| Index::new(&self.0))
        }
    }
    let index = Rc::new(Lazy(entries.to_vec(), std::cell::OnceCell::new()));
    let text = |args: &[JsValue], i: usize, ctx: &mut boa_engine::Context| {
        Ok::<_, boa_engine::JsError>(
            args.get(i)
                .cloned()
                .unwrap_or_default()
                .to_string(ctx)?
                .to_std_string_lossy(),
        )
    };
    let strings = |v: Vec<String>, ctx: &mut boa_engine::Context| -> JsValue {
        JsArray::from_iter(
            v.into_iter()
                .map(|s| JsValue::from(JsString::from(s.as_str()))),
            ctx,
        )
        .into()
    };
    let natives = JsObject::with_null_proto();
    let i = index.clone();
    // SAFETY: the closures capture no garbage-collected values.
    let search = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let input = args
                .first()
                .cloned()
                .unwrap_or_default()
                .to_json(ctx)?
                .unwrap_or(Value::Null);
            match i.get().search(&input) {
                Ok(v) => JsValue::from_json(&v, ctx),
                Err(e) => Err(JsNativeError::error().with_message(e).into()),
            }
        })
    };
    let i = index.clone();
    let closest = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let v = i.get().closest(&text(args, 0, ctx)?, &text(args, 1, ctx)?);
            Ok(strings(v, ctx))
        })
    };
    let i = index;
    let within = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let v = i.get().within(&text(args, 0, ctx)?);
            Ok(strings(v, ctx))
        })
    };
    for (name, f) in [("search", search), ("closest", closest), ("within", within)] {
        let f = FunctionObjectBuilder::new(ctx.realm(), f)
            .name(JsString::from(name))
            .build();
        natives.set(JsString::from(name), f, false, ctx)?;
    }
    let call = ctx.global_object().get(js_string!("__call"), ctx)?;
    natives.set(js_string!("call"), call, false, ctx)?;
    let install = ctx.eval(Source::from_bytes(include_str!("tools.js")))?;
    let builder = install
        .as_callable()
        .ok_or_else(|| JsNativeError::typ().with_message("tools.js did not load"))?
        .call(&JsValue::undefined(), &[natives.into()], ctx)?;
    ctx.global_object()
        .set(js_string!("__tools"), builder, false, ctx)?;
    Ok(())
}

/// The prelude line that makes `tools`.
pub fn prelude(entries: &[Entry]) -> String {
    let list: Vec<Value> = entries
        .iter()
        .map(|e| json!([e.tool, canonical(e)]))
        .collect();
    format!(
        "const tools = __tools({});\ndelete globalThis.__tools;\n",
        Value::Array(list)
    )
}

/// The exact signature of the `search(...)` global, for the instructions.
pub fn search_signature() -> String {
    let int = |min: &str| json!({"type": "integer", min: 0});
    let input = json!({"type": "object", "properties": {
        "query": {"type": "string"},
        "namespace": {"type": "string"},
        "limit": int("exclusiveMinimum"),
        "offset": int("minimum"),
    }});
    let output = json!({"type": "object", "properties": {
        "items": {"type": "array", "items": {"type": "object", "properties": {
            "path": {"type": "string"}, "description": {"type": "string"}, "signature": {"type": "string"},
        }, "required": ["path", "description", "signature"]}},
        "remaining": int("minimum"),
        "next": {"anyOf": [{"type": "object", "properties": {"offset": int("minimum")}, "required": ["offset"]}, {"type": "null"}]},
    }, "required": ["items", "remaining", "next"]});
    format!(
        "search(query: string | {}): {}",
        signature::typescript(&input, true),
        signature::typescript(&output, true)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ns: &str, name: &str, description: &str) -> Entry {
        Entry {
            tool: format!("{ns}_{name}"),
            namespace: ns.into(),
            name: name.into(),
            description: description.into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn paths(v: &Value) -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["path"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_long_description_indexes_in_linear_time() {
        // Distinct words; a list of weights made this quadratic.
        let words: Vec<String> = (0..60_000).map(|i| format!("w{i}x")).collect();
        let start = std::time::Instant::now();
        let index = Index::new(&[entry("ns", "tool", &words.join(" "))]);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(index.rank(&[0], "w59999x"), vec![0]);
    }

    #[test]
    fn oc_search_scopes_to_a_nested_namespace_subtree() {
        let index = Index::new(&[
            entry("slack", "admin", "Admin"),
            entry("slack", "admin.invite", "Invite"),
            entry("slack", "admin.users.list", "List users"),
            entry("slack", "administrator.list", "List administrators"),
            entry("slack", "read", "Read Slack"),
        ]);
        let v = index
            .search(&json!({"query": "", "namespace": "slack.admin"}))
            .unwrap();
        assert_eq!(
            paths(&v),
            [
                "tools.slack.admin",
                "tools.slack.admin.invite",
                "tools.slack.admin.users.list"
            ]
        );
    }

    #[test]
    fn oc_search_finds_exact_paths_and_ranks_by_words() {
        let index = Index::new(&[
            entry("github", "list_issues", "List issues in a repository"),
            entry("github", "list_user_groups", "List the groups of a user"),
            entry("github", "list_users", "List users"),
            entry(
                "context7",
                "resolve-library-id",
                "Resolve a Context7 library ID",
            ),
        ]);
        assert_eq!(
            paths(&index.search(&json!("tools.github.list_users")).unwrap()),
            ["tools.github.list_users"]
        );
        assert_eq!(
            paths(&index.search(&json!({"query": "users"})).unwrap())[0],
            "tools.github.list_users"
        );
        assert_eq!(
            paths(&index.search(&json!({"query": "resolve library"})).unwrap())[0],
            "tools.context7[\"resolve-library-id\"]"
        );
        let page = index.search(&json!({"query": "", "limit": 3})).unwrap();
        assert_eq!(page["remaining"], 1);
        assert_eq!(page["next"], json!({"offset": 3}));
        assert!(index.search(&json!({"limit": 0})).is_err());
    }

    async fn run(entries: &[Entry], code: &str) -> Result<Value, String> {
        super::super::execute_with(
            None,
            code,
            entries,
            20,
            std::time::Duration::from_secs(10),
            |tool, _| async move { Ok::<String, String>(tool) },
        )
        .await
        .result
    }

    fn tools() -> Vec<Entry> {
        vec![
            entry("github", "list_issues", "List issues"),
            entry("github", "get_issue", "Get one issue"),
            entry("memory", "search", "Search memory"),
            entry("api", "issues.list", "List issues"),
            entry("gh", "issues", "All issues"),
            entry("gh", "issues.list", "List issues"),
            entry("notes", "issues.archive", "Issue archive"),
            entry("notes", "issues.create", "Issue create"),
            entry("notes", "issues.get", "Issue get"),
            entry("notes", "issues.issues", "Issue issues"),
            entry("notes", "issues.list", "Issue list"),
        ]
    }

    #[tokio::test]
    async fn oc_dotted_tool_names_nest_and_every_spelling_calls_the_same_tool() {
        let v = run(
            &tools(),
            r#"return [await tools.api.issues.list({}), await tools.api["issues.list"]({}), await tools["api.issues"].list({}),
              Object.keys(tools.api), Object.keys(tools.api.issues), Object.keys(tools["api.issues"]),
              await tools.gh.issues({}), await tools.gh.issues.list(), Object.keys(tools.gh.issues)]"#,
        )
        .await
        .unwrap();
        assert_eq!(
            v,
            json!([
                "api_issues.list",
                "api_issues.list",
                "api_issues.list",
                ["issues"],
                ["list"],
                ["list"],
                "gh_issues",
                "gh_issues.list",
                ["list"]
            ])
        );
    }

    #[tokio::test]
    async fn oc_tool_references_enumerate_like_namespaces() {
        let v = run(
            &tools(),
            r#"const names = []
              for (const ns in tools) for (const name in tools[ns]) names.push(ns + "." + name)
              return [Object.keys(tools), Object.keys(tools.github), Object.keys(tools.github.list_issues), typeof search,
                typeof tools, typeof tools.github, names.slice(0, 3)]"#,
        )
        .await
        .unwrap();
        assert_eq!(
            v,
            json!([
                ["github", "memory", "api", "gh", "notes"],
                ["list_issues", "get_issue"],
                [],
                "function",
                "object",
                "function",
                ["github.list_issues", "github.get_issue", "memory.search"]
            ])
        );
    }

    #[tokio::test]
    async fn oc_unknown_paths_fail_with_the_closest_match() {
        let fail = |code: &'static str| async move { run(&tools(), code).await.unwrap_err() };
        assert!(fail("return Object.keys(tools.nonexistent)")
            .await
            .contains("Unknown tool namespace 'nonexistent'."));
        assert!(fail("return await tools.gh.issues.missing({})")
            .await
            .contains("Unknown tool 'gh.issues.missing'."));
        assert!(fail(r#"return await tools.gh.issues["get-list"]({})"#)
            .await
            .contains("Unknown tool 'gh.issues.get-list'. Did you mean tools.gh.issues.list?"));
        assert!(fail("return await tools.api.issues({})").await.contains(
            "Tool 'api.issues' is not callable, it is a namespace. Did you mean tools.api.issues.list? Use search to find available tools."
        ));
        assert!(fail("return await tools.notes.issues({})").await.contains(
            "Tool 'notes.issues' is not callable, it is a namespace. Did you mean tools.notes.issues.issues, tools.notes.issues.archive, or tools.notes.issues.create? 2 more under 'notes.issues'. Use search({ namespace: \"notes.issues\" }) to list them."
        ));
        assert!(fail("return await tools.github.get_issue({}, {})")
            .await
            .contains("Tool 'github.get_issue' expects at most one input object."));
    }

    #[tokio::test]
    async fn oc_search_returns_callable_paths_inside_programs() {
        let v = run(
            &tools(),
            r#"const r = search({ query: "", namespace: "gh.issues" })
              return [r.items.map((i) => i.path), search("tools.gh.issues.list").items.map((i) => i.path),
                search("memory").items[0].signature]"#,
        )
        .await
        .unwrap();
        assert_eq!(
            v,
            json!([
                ["tools.gh.issues", "tools.gh.issues.list"],
                ["tools.gh.issues.list"],
                "tools.memory.search(): Promise<unknown>"
            ])
        );
    }

    #[test]
    fn words_split_stem_and_keep_camel_case_whole() {
        assert_eq!(
            search_words("listUserGroups statuses settings CronJob"),
            [
                "list",
                "user",
                "group",
                "status",
                "setting",
                "cron",
                "job",
                "listusergroup",
                "cronjob"
            ]
        );
    }
}
