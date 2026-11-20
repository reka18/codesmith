//! Ports of OpenCode's tool input repair tests (MIT):
//! `plugin/tool-input-repair.test.ts` and
//! `plugin/tool-input-repair-integration.test.ts`. Each test names the
//! OpenCode test it ports; see `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: JavaScript identity (`toBe`) is checked as equality; the inputs
//! are borrowed, so they are never mutated.

mod support;

use codesmith_core::tool::input_repair::{alias, alias_note, repair};
use serde_json::{json, Value};

fn object(properties: Value, required: &[&str]) -> Value {
    let mut o = json!({"type": "object", "properties": properties});
    if !required.is_empty() {
        o["required"] = json!(required);
    }
    o
}

fn with(mut schema: Value, extra: Value) -> Value {
    for (k, v) in extra.as_object().unwrap() {
        schema[k] = v.clone();
    }
    schema
}

// plugin/tool-input-repair.test.ts: "preserves valid input identity, nested containers, and unknown properties"
#[test]
fn oc_repair_preserves_valid_input_and_unknown_properties() {
    let input = json!({"count": 2, "nested": {"enabled": true}, "items": [2, 3], "extra": "keep"});
    let schema = object(
        json!({
            "count": {"type": "integer"},
            "nested": object(json!({"enabled": {"type": "boolean"}}), &[]),
            "items": {"type": "array", "items": {"type": "number"}},
        }),
        &[],
    );
    assert_eq!(repair(&input, &schema), input);
}

// plugin/tool-input-repair.test.ts: "parses root objects and repairs nested stringified containers"
#[test]
fn oc_repair_parses_root_objects_and_nested_stringified_containers() {
    let schema = object(
        json!({
            "count": {"type": "integer"},
            "item": object(json!({"enabled": {"type": "boolean"}}), &[]),
            "list": {"type": "array", "items": {"type": "integer"}},
        }),
        &[],
    );
    assert_eq!(
        repair(
            &json!("{\"count\":\"2\",\"item\":\"{\\\"enabled\\\":\\\"false\\\"}\",\"list\":\"[\\\"3\\\"]\"}"),
            &schema
        ),
        json!({"count": 2, "item": {"enabled": false}, "list": [3]})
    );
    assert_eq!(repair(&json!("{broken"), &schema), json!("{broken"));
    assert_eq!(repair(&json!("[]"), &schema), json!("[]"));
    assert_eq!(repair(&Value::Null, &schema), Value::Null);
}

// plugin/tool-input-repair.test.ts: "removes extras only from explicitly closed objects without mutating inputs"
#[test]
fn oc_repair_removes_extras_only_from_closed_objects() {
    let input = json!({
        "known": "2", "extra": true,
        "closed": {"keep": "3", "extra": true},
        "open": {"keep": "4", "extra": true},
        "items": [{"keep": "5", "extra": true}],
    });
    let closed = with(
        object(json!({"keep": {"type": "integer"}}), &[]),
        json!({"additionalProperties": false}),
    );
    let schema = with(
        object(
            json!({
                "known": {"type": "integer"},
                "closed": closed,
                "open": object(json!({"keep": {"type": "integer"}}), &[]),
                "items": {"type": "array", "items": closed},
            }),
            &[],
        ),
        json!({"additionalProperties": false}),
    );
    assert_eq!(
        repair(&input, &schema),
        json!({"known": 2, "closed": {"keep": 3}, "open": {"keep": 4, "extra": true}, "items": [{"keep": 5}]})
    );
    assert_eq!(input["extra"], json!(true));
    assert_eq!(
        repair(
            &json!({"extra": true}),
            &with(
                object(json!({}), &[]),
                json!({"additionalProperties": false})
            )
        ),
        json!({})
    );
}

// plugin/tool-input-repair.test.ts: "preserves unknown keys when patterned ownership cannot be determined"
#[test]
fn oc_repair_preserves_unknown_keys_under_pattern_properties() {
    let input = json!({"known": 1, "match": "2", "extra": true});
    let schema = with(
        object(json!({"known": {"type": "integer"}}), &[]),
        json!({"additionalProperties": false, "patternProperties": {"^match$": {"type": "integer"}}}),
    );
    assert_eq!(repair(&input, &schema), input);
}

