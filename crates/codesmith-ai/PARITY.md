# Provider parity with OpenCode

CodeSmith's provider protocols (`src/openai.rs`, `src/responses.rs`,
`src/anthropic.rs`, `src/gemini.rs`) are pinned to OpenCode's by porting
OpenCode's provider unit tests (`packages/ai/test`, MIT) as table-driven Rust
tests in [`src/parity_tests.rs`](src/parity_tests.rs) (provider options and
reasoning replay in [`src/parity_options.rs`](src/parity_options.rs), prompt
caching and tool choice in [`src/parity_cache.rs`](src/parity_cache.rs),
stream lifecycle, partial output and Gemini continuation in
[`src/parity_stream.rs`](src/parity_stream.rs), endpoints, auth, billed
cost, compatibility overrides and request encoding in
[`src/parity_endpoint.rs`](src/parity_endpoint.rs), hosted tools in
[`src/parity_hosted.rs`](src/parity_hosted.rs), the Responses
WebSocket transport and incremental continuation in
[`src/parity_ws.rs`](src/parity_ws.rs), declared reasoning fields, signed
text and tool namespaces in
[`src/parity_metadata.rs`](src/parity_metadata.rs) (and the two
default-auth cases in `codesmith-core/src/config.rs`), the partial JSON parser in
[`src/partial_json.rs`](src/partial_json.rs), and the session's handling of a
failed step's partial output in `codesmith-core/tests/runner_parity.rs`).
Response parsing is also checked against OpenCode's recorded traffic in
`src/replay_tests.rs`; this matrix covers the unit tests, which are mostly
about how requests are built.

Every OpenCode test in these files was examined and put in one bucket:

- **ported**: CodeSmith already behaved this way; the test is ported.
- **ported after a fix**: CodeSmith was wrong or missing something cheap that
  matters with real providers; it was fixed and the test ported.
- **gap**: the test depends on a feature CodeSmith does not have, on
  behaviour CodeSmith differs on by design, or does not apply.

Files: `provider/openai-chat`, `provider/openai-compatible-chat` (both counted
as OpenAI Chat), `provider/openai-responses`, `provider/anthropic-messages`,
`provider/anthropic-thinking-binding` (Anthropic), `provider/gemini`, and, as
Shared, `provider/empty-tool-choice`, `provider/conversation-lowering`,
`tool-schema-projection`, `tool-history`, `request-encoding`, `partial-json`,
`sanitize`, `error-partial`, `cache-policy`. Parametrized tests count once per generated case.

## Summary

| Protocol | Tests examined | Ported (already passing) | Ported after a fix | Gaps |
|---|---:|---:|---:|---:|
| OpenAI Chat | 115 | 15 | 100 | 0 |
| OpenAI Responses | 159 | 22 | 133 | 4 |
| Anthropic Messages | 95 | 11 | 84 | 0 |
| Gemini | 61 | 12 | 48 | 1 |
| Shared | 85 | 10 | 61 | 14 |
| **Total** | **515** | **70** | **426** | **19** |

Every remaining gap is by design or not applicable; no missing provider
feature is left on the roadmap.

Where a ported case drops part of an OpenCode test (a feature CodeSmith lacks,
or a configuration knob it has no equivalent for), the case says so in its
name or the list below; "(adapted)" marks a case rephrased for CodeSmith's
message model (tool results in user messages where OpenCode has tool
messages, consecutive user turns folded into one, string tool results, and
cache markers from a `body` override where OpenCode
puts a manual cache hint on a part).

## Fixes made while porting

OpenAI Chat (`openai.rs`)
- Empty system prompts are no longer sent as an empty system message.
- Assistant messages with no text (or whitespace only), no tool calls and no
  reasoning are dropped instead of sent as `content: null` without tool calls.
- Reasoning is replayed as `reasoning_content`, or under the field it
  streamed in (reasoning-only turns as `content: ""`); DeepSeek endpoints/models get it on every assistant message.
- OpenAI and Azure endpoints get `max_completion_tokens` (reasoning models
  reject `max_tokens`) and an explicit `store: false`; other servers keep
  `max_tokens` and get no `store`.
- Tool call ids are normalized per model family: 40 characters for OpenAI
  (direct or `openai/...`), `[A-Za-z0-9_-]` for Claude behind gateways, 9
  alphanumerics for Mistral-family models.
- Mistral-family models get an assistant `Done.` turn between tool results and
  user text.
- Gemini thought signatures on the OpenAI-compatible endpoint
  (`extra_content.google.thought_signature`) are parsed onto the tool-call id
  and replayed.
- Streamed refusals are assistant text; reasoning is read from the first of
  `reasoning_content`, `reasoning`, `reasoning_text` (no double counting).
- Tool-call deltas: index-less calls are matched by id or continue the latest
  call; empty or repeated ids/names on later deltas no longer clobber the
  first; tool calls cut off by `length` or `content_filter` are dropped.
- The first finish reason stands; an empty one means none. A stream that
  ends without one is cut short (a retryable error, OpenCode's incomplete
  stream) unless the provider sets `requireFinishReason: false`, which
  infers stop.
- `finish_reason: network_error` / `error` become (retryable / non-retryable)
  errors; in-stream `error` objects report their message and are retryable
  when their code says so.
- A top-level `reasoning_tokens` outside `completion_tokens` (GitHub Copilot)
  is counted as output.
- Fallback tool-call ids are unique across requests (`tool_…`), not `call_0`.

OpenAI Responses (`responses.rs`)
- Function calls are tracked from `output_item.added` by output index or item
  id: `function_call_arguments.delta/done` accumulate, `output_item.done` is
  authoritative, and calls never completed by an item are finalized from the
  `response.completed` output (or what streamed). An incomplete response drops
  them.
- `response.refusal.delta` is assistant text.
- `cache_write_tokens` are read and excluded from non-cached input.
- Errors (`error` events at the top level, under `error` or
  `response.error`; `response.failed`) read `code: message` and are retryable
  for rate limits, overloads and server errors.
- User input items carry `type: "message"`.

Anthropic Messages (`anthropic.rs`)
- Empty system prompts are omitted; whitespace-only text blocks are dropped.
- Tool ids are scrubbed to `[A-Za-z0-9_-]` (a Gemini signature is stripped
  first); `is_error` is sent only when true.
- Tool results lead their user turn (text that arrived between a call and its
  result moves after the result).
- Complete tool input from `content_block_start` is kept; tool blocks with no
  `content_block_stop` settle at `message_stop`; text/thinking deltas aimed at
  unknown block types are ignored.
- Unknown named SSE events with non-JSON data are skipped; untyped `{error}`
  envelopes are errors; `error` events read `type: message` and are
  retryable when overloaded/rate-limited/server errors (previously every
  in-stream error was a non-retryable decode error).
- The stream ends at `message_stop` instead of waiting for EOF.

Gemini (`gemini.rs`)
- Tool schemas go in `parametersJsonSchema` with OpenCode's Gemini
  normalization (draft-04 exclusive bounds, tuples to `prefixItems`, `required`
  limited to declared properties, unsafe recursive `$ref`s cut) instead of the
  lossy OpenAPI-subset trimming that dropped `oneOf`, `const`, `$ref`,
  `additionalProperties`.
- Unsigned tool calls replay with the `skip_thought_signature_validator`
  sentinel (only before the first signed call of a batch), so Gemini 3 accepts
  history it did not sign.
- Function call/response ids are sent for Gemini 3+ and non-Gemini models.
- Function responses get a user turn of their own (Gemini rejects a
  function-response turn with other parts); parallel results still merge.
- Missing or repeated function-call ids get fresh unique ids.
- `toolUsePromptTokenCount` counts as input.
- `MALFORMED_FUNCTION_CALL`, `UNEXPECTED_TOOL_CALL`, `MALFORMED_RESPONSE`
  (retryable) and `NO_IMAGE`, `TOO_MANY_TOOL_CALLS`,
  `MISSING_THOUGHT_SIGNATURE` finishes are errors; in-stream errors are
  classified by status.

Media (`lib.rs` and every protocol; the 41 cases in the `*_media` tests)
- `Part::Media` (user attachments) and `media` on `Part::ToolResult` (images
  and files a tool returned, such as `read` on a PNG or PDF), each a MIME type
  plus inline base64 or a URL. Both are additive in the stored JSON: results
  without media omit the key, and sessions stored before it still load.
- OpenAI Chat: images (and video) as `image_url`/`video_url` parts by URL or
  data URL, PDFs as inline `file` parts (`document.pdf` by default);
  tool-result media follows the tool messages as one user message (after the
  Mistral `Done.` bridge). PDFs by URL and other media types are rejected.
- Responses: `input_image`, `input_file` (inline `file_data` with a filename,
  or `file_url`) and `input_video`, in user messages and as structured
  `function_call_output` arrays; nothing is rejected.
- Anthropic: `image` and `document` blocks (base64, http(s) URL, plain text
  documents; PDF titles from the filename), inside `tool_result` content for
  tool media. Other types are rejected.
- Gemini: `inlineData` (and `fileData` for http(s) video) in user turns;
  tool media nests inside `functionResponse.parts`, except on Gemini 2.5 and
  for `fileData`, which go to a following `Attached media from tool result:`
  user turn shared by a batch of results.
- `Media.meta` holds per-part provider options and state keyed by provider
  (OpenCode's `providerMetadata`; absent from stored JSON when empty):
  Anthropic `file_id` (a `file` source), `title`, `context`, `citations`
  and `transformations.oversized_image` (`downsize` or `error`; anything
  malformed is an `Error::Invalid`), Responses `openai.detail`, and Gemini's
  `google.thoughtSignature` on generated media.
- Video: Anthropic `video` blocks, inline or by any URL, in user turns and
  tool results; on Responses, xAI (`api.x.ai`) takes video as a named
  `input_file`. Meta (`api.meta.ai`) cannot see video inside tool results:
  on Responses a batch of results sends its videos in a user message after
  it (a result left empty says so), and on Messages they move after the
  turn's results.
- Media in an assistant message (generated images): Gemini replays it as
  model `inlineData` with its thought signature; Responses, which has no
  assistant image item, as user input in place.
- Rejections are a new `Error::Invalid`, raised by `check` before anything is
  sent, so a session fails fast and never retries them.
- `normalize_tool_history` keeps a media-only result's empty text.

Provider options (`options.rs`, every protocol; `src/parity_options.rs`)
- `Request.options` carries OpenCode's provider options as a flat object
  (camelCase keys, snake_case accepted), set from the provider's and model's
  `options` (alias `settings`) and the selected variant. Each protocol lowers
  what it knows and ignores the rest.
- OpenAI Chat: `reasoningEffort` → `reasoning_effort`, `textVerbosity` →
  `verbosity`, `serviceTier`, `metadata`, `safetyIdentifier`,
  `parallelToolCalls` (with tools), `store` (OpenAI/Azure), generation knobs,
  and the compatible switches `reasoning` (OpenRouter; `max_tokens` fitted
  under the output limit), `thinking`, `enableThinking`, `thinkingBudget`,
  `includeReasoning`; Z.ai GLM models after 4.5 get `tool_stream` with tools.
  Malformed options are dropped one by one.
- OpenAI Responses: `reasoning: {effort, summary, mode, context}`,
  `text.verbosity`, `service_tier`, `include` (an empty list sends none),
  `store`, `metadata`, `safety_identifier`, `stream_options`,
  `top_logprobs` (0-20), `truncation`, `max_tool_calls`,
  `parallel_tool_calls`, `allowedTools` as an `allowed_tools` tool choice,
  penalties and `top_p`. On OpenAI's own endpoint `include:
  ["reasoning.encrypted_content"]` is the default, and GPT-5 reasoning models
  default to medium effort with `summary: auto`.
- Anthropic: `thinking` (`enabled` needs `budgetTokens`/`budget_tokens`,
  fitted to half of `max_tokens` but at least 1024; `adaptive`; `disabled`,
  `between_tools`), `effort` / `output_config`, `service_tier`, `speed`,
  `metadata`, `container`, `inference_geo`, `cache_control`,
  `contextManagement`, `top_p`, `top_k`, `stop_sequences`. Malformed known
  options are an `Error::Invalid` from `check`. Claude 5.1+ default to
  adaptive thinking with block binding; `anthropic-beta` always carries
  interleaved thinking, plus compaction, fast mode and thinking binding when
  used, appended to any configured betas.
- Gemini: `generationConfig.thinkingConfig` (`includeThoughts` defaults to
  true, `thinkingBudget` fitted to half the output, minimum 512, malformed
  fields dropped), `cachedContent`, `safetySettings`, `serviceTier`, and the
  generation knobs.
- Model variants (`provider/model#name`, OpenCode's syntax) layer options,
  body fields and headers over the model's: built-in reasoning variants per
  protocol and model family (after OpenCode's `Variant.resolve` with effort
  support), overridden or extended by a model's `variants` in config.

Reasoning replay (`lib.rs` and every protocol)
- `Part::Reasoning` holds a `Reasoning { text, meta }`; `meta` is the
  producing protocol's replay state and is read back only by that protocol.
  Reasoning without state is still stored as a plain string, so sessions from
  before load unchanged. `Event::ReasoningEnd` closes a reasoning block with
  its state, and `Collector` builds a message's parts in stream order (Chat
  reasoning stays one part around text, as before).
- Anthropic: thinking signatures (from `content_block_start` or
  `signature_delta`, also when `message_stop` closes the block) and
  `redacted_thinking` data are kept and sent back as `thinking` /
  `redacted_thinking` blocks; unsigned reasoning is dropped for Anthropic and
  sent with an empty signature to Kimi, MiMo, Qwen and MiniMax endpoints.
- Responses: reasoning items keep their id and `encrypted_content`; finals
  that streamed no deltas supply the text once; an item never completed ends
  with what its start said. Replay merges parts of one item.
- Gemini: thought signatures on thought parts are kept and replayed as
  `{text, thought: true, thoughtSignature}`.
- Chat: `reasoning_details` (OpenRouter and Kimi dialects; consecutive text
  and summary deltas merged; unknown entries dropped; Kimi's stream index
  stripped on replay), the latest `reasoning_opaque`, and the field reasoning
  streamed under (replayed under that field; fully structured reasoning gets
  no scalar field; Kimi details keep `reasoning_content`).

Responses item ids and phases (`lib.rs`, `responses.rs`;
`src/parity_tests.rs` `openai_responses_item_metadata`)
- Text and tool-call parts carry provider state as reasoning does:
  `Part::Text` holds a `Text { text, meta }` (stored as a plain string when
  there is no state, so older sessions load unchanged) and
  `Part::ToolCall` an optional `meta` (absent from stored JSON when unset).
  `Event::PartMeta` gives the state of the text block or tool call that just
  ended, and `Collector` attaches it to that part.
- Responses: a text block keeps its message item id and `phase`
  (`commentary`, `final_answer`, or an explicit `null`, from
  `output_item.added` or `.done`), and a function call its item id. Replay
  sends consecutive text of one item as one `message` with that `id`,
  `phase` and `status: "completed"`, and calls with their `id`; an id
  without a nonempty prefix and suffix around `_` is left out, its item
  kept. Text routes by output index before item id, and an
  `output_text.done` whose item streamed nothing supplies the text.

Effort updates (`Part::Effort`; `src/parity_options.rs` `effort_updates`, after
OpenCode's `effort-updates.test.ts`, not counted above)
- A `Role::System` message whose only part is `Part::Effort { effort,
  previous }` changes the reasoning effort from that point on (OpenCode's
  `Message.effort`). Claude Opus 5, every Claude 5.1 model and later
  versions (unless thinking is off) take it as a native `system` message
  with `output_config.effort` (with the `mid-conversation-output-config`
  beta), and GPT-6 and later on Responses (not in `pro` mode or with
  context management) as a `configuration_update` item, consecutive ones
  coalesced; an `effortUpdates` option overrides the detection. The
  top-level effort stays at the one before the first update, so the cached
  prefix survives; history whose last update disagrees with the requested
  effort drops its updates. Other routes drop the updates, and
  `normalize_tool_history` holds them after pending results like other
  system updates. The Anthropic message tail's cache marker skips a
  trailing update. Sessions do not write effort updates yet: a variant
  switch still changes the request's effort.

Prompt caching (`cache.rs`, every protocol), on by default as in OpenCode
- `Request.cache` is OpenCode's cache policy: `auto` (the default), off, or
  exact placements (`tools`, `system`, `messages`: a tail count,
  latest user or latest assistant message) with an optional `ttl`. Config:
  `cache` on a provider or model (also `settings.cache` / `options.cache`):
  `false`/`"none"`, `true`/`"auto"`, or `{ttl, tools, system, messages}`.
- Anthropic Messages: `cache_control` breakpoints on the last tool, the first
  and last system blocks (the system prompt becomes a block list) and the
  last message's last text block (else its last block that is not
  thinking), at most four per request (three beside the `cacheControl`
  option's automatic caching), spent tools → system → messages.
  Placed on the lowered body, so markers already there (a `body` override)
  are kept, count against the cap, and the excess is dropped in that order.
  The policy's `ttl` rides on every marker.
- OpenAI Chat: the same breakpoints in Anthropic's format where the server
  takes them: OpenRouter (Anthropic models; Qwen without tools; other
  upstreams none), DigitalOcean, DashScope Qwen models, or any server whose
  options declare `cacheFormat: anthropic | alibaba`. Tool objects carry the
  marker beside `function`; system, user and tool messages carry it on a text
  content part.
- OpenAI Chat and Responses: `Request.prompt_cache_key` (the session id,
  clamped to 64 characters) as `prompt_cache_key`: always on Responses
  (unless `setCacheKey: false`), on Chat for OpenAI, Azure, Cerebras,
  DeepInfra, DigitalOcean, Fireworks, Meta, Moonshot, OpenRouter and Venice
  (or `setCacheKey: true`); xAI gets the `x-grok-conv-id` header instead. The
  `ttl` goes out as `prompt_cache_retention` for `24h`, otherwise as
  `prompt_cache_options.ttl`, unless breakpoints carry it. Caching off sends
  neither key nor lifetime.
- Gemini: nothing (implicit caching; `cachedContent` stays an option).

Tool choice and parallel tool calls (every protocol)
- `Request.tool_choice` (`Auto`, `None`, `Required`, `Tool(name)`), sent only
  with declared tools: Chat `"auto"|"none"|"required"|{type: function,
  function: {name}}`, Responses the same with `{type: function, name}` (an
  `allowedTools` option wins), Anthropic `{type: auto|none|any|tool}`, Gemini
  `toolConfig.functionCallingConfig` `AUTO|NONE|ANY` (`allowedFunctionNames`
  for a named tool). A named choice without a name is an `Error::Invalid`.
- `Request.parallel_tool_calls`: Chat `parallel_tool_calls` (with tools),
  Responses `parallel_tool_calls`, Anthropic `disable_parallel_tool_use` on
  the tool choice; a `parallelToolCalls` option wins.
- The session's forced final step sends `ToolChoice::None` instead of a
  per-protocol body field, keeping the tool definitions (and the cached
  prefix).

OpenAI Responses (`responses.rs`)
- The system prompt leads `input` as a `developer` message, as in OpenCode
  (newer GPT models never cache top-level `instructions`), where it was sent
  as `instructions`.

Responses WebSocket transport (`ws.rs`, `ws/continuation.rs`,
`ws/session.rs`; `src/parity_ws.rs`, `codesmith-core/tests/websocket_session.rs`)
- New: a session's steps can go over the Responses WebSocket API
  (`settings.transport`, defaulting to `websocket` for OpenAI, xAI on
  Responses and Azure v1 endpoints as in OpenCode). One socket per session
  (tokio-tungstenite with rustls), reused across steps and replaced after 55
  minutes (24 for xAI) or when the URL or headers change; the message is the
  HTTP body without `stream`, `stream_options` and `background`, and the
  handshake the HTTP request's URL (`wss://`) and headers, with OpenAI's
  `OpenAI-Beta: responses_websockets=2026-02-06` default and Azure's
  `api-version` removed.
- Incremental continuation: a completed response is a checkpoint, and the
  next step sends only the input items after its input and output, with
  `previous_response_id`, when no other field changed (items compare
  without response-only metadata and with tool arguments as JSON; reasoning
  re-encrypted at completion keeps its streamed copy). xAI declines it with
  `store: false` and drops `instructions` from it.
- Stream checks on the socket: one `response.created` with an id before any
  other response event, the same id throughout, nothing after the terminal
  event; keepalives and provider notifications may come first.
- Recovery: `previous_response_not_found`, `websocket_connection_limit_reached`
  and an unclassified `invalid_request_error` (no code) against an
  incremental send are resent in full on a new socket before any output;
  every error frame drops the socket. A failed connect or a send that never
  left falls back to HTTP (a failed connect for the rest of the session), as
  does a 1009 close; five sockets lost mid-response in a row keep the
  session on HTTP. An abandoned response drops its socket.

Shared (`lib.rs`, `schema.rs`)
- `normalize_tool_history` runs before every request: a call without a result
  gets an error result (`Tool result missing`) before the next assistant turn,
  user text that arrives while calls are pending moves after their results,
  empty results become `(no tool output)`, and results take the name of their
  call. Not in OpenCode: a second result for an already answered call is
  dropped (the first stands), since providers reject the pair.
- Tool schemas get an object root (`{}` becomes `{"type": "object"}`), then the
  model family's rules: Gemini normalization for `gemini` models on any
  protocol, Moonshot rules (no `$ref` siblings, typed enums, tuple items) for
  `kimi` models.
- In-stream provider errors are a new `Error::Provider { message, retryable }`
  so the session retries transient ones instead of failing the turn.
- The four stream loops share one driver (`drive`) over per-protocol
  `Decoder`s, which the replay harness and parity tests use directly; a last
  frame without its blank line is still decoded.

Mid-conversation system updates (every protocol; `Role::System` in `lib.rs`,
`src/parity_tests.rs` `system_updates`)
- Messages have a `system` role for text the harness adds mid-conversation
  (OpenCode's `Message.system`), lowered per protocol: on Anthropic Messages,
  Claude Opus 4.8, every Claude 5 model and later versions (or any model with
  a `systemUpdates: true` option) get native `role: "system"` messages, held
  until the next assistant turn and sent after the latest user turn or tool
  result, with consecutive updates together; where no user turn precedes
  them, and on other models, they are `<system-update>` user text in place.
  Responses sends a `developer` message in place; Chat and Gemini send
  escaped `<system-update>` user text, joining the preceding user turn (Chat
  also the media of preceding tool results; Gemini starts a user turn of its
  own after function responses, after any pending tool media).
- `normalize_tool_history` moves a system update that arrives while calls are
  pending after their results, as it does user text.

Streams (`lib.rs`, all protocols)
- Decoders emit block lifecycle events as OpenCode does: `TextStart`/`TextEnd`
  and `ReasoningStart` with block ids (Chat has one text and one reasoning
  block per response; Gemini numbers `text-N` and `reasoning-N`; Anthropic
  opens a block at `content_block_start`, so an empty text block is kept),
  and `ToolInputStart`/`ToolInputDelta`/`ToolInputEnd` around each call whose
  arguments stream (Chat, Anthropic, Responses; a Responses call that arrives
  only completed still opens its input).
- The `Collector` keeps separate text blocks as separate parts, starts a new
  reasoning part at each reasoning start (separated Gemini thoughts no longer
  merge), and reports forming tool calls with their input parsed so far
  (`partial_json`, a port of OpenCode's partial JSON parser).
- `assemble` consumes a stream into a `Response`; a stream that fails, or ends
  without finishing, is a `Failure` carrying the text, reasoning and complete
  calls that arrived first (OpenCode's `AIError.partial`). The session keeps
  that partial text and reasoning in the history when the failure ends the
  turn (OpenCode's "durably closes partial text/reasoning when the provider
  stream fails"); a retried attempt still drops its partial output.
- Responses: text for a message item that a later item concluded is dropped,
  and a repeated `output_item.added` for a known reasoning or call item is
  ignored.
- Gemini: a response Gemini splits across requests is continued. After
  `CONTINUATION`, a request that ends at a checkpoint without finishing, or a
  dropped connection after a checkpoint (not a timeout), the body is resent
  with the latest `continuationToken`; blocks stay open across requests,
  `CONTINUATION` without a token is an error, a length finish is not
  continued, and usage counts the first request's input with every
  request's output. A blocked prompt now fails at the end of the stream, so a
  continued request can still finish. Not in OpenCode: a request that drops
  or ends after output its latest checkpoint does not cover is not
  continued (Gemini would generate that output again, and a repeated call
  would run twice); it fails retryably so the whole response is retried.

Finish reasons and usage (`lib.rs`, all protocols)
- A finish carries a `FinishReason` in OpenCode's algebra (`stop`, `length`,
  `tool-calls`, `content-filter`, `error`, `unknown`) with the provider's raw
  reason (Chat's `native_finish_reason` when given). Chat maps `end` to stop
  (and still counts `max_tokens` / `model_length` as length); Anthropic maps
  `pause_turn` to stop and `model_context_window_exceeded` to length, and a
  refusal carries its `stop_details` category and explanation; Gemini's
  safety finishes are content-filter and `STOP` with calls is tool-calls;
  Responses maps `content_filter`. A content-filter finish names the filter
  that fired: Copilot's `copilot_annotations`, Azure's
  `content_filter_results`, Copilot Responses' `content_filters`. The
  session and ACP read the typed reason, and a content-filter failure reads
  "Provider blocked the response (category): explanation" as OpenCode's
  does.
- `Usage` has reasoning tokens and a total (the provider's, else the sum of
  the counts; left out when neither input nor output was reported), and the
  finish carries the provider's usage payload as it came (a continued Gemini
  response sums its requests' counts). OpenCode keys that payload by a
  provider namespace; CodeSmith has one provider per stream, so the two
  namespace cases check the payload and the reasoning state (adapted). The
  recorded-traffic replay now checks the fixtures' `reasoningTokens`.
- The session pairs each streamed tool input with the call it forms by
  order (a call follows its input's end, even when its id gained a Gemini
  signature on the Chat endpoint) and tells the front ends: the TUI shows
  the call's row while its arguments stream and ACP announces it `pending`
  then, as OpenCode's ACP does; a call that is dropped (cut off by the
  output limit) or never runs fails.

Endpoints, auth, billing, compatibility and encoding (`endpoint.rs`,
`billing.rs`, `src/parity_endpoint.rs`)
- Providers take query parameters added to every request URL (Azure's
  `api-version`), a header for the key in place of the protocol's own
  (Azure's `api-key`, which also drops a configured `Authorization`
  header), and a gzip or zstd request body (OpenCode's `encoding`). Every
  protocol builds its request in one place; `Provider::prepare` returns it
  unsent. Azure's URL rules (`/openai/v1` with `api-version`, deployment
  URLs, proxies as given) are `azure_endpoint`, used by the built-in
  `azure` provider.
- Usage carries the cost the endpoint reports it billed: `usage.cost`
  (OpenRouter, LiteLLM, OpenCode Zen) on Chat, Responses, Anthropic and
  Gemini, else GitHub Copilot's `copilot_usage.total_nano_aiu` beside the
  usage; a malformed or negative cost is dropped. A continued Gemini
  response sums its requests' costs. Session cost and `codesmith stats` use
  a step's billed cost when it has one and price its tokens otherwise, as
  OpenCode's session usage does.
- OpenAI Chat takes explicit overrides of what it detects (OpenCode's model
  `compatibility`): `maxTokensField`, `streamUsage: false`,
  `requireReasoning`, `requireAssistantAfterTool`, `requireFinishReason`
  and `reasoningField`; Anthropic Messages takes `requireSignature`.
- Providers carry their configured id (`Provider.name`), for what OpenCode
  detects by provider: the `deepseek` provider requires reasoning on Chat,
  and the `kimi-for-coding`, `moonshotai`, `moonshotai-cn` and Xiaomi
  providers take unsigned reasoning on Anthropic Messages.
- "loads OpenAI default auth from Effect Config" and "lets explicit auth
  override OpenAI default API key auth" already held (config resolves the
  key from `settings.apiKey`, then the provider's variables); they are
  config tests, and the first reads a test variable in place of
  `OPENAI_API_KEY` (adapted).

Hosted (provider-executed) tools (`hosted.rs`, Responses, Anthropic,
Gemini; the 21 cases in `src/parity_hosted.rs`)
- A `hostedTools` provider option declares them (short names or wire
  declarations, beside function tools); Responses checks
  `image_generation` options before sending, a tool choice naming it sends
  `{"type": "image_generation"}`, and Anthropic adds the `web_fetch` and
  `code_execution` betas.
- Responses `web_search_call`, `file_search_call`, `code_interpreter_call`,
  `mcp_call`, `web_search_preview_call`, `computer_call` and
  `image_generation_call` items, Anthropic `server_tool_use` with its
  `*_tool_result` block (errors from `*_tool_result_error`; malformed
  input ends the stream), and Gemini `executableCode` /
  `codeExecutionResult` parts and search grounding decode as
  `Event::HostedCall` / `Event::HostedResult`, stored as one `Part::Hosted`
  naming the protocol that ran it. They never run locally and do not
  continue the turn; the session shows them as settled tool rows.
- Replay: Responses sends a complete item from this API under its own id
  (once per message), and everything else (errors, other protocols'
  results, unknown item types, ids that do not match) as portable user
  content, generated images as `input_image`; Anthropic sends
  `server_tool_use` plus its result block and rejects results of unknown
  server tools; Gemini sends code execution back as its parts. Hosted parts
  of other protocols are left out of Anthropic, Gemini and Chat requests.
- Adapted: OpenCode's two parts (call and result) are one part here; its
  `store: true` / `false` variants lower the same way (CodeSmith's Responses
  requests are stateless); "foreign" results (another provider's metadata
  key) are results from another protocol; the malformed server tool input
  error names the route (`Anthropic Messages`) where OpenCode names the
  adapter.

Strict stream validation (every protocol; `src/shape.rs`)
- Recognized events are checked against OpenCode's event schemas: a field
  CodeSmith reads with the wrong JSON type, or a required item missing, is
  invalid output (a non-retryable `Error::Decode` named after OpenCode's
  `Invalid <route> stream event`). Unknown fields, event types, Anthropic
  block and delta types and Gemini part kinds still pass.
- Chat: tool call changes after the finish reason, and a call still
  without a name when a finish confirms the calls, are invalid.
- Responses: text, refusal, reasoning and function argument events need
  their `item_id`; refusals need their text; a reasoning item may not start
  while another is open. Item ids resolve through the output index (as
  OpenCode's `normalize`), and text or reasoning for an item that is not
  open is dropped.
- Anthropic: a `message_delta` needs a delta, a `tool_use` block an index
  and id, and a recognized event name JSON data.

Declared reasoning fields, signed text and tool namespaces (`lib.rs`,
`openai.rs`, `anthropic.rs`, `gemini.rs`, `responses.rs`;
`src/parity_metadata.rs`)
- Chat `compatibility.reasoningField`: the declared field is read before
  the known ones while streaming and stored as the reasoning's field; every
  assistant message's reasoning is written there (empty when there was none,
  since a declared field makes reasoning required unless `requireReasoning:
  false`); `reasoning_details` and `reasoning_opaque` from earlier turns are
  sent only when no field, or `reasoning_details`, is declared. A field that
  is one of the message's own (`role`, `content`, `refusal`, `tool_calls`)
  is an `Error::Invalid` before anything is sent.
- Chat reasoning that is only state (empty `reasoning_details`, an opaque
  value) opens its block before the output that follows, so it is stored
  before that text; the `Collector` keeps a started reasoning block's place
  until its state arrives and drops it if none does.
- Anthropic `compatibility.requireSignature` overrides whether unsigned
  reasoning is dropped or replayed with an empty signature.
- Gemini signatures on visible text (`thoughtSignature` on a text part,
  also an empty one that closes the block) are kept in the text part's
  provider state (an `Event::PartMeta` at the block's end) and replayed on
  that text.
- Tool namespaces (`Request.namespaces`, OpenCode's `ToolNamespace`):
  Responses declares them natively (`type: namespace`, a default
  description, nested levels flattened into `<nested>_<name>` leaves),
  decodes a call's `namespace` and replays declared calls with it and
  their item id; the other protocols declare the leaves flat as
  `<namespace>_<name>`, after the plain tools (so cache breakpoints land on
  the last leaf). Calls are stored under the flat name on every protocol.

## Partially ported

- `openai-responses` "preserves tool namespaces through streaming and history
  replay": the call is stored as `crm_lookup` rather than `lookup` with a
  namespace, and replays with its namespace only when the request declares
  that namespace.
- `anthropic-messages` "round-trips compatible provider metadata in its own
  namespace": the stop sequence is not kept, and state is not keyed by a
  namespace. "prepares the composed native continuation request" checks a
  plain-string system prompt (OpenCode sends one text block).
- `cache-policy` "marks the final leaf inside a tool namespace" checks the
  lowered Anthropic body, not the policy's output.
- `gemini` "preserves thoughtSignature on visible text parts" and "flushes a
  trailing empty signed text part": the signature is checked on the stored
  part and the `PartMeta` event after the text's end, not on OpenCode's
  text-delta metadata.

- Responses WebSocket (`src/parity_ws.rs`): errors carry no HTTP context,
  cause or OpenCode reason tag, so the rejection and control-event cases
  check the recovery and message; delivery state is read from the
  transport's last failure; the "already closed" and "system code" cases
  use tokio-tungstenite against a server that hangs up and a closed port
  (`ECONNREFUSED` for OpenCode's `ENOTFOUND`); the sanitizing case checks
  non-ASCII text (Rust strings hold no lone surrogates); the unsupported
  Azure case logs no warning; the composed continuation request replays
  assistant text as `type: "message"` items; "closes a direct WebSocket
  execution" is the session's socket, dropped when a response is abandoned.
  Not ported: OpenCode's interceptor hooks (`experimental.ws.*`), metrics,
  and the connect and idle timeouts' configuration (fixed at 15 seconds and
  30 minutes).

- `gemini` "does not resume a response cut off by its own timeout": a reqwest
  read timeout stands for OpenCode's request timeout.
- `error-partial` "loop reports only the failed step as partial" is a session
  test (`runner_parity.rs`): the failed step's partial text is the last
  assistant message, without the earlier step's call. The session's port of
  "durably closes partial tool input" keeps no message: a call whose input
  never completed did not run and is left out (OpenCode keeps it as a failed
  tool).
- `gemini` "assigns unique ids to separated reasoning blocks": delta ids are
  implied (a delta belongs to the latest open block of its kind), and an
  unsigned thought block has no end event.

- `openai-compatible-chat` "matches AI SDK compatible tool request body
  fixture": no `tool_choice` or `strict`.
- `anthropic-messages` "prepares Anthropic Messages target": a system-only
  cache policy stands for OpenCode's manual system hint.
- `openai-compatible-chat` "prepares generic Chat target": no `strict` flag
  on tools (the endpoint query is checked by "posts to the configured
  compatible endpoint …").
- `gemini` "merges parallel tool results into one function-response turn":
  CodeSmith's function response payload is `{"output": …}` / `{"error": …}`
  (Google's documented keys) where OpenCode sends `{name, content}`.
- `tool-schema-projection` "declares every tool schema root as an object" and
  "selects tool schema handling from the model name …": the explicit
  `sanitizer` override cases are omitted.
- Provider options and reasoning replay (`src/parity_options.rs`):
  "forwards standard Gemini generation
  options" passes the knobs as options (CodeSmith's request has only
  `max_tokens` and `temperature` as fields); "reconciles reasoning summaries that arrive only
  as finals" and "joins streamed summary blocks …" add the answer that
  follows the reasoning (see deliberate differences); "replays the latest
  reasoning_opaque …" and "accepts empty IDs …" check the stored part, not
  OpenCode's lifecycle events.
- Media: "continues a conversation with user media content" checks the
  request only; the Gemini cases send `{"output": …}` (as above); "bridges
  image tool results …" uses a Mistral model where OpenCode sets an explicit
  compatibility flag; "forwards native video URLs …" covers generateContent,
  not Interactions; "serializes Messages video inputs" has no per-part
  cache hint (CodeSmith has none) or provider file reference (CodeSmith
  media has no reference source), and the Meta cases detect Meta by its
  endpoint where OpenCode uses its routes.
- Responses item metadata: "preserves final reasoning metadata …" checks
  the one reasoning block CodeSmith makes per item (see deliberate
  differences); "preserves provider-issued item ids …" drops the reasoning
  item that has neither encrypted state nor a valid id, which OpenCode
  replays without its id.

## Deliberate differences

- Responses: a reasoning item is replayed only with encrypted content and
  only when output follows it in the message (CodeSmith never stores
  responses server-side).
- Stream validation keeps two leniencies: a Chat tool call with a name but
  no id gets a fresh id (OpenCode rejects it), and in-stream error
  payloads are reported whatever their field types (OpenCode reports a
  mistyped one as an invalid event).
- Gemini prompt blocks without candidates are an error naming the block reason
  (raised when the stream ends without a finish).
- Responses: a reasoning item is one block and one stored part, its summary
  parts joined by blank lines; OpenCode streams each summary part as its own
  block.
- Chat: a system update right after tool results still gets the Mistral
  assistant bridge (OpenCode bridges only before user messages); Mistral
  rejects user content straight after a tool message.

## By design or not applicable


### Not applicable: Effect Schema tool definitions (7 tests)

Tests of converting Effect `Schema.Struct` tool inputs to JSON Schema. CodeSmith tools are plain JSON Schema already.

_Shared 7._

<details><summary>Tests</summary>

- `tool-schema-projection` — only normalizes typed empty input structs, preserving raw schemas and output schemas
- `tool-schema-projection` — retains empty input descriptions and preserves named outputs and explicit raw schemas
- `tool-schema-projection` — normalizes inputs whose encoded side is an empty struct
- `tool-schema-projection` — ${scenario.route.id} prepares empty Effect Struct tools alongside nonempty and raw schemas (×4)

</details>


### Deliberate differences (7 tests)

Gemini prompt-safety blocks (`promptFeedback.blockReason` without candidates) are an error in CodeSmith (the user sees why), not a content-filter finish; `OpenResponses.lowerConversation` is an OpenCode compaction helper with no counterpart. CodeSmith's Responses requests are always stateless, so a reasoning item goes back only with its encrypted content (an id alone cannot be resolved with `store: false`) and only before the output item that follows it (the API rejects a reasoning item without its following item). A Responses reasoning item is one block and one stored part (its summary parts joined by blank lines), not a block per summary part. `generate` with an output schema has no counterpart (CodeSmith has no structured-output call).

_OpenAI Responses 4, Gemini 1, Shared 2._

<details><summary>Tests</summary>

- `openai-responses` — replays complete reasoning items when storage is enabled
- `openai-responses` — replays stateless reasoning without encrypted state
- `gemini` — preserves candidate-less prompt safety blocks as content-filter outcomes
- `conversation-lowering` — conversation lowering excludes generation settings and tool definitions
- `openai-responses` — streams each reasoning summary part as a separate block
- `openai-responses` — concludes reasoning at implicit summary boundaries
- `error-partial` — generate with schema keeps the raw output when the token limit cuts off the forced tool call

</details>


### Not applicable: cache policy internals (3 tests)

Bedrock `cachePoint` markers (CodeSmith has no Bedrock Converse protocol), de-duplicating same-named tools before counting manual tool hints (CodeSmith has no per-part hints, and session tool names are unique), and `applyCachePolicy` returning the same object when nothing changes (a JavaScript identity check).

_Shared 3._

<details><summary>Tests</summary>

- `cache-policy` — 'auto' on Bedrock emits cachePoint markers in the right places
- `cache-policy` — deduplicates tools before counting cache hints
- `cache-policy` — returns the same request reference when policy is a no-op (pure function)

</details>


### Not applicable: lone surrogates (2 tests)

Rust strings are always valid UTF-8, so the surrogate sanitizer has nothing to do.

_Shared 2._

<details><summary>Tests</summary>

- `sanitize` — replaces lone surrogates in nested message text, tool input, and tool output
- `sanitize` — keeps clean requests equal and shares unchanged nested values

</details>
