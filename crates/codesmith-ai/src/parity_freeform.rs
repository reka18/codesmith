//! Freeform tools as OpenAI custom tools, after OpenCode's
//! `packages/ai/test/provider/freeform-tools.test.ts` and
//! `openai-freeform.test.ts` (MIT).

use crate::{
    anthropic, decode_all, openai, responses, Collector, Event, Freeform, Message, Part, Protocol,
    ReasoningMeta, Request, Role, ToolChoice, ToolSpec,
};
use serde_json::{json, Value};

const GRAMMAR: &str = r#"start: "ZZ" /[0-9]{3}/"#;

fn run(freeform: Option<Freeform>) -> ToolSpec {
    ToolSpec {
        name: "run".into(),
        description: "Run code".into(),
        parameters: json!({"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}),
        freeform,
    }
}

fn lark() -> ToolSpec {
    run(Some(Freeform::Lark {
        grammar: GRAMMAR.into(),
    }))
}

/// The same tool without `freeform`, as declared for a model without
/// custom tools.
fn plain() -> ToolSpec {
    run(None)
}

fn request(tool: ToolSpec) -> Request {
    Request {
        model: "gpt-5.4".into(),
        messages: vec![Message::user("Run it.")],
        tools: vec![tool],
        tool_choice: Some(ToolChoice::Tool("run".into())),
        ..Default::default()
    }
}

fn chat_body(r: &Request) -> Value {
    openai::body_for(
        r,
        &openai::Compat::detect("https://api.openai.com/v1", &r.model),
    )
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn deltas(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolInputDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn calls(events: &[Event]) -> Vec<(String, String, Value)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCall { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

const RESPONSES_LARK: fn() -> Value = || {
    json!({"type": "custom", "name": "run", "description": "Run code",
        "format": {"type": "grammar", "syntax": "lark", "definition": GRAMMAR}})
};
const CHAT_LARK: fn() -> Value = || {
    json!({"type": "custom", "custom": {"name": "run", "description": "Run code",
        "format": {"type": "grammar", "grammar": {"syntax": "lark", "definition": GRAMMAR}}}})
};

// freeform-tools.test.ts: "encodes each format as a Responses custom tool"
#[test]
fn oc_freeform_encodes_each_format_as_a_responses_custom_tool() {
    let tools = |f| responses::body(&request(run(Some(f))))["tools"].clone();
    assert_eq!(
        tools(Freeform::Text),
        json!([{"type": "custom", "name": "run", "description": "Run code", "format": {"type": "text"}}])
    );
    assert_eq!(
        tools(Freeform::Regex {
            pattern: "ZZ[0-9]{3}".into()
        }),
        json!([{"type": "custom", "name": "run", "description": "Run code",
            "format": {"type": "grammar", "syntax": "regex", "definition": "ZZ[0-9]{3}"}}])
    );
    let b = responses::body(&request(lark()));
    assert_eq!(b["tools"], json!([RESPONSES_LARK()]));
    assert_eq!(b["tool_choice"], json!({"type": "custom", "name": "run"}));
}

// freeform-tools.test.ts: "encodes each format as a Chat custom tool"
#[test]
fn oc_freeform_encodes_each_format_as_a_chat_custom_tool() {
    let tools = |f| chat_body(&request(run(Some(f))))["tools"].clone();
    assert_eq!(
        tools(Freeform::Text),
        json!([{"type": "custom", "custom": {"name": "run", "description": "Run code", "format": {"type": "text"}}}])
    );
    assert_eq!(
        tools(Freeform::Regex {
            pattern: "ZZ[0-9]{3}".into()
        }),
        json!([{"type": "custom", "custom": {"name": "run", "description": "Run code",
            "format": {"type": "grammar", "grammar": {"syntax": "regex", "definition": "ZZ[0-9]{3}"}}}}])
    );
    let b = chat_body(&request(lark()));
    assert_eq!(b["tools"], json!([CHAT_LARK()]));
    assert_eq!(
        b["tool_choice"],
        json!({"type": "custom", "custom": {"name": "run"}})
    );
}

// freeform-tools.test.ts: "encodes custom tools in every route that reuses the
// OpenAI wire format"
// (adapted) CodeSmith's routes share the two bodies; a few servers stand for them.
#[test]
fn oc_freeform_encodes_custom_tools_in_every_route_that_reuses_the_openai_wire_format() {
    let r = request(lark());
    for url in [
        "https://fixture.openai.azure.com/openai/v1",
        "https://openrouter.ai/api/v1",
        "https://inference.do-ai.run/v1",
        "https://api.venice.ai/api/v1",
        "https://responses.test/v1",
    ] {
        assert_eq!(
            responses::body_for(&r, url)["tools"],
            json!([RESPONSES_LARK()]),
            "{url}"
        );
    }
    for url in [
        "https://fixture.openai.azure.com/openai/v1",
        "https://openrouter.ai/api/v1",
        "https://ai-gateway.vercel.sh/v1",
        "https://chat.test/v1",
    ] {
        let b = openai::body_for(&r, &openai::Compat::detect(url, &r.model));
        assert_eq!(b["tools"], json!([CHAT_LARK()]), "{url}");
    }
}

// freeform-tools.test.ts: "sends JSON tools for tools without freeform and for
// other protocols"
#[test]
fn oc_freeform_sends_json_tools_for_tools_without_freeform_and_for_other_protocols() {
    let b = responses::body(&request(plain()));
    assert_eq!(b["tools"][0]["type"], "function");
    assert_eq!(b["tools"][0]["name"], "run");
    assert_eq!(b["tool_choice"], json!({"type": "function", "name": "run"}));
    let b = chat_body(&request(plain()));
    assert_eq!(b["tools"][0]["type"], "function");
    assert_eq!(b["tools"][0]["function"]["name"], "run");
    let b = anthropic::body(&request(lark()));
    assert_eq!(b["tools"][0]["name"], "run");
    assert_eq!(
        b["tools"][0]["input_schema"]["properties"]["code"]["type"],
        "string"
    );
}

// freeform-tools.test.ts: "streams Responses custom calls as JSON argument deltas"
#[test]
fn oc_freeform_streams_responses_custom_calls_as_json_argument_deltas() {
    let r = request(lark());
    let item = json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "run", "input": ""});
    let mut done = item.clone();
    done["input"] = json!("ZZ\"1\n");
    let body = sse(&[
        json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
        json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "item_id": "ctc_1", "delta": "ZZ"}),
        json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "item_id": "ctc_1", "delta": "\"1\n"}),
        json!({"type": "response.custom_tool_call_input.done", "output_index": 0, "item_id": "ctc_1", "input": "ZZ\"1\n"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": done}),
        json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let events = decode_all(responses::Decode::for_request(&r), &body).unwrap();
    let d = deltas(&events);
    assert_eq!(d, ["{\"code\":\"", "ZZ", "\\\"1\\n", "\"}"]);
    assert_eq!(
        serde_json::from_str::<Value>(&d.concat()).unwrap(),
        json!({"code": "ZZ\"1\n"})
    );
    assert_eq!(
        calls(&events),
        [("call_1".into(), "run".into(), json!({"code": "ZZ\"1\n"}))]
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Finish { reason, .. } if reason.normalized == crate::Reason::ToolCalls)));

    let mut c = Collector::default();
    for e in &events {
        c.push(e);
    }
    let replay = Request {
        messages: vec![Message {
            role: Role::Assistant,
            parts: c.finish(),
        }],
        ..request(lark())
    };
    assert_eq!(
        responses::body(&replay)["input"][0],
        json!({"type": "custom_tool_call", "call_id": "call_1", "name": "run", "input": "ZZ\"1\n", "id": "ctc_1"})
    );
}

// freeform-tools.test.ts: "parses a Responses custom call that arrives only when done"
#[test]
fn oc_freeform_parses_a_responses_custom_call_that_arrives_only_when_done() {
    let item = json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "run", "input": "ZZ123"});
    let body = sse(&[
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let events = decode_all(responses::Decode::for_request(&request(lark())), &body).unwrap();
    assert_eq!(
        calls(&events),
        [("call_1".into(), "run".into(), json!({"code": "ZZ123"}))]
    );
}

// freeform-tools.test.ts: "streams Chat custom calls as JSON argument deltas"
#[test]
fn oc_freeform_streams_chat_custom_calls_as_json_argument_deltas() {
    let chunk = |call: Value| {
        let mut call = call;
        call["index"] = json!(0);
        json!({"id": "chatcmpl_1", "choices": [{"index": 0, "delta": {"tool_calls": [call]}, "finish_reason": null}]})
    };
    let body = sse(&[
        chunk(json!({"id": "call_1", "type": "custom", "custom": {"name": "run", "input": ""}})),
        chunk(json!({"custom": {"input": "ZZ"}})),
        chunk(json!({"custom": {"input": "\"1\n"}})),
        json!({"id": "chatcmpl_1", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ]);
    let r = request(lark());
    let compat = openai::Compat::detect("https://api.openai.com/v1", &r.model);
    let events = decode_all(openai::Decode::for_request(&compat, &r), &body).unwrap();
    assert_eq!(deltas(&events), ["{\"code\":\"", "ZZ", "\\\"1\\n", "\"}"]);
    assert_eq!(
        calls(&events),
        [("call_1".into(), "run".into(), json!({"code": "ZZ\"1\n"}))]
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Finish { reason, .. } if reason.normalized == crate::Reason::ToolCalls)));
}

// freeform-tools.test.ts: "replays history the way each tool is declared in the
// current request"
// (adapted) a call's item id is its Responses meta; CodeSmith tells a custom
// call's id by its `ctc_` prefix, and sends a result's text as it is.
#[test]
fn oc_freeform_replays_history_the_way_each_tool_is_declared_in_the_current_request() {
    let history = |item: &str| {
        let mut meta = ReasoningMeta::of(Protocol::OpenaiResponses);
        meta.item_id = Some(item.into());
        vec![
            Message::user("Run it."),
            Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall {
                    id: "call_1".into(),
                    name: "run".into(),
                    input: json!({"code": "ZZ123"}),
                    meta: Some(meta),
                }],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "call_1".into(),
                    name: "run".into(),
                    content: "ok".into(),
                    is_error: false,
                    media: vec![],
                }],
            },
        ]
    };
    let input = |tool: ToolSpec, item: &str| {
        let r = Request {
            messages: history(item),
            ..request(tool)
        };
        let b = responses::body(&r);
        b["input"].as_array().unwrap()[1..].to_vec()
    };
    let custom_call =
        json!({"type": "custom_tool_call", "call_id": "call_1", "name": "run", "input": "ZZ123"});
    let custom_out =
        json!({"type": "custom_tool_call_output", "call_id": "call_1", "output": "ok"});
    let function_call = json!({"type": "function_call", "call_id": "call_1", "name": "run", "arguments": "{\"code\":\"ZZ123\"}"});
    let function_out = json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"});
    let with_id = |v: &Value, id: &str| {
        let mut v = v.clone();
        v["id"] = json!(id);
        v
    };
    assert_eq!(
        input(lark(), "ctc_1"),
        [with_id(&custom_call, "ctc_1"), custom_out.clone()]
    );
    assert_eq!(
        input(plain(), "ctc_1"),
        [function_call.clone(), function_out.clone()]
    );
    assert_eq!(input(lark(), "fc_1"), [custom_call, custom_out]);
    assert_eq!(
        input(plain(), "fc_1"),
        [with_id(&function_call, "fc_1"), function_out]
    );

    let messages = |tool: ToolSpec| {
        let r = Request {
            messages: history("ctc_1"),
            ..request(tool)
        };
        chat_body(&r)["messages"].as_array().unwrap()[1..].to_vec()
    };
    let m = messages(lark());
    assert_eq!(
        m[0]["tool_calls"],
        json!([{"id": "call_1", "type": "custom", "custom": {"name": "run", "input": "ZZ123"}}])
    );
    assert_eq!(m[1]["role"], "tool");
    assert_eq!(m[1]["tool_call_id"], "call_1");
    let m = messages(plain());
    assert_eq!(m[0]["tool_calls"][0]["type"], "function");
    assert_eq!(
        m[0]["tool_calls"][0]["function"],
        json!({"name": "run", "arguments": "{\"code\":\"ZZ123\"}"})
    );
    let r = Request {
        messages: history("ctc_1"),
        ..request(lark())
    };
    let b = anthropic::body(&r);
    let call = &b["messages"][1]["content"][0];
    assert_eq!(call["type"], "tool_use");
    assert_eq!(call["input"], json!({"code": "ZZ123"}));
}