// plugin/tool-input-repair.test.ts: "preserves properties that may belong to composed object schemas"
#[test]
fn oc_repair_preserves_properties_of_composed_schemas() {
    let input = json!({"name": "example", "extra": true});
    for keyword in ["allOf", "anyOf", "oneOf"] {
        let schema = json!({
            "type": "object",
            keyword: [object(json!({"name": {"type": "string"}}), &[])],
            "additionalProperties": false,
        });
        assert_eq!(repair(&input, &schema), input, "{keyword}");
    }
}

// plugin/tool-input-repair.test.ts: "removes only optional nonnullable nulls and non-object empty placeholders"
#[test]
fn oc_repair_removes_only_optional_nulls_and_placeholders() {
    let input = json!({
        "optional": null, "required": null, "nullable": null, "union": null,
        "constant": null, "permissive": null, "referenced": null, "placeholder": {},
        "array": {}, "requiredPlaceholder": {}, "object": {}, "unknown": null,
    });
    let schema = object(
        json!({
            "optional": {"type": "string"},
            "required": {"type": "string"},
            "nullable": {"type": "string", "nullable": true},
            "union": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
            "constant": {"anyOf": [{"type": "integer"}, {"const": null}]},
            "permissive": {"anyOf": [{"type": "integer"}, true]},
            "referenced": {"anyOf": [{"type": "integer"}, {"$ref": "#/$defs/nullable"}]},
            "placeholder": {"type": "integer"},
            "array": {"type": "array", "items": {"type": "string"}},
            "requiredPlaceholder": {"type": "boolean"},
            "object": {"type": "object"},
            "unknown": {},
        }),
        &["required", "requiredPlaceholder"],
    );
    assert_eq!(
        repair(&input, &schema),
        json!({
            "required": null, "nullable": null, "union": null, "constant": null,
            "permissive": null, "referenced": null, "requiredPlaceholder": {},
            "object": {}, "unknown": null,
        })
    );
}

// plugin/tool-input-repair.test.ts: "preserves nulls whose validity is hidden inside compositions"
#[test]
fn oc_repair_preserves_nulls_hidden_inside_compositions() {
    let input = json!({"wrapped": null, "enumerated": null, "permissive": null, "constant": null});
    let schema = object(
        json!({
            "wrapped": {"anyOf": [{"type": ["string", "null"]}]},
            "enumerated": {"anyOf": [{"enum": [null, "keep"]}]},
            "permissive": {"anyOf": [{}]},
            "constant": {"type": "string", "const": "keep"},
        }),
        &[],
    );
    assert_eq!(repair(&input, &schema), input);
}

// plugin/tool-input-repair.test.ts: "preserves optional-looking nulls when the parent composes requirements"
#[test]
fn oc_repair_preserves_nulls_when_the_parent_composes_requirements() {
    let input = json!({"value": null});
    let schema = with(
        object(json!({"value": {"type": "string"}}), &[]),
        json!({"allOf": [{"required": ["value"]}]}),
    );
    assert_eq!(repair(&input, &schema), input);
}

// plugin/tool-input-repair.test.ts: "coerces numeric and boolean strings while preserving invalid and existing values"
#[test]
fn oc_repair_coerces_numeric_and_boolean_strings() {
    let input = json!({
        "number": "1.5", "integer": "42", "enabled": "true", "disabled": "false",
        "valid": 3, "empty": " ", "infinite": "Infinity", "fractional": "1.5",
        "unsafe": "9007199254740992", "uppercase": "TRUE",
    });
    let schema = object(
        json!({
            "number": {"type": "number"}, "integer": {"type": "integer"},
            "enabled": {"type": "boolean"}, "disabled": {"type": "boolean"},
            "valid": {"type": "integer"}, "empty": {"type": "number"},
            "infinite": {"type": "number"}, "fractional": {"type": "integer"},
            "unsafe": {"type": "integer"}, "uppercase": {"type": "boolean"},
        }),
        &[],
    );
    let mut want = input.clone();
    want["number"] = json!(1.5);
    want["integer"] = json!(42);
    want["enabled"] = json!(true);
    want["disabled"] = json!(false);
    assert_eq!(repair(&input, &schema), want);
}

