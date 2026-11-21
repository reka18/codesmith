// ---------------------------------------------------------------------
// Ported from OpenCode's `packages/codemode/test/openapi.test.ts` (MIT);
// the two fixtures are OpenCode's. Requests go to a loopback server,
// never the internet.
//
// Intentional differences:
// - CodeSmith's JSON objects keep their keys sorted, so property lists
//   and rendered types list fields alphabetically (OpenCode keeps
//   document order). The expectations here are OpenCode's, reordered.
// - A tool's `$defs` holds only the component definitions its schema
//   reaches, not every component.
// - The response cap is 10 MiB (OpenCode reads 50 MiB).
// - JavaScript-only cases (`undefined` inside arrays, prototype-inherited
//   keys, cyclic objects) have no JSON counterpart: they are tested with
//   the nearest JSON input (an unsupported nested value, an own key
//   named `__proto__`), or not at all where JSON cannot express them.
// - Calls also ask the `openapi` permission and keep credentials on the
//   configured origin (see the tests after the ported ones).
// ---------------------------------------------------------------------
use super::*;
use crate::codemode::fixture::{gate, serve, Answer, Seen};
use crate::codemode::{execute_with, ts_type, Outcome};
use crate::permission::{Effect, Reply, Rule};
use serde_json::json;

const BASE: &str = "http://localhost:4096";

fn happy_path() -> Value {
    serde_json::from_str(include_str!("fixtures/openapi-happy-path.json")).unwrap()
}

fn transports() -> Value {
    serde_json::from_str(include_str!("fixtures/openapi-transports.json")).unwrap()
}

fn gen(doc: &Value) -> Generated {
    from_spec(doc, Some(BASE))
}

fn tool<'a>(g: &'a Generated, name: &str) -> &'a Tool {
    g.tool(name).unwrap_or_else(|| {
        panic!(
            "{name} was not generated; tools: {:?}, skipped: {:?}",
            g.tools.iter().map(Tool::dotted).collect::<Vec<_>>(),
            g.skipped
        )
    })
}

fn names(g: &Generated) -> Vec<String> {
    g.tools.iter().map(Tool::dotted).collect()
}

fn input_ts(t: &Tool) -> String {
    ts_type(&t.input)
}

fn output_ts(t: &Tool) -> String {
    t.output
        .as_ref()
        .map(ts_type)
        .unwrap_or_else(|| "unknown".into())
}

fn keys(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

fn single_operation(operation: Value, method: &str) -> Value {
    let mut op = json!({"operationId": "test", "responses": {"200": {"description": "Success"}}});
    for (k, v) in operation.as_object().unwrap() {
        if v.is_null() {
            op.as_object_mut().unwrap().remove(k);
        } else {
            op[k] = v.clone();
        }
    }
    json!({"openapi": "3.1.0", "paths": {"/test": {method: op}}})
}

fn with(mut doc: Value, extra: Value) -> Value {
    for (k, v) in extra.as_object().unwrap() {
        doc[k] = v.clone();
    }
    doc
}

fn directional_spec(openapi: &str) -> Value {
    json!({
        "openapi": openapi,
        "paths": {"/users": {"post": {
            "operationId": "users.create",
            "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}}},
            "responses": {"200": {"description": "Created", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}}}}
        }}},
        "components": {"schemas": {
            "ReadOnlyID": {"type": "string", "readOnly": true},
            "User": {
                "type": "object", "additionalProperties": false,
                "required": ["id", "name", "password", "profile", "generated"],
                "properties": {
                    "id": {"type": "string", "readOnly": true},
                    "name": {"type": "string"},
                    "password": {"type": "string", "writeOnly": true},
                    "profile": {
                        "type": "object", "additionalProperties": false,
                        "required": ["createdAt", "secret", "label"],
                        "properties": {
                            "createdAt": {"type": "string", "readOnly": true},
                            "secret": {"type": "string", "writeOnly": true},
                            "label": {"type": "string"}
                        }
                    },
                    "generated": {"$ref": "#/components/schemas/ReadOnlyID"}
                }
            }
        }}
    })
}

/// Every request is allowed.
fn open_gate() -> crate::permission::Gate {
    gate(vec![Rule::new(ACTION, "*", Effect::Allow)], Reply::Once).0
}

fn api_at(doc: &Value, base: &str, resolver: Option<Arc<Resolver>>) -> Api {
    Api::new("api", doc, Some(base), vec![], resolver).unwrap()
}

/// Call `name` of `api` once.
async fn invoke(api: &Api, name: &str, input: Value) -> Result<Value, String> {
    let t = tool(&api.generated, name);
    call(api, t, &input, &open_gate()).await
}

/// Run `code` in Code Mode with `apis`' tools, each call allowed.
async fn program(apis: &[Arc<Api>], code: &str) -> Outcome {
    let g = open_gate();
    let g = &g;
    let mut entries = Vec::new();
    for a in apis {
        merge_entries(&mut entries, a.entries(|_| true));
    }
    execute_with(
        None,
        code,
        &entries,
        50,
        Duration::from_secs(30),
        |name, input| async move {
            let (a, t) = find(apis, &name).ok_or_else(|| format!("no tool {name}"))?;
            call(a, t, &input, g).await.map(|v| result_text(&v))
        },
    )
    .await
}

fn resolver(
    f: impl Fn(&AuthContext) -> Option<Credential> + Send + Sync + 'static,
) -> Option<Arc<Resolver>> {
    let r: Arc<Resolver> = Arc::new(move |cx: &AuthContext| Ok(f(cx)));
    Some(r)
}

fn last(log: &Arc<std::sync::Mutex<Vec<Seen>>>) -> Seen {
    log.lock().unwrap().last().cloned().expect("a request")
}