// freeform-tools.test.ts: "rejects freeform tools without exactly one required
// string parameter"
// (adapted) CodeSmith does not reject such a tool: it goes out as a JSON tool.
#[test]
fn oc_freeform_rejects_freeform_tools_without_exactly_one_required_string_parameter() {
    let with = |parameters: Value| ToolSpec {
        parameters,
        ..run(Some(Freeform::Text))
    };
    for parameters in [
        json!({"type": "object", "properties": {"code": {"type": "number"}}, "required": ["code"]}),
        json!({"type": "object", "properties": {"code": {"type": "string"}}}),
        json!({"type": "object", "properties": {"code": {"type": "string"}, "lang": {"type": "string"}}, "required": ["code", "lang"]}),
    ] {
        let t = with(parameters);
        assert!(t.custom().is_none(), "{t:?}");
        assert_eq!(responses::body(&request(t))["tools"][0]["type"], "function");
    }
    assert_eq!(
        run(Some(Freeform::Text)).custom(),
        Some(("code".into(), &Freeform::Text))
    );
}

// openai-freeform.test.ts: "converts between raw text and the JSON input"
#[test]
fn oc_openai_freeform_converts_between_raw_text_and_the_json_input() {
    assert_eq!(crate::custom_raw("code", &json!({"code": "x"})), "x");
    assert_eq!(
        crate::custom_raw("code", &json!({"code": 1})),
        "{\"code\":1}"
    );
    assert_eq!(crate::custom_raw("code", &Value::Null), "{}");
    assert_eq!(crate::custom_input(Some("code"), "x"), json!({"code": "x"}));
    // A call to an undeclared custom tool fills `input`.
    assert_eq!(crate::custom_input(None, "x"), json!({"input": "x"}));
}