// plugin/tool-input-repair.test.ts: "wraps compatible scalars after repairing array items"
#[test]
fn oc_repair_wraps_compatible_scalars_in_arrays() {
    let input = json!({
        "text": "one", "integer": "42", "boolean": "false", "item": "{\"count\":\"2\"}",
        "incompatible": 2, "fractional": 1.5, "unconstrained": "4",
    });
    let schema = object(
        json!({
            "text": {"type": "array", "items": {"type": "string"}},
            "integer": {"type": "array", "items": {"type": "integer"}},
            "boolean": {"type": "array", "items": {"type": "boolean"}},
            "item": {"type": "array", "items": object(json!({"count": {"type": "integer"}}), &[])},
            "incompatible": {"type": "array", "items": {"type": "string"}},
            "fractional": {"type": "array", "items": {"type": "integer"}},
            "unconstrained": {"type": "array"},
        }),
        &[],
    );
    assert_eq!(
        repair(&input, &schema),
        json!({
            "text": ["one"], "integer": [42], "boolean": [false], "item": [{"count": 2}],
            "incompatible": 2, "fractional": 1.5, "unconstrained": "4",
        })
    );
}

// plugin/tool-input-repair.test.ts: "repairs nested question-like inputs without mutating original containers"
#[test]
fn oc_repair_repairs_nested_question_like_inputs() {
    let question = json!({"question": "Pick one", "multiple": "false", "options": {"label": "First", "description": null}});
    let input = json!({"questions": [question]});
    let schema = object(
        json!({"questions": {"type": "array", "items": object(json!({
            "question": {"type": "string"},
            "multiple": {"type": "boolean"},
            "options": {"type": "array", "items": object(json!({"label": {"type": "string"}, "description": {"type": "string"}}), &["label"])},
        }), &[])}}),
        &[],
    );
    assert_eq!(
        repair(&input, &schema),
        json!({"questions": [{"question": "Pick one", "multiple": false, "options": [{"label": "First"}]}]})
    );
    assert_eq!(input, json!({"questions": [question]}));
}

// plugin/tool-input-repair.test.ts: "repairs unique nullable alternatives while preserving accepted union values"
#[test]
fn oc_repair_repairs_unique_nullable_alternatives() {
    let input = json!({
        "number": "2", "boolean": "false", "nullable": null, "typed": "3",
        "typedBoolean": "true", "accepted": "4", "valid": 5,
    });
    let schema = object(
        json!({
            "number": {"anyOf": [{"type": "number"}, {"type": "null"}]},
            "boolean": {"oneOf": [{"type": "boolean"}, {"type": "null"}]},
            "nullable": {"anyOf": [{"type": "number"}, {"type": "null"}]},
            "typed": {"type": ["integer", "null"]},
            "typedBoolean": {"type": ["boolean", "null"]},
            "accepted": {"anyOf": [{"type": "string"}, {"type": "number"}]},
            "valid": {"type": ["number", "null"]},
        }),
        &[],
    );
    let mut want = input.clone();
    want["number"] = json!(2);
    want["boolean"] = json!(false);
    want["typed"] = json!(3);
    want["typedBoolean"] = json!(true);
    assert_eq!(repair(&input, &schema), want);
}

// plugin/tool-input-repair.test.ts: "repairs tuple positions and rest items while preserving valid array identity"
#[test]
fn oc_repair_repairs_tuple_positions_and_rest_items() {
    let input = json!({"prefix": ["2", "false", "3"], "draft": "[\"4\",\"true\"]", "valid": [2, false], "scalar": "5"});
    let schema = object(
        json!({
            "prefix": {"type": "array", "prefixItems": [{"type": "integer"}, {"type": "boolean"}], "items": {"type": "number"}},
            "draft": {"type": "array", "items": [{"type": "integer"}, {"type": "boolean"}]},
            "valid": {"type": "array", "prefixItems": [{"type": "integer"}, {"type": "boolean"}]},
            "scalar": {"type": "array", "prefixItems": [{"type": "integer"}]},
        }),
        &[],
    );
    assert_eq!(
        repair(&input, &schema),
        json!({"prefix": [2, false, 3], "draft": [4, true], "valid": [2, false], "scalar": "5"})
    );
}