// ----- ported ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn oc_covers_a_representative_api_from_generation_through_execution() {
    let (base, log) = serve(|s| {
        if s.method == "POST" {
            return Answer::new(
                201,
                "application/vnd.example+json",
                json!({"id": "user-2", "name": "Grace", "email": "grace@example.test", "role": "admin"}).to_string(),
            );
        }
        if s.method == "DELETE" {
            return Answer::new(204, "", Vec::new());
        }
        if s.path() == "/search" {
            return Answer::new(200, "text/plain", "2 matches");
        }
        Answer::json(json!({"id": "user-1", "name": "Ada", "email": "ada@example.test", "role": "member"}))
    })
    .await;
    let resolutions = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let r = resolutions.clone();
    let api = api_at(
        &happy_path(),
        &base,
        resolver(move |cx| {
            r.lock().unwrap().push(cx.name.to_string());
            Some(if cx.name == "BearerAuth" {
                Credential::Bearer("bearer-secret".into())
            } else {
                Credential::ApiKey("api-secret".into())
            })
        }),
    );
    let g = &api.generated;
    assert!(g.skipped.is_empty(), "{:?}", g.skipped);
    let (get, create, search, remove) = (
        tool(g, "users.get"),
        tool(g, "users.create"),
        tool(g, "search.run"),
        tool(g, "users.remove"),
    );
    assert_eq!(
        input_ts(get),
        r#"{ "X-Trace-ID"?: string; include?: Array<string>; userId: string; verbose?: boolean }"#
    );
    assert_eq!(
        input_ts(create),
        r#"{ email: string; name: string; role?: "admin" | "member" }"#
    );
    assert_eq!(
        input_ts(search),
        "{ filter?: { page?: number; query: string }; tags?: Array<string> }"
    );
    assert_eq!(input_ts(remove), "{ userId: string }");
    assert!(output_ts(get).contains("id: string"), "{}", output_ts(get));
    assert!(output_ts(create).contains(r#"role?: "admin" | "member""#));
    assert_eq!(output_ts(search), "string");
    assert_eq!(output_ts(remove), "null");

    let out = program(
        &[Arc::new(api)],
        r#"
          const user = await tools.api.users.get({
            userId: "user-1",
            include: ["profile", "permissions"],
            verbose: true,
            "X-Trace-ID": "trace-1",
          })
          const created = await tools.api.users.create({
            name: "Grace",
            email: "grace@example.test",
            role: "admin",
          })
          const summary = await tools.api.search.run({
            filter: { query: "effect", page: 2 },
            tags: ["typescript", "runtime"],
          })
          const removed = await tools.api.users.remove({ userId: "user-1" })
          return { user, created, summary, removed }
        "#,
    )
    .await;
    let v = out.result.unwrap();
    assert_eq!(v["user"]["id"], "user-1");
    assert_eq!(v["user"]["name"], "Ada");
    assert_eq!(v["created"]["id"], "user-2");
    assert_eq!(v["created"]["name"], "Grace");
    assert_eq!(v["summary"], "2 matches");
    assert_eq!(v["removed"], Value::Null);
    assert_eq!(
        *resolutions.lock().unwrap(),
        ["BearerAuth", "ApiKey", "BearerAuth"]
    );
    let reqs = log.lock().unwrap().clone();
    assert_eq!(reqs.len(), 4);
    assert_eq!(reqs[0].path(), "/users/user-1");
    assert_eq!(
        reqs[0].param("include").as_deref(),
        Some("profile,permissions")
    );
    assert_eq!(reqs[0].param("verbose").as_deref(), Some("true"));
    assert_eq!(reqs[0].headers["x-trace-id"], "trace-1");
    assert_eq!(reqs[0].headers["authorization"], "Bearer bearer-secret");
    assert_eq!(reqs[1].param("api_key").as_deref(), Some("api-secret"));
    assert_eq!(
        reqs[1].json(),
        json!({"name": "Grace", "email": "grace@example.test", "role": "admin"})
    );
    assert_eq!(reqs[2].param("filter[query]").as_deref(), Some("effect"));
    assert_eq!(reqs[2].param("filter[page]").as_deref(), Some("2"));
    let tags: Vec<String> = reqs[2]
        .query()
        .into_iter()
        .filter(|(k, _)| k == "tags")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(tags, ["typescript", "runtime"]);
    assert!(!reqs[2].headers.contains_key("authorization"));
    assert_eq!(reqs[3].path(), "/users/user-1");
    assert_eq!(reqs[3].headers["authorization"], "Bearer bearer-secret");
}

#[test]
fn oc_generates_supported_operations_and_reports_unsupported_transports() {
    let g = gen(&transports());
    let skipped: Vec<(String, String, String)> = g
        .skipped
        .iter()
        .map(|s| (s.method.clone(), s.path.clone(), s.reason.clone()))
        .collect();
    let mut want = vec![
        ("GET", "/events", "SSE operations are not supported"),
        ("GET", "/files/{path}", "binary responses are not supported"),
        (
            "PUT",
            "/files/{path}",
            "request body has no JSON content (declared: application/octet-stream)",
        ),
        (
            "GET",
            "/terminals/{terminalID}/connect",
            "WebSocket operations are not supported",
        ),
    ]
    .into_iter()
    .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
    .collect::<Vec<_>>();
    let mut skipped = skipped;
    skipped.sort();
    want.sort();
    assert_eq!(skipped, want);
    let get = tool(&g, "records.get");
    assert_eq!(input_ts(get), "{ recordID: string }");
    assert_eq!(output_ts(get), "{ id: string; value: string }");
    for gone in [
        "events.subscribe",
        "files.read",
        "files.write",
        "terminals.connect",
    ] {
        assert!(g.tool(gone).is_none(), "{gone}");
    }
}

#[test]
fn oc_preserves_operation_path_sanitization_and_collision_handling() {
    let response = json!({"responses": {"200": {"description": "Success"}}});
    let op = |id: &str| with(response.clone(), json!({"operationId": id}));
    let g = gen(&json!({
        "openapi": "3.1.0",
        "paths": {
            "/first": {"get": op("group.item")},
            "/second": {"get": op("group.item")},
            "/third": {"get": op("group..other")},
        }
    }));
    for name in ["group.item", "group_item_2", "group.operation.other"] {
        tool(&g, name);
    }
}

#[test]
fn oc_does_not_reserve_names_for_unsupported_operations_between_duplicate_operation_ids() {
    let operation =
        json!({"operationId": "group.item", "responses": {"200": {"description": "Success"}}});
    for unsupported in [false, true] {
        let mut paths = json!({"/first": {"get": operation}, "/last": {"get": operation}});
        if unsupported {
            paths["/middle"] =
                json!({"get": with(operation.clone(), json!({"x-websocket": true}))});
        }
        let g = gen(&json!({"openapi": "3.1.0", "paths": paths}));
        assert_eq!(names(&g), ["group.item", "group_item_2"]);
        assert_eq!(tool(&g, "group.item").description, "GET /first");
        assert_eq!(tool(&g, "group_item_2").description, "GET /last");
        let skipped: Vec<_> = g
            .skipped
            .iter()
            .map(|s| (s.method.as_str(), s.path.as_str(), s.reason.as_str()))
            .collect();
        if unsupported {
            assert_eq!(
                skipped,
                [("GET", "/middle", "WebSocket operations are not supported")]
            );
        } else {
            assert!(skipped.is_empty());
        }
    }
}

#[test]
fn oc_synthesizes_flat_operation_ids_from_methods_and_paths() {
    let response = json!({"responses": {"200": {"description": "Success"}}});
    let g = gen(&json!({
        "openapi": "3.1.0",
        "paths": {
            "/users": {"get": response, "post": response},
            "/users/{id}": {"get": response, "patch": response, "delete": response},
            "/organizations/{organizationId}/users/{id}": {"get": response},
        }
    }));
    for name in [
        "getUsers",
        "postUsers",
        "getUsersById",
        "patchUsersById",
        "deleteUsersById",
        "getOrganizationsByOrganizationidUsersById",
    ] {
        tool(&g, name);
    }
}

#[test]
fn oc_lets_operation_parameters_override_matching_path_parameters() {
    let g = gen(&json!({
        "openapi": "3.1.0",
        "paths": {"/test": {
            "parameters": [{"name": "limit", "in": "query", "schema": {"type": "string"}}],
            "get": {
                "operationId": "test",
                "parameters": [
                    {"name": "limit", "in": "query", "schema": {"type": "boolean"}},
                    {"name": "limit", "in": "query", "required": true, "schema": {"type": "number"}}
                ],
                "responses": {"200": {"description": "Success"}}
            }
        }}
    }));
    assert_eq!(input_ts(tool(&g, "test")), "{ limit: number }");
}

#[test]
fn oc_normalizes_openapi_3_0_schemas() {
    let g = gen(&json!({
        "openapi": "3.0.3",
        "paths": {"/search": {"get": {
            "operationId": "search",
            "parameters": [{"in": "query", "name": "value", "schema": {"type": "string", "nullable": true, "minLength": 2}}],
            "responses": {"200": {"description": "Success"}}
        }}}
    }));
    let search = tool(&g, "search");
    assert_eq!(input_ts(search), "{ value?: string | null }");
    assert_eq!(search.input["properties"]["value"]["minLength"], 2);
}

#[test]
fn oc_preserves_schema_local_definitions_alongside_component_definitions() {
    let g = gen(&json!({
        "openapi": "3.1.0",
        "paths": {"/test": {"get": {
            "operationId": "test",
            "responses": {"200": {"description": "Success", "content": {"application/json": {
                "schema": {"$ref": "#/$defs/Local", "$defs": {"Local": {"type": "string"}}}
            }}}}
        }}},
        "components": {"schemas": {"Global": {"type": "number"}}}
    }));
    let out = tool(&g, "test").output.clone().unwrap();
    assert_eq!(out["$defs"]["Local"], json!({"type": "string"}));
    // Adapted: OpenCode carries every component (`Global` too); CodeSmith
    // only those the schema reaches.
    assert_eq!(output_ts(tool(&g, "test")), "string");
}

#[test]
fn oc_projects_read_only_and_write_only_properties_by_schema_direction() {
    for version in ["3.0.3", "3.1.0"] {
        let g = gen(&directional_spec(version));
        let t = tool(&g, "users.create");
        assert_eq!(
            input_ts(t),
            "{ name: string; password: string; profile: { label: string; secret: string } }",
            "{version}"
        );
        assert_eq!(
            output_ts(t),
            "{ generated: string; id: string; name: string; profile: { createdAt: string; label: string } }",
            "{version}"
        );
        let out = t.output.clone().unwrap();
        // Adapted: the flattened body reaches no component, so the
        // projected request `User` is the input itself.
        let request_user = &t.input;
        let response_user = &out["$defs"]["User"];
        assert_eq!(
            keys(&request_user["properties"]),
            ["name", "password", "profile"]
        );
        assert_eq!(
            request_user["required"],
            json!(["name", "password", "profile"])
        );
        assert_eq!(
            keys(&response_user["properties"]),
            ["generated", "id", "name", "profile"]
        );
        assert_eq!(
            response_user["required"],
            json!(["id", "name", "profile", "generated"])
        );
    }
}

fn post_body(schema: Value) -> Value {
    single_operation(
        json!({"requestBody": {"required": true, "content": {"application/json": {"schema": schema}}}}),
        "post",
    )
}

#[test]
fn oc_projects_directional_annotations_through_local_refs_and_all_of_composition() {
    let g = gen(&post_body(json!({
        "type": "object", "additionalProperties": false,
        "required": ["local", "composed", "name"],
        "properties": {
            "local": {"$ref": "#/$defs/ReadOnlyValue"},
            "composed": {"allOf": [{"$ref": "#/$defs/ReadOnlyValue"}]},
            "name": {"type": "string"}
        },
        "$defs": {"ReadOnlyValue": {"type": "string", "readOnly": true}}
    })));
    assert_eq!(input_ts(tool(&g, "test")), "{ name: string }");
}

#[test]
fn oc_honors_declarations_that_are_siblings_of_a_ref() {
    let doc = with(
        post_body(json!({
            "type": "object", "additionalProperties": false, "required": ["record"],
            "properties": {"record": {
                "$ref": "#/components/schemas/Base",
                "properties": {"extra": {"type": "string", "readOnly": true}, "note": {"type": "string"}},
                "required": ["extra", "note", "id"]
            }}
        })),
        json!({"components": {"schemas": {"Base": {
            "type": "object", "required": ["id", "name"],
            "properties": {"id": {"type": "string", "readOnly": true}, "name": {"type": "string"}}
        }}}}),
    );
    let g = gen(&doc);
    let t = tool(&g, "test");
    let record = &t.input["properties"]["record"];
    let base = &t.input["$defs"]["Base"];
    assert_eq!(keys(&record["properties"]), ["note"]);
    assert_eq!(record["required"], json!(["note"]));
    assert_eq!(keys(&base["properties"]), ["name"]);
    assert_eq!(base["required"], json!(["name"]));
}

#[test]
fn oc_honors_directional_declarations_on_intermediate_reference_hops() {
    let doc = with(
        post_body(json!({
            "type": "object", "additionalProperties": false, "required": ["secret", "name"],
            "properties": {"secret": {"$ref": "#/components/schemas/Middle"}, "name": {"type": "string"}}
        })),
        json!({"components": {"schemas": {
            "Middle": {"$ref": "#/components/schemas/Plain", "readOnly": true},
            "Plain": {"type": "string"}
        }}}),
    );
    assert_eq!(input_ts(tool(&gen(&doc), "test")), "{ name: string }");
}

#[test]
fn oc_projects_cyclic_component_references_without_hanging() {
    let doc = with(
        post_body(json!({"$ref": "#/components/schemas/Node"})),
        json!({"components": {"schemas": {"Node": {
            "type": "object", "required": ["id", "name", "child"],
            "properties": {
                "id": {"type": "string", "readOnly": true},
                "name": {"type": "string"},
                "child": {"$ref": "#/components/schemas/Node"}
            }
        }}}}),
    );
    let g = gen(&doc);
    let node = &tool(&g, "test").input["$defs"]["Node"];
    assert_eq!(keys(&node["properties"]), ["child", "name"]);
    assert_eq!(node["required"], json!(["name", "child"]));
}

#[test]
fn oc_projects_diamond_shaped_reference_graphs_in_linear_time() {
    let depth = 30;
    let mut schemas = serde_json::Map::new();
    for i in 0..depth {
        let s = if i == depth - 1 {
            json!({"type": "object", "properties": {"id": {"type": "string", "readOnly": true}, "name": {"type": "string"}}})
        } else {
            let next = format!("#/components/schemas/C{}", i + 1);
            json!({"allOf": [{"$ref": next}, {"$ref": next}]})
        };
        schemas.insert(format!("C{i}"), s);
    }
    let doc = with(
        post_body(json!({"$ref": "#/components/schemas/C0"})),
        json!({"components": {"schemas": schemas}}),
    );
    let started = std::time::Instant::now();
    let g = gen(&doc);
    assert!(started.elapsed() < Duration::from_secs(10));
    let leaf = &tool(&g, "test").input["$defs"][format!("C{}", depth - 1)];
    assert_eq!(keys(&leaf["properties"]), ["name"]);
}

#[test]
fn oc_resolves_hiding_through_reference_cycles_regardless_of_evaluation_order() {
    let schemas = json!({
        "Wrap": {"allOf": [{"$ref": "#/components/schemas/Loop"}]},
        "Loop": {"allOf": [{"$ref": "#/components/schemas/Wrap"}, {"readOnly": true}]}
    });
    // CodeSmith's sorted keys visit `a` first either way; the two orders
    // are which of the cycle's members it enters at.
    for (a, b) in [("Loop", "Wrap"), ("Wrap", "Loop")] {
        let doc = with(
            post_body(json!({
                "type": "object", "additionalProperties": false,
                "required": ["a", "b", "name"],
                "properties": {
                    "a": {"$ref": format!("#/components/schemas/{a}")},
                    "b": {"$ref": format!("#/components/schemas/{b}")},
                    "name": {"type": "string"}
                }
            })),
            json!({"components": {"schemas": schemas}}),
        );
        assert_eq!(
            input_ts(tool(&gen(&doc), "test")),
            "{ name: string }",
            "{a} first"
        );
    }
}

#[test]
fn oc_keeps_not_if_and_contains_subschemas_unprojected() {
    let not = json!({"required": ["secret"], "properties": {"secret": {"type": "string", "readOnly": true}}});
    let if_ =
        json!({"required": ["kind"], "properties": {"kind": {"type": "string", "readOnly": true}}});
    let g = gen(&post_body(json!({
        "type": "object", "additionalProperties": false, "required": ["record"],
        "properties": {"record": {"type": "object", "not": not, "if": if_}}
    })));
    let record = &tool(&g, "test").input["properties"]["record"];
    assert_eq!(record["not"], not);
    assert_eq!(record["if"], if_);
}

#[test]
fn oc_does_not_hide_properties_whose_direction_is_declared_only_in_any_of_or_one_of_alternatives() {
    let alternatives = json!([{"type": "string", "readOnly": true}, {"type": "number"}]);
    let g = gen(&post_body(json!({
        "type": "object", "additionalProperties": false, "required": ["choice", "pick"],
        "properties": {"choice": {"anyOf": alternatives}, "pick": {"oneOf": alternatives}}
    })));
    let props = &tool(&g, "test").input["properties"];
    assert_eq!(keys(props), ["choice", "pick"]);
    assert_eq!(props["choice"]["anyOf"], alternatives);
    assert_eq!(props["pick"]["oneOf"], alternatives);
}

#[test]
fn oc_does_not_misresolve_shadowed_local_defs_when_flattening_body_fields() {
    let g = gen(&post_body(json!({
        "type": "object", "additionalProperties": false, "required": ["record"],
        "$defs": {"Value": {"type": "string"}},
        "properties": {"record": {
            "type": "object", "required": ["x"],
            "properties": {"x": {"$ref": "#/$defs/Value"}},
            "$defs": {"Value": {"type": "string", "readOnly": true}}
        }}
    })));
    let record = &tool(&g, "test").input["properties"]["record"];
    assert_eq!(keys(&record["properties"]), ["x"]);
    assert_eq!(record["required"], json!(["x"]));
}

#[test]
fn oc_projects_directional_annotations_inside_parameter_schemas() {
    let g = gen(&single_operation(
        json!({"parameters": [{
            "name": "filter", "in": "query", "required": true,
            "schema": {"type": "object", "required": ["state", "id"], "properties": {"state": {"type": "string"}, "id": {"type": "string", "readOnly": true}}}
        }]}),
        "get",
    ));
    assert_eq!(input_ts(tool(&g, "test")), "{ filter: { state: string } }");
}

/// Adapted: JSON has no inherited keys. What stays pinned is that only a
/// schema's own `readOnly` hides it: one named `readOnly` as a property
/// (data, not an annotation) does not.
#[test]
fn oc_ignores_inherited_directional_annotations() {
    let g = gen(&single_operation(
        json!({"parameters": [{
            "name": "filter", "in": "query", "required": true,
            "schema": {
                "type": "object",
                "properties": {
                    "value": {"type": "object", "properties": {"readOnly": {"type": "boolean"}}, "required": ["readOnly"]},
                    "id": {"type": "string", "readOnly": true}
                },
                "required": ["value", "id"]
            }
        }]}),
        "get",
    ));
    assert_eq!(
        input_ts(tool(&g, "test")),
        "{ filter: { value: { readOnly: boolean } } }"
    );
}

#[test]
fn oc_cleans_required_properties_across_all_of_branches() {
    let g = gen(&post_body(json!({
        "type": "object", "required": ["id", "name"],
        "allOf": [{"type": "object", "required": ["id", "name"], "properties": {"id": {"type": "string", "readOnly": true}, "name": {"type": "string"}}}]
    })));
    let body = &tool(&g, "test").input["properties"]["body"];
    assert_eq!(body["required"], json!(["name"]));
    assert_eq!(body["allOf"][0]["required"], json!(["name"]));
    assert_eq!(keys(&body["allOf"][0]["properties"]), ["name"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_keeps_directional_schemas_model_facing_while_preserving_runtime_pass_through() {
    let (base, log) = serve(|_| {
        Answer::json(json!({
            "id": "server-id", "name": "Ada", "password": "returned-by-server",
            "profile": {"createdAt": "today", "secret": "returned-secret", "label": "primary"},
            "generated": "generated-id"
        }))
    })
    .await;
    let api = api_at(&directional_spec("3.1.0"), &base, None);
    let result = invoke(
        &api,
        "users.create",
        json!({
            "id": "ignored-top-level",
            "generated": "ignored-generated",
            "name": "Ada",
            "password": "request-secret",
            "profile": {"createdAt": "sent-nested", "secret": "nested-secret", "label": "primary"}
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        last(&log).json(),
        json!({
            "name": "Ada",
            "password": "request-secret",
            "profile": {"createdAt": "sent-nested", "secret": "nested-secret", "label": "primary"}
        })
    );
    assert_eq!(result["password"], "returned-by-server");
    assert_eq!(result["profile"]["secret"], "returned-secret");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_exposes_generated_operations_through_code_mode_discovery() {
    let api = Arc::new(api_at(&happy_path(), BASE, None));
    let out = program(
        &[api],
        r#"return search({ query: "get a user", namespace: "api", limit: 1 })"#,
    )
    .await;
    let v = out.result.unwrap();
    assert_eq!(v["items"][0]["path"], "tools.api.users.get");
    assert_eq!(v["items"][0]["description"], "Get a user");
    assert!(v.to_string().contains("userId: string"), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_serializes_supported_simple_and_form_parameter_shapes() {
    let (base, log) = serve(|_| Answer::json(json!({"ok": true}))).await;
    let doc = json!({
        "openapi": "3.1.0",
        "paths": {"/items/{keys}": {"get": {
            "operationId": "items",
            "parameters": [
                {"name": "keys", "in": "path", "required": true, "schema": {"type": "array", "items": {"type": "string"}}},
                {"name": "tags", "in": "query", "style": "form", "explode": false, "schema": {"type": "array"}},
                {"name": "filter", "in": "query", "style": "form", "explode": true, "schema": {"type": "object"}},
                {"name": "nullable", "in": "query", "required": true, "schema": {"type": ["string", "null"]}},
                {"name": "constructor", "in": "query", "schema": {"type": "string"}},
                {"name": "meta", "in": "header", "style": "simple", "explode": true, "schema": {"type": "object"}}
            ],
            "responses": {"200": {"description": "Success"}}
        }}}
    });
    let api = api_at(&doc, &base, None);
    invoke(
        &api,
        "items",
        json!({
            "keys": ["a!", "b*"],
            "tags": ["x", "y"],
            "filter": {"state": "open", "page": 2},
            "nullable": null,
            "constructor": "safe",
            "meta": {"a": "b", "c": "d"}
        }),
    )
    .await
    .unwrap();
    let r = last(&log);
    assert_eq!(r.path(), "/items/a%21,b%2A");
    assert_eq!(r.param("tags").as_deref(), Some("x,y"));
    assert_eq!(r.param("state").as_deref(), Some("open"));
    assert_eq!(r.param("page").as_deref(), Some("2"));
    assert_eq!(r.param("nullable").as_deref(), Some("null"));
    assert_eq!(r.param("constructor").as_deref(), Some("safe"));
    assert_eq!(r.headers["meta"], "a=b,c=d");
    // Adapted: `[undefined]` is `[null]` in JSON.
    let e = invoke(&api, "items", json!({"keys": [{}], "nullable": null}))
        .await
        .unwrap_err();
    assert!(e.contains("unsupported nested value"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_preserves_ordered_exploded_and_deep_object_query_parameters() {
    let (base, log) = serve(|_| Answer::json(json!({"ok": true}))).await;
    let doc = single_operation(
        json!({"parameters": [
            {"name": "tags", "in": "query", "style": "form", "explode": true, "schema": {"type": "array"}},
            {"name": "filter", "in": "query", "style": "form", "explode": true, "schema": {"type": "object"}},
            {"name": "location", "in": "query", "style": "deepObject", "explode": true, "schema": {"type": "object"}}
        ]}),
        "get",
    );
    let api = api_at(&doc, &base, None);
    invoke(
        &api,
        "test",
        json!({
            "tags": ["first value", "second&value"],
            "filter": {"state": "open now", "page": 2},
            "location": {"directory": "/tmp/a b", "workspace": "work&1"}
        }),
    )
    .await
    .unwrap();
    // Adapted: `filter`'s keys come sorted (`page` before `state`).
    assert_eq!(
        last(&log).target,
        "/test?tags=first+value&tags=second%26value&page=2&state=open+now&location%5Bdirectory%5D=%2Ftmp%2Fa+b&location%5Bworkspace%5D=work%261"
    );
    for (input, want) in [
        (
            json!({"tags": [{}]}),
            "Parameter 'tags' contains an unsupported nested value.",
        ),
        (
            json!({"filter": {"state": {}}}),
            "Query parameter 'filter' contains an unsupported nested value.",
        ),
        (
            json!({"location": {"directory": []}}),
            "Deep-object parameter 'location' contains an unsupported nested value.",
        ),
    ] {
        let e = invoke(&api, "test", input).await.unwrap_err();
        assert!(e.contains(want), "{e}");
    }
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn oc_skips_unsupported_parameter_encodings_and_malformed_security() {
    let ok = json!({"200": {"description": "Success"}});
    let g = gen(&json!({
        "openapi": "3.1.0",
        "security": [{"bearer": []}],
        "paths": {
            "/cookie": {"get": {"operationId": "cookie", "parameters": [{"name": "session", "in": "cookie", "schema": {"type": "string"}}], "responses": ok}},
            "/reserved": {"get": {"operationId": "reserved", "parameters": [{"name": "query", "in": "query", "allowReserved": true, "schema": {"type": "string"}}], "responses": ok}},
            "/invalid-style": {"get": {"operationId": "invalidStyle", "parameters": [{"name": "query", "in": "query", "style": 42, "schema": {"type": "string"}}], "responses": ok}},
            "/security": {"get": {"operationId": "security", "security": null, "responses": ok}}
        }
    }));
    assert!(g.tools.is_empty());
    let mut reasons: Vec<(String, String)> = g
        .skipped
        .iter()
        .map(|s| (s.path.clone(), s.reason.clone()))
        .collect();
    reasons.sort();
    assert_eq!(
        reasons,
        [
            ("/cookie", "cookie parameter 'session' is not supported"),
            ("/invalid-style", "parameter 'query' has an invalid style"),
            (
                "/reserved",
                "parameter 'query' uses unsupported allowReserved encoding"
            ),
            ("/security", "security declaration is not an array"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
    );
}

#[test]
fn oc_fails_closed_on_prototype_named_missing_security_schemes() {
    let g = gen(&single_operation(
        json!({"security": [{"__proto__": []}]}),
        "get",
    ));
    assert!(g.tools.is_empty());
    assert_eq!(
        g.skipped[0].reason,
        "security requirement references missing or malformed scheme: __proto__"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_resolves_bearer_authentication_without_exposing_it_as_input() {
    let (base, log) = serve(|_| Answer::json(json!({"ok": true}))).await;
    let doc = with(
        single_operation(json!({"operationId": null}), "get"),
        json!({
            "security": [{"bearer": []}],
            "components": {"securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}}}
        }),
    );
    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let c = contexts.clone();
    let api = api_at(
        &doc,
        &base,
        resolver(move |cx| {
            c.lock().unwrap().push((
                cx.name.to_string(),
                cx.scheme.clone(),
                cx.scopes.to_vec(),
                cx.operation.clone(),
            ));
            Some(Credential::Bearer("secret".into()))
        }),
    );
    invoke(&api, "getTest", json!({})).await.unwrap();
    assert_eq!(input_ts(tool(&api.generated, "getTest")), "{}");
    assert_eq!(last(&log).headers["authorization"], "Bearer secret");
    let contexts = contexts.lock().unwrap();
    assert_eq!(contexts.len(), 1);
    let (name, scheme, scopes, op) = &contexts[0];
    assert_eq!(name, "bearer");
    assert_eq!(
        *scheme,
        Scheme::Http {
            scheme: "bearer".into()
        }
    );
    assert!(scopes.is_empty());
    assert_eq!(
        *op,
        Operation {
            operation_id: None,
            method: "GET".into(),
            path: "/test".into(),
            summary: None,
            description: None,
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_applies_authentication_carriers_without_prototype_or_collision_loss() {
    let (base, log) = serve(|_| Answer::json(json!({"ok": true}))).await;
    let authenticated = |security: Value, schemes: Value| {
        let doc = with(
            single_operation(json!({}), "get"),
            json!({"security": security, "components": {"securitySchemes": schemes}}),
        );
        api_at(
            &doc,
            &base,
            resolver(|_| Some(Credential::ApiKey("secret".into()))),
        )
    };
    let prototype = authenticated(
        json!([{"key": []}]),
        json!({"key": {"type": "apiKey", "in": "query", "name": "__proto__"}}),
    );
    invoke(&prototype, "test", json!({})).await.unwrap();
    assert_eq!(last(&log).param("__proto__").as_deref(), Some("secret"));

    let duplicate = authenticated(
        json!([{"first": [], "second": []}]),
        json!({
            "first": {"type": "apiKey", "in": "header", "name": "x-key"},
            "second": {"type": "apiKey", "in": "header", "name": "x-key"}
        }),
    );
    let e = invoke(&duplicate, "test", json!({})).await.unwrap_err();
    assert!(e.contains("multiple credentials"), "{e}");

    let cookie = authenticated(
        json!([{"key": []}]),
        json!({"key": {"type": "apiKey", "in": "cookie", "name": "session"}}),
    );
    assert!(cookie.generated.tools.is_empty());
    assert_eq!(
        cookie.generated.skipped[0].reason,
        "cookie authentication 'key' is not supported"
    );

    let doc = with(
        single_operation(json!({}), "get"),
        json!({
            "security": [{"cookie": []}, {"bearer": []}],
            "components": {"securitySchemes": {
                "cookie": {"type": "apiKey", "in": "cookie", "name": "session"},
                "bearer": {"type": "http", "scheme": "bearer"}
            }}
        }),
    );
    let alternative = api_at(
        &doc,
        &base,
        resolver(|cx| (cx.name == "bearer").then(|| Credential::Bearer("secret".into()))),
    );
    invoke(&alternative, "test", json!({})).await.unwrap();
    assert_eq!(last(&log).headers["authorization"], "Bearer secret");
}

#[test]
fn oc_honors_server_precedence_and_rejects_ambiguous_base_urls() {
    let doc = with(
        single_operation(
            json!({"servers": [{"url": "https://operation.example/v1"}]}),
            "get",
        ),
        json!({"servers": [{"url": "https://document.example"}]}),
    );
    let g = from_spec(&doc, None);
    // Adapted: no request is sent (that would leave this machine); the
    // URL it would go to is checked instead.
    assert_eq!(
        tool(&g, "test").plan.url,
        "https://operation.example/v1/test"
    );

    let invalid = from_spec(&doc, Some("https://example.com/api?tenant=one"));
    assert!(invalid.tools.is_empty());
    assert!(invalid.skipped[0]
        .reason
        .contains("unsupported query string or fragment"));

    let malformed = from_spec(&doc, Some("https:/example.com"));
    assert!(malformed.tools.is_empty());
    assert!(
        malformed.skipped[0]
            .reason
            .contains("not an absolute HTTP(S) URL"),
        "{:?}",
        malformed.skipped
    );
}

#[test]
fn oc_resolves_chained_response_refs_before_detecting_unsupported_transports() {
    let doc = with(
        single_operation(
            json!({"responses": {"200": {"$ref": "#/components/responses/First"}}}),
            "get",
        ),
        json!({"components": {"responses": {
            "First": {"$ref": "#/components/responses/Stream"},
            "Stream": {"content": {"text/event-stream": {"schema": {"type": "string"}}}}
        }}}),
    );
    let g = gen(&doc);
    assert!(g.tools.is_empty());
    assert_eq!(g.skipped[0].reason, "SSE operations are not supported");
}

#[test]
fn oc_resolves_response_schemas_before_detecting_binary_output() {
    let doc = with(
        single_operation(
            json!({"responses": {"200": {"content": {"text/plain": {"schema": {"$ref": "#/components/schemas/File"}}}}}}),
            "get",
        ),
        json!({"components": {"schemas": {"File": {"type": "string", "format": "binary"}}}}),
    );
    let g = gen(&doc);
    assert!(g.tools.is_empty());
    assert_eq!(g.skipped[0].reason, "binary responses are not supported");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_validates_composite_parameters_before_resolving_auth() {
    let (base, log) = serve(|_| Answer::json(json!({"ok": true}))).await;
    let doc = with(
        single_operation(
            json!({"parameters": [{"name": "filter", "in": "query", "style": "form", "explode": true, "schema": {"type": "object"}}]}),
            "get",
        ),
        json!({
            "security": [{"bearer": []}],
            "components": {"securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}}}
        }),
    );
    let resolutions = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let r = resolutions.clone();
    let api = api_at(
        &doc,
        &base,
        resolver(move |cx| {
            r.lock().unwrap().push(cx.name.to_string());
            Some(Credential::Bearer("secret".into()))
        }),
    );
    // Adapted: `{value: undefined}` is `{value: {}}` (a nested value).
    let e = invoke(&api, "test", json!({"filter": {"value": {}}}))
        .await
        .unwrap_err();
    assert!(e.contains("unsupported nested value"), "{e}");
    assert!(resolutions.lock().unwrap().is_empty());
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_preserves_json_media_types() {
    let (base, log) = serve(|_| Answer::json(json!({"ok": true}))).await;
    let doc = single_operation(
        json!({"requestBody": {"required": true, "content": {"application/merge-patch+json": {"schema": {"type": "object"}}}}}),
        "post",
    );
    let api = api_at(&doc, &base, None);
    invoke(&api, "test", json!({"body": {"name": "updated"}}))
        .await
        .unwrap();
    let r = last(&log);
    assert_eq!(r.headers["content-type"], "application/merge-patch+json");
    assert_eq!(r.json(), json!({"name": "updated"}));
    // OpenCode's unencodable (cyclic) body cannot be written in JSON.
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_rejects_oversized_and_malformed_json_responses() {
    let (base, _) = serve(|s| match s.path() {
        "/oversized/test" => {
            let mut a = Answer::new(200, "", Vec::new());
            a.claimed = Some(MAX_RESPONSE_BYTES as u64 + 1);
            a.length = false;
            a
        }
        "/malformed/test" => Answer::new(200, "application/json", "{"),
        _ => {
            let mut a = Answer::new(200, "", vec![0u8; MAX_RESPONSE_BYTES + 1]);
            a.length = false;
            a
        }
    })
    .await;
    let doc = single_operation(json!({}), "get");
    let cap = format!(
        "response exceeds {} MiB",
        MAX_RESPONSE_BYTES / (1024 * 1024)
    );
    for (prefix, want) in [
        ("oversized", cap.as_str()),
        ("malformed", "returned malformed JSON"),
        ("chunked", cap.as_str()),
    ] {
        let api = api_at(&doc, &format!("{base}/{prefix}"), None);
        let e = invoke(&api, "test", json!({})).await.unwrap_err();
        assert!(e.contains(want), "{prefix}: {e}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_keeps_non_json_responses_raw_and_unions_every_success_output() {
    let (base, _) = serve(|_| Answer::new(200, "text/plain", "123")).await;
    let doc = single_operation(
        json!({"responses": {
            "200": {"description": "Text", "content": {"text/plain": {"schema": {"type": "string"}}}},
            "204": {"description": "Empty"}
        }}),
        "get",
    );
    let api = api_at(&doc, &base, None);
    assert_eq!(output_ts(tool(&api.generated, "test")), "string | null");
    assert_eq!(invoke(&api, "test", json!({})).await, Ok(json!("123")));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_fails_missing_required_parameters_before_auth_and_network() {
    let (base, log) = serve(|_| Answer::json(json!({}))).await;
    let api = Arc::new(api_at(&transports(), &base, None));
    let out = program(&[api], "return await tools.api.records.get({})").await;
    let e = out.result.unwrap_err();
    assert!(
        e.contains("Missing required path parameter 'recordID'"),
        "{e}"
    );
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_prefixes_cross_location_collisions_and_reconstructs_the_http_request() {
    let doc = json!({
        "openapi": "3.1.0",
        "info": {"title": "collision", "version": "1.0.0"},
        "paths": {
            "/echo": {"post": {
                "operationId": "echo",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "string"}}}},
                "responses": {"204": {"description": "Echoed"}}
            }},
            "/things/{id}": {"post": {
                "operationId": "things.update",
                "parameters": [
                    {"name": "id", "in": "path", "required": true, "schema": {"type": "string"}},
                    {"name": "id", "in": "query", "required": true, "schema": {"type": "string"}},
                    {"name": "path_id", "in": "query", "schema": {"type": "string"}},
                    {"name": "id", "in": "header", "required": true, "schema": {"type": "string"}}
                ],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false
                }}}},
                "responses": {"204": {"description": "Updated"}}
            }}
        }
    });
    let (base, log) = serve(|_| Answer::new(204, "", Vec::new())).await;
    let api = api_at(&doc, &base, None);
    // Adapted: the tools sit under `api`, and fields come sorted.
    assert_eq!(
        input_ts(tool(&api.generated, "things.update")),
        "{ body_id: string; header_id: string; path_id: string; path_id_2?: string; query_id: string }"
    );
    assert_eq!(input_ts(tool(&api.generated, "echo")), "{ body: string }");
    let out = program(
        &[Arc::new(api)],
        r#"
            const updated = await tools.api.things.update({ path_id: "path", query_id: "query", path_id_2: "literal", header_id: "header", body_id: "body" })
            const echoed = await tools.api.echo({ body: "hello" })
            return { updated, echoed }
        "#,
    )
    .await;
    assert!(out.result.is_ok(), "{:?}", out.result);
    let reqs = log.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].path(), "/things/path");
    assert_eq!(reqs[0].param("id").as_deref(), Some("query"));
    assert_eq!(reqs[0].param("path_id").as_deref(), Some("literal"));
    assert_eq!(reqs[0].headers["id"], "header");
    assert_eq!(reqs[0].json(), json!({"id": "body"}));
    assert_eq!(reqs[1].json(), json!("hello"));
}

#[test]
fn oc_keeps_bodies_nested_when_flattening_would_lose_schema_semantics() {
    let body = |schema: Value, required: bool| json!({"required": required, "content": {"application/json": {"schema": schema}}});
    let cases = [
        (
            "optional",
            body(
                json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"], "additionalProperties": false}),
                false,
            ),
        ),
        (
            "dictionary",
            body(
                json!({"type": "object", "additionalProperties": {"type": "string"}}),
                true,
            ),
        ),
        (
            "composed",
            body(
                json!({"type": "object", "allOf": [{"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}], "additionalProperties": false}),
                true,
            ),
        ),
        (
            "nullable",
            body(
                json!({"type": ["object", "null"], "properties": {"name": {"type": "string"}}, "additionalProperties": false}),
                true,
            ),
        ),
    ];
    let mut paths = serde_json::Map::new();
    for (name, rb) in &cases {
        paths.insert(
            format!("/body/{name}"),
            json!({"post": {"operationId": format!("body.{name}"), "requestBody": rb, "responses": {"204": {"description": "Accepted"}}}}),
        );
    }
    let g = gen(
        &json!({"openapi": "3.1.0", "info": {"title": "bodies", "version": "1.0.0"}, "paths": paths}),
    );
    for (name, _) in &cases {
        let t = tool(&g, &format!("body.{name}"));
        assert_eq!(keys(&t.input["properties"]), ["body"], "{name}");
    }
    assert_eq!(
        input_ts(tool(&g, "body.optional")),
        "{ body?: { name: string } }"
    );
}

// ----- CodeSmith's own: permissions, credentials, config ----------------

fn secured(security: Value, schemes: Value) -> Value {
    with(
        json!({
            "openapi": "3.1.0",
            "paths": {
                "/test": {
                    "get": {"operationId": "test", "responses": {"200": {"description": "Success"}}},
                    "post": {"operationId": "make", "responses": {"200": {"description": "Success"}}}
                }
            }
        }),
        json!({"security": security, "components": {"securitySchemes": schemes}}),
    )
}

/// A call asks `openapi` for `<METHOD> <url>`; "always" saves that method
/// on that origin only.
#[tokio::test(flavor = "multi_thread")]
async fn calls_ask_the_openapi_permission_and_always_covers_the_origin() {
    let (base, log) = serve(|_| Answer::json(json!({"ok": true}))).await;
    let doc = single_operation(
        json!({"parameters": [{"name": "q", "in": "query", "schema": {"type": "string"}}]}),
        "get",
    );
    let api = api_at(&doc, &base, None);
    let t = tool(&api.generated, "test");
    let (g, asked) = gate(vec![], Reply::Always);
    call(&api, t, &json!({"q": "a"}), &g).await.unwrap();
    call(&api, t, &json!({"q": "b"}), &g).await.unwrap();
    let asked = asked.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].action, "openapi");
    assert_eq!(asked[0].resources, vec![format!("GET {base}/test?q=a")]);
    assert_eq!(asked[0].save, vec![format!("GET {base}/*")]);
    assert_eq!(
        asked[0].title,
        format!("Call api.test: GET {base}/test?q=a")
    );
    assert_eq!(log.lock().unwrap().len(), 2);
    // Another origin for the same API asks again.
    let other = api_at(&doc, &base.replace("127.0.0.1", "localhost"), None);
    let (g2, asked2) = gate(vec![], Reply::Reject(None));
    let mut g2 = g2;
    g2.share_approvals(g.approvals());
    let e = call(&other, tool(&other.generated, "test"), &json!({}), &g2)
        .await
        .unwrap_err();
    assert!(e.contains("declined"), "{e}");
    assert_eq!(asked2.asked.lock().unwrap().len(), 1);
    assert_eq!(log.lock().unwrap().len(), 2);
}

/// A deny rule stops the call before anything is resolved or sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_denied_call_sends_nothing() {
    let (base, log) = serve(|_| Answer::json(json!({}))).await;
    let resolved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let r = resolved.clone();
    let api = api_at(
        &secured(
            json!([{"b": []}]),
            json!({"b": {"type": "http", "scheme": "bearer"}}),
        ),
        &base,
        resolver(move |_| {
            r.store(true, std::sync::atomic::Ordering::SeqCst);
            Some(Credential::Bearer("tok".into()))
        }),
    );
    let (g, _) = gate(vec![Rule::new(ACTION, "POST *", Effect::Deny)], Reply::Once);
    let e = call(&api, tool(&api.generated, "make"), &json!({}), &g)
        .await
        .unwrap_err();
    assert!(e.contains("Permission denied"), "{e}");
    assert!(!resolved.load(std::sync::atomic::Ordering::SeqCst));
    assert!(log.lock().unwrap().is_empty());
    // The rule also hides the operation from the catalog.
    let rules = [Rule::new(ACTION, "POST *", Effect::Deny)];
    let shown: Vec<String> = api
        .entries(|t| {
            crate::permission::evaluate(ACTION, &template_resource(t), &rules) != Effect::Deny
        })
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(shown, ["test"]);
}

/// Credentials and configured headers go to the API's origin, and stop
/// for good once a redirect leaves it, even when a later hop comes back.
#[tokio::test(flavor = "multi_thread")]
async fn credentials_stay_on_the_configured_origin() {
    let a_url = Arc::new(std::sync::Mutex::new(String::new()));
    let a = a_url.clone();
    let (b, b_log) =
        serve(move |_| Answer::redirect(302, &format!("{}/final", a.lock().unwrap()))).await;
    let to = format!("{b}/away");
    let (base, a_log) = serve(move |s| match s.path() {
        "/test" => Answer::redirect(302, "/hop"),
        "/hop" => Answer::redirect(307, &to),
        _ => Answer::json(json!({"done": true})),
    })
    .await;
    *a_url.lock().unwrap() = base.clone();
    let doc = secured(
        json!([{"h": [], "q": []}]),
        json!({
            "h": {"type": "http", "scheme": "bearer"},
            "q": {"type": "apiKey", "in": "query", "name": "key"}
        }),
    );
    let api = Api::new(
        "api",
        &doc,
        Some(&base),
        vec![("x-tenant".into(), "acme".into())],
        resolver(|cx| {
            Some(if cx.name == "h" {
                Credential::Bearer("bearer-secret".into())
            } else {
                Credential::ApiKey("query-secret".into())
            })
        }),
    )
    .unwrap();
    let v = call(&api, tool(&api.generated, "test"), &json!({}), &open_gate())
        .await
        .unwrap();
    assert_eq!(v, json!({"done": true}));
    let on_origin = a_log.lock().unwrap().clone();
    let paths: Vec<&str> = on_origin.iter().map(Seen::path).collect();
    assert_eq!(paths, ["/test", "/hop", "/final"]);
    for r in &on_origin[..2] {
        assert_eq!(r.headers["authorization"], "Bearer bearer-secret");
        assert_eq!(r.headers["x-tenant"], "acme");
        assert_eq!(r.param("key").as_deref(), Some("query-secret"));
    }
    let away = b_log.lock().unwrap().clone();
    assert_eq!(away.len(), 1);
    for r in [&away[0], &on_origin[2]] {
        assert!(!r.headers.contains_key("authorization"), "{}", r.target);
        assert!(!r.headers.contains_key("x-tenant"), "{}", r.target);
        assert_eq!(r.param("key"), None, "{}", r.target);
    }
}

/// Each redirect target is checked against the deny rules (with the
/// credential left out of the URL a rule sees) and must be HTTP(S).
#[tokio::test(flavor = "multi_thread")]
async fn redirect_targets_are_checked() {
    let (base, log) = serve(|s| match (s.method.as_str(), s.path()) {
        ("GET", "/test") if s.param("key").is_some() => {
            Answer::redirect(302, "/blocked?key=query-secret&x=1")
        }
        ("POST", "/test") => Answer::redirect(302, "file:///etc/passwd"),
        _ => Answer::json(json!({})),
    })
    .await;
    let doc = secured(
        json!([{"q": []}]),
        json!({"q": {"type": "apiKey", "in": "query", "name": "key"}}),
    );
    let api = api_at(
        &doc,
        &base,
        resolver(|_| Some(Credential::ApiKey("query-secret".into()))),
    );
    let rules = vec![
        Rule::new(ACTION, "*", Effect::Allow),
        Rule::new(ACTION, &format!("GET {base}/blocked?x=1"), Effect::Deny),
    ];
    let (g, _) = gate(rules, Reply::Once);
    let e = call(&api, tool(&api.generated, "test"), &json!({}), &g)
        .await
        .unwrap_err();
    assert!(e.contains("Permission denied"), "{e}");
    assert!(!e.contains("query-secret"), "{e}");
    let e = call(&api, tool(&api.generated, "make"), &json!({}), &g)
        .await
        .unwrap_err();
    assert!(e.contains("redirect refused"), "{e}");
    let seen: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|s| format!("{} {}", s.method, s.path()))
        .collect();
    assert_eq!(seen, ["GET /test", "POST /test"]);
}

/// A redirect to another origin asks the `openapi` permission for its
/// target; declining stops the call before the target is requested.
#[tokio::test(flavor = "multi_thread")]
async fn a_cross_origin_redirect_asks_for_its_target() {
    let (other, other_log) = serve(|_| Answer::json(json!({"ok": true}))).await;
    let to = format!("{other}/elsewhere");
    let (base, _) = serve(move |_| Answer::redirect(307, &to)).await;
    let api = api_at(&secured(json!([]), json!({})), &base, None);
    let rules = vec![Rule::new(ACTION, &format!("POST {base}/*"), Effect::Allow)];
    let (g, asked) = gate(rules.clone(), Reply::Reject(None));
    let e = call(&api, tool(&api.generated, "make"), &json!({}), &g)
        .await
        .unwrap_err();
    assert!(e.contains("declined"), "{e}");
    assert!(other_log.lock().unwrap().is_empty());
    let resources: Vec<Vec<String>> = asked
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.resources.clone())
        .collect();
    assert_eq!(resources, [vec![format!("POST {other}/elsewhere")]]);

    let (g, asked) = gate(rules, Reply::Once);
    call(&api, tool(&api.generated, "make"), &json!({}), &g)
        .await
        .unwrap();
    assert_eq!(asked.asked.lock().unwrap().len(), 1);
    assert_eq!(last(&other_log).method, "POST");
}

/// A deny rule holds for a percent-encoded spelling of the URL it names.
#[tokio::test(flavor = "multi_thread")]
async fn deny_rules_match_percent_decoded_urls() {
    let (base, log) = serve(|s| match s.path() {
        "/test" => Answer::redirect(302, "/%62locked"),
        _ => Answer::json(json!({})),
    })
    .await;
    let api = api_at(&secured(json!([]), json!({})), &base, None);
    let rules = vec![
        Rule::new(ACTION, "*", Effect::Allow),
        Rule::new(ACTION, &format!("GET {base}/blocked*"), Effect::Deny),
    ];
    let (g, _) = gate(rules, Reply::Once);
    let e = call(&api, tool(&api.generated, "test"), &json!({}), &g)
        .await
        .unwrap_err();
    assert!(e.contains("Permission denied"), "{e}");
    let seen: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.target.clone())
        .collect();
    assert_eq!(seen, ["/test"]);
}

/// An error body that echoes a credential does not show it.
#[tokio::test(flavor = "multi_thread")]
async fn errors_never_show_credentials() {
    let (base, _) = serve(|s| {
        Answer::new(
            401,
            "text/plain",
            format!(
                "bad token {}",
                s.headers.get("authorization").cloned().unwrap_or_default()
            ),
        )
    })
    .await;
    let doc = secured(
        json!([{"b": []}]),
        json!({"b": {"type": "http", "scheme": "bearer"}}),
    );
    let api = api_at(
        &doc,
        &base,
        resolver(|_| Some(Credential::Bearer("s3cret-token".into()))),
    );
    let e = invoke(&api, "test", json!({})).await.unwrap_err();
    assert!(e.contains("HTTP 401"), "{e}");
    assert!(!e.contains("s3cret-token"), "{e}");
    // Nor does debug output.
    let c = CredentialConfig::Bearer {
        token: "s3cret-token".into(),
    };
    let cfg = ApiConfig {
        spec: Some(json!("spec.json")),
        base_url: Some(base.clone()),
        headers: [("x-key".to_string(), "s3cret-token".to_string())].into(),
        auth: [("b".to_string(), c.clone())].into(),
        ..Default::default()
    };
    for shown in [
        format!("{c:?}"),
        format!("{cfg:?}"),
        format!("{:?}", Credential::Bearer("s3cret-token".into())),
        format!("{api:?}"),
    ] {
        assert!(!shown.contains("s3cret-token"), "{shown}");
    }
}

/// Credentials and configured headers need `base_url`, so the spec alone
/// cannot choose where they go.
#[test]
fn credentials_need_a_base_url() {
    let doc = with(
        secured(
            json!([{"b": []}]),
            json!({"b": {"type": "http", "scheme": "bearer"}}),
        ),
        json!({"servers": [{"url": "https://spec.example"}]}),
    );
    let e = Api::new("api", &doc, None, vec![], resolver(|_| None)).unwrap_err();
    assert!(e.contains("need `base_url`"), "{e}");
    let e = Api::new("api", &doc, None, vec![("x".into(), "y".into())], None).unwrap_err();
    assert!(e.contains("need `base_url`"), "{e}");
    assert!(Api::new("api", &doc, None, vec![], None).is_ok());
}

/// Specs load from a file, a URL or inline; credentials come from config
/// (unset ones try the next alternative); problems are reported once.
#[tokio::test(flavor = "multi_thread")]
async fn specs_load_from_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("spec.json"), happy_path().to_string()).unwrap();
    std::fs::write(dir.path().join("swagger.json"), r#"{"swagger": "2.0"}"#).unwrap();
    std::fs::write(dir.path().join("spec.yaml"), "openapi: 3.1.0").unwrap();
    let (spec_url, spec_log) = serve(|_| Answer::json(transports())).await;
    let config: BTreeMap<String, ApiConfig> = serde_json::from_value(json!({
        "file": {"spec": "spec.json", "base_url": "http://127.0.0.1:9", "auth": {
            "BearerAuth": {"type": "bearer", "token": ""},
            "ApiKey": {"type": "api_key", "value": "k"}
        }},
        "url": {"spec": format!("{spec_url}/openapi.json")},
        "inline": {"spec": single_operation(json!({}), "get")},
        "off": {"spec": "spec.json", "enabled": false},
        "swagger": {"spec": "swagger.json"},
        "yaml": {"spec": "spec.yaml"},
        "bad name": {"spec": "spec.json"}
    }))
    .unwrap();
    let notices = load(&config, dir.path()).await;
    assert_eq!(notices.len(), 3, "{notices:?}");
    assert!(notices
        .iter()
        .any(|n| n.contains("Swagger 2.0 is not supported")));
    assert!(notices
        .iter()
        .any(|n| n.contains("YAML specs are not supported")));
    assert!(notices.iter().any(|n| n.contains("a name is letters")));
    // Reported once.
    assert!(load(&config, dir.path()).await.is_empty());
    assert_eq!(spec_log.lock().unwrap().len(), 1);
    let apis = loaded(&config, dir.path());
    let mut names: Vec<&str> = apis.iter().map(|a| a.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["file", "inline", "url"]);
    let file = apis.iter().find(|a| a.name == "file").unwrap();
    // An empty (unset) bearer token makes `users.get` fall back to no
    // credential: it requires BearerAuth, so it fails before sending.
    let (api, t) = find(&apis, &file.tool_name(tool(&file.generated, "users.get"))).unwrap();
    let e = call(api, t, &json!({"userId": "u"}), &open_gate())
        .await
        .unwrap_err();
    assert!(e.contains("no credential available for: BearerAuth"), "{e}");
    // An entry that does not parse is reported, and the config still loads.
    let c: crate::config::Config = serde_json::from_value(json!({"openapi": {
        "typo": {"spec": "spec.json", "token": "s3cret"},
        "kind": {"spec": "spec.json", "base_url": "http://127.0.0.1:9", "auth": {"x": {"type": "magic", "value": "s3cret"}}},
        "ok": {"spec": "spec.json"}
    }}))
    .unwrap();
    let notices = load(&c.openapi, dir.path()).await;
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices.iter().all(|n| !n.contains("s3cret")), "{notices:?}");
    assert!(notices
        .iter()
        .any(|n| n.contains("openapi.typo") && n.contains("token")));
    assert!(notices
        .iter()
        .any(|n| n.contains("openapi.kind") && n.contains("magic")));
    assert_eq!(loaded(&c.openapi, dir.path()).len(), 1);
}

/// OpenAPI tools join the other Code Mode tools without taking a path
/// another tool has.
#[test]
fn entries_do_not_shadow_other_tools() {
    let entry = |ns: &str, name: &str| crate::codemode::Entry {
        tool: format!("{ns}_{name}"),
        namespace: ns.into(),
        name: name.into(),
        description: String::new(),
        schema: json!({"type": "object"}),
        output: None,
        freeform: false,
    };
    let api = api_at(&happy_path(), BASE, None);
    let mut entries = vec![entry("api", "users.get"), entry("", "read")];
    merge_entries(&mut entries, api.entries(|_| true));
    let paths: Vec<String> = entries
        .iter()
        .map(|e| format!("{}.{}", e.namespace, e.name))
        .collect();
    assert!(paths.contains(&"api.users.create".to_string()));
    assert_eq!(paths.iter().filter(|p| *p == "api.users.get").count(), 1);
    assert_eq!(entries[0].tool, "api_users.get");
    assert!(is_tool(&entries.last().unwrap().tool));
    assert!(!is_tool("read"));
}