// plugin/tool-input-repair.test.ts: "repairs typed dictionaries and straightforward local references"
#[test]
fn oc_repair_repairs_typed_dictionaries_and_local_references() {
    let input = json!({
        "modern": "2", "legacy": "false", "nested": {"count": "3"}, "dictionary": {"first": "4"},
        "missing": "5", "pointer": "6", "escaped": "7",
    });
    let schema = with(
        object(
            json!({
                "modern": {"$ref": "#/$defs/integer"},
                "legacy": {"$ref": "#/definitions/boolean"},
                "nested": {"$ref": "#/$defs/nested"},
                "dictionary": {"type": "object", "additionalProperties": {"$ref": "#/$defs/integer"}},
                "missing": {"$ref": "#/$defs/missing"},
                "pointer": {"$ref": "#/$defs/nested/properties/count"},
                "escaped": {"$ref": "#/$defs/a~1b~0c"},
            }),
            &[],
        ),
        json!({
            "$defs": {
                "integer": {"type": "integer"},
                "a/b~c": {"type": "integer"},
                "nested": object(json!({"count": {"$ref": "#/$defs/integer"}}), &[]),
            },
            "definitions": {"boolean": {"type": "boolean"}},
        }),
    );
    assert_eq!(
        repair(&input, &schema),
        json!({
            "modern": 2, "legacy": false, "nested": {"count": 3}, "dictionary": {"first": 4},
            "missing": "5", "pointer": "6", "escaped": 7,
        })
    );
}

// plugin/tool-input-repair.test.ts: "leaves ambiguous unions, compositions, and unsupported roots unchanged"
#[test]
fn oc_repair_leaves_ambiguous_unions_compositions_and_other_roots_unchanged() {
    let input = json!({"numeric": "2", "objects": {"value": "3"}, "both": "4", "composed": "5", "unknown": "6"});
    let schema = object(
        json!({
            "numeric": {"anyOf": [{"type": "number"}, {"type": "integer"}]},
            "objects": {"oneOf": [
                object(json!({"value": {"type": "integer"}}), &["value"]),
                object(json!({"value": {"type": "number"}}), &["value"]),
            ]},
            "both": {"anyOf": [{"type": "integer"}], "oneOf": [{"type": "integer"}]},
            "composed": {"allOf": [{"type": "integer"}]},
            "unknown": {},
        }),
        &[],
    );
    assert_eq!(repair(&input, &schema), input);
    assert_eq!(
        repair(
            &input,
            &json!({"properties": {"numeric": {"type": "integer"}}})
        ),
        input
    );
    assert_eq!(
        repair(
            &input,
            &json!({"allOf": [object(json!({"numeric": {"type": "integer"}}), &[])]})
        ),
        input
    );
}

#[test]
fn bash_runs_as_shell_only_when_no_tool_is_called_bash() {
    let offered = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(alias("bash", &offered(&["shell", "read"])), Some("shell"));
    assert_eq!(alias("bash", &offered(&["bash", "shell"])), None);
    assert_eq!(alias("bash", &offered(&["read"])), None);
    assert_eq!(alias("shell", &offered(&["shell"])), None);
    assert_eq!(
        alias_note("bash", "shell"),
        "There is no \"bash\" tool, so this ran as \"shell\". Call \"shell\" directly."
    );
}

// --- In a session (plugin/tool-input-repair-integration.test.ts) ---

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// A session with the `fix` MCP server (its `count` tool), Code Mode on or off.
async fn session(codemode: bool) -> Option<support::H> {
    let py = python()?;
    let fixture = format!(
        "{}/tests/fixtures/mcp_repair.py",
        env!("CARGO_MANIFEST_DIR")
    );
    Some(
        support::H::with(
            json!({
                "codemode": codemode,
                "mcp": {"servers": {"fix": {"type": "local", "command": [py, fixture]}}},
            }),
            None,
        )
        .await,
    )
}

/// The results of the latest step's calls: (name, content).
fn results(h: &support::H) -> Vec<(String, String)> {
    h.s.messages
        .iter()
        .rev()
        .find_map(|m| {
            let r: Vec<(String, String)> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    codesmith_ai::Part::ToolResult { name, content, .. } => {
                        Some((name.clone(), content.clone()))
                    }
                    _ => None,
                })
                .collect();
            (!r.is_empty()).then_some(r)
        })
        .unwrap()
}

// plugin/tool-input-repair-integration.test.ts: "repairs tool input before validating its original schema"
#[tokio::test(flavor = "multi_thread")]
async fn oc_repair_repairs_tool_input_before_running_it() {
    let Some(mut h) = session(false).await else {
        return;
    };
    h.prompt(r#"call fix_count "{\"count\":\"2\",\"detail\":\"true\",\"extra\":true}""#)
        .await;
    assert_eq!(
        results(&h),
        [(
            "fix_count".to_string(),
            r#"got: {"count": 2, "detail": true}"#.to_string()
        )]
    );
    // A tool that is not offered is not run.
    h.prompt(r#"call fix_gone {"count": 1}"#).await;
    let (_, gone) = &results(&h)[0];
    assert!(gone.contains("fix_gone"), "{gone}");
    assert!(!gone.starts_with("got:"), "{gone}");
}

// plugin/tool-input-repair-integration.test.ts: "runs a call to another harness's tool name as the matching tool and says so"
#[tokio::test(flavor = "multi_thread")]
async fn oc_repair_runs_bash_as_shell_and_says_so() {
    let mut h = support::H::new().await;
    h.prompt(r#"call bash {"command": "echo ran-as-shell", "timeout": "5000"}"#)
        .await;
    let r = results(&h);
    assert_eq!(r[0].0, "bash", "the result answers the call as made");
    assert!(r[0].1.contains("ran-as-shell"), "{}", r[0].1);
    assert!(
        r[0].1.ends_with(
            "\n\nThere is no \"bash\" tool, so this ran as \"shell\". Call \"shell\" directly."
        ),
        "{}",
        r[0].1
    );
    h.prompt(r#"call shell {"command": "echo direct"}"#).await;
    let r = results(&h);
    assert!(
        r[0].1.contains("direct") && !r[0].1.contains("There is no"),
        "{}",
        r[0].1
    );
}

// plugin/tool-input-repair-integration.test.ts: "repairs namespaced inner tool input called from Code Mode"
#[tokio::test(flavor = "multi_thread")]
async fn oc_repair_repairs_inner_tool_input_called_from_code_mode() {
    let Some(mut h) = session(true).await else {
        return;
    };
    let code = json!({"code": "return await tools.fix.count({ count: \"3\" })"});
    h.prompt(&format!("call execute {code}")).await;
    let r = results(&h);
    assert!(
        r[0].1.contains(r#"got: {\"count\": 3}"#) || r[0].1.contains(r#"got: {"count": 3}"#),
        "{}",
        r[0].1
    );
}

// plugin/tool-input-repair-integration.test.ts: "repairs nested Code Mode calls against the Code Mode input schema"
#[tokio::test(flavor = "multi_thread")]
async fn oc_repair_repairs_code_mode_calls_against_the_tool_schema() {
    let Some(mut h) = session(true).await else {
        return;
    };
    let code = json!({"code": "return await tools.fix.count({ count: \"3\", detail: \"true\", extra: true })"});
    h.prompt(&format!("call execute {code}")).await;
    let r = results(&h);
    assert!(
        r[0].1.contains(r#"{\"count\": 3, \"detail\": true}"#)
            || r[0].1.contains(r#"{"count": 3, "detail": true}"#),
        "{}",
        r[0].1
    );
}
