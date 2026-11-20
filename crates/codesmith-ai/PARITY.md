# Provider parity with OpenCode

CodeSmith's provider protocols (`src/openai.rs`, `src/responses.rs`,
`src/anthropic.rs`, `src/gemini.rs`) are pinned to OpenCode's by porting
OpenCode's provider unit tests (`packages/ai/test`, MIT) as table-driven Rust
tests in [`src/parity_tests.rs`](src/parity_tests.rs). Response parsing is also
checked against OpenCode's recorded traffic in `src/replay_tests.rs`; this
matrix covers the unit tests, which are mostly about how requests are built.

Every OpenCode test in these files was examined and put in one bucket:

- **ported**: CodeSmith already behaved this way; the test is ported.
- **ported after a fix**: CodeSmith was wrong or missing something cheap that
  matters with real providers; it was fixed and the test ported.
- **gap**: the test depends on a feature CodeSmith does not have (the roadmap
  below), on behaviour CodeSmith differs on by design, or does not apply.

Files: `provider/openai-chat`, `provider/openai-compatible-chat` (both counted
as OpenAI Chat), `provider/openai-responses`, `provider/anthropic-messages`,
`provider/anthropic-thinking-binding` (Anthropic), `provider/gemini`, and, as
Shared, `provider/empty-tool-choice`, `provider/conversation-lowering`,
`tool-schema-projection`, `tool-history`, `request-encoding`, `partial-json`,
`sanitize`, `error-partial`. Parametrized tests count once per generated case.

## Summary

| Protocol | Tests examined | Ported (already passing) | Ported after a fix | Gaps |
|---|---:|---:|---:|---:|
| OpenAI Chat | 115 | 15 | 40 | 60 |
| OpenAI Responses | 159 | 20 | 34 | 105 |
| Anthropic Messages | 95 | 9 | 19 | 67 |
| Gemini | 61 | 12 | 22 | 27 |
| Shared | 56 | 10 | 10 | 36 |
| **Total** | **486** | **66** | **125** | **295** |

Of the gaps, 268 are missing provider features (the roadmap) and
27 are by design or not applicable.

Where a ported case drops part of an OpenCode test (a feature CodeSmith lacks,
or a configuration knob it has no equivalent for), the case says so in its
name or the list below; "(adapted)" marks a case rephrased for CodeSmith's
message model (user text where OpenCode has system-update or tool messages,
string tool results).

## Fixes made while porting

OpenAI Chat (`openai.rs`)
- Empty system prompts are no longer sent as an empty system message.
- Assistant messages with no text (or whitespace only), no tool calls and no
  reasoning are dropped instead of sent as `content: null` without tool calls.
- Reasoning is replayed as `reasoning_content` (reasoning-only turns as
  `content: ""`); DeepSeek endpoints/models get it on every assistant message.
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
- The first finish reason stands; an empty one means none (inferred stop).
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

Media (`lib.rs` and every protocol; the 34 cases in the `*_media` tests)
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
- Rejections are a new `Error::Invalid`, raised by `check` before anything is
  sent, so a session fails fast and never retries them.
- `normalize_tool_history` keeps a media-only result's empty text.

Shared (`lib.rs`, `schema.rs`)
- `normalize_tool_history` runs before every request: a call without a result
  gets an error result (`Tool result missing`) before the next assistant turn,
  user text that arrives while calls are pending moves after their results,
  empty results become `(no tool output)`, and results take the name of their
  call.
- Tool schemas get an object root (`{}` becomes `{"type": "object"}`), then the
  model family's rules: Gemini normalization for `gemini` models on any
  protocol, Moonshot rules (no `$ref` siblings, typed enums, tuple items) for
  `kimi` models.
- In-stream provider errors are a new `Error::Provider { message, retryable }`
  so the session retries transient ones instead of failing the turn.
- The four stream loops share one driver (`drive`) over per-protocol
  `Decoder`s, which the replay harness and parity tests use directly; a last
  frame without its blank line is still decoded.

## Partially ported

- `openai-chat` "parses and replays OpenAI-compatible reasoning fields": parsing
  only; the replay field is always `reasoning_content` (see structured
  reasoning replay).
- `openai-compatible-chat` "matches AI SDK compatible tool request body
  fixture": no `tool_choice` or `strict`.
- `openai-compatible-chat` "bridges tool results for Mistral-family models …"
  and "requires reasoning for DeepSeek …": the detected cases; explicit
  compatibility overrides and provider-name detection are not modelled.
- `openai-responses` "preserves standard refusal content …": text only, no
  item id/phase replay.
- `anthropic-messages` "prepares Anthropic Messages target": without the
  system cache hint; "filters whitespace-only text …": without the signed
  empty thinking block.
- `gemini` "merges parallel tool results into one function-response turn":
  CodeSmith's function response payload is `{"output": …}` / `{"error": …}`
  (Google's documented keys) where OpenCode sends `{name, content}`.
- `tool-schema-projection` "declares every tool schema root as an object" and
  "selects tool schema handling from the model name …": the explicit
  `sanitizer` override cases are omitted.
- `tool-history` cases: provider-executed parts are omitted.
- Media: "continues a conversation with user media content" checks the
  request only; the Gemini cases send `{"output": …}` (as above); "bridges
  image tool results …" uses a Mistral model where OpenCode sets an explicit
  compatibility flag; "forwards native video URLs …" covers generateContent,
  not Interactions.

## Deliberate differences

- Responses: the system prompt goes in `instructions`, not a leading
  `developer` message (`prepares OpenAI Responses target`, also the encrypted
  reasoning gap).
- Finish reasons are raw provider strings; see "Normalized finish reasons".
- Streams are decoded leniently; see "Strict stream validation".
- Gemini prompt blocks without candidates are an error naming the block reason.

## Roadmap: provider features CodeSmith lacks


### Provider options (reasoning effort, thinking, service tier, verbosity, generation options) (41 tests)

Typed per-provider request options. Arbitrary top-level fields already pass through `Request.body` (e.g. `reasoning_effort`), but nothing is typed, validated or lowered per protocol (Anthropic thinking budgets fitted to `max_tokens`, Gemini `thinkingConfig`, Responses `reasoning` object, beta headers).

_OpenAI Chat 5, OpenAI Responses 11, Anthropic Messages 21, Gemini 3, Shared 1._

<details><summary>Tests</summary>

- `openai-chat` — maps OpenAI provider options to Chat options
- `openai-chat` — keeps valid Chat options when a sibling option is malformed
- `openai-chat` — maps shared OpenAI options to Chat fields
- `openai-chat` — passes through custom OpenAI-compatible reasoning effort strings
- `openai-compatible-chat` — enables ZAI tool streaming except for GLM 4.5 models
- `openai-responses` — lowers semantic service tier options
- `openai-responses` — passes through custom OpenAI reasoning effort strings
- `openai-responses` — lowers reasoning mode and context into the reasoning object
- `openai-responses` — passes through custom OpenAI text verbosity strings
- `openai-responses` — passes through provider-defined and future service tiers
- `openai-responses` — maps OpenAI provider options to Responses options
- `openai-responses` — drops a malformed provider option without discarding its siblings
- `openai-responses` — accepts the full ResponseIncludable union
- `openai-responses` — passes forward-compatible includable values through
- `openai-responses` — treats an explicit empty include as no include at all
- `openai-responses` — passes an unknown includable value through
- `anthropic-messages` — lowers adaptive thinking settings with effort
- `anthropic-messages` — normalizes enabled and disabled thinking settings
- `anthropic-messages` — fits the thinking budget to half the output limit
- `anthropic-messages` — rejects enabled thinking without a budget
- `anthropic-messages` — lowers passthrough provider options and accepts either key spelling
- `anthropic-messages` — sends fast mode speed with its beta header
- `anthropic-messages` — forwards unknown values for pass-through string enums
- `anthropic-messages` — ignores unknown provider options and rejects malformed known ones
- `anthropic-thinking-binding` — thinking-binding defaults for ${id} (×12)
- `anthropic-thinking-binding` — preserves explicit thinking settings and combines required beta headers
- `gemini` — normalizes Gemini thinking options
- `gemini` — fits the thinking budget to half the output limit
- `gemini` — forwards standard Gemini generation options
- `tool-history` — moves effort updates after the results of pending calls

</details>


### Structured reasoning replay on Chat (`reasoning_details`, `reasoning_opaque`, per-field memory) (27 tests)

CodeSmith keeps reasoning as plain text and replays it as `reasoning_content`. OpenRouter/Kimi `reasoning_details`, Copilot `reasoning_opaque` and the field a model streamed reasoning under are not stored, so they cannot be replayed verbatim.

_OpenAI Chat 27._

<details><summary>Tests</summary>

- `openai-chat` — writes reasoning to a configured custom field on every assistant message
- `openai-chat` — preserves observed reasoning fields when reasoning is required
- `openai-chat` — omits empty configured reasoning fields when reasoning is explicitly optional
- `openai-chat` — rejects reasoning fields that conflict with assistant message fields
- `openai-chat` — orders metadata-only reasoning before refusal output
- `openai-chat` — does not replay reasoning details when the model declares another reasoning field
- `openai-chat` — keeps replayed reasoning details when the model declares reasoning_details
- `openai-chat` — keeps replayed reasoning details when the model declares no reasoning field
- `openai-chat` — parses and replays a configured custom reasoning field
- `openai-chat` — preserves and replays reasoning details alongside scalar reasoning
- `openai-chat` — uses reasoning details as display fallback without inventing a scalar replay field
- `openai-chat` — drops unknown reasoning details while using scalar display text
- `openai-chat` — merges Kimi summary deltas by index and replays details without the streaming index
- `openai-chat` — displays Kimi summaries and replays reasoning_content when no scalar reasoning streams
- `openai-chat` — strips the streaming index from previously stored Kimi details
- `openai-chat` — merges consecutive OpenRouter summary deltas and replays them unmodified
- `openai-chat` — uses scalar display text for signature-only reasoning details
- `openai-chat` — preserves scalar reasoning after content starts in one lifecycle
- `openai-chat` — keeps one reasoning lifecycle across many content chunks
- `openai-chat` — preserves an explicitly empty reasoning details array
- `openai-chat` — attaches signature-only details that arrive after content
- `openai-chat` — preserves metadata-only reasoning arriving after the finish reason
- `openai-chat` — flushes details-only display reasoning when the stream ends
- `openai-chat` — replays details from multiple reasoning parts in order
- `openai-chat` — retains scalar replay for mixed structured reasoning parts
- `openai-compatible-chat` — replays the latest reasoning_opaque with the concatenated reasoning text
- `openai-compatible-chat` — keeps a reasoning_opaque that arrives without reasoning text

</details>


### Responses WebSocket transport and incremental continuation (26 tests)

OpenCode can stream Responses over a WebSocket and send only new input against a previous response checkpoint. CodeSmith is stateless HTTP only (`store: false`, full history each turn).

_OpenAI Responses 26._

<details><summary>Tests</summary>

- `openai-responses` — prepares one OpenAI Responses route for either transport
- `openai-responses` — streams OpenAI Responses over WebSocket
- `openai-responses` — rejects out-of-order and mismatched WebSocket response events
- `openai-responses` — tolerates keepalive and provider notifications before response.created
- `openai-responses` — continues a streamed tool call with only the new tool output
- `openai-responses` — continues a tool call from authoritative completed response output
- `openai-responses` — continues a custom tool call from completed response output
- `openai-responses` — continues a promoted steer after assistant output with response-only text metadata
- `openai-responses` — continues streamed reasoning when completion re-encrypts the same item
- `openai-responses` — uses a full request when any non-input invariant changes
- `openai-responses` — stages no checkpoint for incomplete or ID-less completion
- `openai-responses` — classifies explicit continuation rejection for runner-owned recovery
- `openai-responses` — retries an incremental send in full when the provider rejects it without a code
- `openai-responses` — shapes the incremental send with the route continuation
- `openai-responses` — builds WebSocket and HTTP fallback from the same final request
- `openai-responses` — sanitizes outbound WebSocket requests and HTTP fallback bodies
- `openai-responses` — builds xAI WebSocket requests without OpenAI handshake headers
- `openai-responses` — builds Azure WebSocket requests with v1 URLs and the route's auth header
- `openai-responses` — keeps unsupported Azure endpoints and API versions on HTTP
- `openai-responses` — uses exactly one HTTP request when no WebSocket executor is supplied
- `openai-responses` — closes a direct WebSocket execution after partial consumption
- `openai-responses` — terminates WebSocket control events without waiting for the socket to close
- `openai-responses` — marks post-send WebSocket failures with delivery state
- `openai-responses` — fails immediately when WebSocket is already closed
- `openai-responses` — keeps the system code when a WebSocket fails to connect
- `openai-responses` — prepares the composed native continuation request

</details>


### Hosted (provider-executed) tools (21 tests)

Web search, code interpreter, computer use, image generation and Anthropic server tools: never declared, so their items are not decoded or replayed.

_OpenAI Responses 16, Anthropic Messages 5._

<details><summary>Tests</summary>

- `openai-responses` — lowers the hosted OpenAI image generation tool
- `openai-responses` — rejects invalid hosted image generation options locally
- `openai-responses` — replays complete hosted tool items when storage is enabled
- `openai-responses` — replays stateless hosted tool results as native provider items
- `openai-responses` — replays OpenAI hosted tool extensions but rejects foreign and unknown items
- `openai-responses` — preserves foreign hosted tool results as portable message content when storage is enabled
- `openai-responses` — does not replay hosted tool items whose result id differs from provider metadata
- `openai-responses` — falls back to portable hosted results when stored item metadata is malformed
- `openai-responses` — continues stateless hosted image generation with the generated image
- `openai-responses` — preserves foreign hosted images as portable image content when storage is enabled
- `openai-responses` — decodes web_search_call as provider-executed tool-call + tool-result
- `openai-responses` — decodes computer_call as provider-executed tool-call + tool-result
- `openai-responses` — replays hosted image results as portable content regardless of storage
- `openai-responses` — preserves failed hosted tool results as portable error content
- `openai-responses` — rejects malformed image generation base64
- `openai-responses` — decodes code_interpreter_call as provider-executed events with code input
- `anthropic-messages` — keeps malformed server tool input terminal
- `anthropic-messages` — decodes server_tool_use + web_search_tool_result as provider-executed events
- `anthropic-messages` — decodes web_search_tool_result_error as provider-executed error result
- `anthropic-messages` — round-trips provider-executed assistant content into server tool blocks
- `anthropic-messages` — rejects round-trip for unknown server tool names

</details>


### Responses encrypted reasoning and output-item replay (20 tests)

No `include: ["reasoning.encrypted_content"]`, no reasoning items, item ids or message `phase` replayed. Stateless reasoning models therefore lose their reasoning between turns. System prompt goes in `instructions` (see deliberate differences).

_OpenAI Responses 20._

<details><summary>Tests</summary>

- `openai-responses` — prepares OpenAI Responses target
- `openai-responses` — requests encrypted reasoning by default
- `openai-responses` — requests encrypted reasoning by default for GPT-5 reasoning models
- `openai-responses` — lets callers opt out of the GPT-5 default include
- `openai-responses` — preserves and replays assistant message phases
- `openai-responses` — accepts empty IDs for native reasoning text deltas
- `openai-responses` — preserves encrypted reasoning metadata for continuation
- `openai-responses` — ignores terminal reasoning output when item completion is missing
- `openai-responses` — does not repeat reasoning already finalized by an output item
- `openai-responses` — recovers pending function calls without reconciling terminal reasoning
- `openai-responses` — reconciles reasoning summaries that arrive only as finals
- `openai-responses` — does not duplicate reasoning finals after streamed deltas
- `openai-responses` — preserves final reasoning metadata when storage is enabled
- `openai-responses` — continues a stateless reasoning conversation
- `openai-responses` — preserves assistant content order around reasoning items
- `openai-responses` — replays complete reasoning items when storage is enabled
- `openai-responses` — preserves provider-issued item ids and removes malformed ids without dropping items
- `openai-responses` — joins streamed summary blocks into one continuation reasoning item
- `openai-responses` — replays stateless reasoning without encrypted state
- `openai-responses` — drops empty reasoning items without summary or encrypted content

</details>


### Mid-conversation system messages (16 tests)

CodeSmith has no system role inside the history; reminders are user text (`<system-reminder>`). OpenCode lowers chronological system updates natively (Anthropic) or as wrapped user/developer text.

_OpenAI Chat 3, OpenAI Responses 1, Anthropic Messages 10, Gemini 2._

<details><summary>Tests</summary>

- `openai-chat` — lowers chronological system updates to escaped user wrappers in order
- `openai-chat` — aggregates consecutive tool images with a following system update
- `openai-chat` — appends system updates without replacing multipart user content
- `openai-responses` — lowers chronological system updates to developer messages in order
- `anthropic-messages` — lowers chronological system updates natively for Claude Opus 4.8 with cache hints
- `anthropic-messages` — supports native chronological system updates on documented and later Claude family versions
- `anthropic-messages` — lowers chronological system updates to wrapped user text for unsupported Anthropic models
- `anthropic-messages` — does not infer native system update support for older or undocumented Claude families
- `anthropic-messages` — falls back for unsupported native chronological system update placement
- `anthropic-messages` — moves system updates to the next assistant turn and sends consecutive updates together
- `anthropic-messages` — keeps wrapped system updates in place for models without native system updates
- `anthropic-messages` — sends Vertex system updates after local tool results as native system messages
- `anthropic-messages` — keeps a terminal direct Anthropic system update native
- `anthropic-messages` — keeps an ordinary terminal Vertex system update native
- `gemini` — lowers chronological system updates to wrapped user text in order
- `gemini` — flushes pending media before system update text

</details>


### Reasoning signatures and redacted thinking (15 tests)

Anthropic `thinking`/`redacted_thinking` blocks with signatures, and Gemini signatures on text/reasoning parts, are not stored or replayed (Gemini tool-call signatures are, via the `id~signature` convention). Needed for extended thinking with tool use on Anthropic.

_Anthropic Messages 12, Gemini 3._

<details><summary>Tests</summary>

- `anthropic-messages` — prepares the composed native continuation request
- `anthropic-messages` — lowers preserved Anthropic reasoning signature metadata
- `anthropic-messages` — drops unsigned reasoning when signatures are required
- `anthropic-messages` — infers empty-signature compatibility across Kimi providers
- `anthropic-messages` — lets an explicit signature requirement override inference
- `anthropic-messages` — round-trips redacted thinking as redacted_thinking blocks
- `anthropic-messages` — round-trips compatible provider metadata in its own namespace
- `anthropic-messages` — preserves a reasoning signature when message_stop closes the block
- `anthropic-messages` — round-trips omitted thinking carried only by a signature delta
- `anthropic-messages` — retains a thinking signature supplied in content_block_start
- `anthropic-messages` — parses redacted thinking into empty reasoning with redactedData metadata
- `anthropic-messages` — round-trips streamed redacted thinking with tool use into a continuation request
- `gemini` — preserves thoughtSignature for reasoning and tool-call continuation
- `gemini` — preserves thoughtSignature on visible text parts
- `gemini` — flushes a trailing empty signed text part at block close

</details>


### Prompt caching (14 tests)

No `cache_control` breakpoints (Anthropic, Anthropic-format Chat gateways), no `prompt_cache_key`/TTL options, no xAI conversation header. Cache usage is still parsed.

_OpenAI Chat 6, OpenAI Responses 4, Anthropic Messages 4._

<details><summary>Tests</summary>

- `openai-chat` — maps the request prompt cache key when the compatibility flag is set
- `openai-chat` — sends the cache TTL as a request option when no markers carry it
- `openai-chat` — omits the prompt cache key without the compatibility flag
- `openai-chat` — omits the prompt cache key when caching is disabled
- `openai-chat` — maps the xAI Chat prompt cache key to conversation affinity header only
- `openai-compatible-chat` — places cache breakpoints only when compatibility declares the format
- `openai-responses` — sends the cache TTL as prompt_cache_options, or prompt_cache_retention for 24h
- `openai-responses` — maps the request prompt cache key
- `openai-responses` — omits the prompt cache key when caching is disabled
- `openai-responses` — omits the prompt cache key when capabilities disable it
- `anthropic-messages` — sends the hint ttl on cache_control
- `anthropic-messages` — emits cache_control on tool definitions and tool-result blocks
- `anthropic-messages` — drops cache_control breakpoints past the 4-per-request cap
- `anthropic-messages` — spends breakpoint budget on tools before system before messages

</details>


### Partial results: streaming partial tool input and partial output on errors (14 tests)

CodeSmith parses tool arguments once complete and has no partial-JSON parser (`partial-json.test.ts`), and errors carry no partial response (`error-partial.test.ts`; the session layer tracks whether anything streamed instead).

_Shared 14._

<details><summary>Tests</summary>

- `partial-json` — parses complete JSON
- `partial-json` — parses partial strings
- `partial-json` — repairs invalid escapes and raw control characters
- `partial-json` — preserves prototype keys in partial objects
- `partial-json` — controls partial collection values independently
- `partial-json` — parses partial literals and numbers
- `partial-json` — distinguishes disallowed partial values from malformed values
- `partial-json` — rejects empty input
- `error-partial` — generate keeps the output that arrived before a mid-stream failure
- `error-partial` — generate keeps the same error when the failure comes before any output
- `error-partial` — generate with schema keeps the raw output when the token limit cuts off the forced tool call
- `error-partial` — stream keeps the output that arrived before a mid-stream failure
- `error-partial` — loop reports only the failed step as partial
- `error-partial` — rejects with the root-exported AIError carrying partial

</details>


### Tool choice and parallel tool calls (14 tests)

CodeSmith always lets the model choose (`auto`): no `tool_choice` (`none`, `required`, named, allowed tools), no `parallel_tool_calls`.

_OpenAI Chat 2, OpenAI Responses 1, Anthropic Messages 1, Gemini 1, Shared 9._

<details><summary>Tests</summary>

- `openai-chat` — maps disableParallelToolUse to parallel_tool_calls only when tools are present
- `openai-compatible-chat` — prepares generic Chat target
- `openai-responses` — maps the canonical parallel tool setting with provider-option precedence
- `anthropic-messages` — keeps tools and sends tool_choice none
- `gemini` — keeps tools and sends function calling mode NONE
- `empty-tool-choice` — ${route.id} preserves ${choice.type} tool choice with active tools (×8)
- `empty-tool-choice` — OpenAI Responses omits allowed tool choice without tool definitions

</details>


### Block lifecycle events (11 tests)

OpenCode emits start/delta/end events with block ids (separate reasoning blocks per summary part, text blocks split around tool calls). CodeSmith streams flat text/reasoning deltas, so these orderings have no equivalent.

_OpenAI Responses 8, Anthropic Messages 1, Gemini 2._

<details><summary>Tests</summary>

- `openai-responses` — routes reasoning summary events by output index
- `openai-responses` — routes native reasoning text deltas by output index
- `openai-responses` — closes sequential output messages before starting the next
- `openai-responses` — streams each reasoning summary part as a separate block
- `openai-responses` — concludes reasoning at implicit summary boundaries
- `openai-responses` — concludes text at implicit message boundaries
- `openai-responses` — opens the tool lifecycle for a done-only function call
- `openai-responses` — ignores duplicate item start events
- `anthropic-messages` — retains empty text blocks
- `gemini` — assigns unique ids to separated reasoning blocks
- `gemini` — separates text blocks around streamed tool calls

</details>


### Gemini stream resumption (11 tests)

Resuming a dropped Gemini stream from a continuation checkpoint, stitching the parts and counting usage once. CodeSmith retries the whole request instead.

_Gemini 11._

<details><summary>Tests</summary>

- `gemini` — stitches text split across three requests into one block
- `gemini` — finishes with a signed tool call from a later request
- `gemini` — forgets a block once the resumed request finishes
- `gemini` — resumes a dropped stream from its latest checkpoint
- `gemini` — counts the usage of a dropped request once resumed
- `gemini` — continues a stream that ends at a checkpoint without finishing
- `gemini` — fails a stream dropped before any checkpoint
- `gemini` — does not resume a response cut off by its own timeout
- `gemini` — fails a continuation without a token
- `gemini` — does not continue a length finish
- `gemini` — reports the first request's input with all output

</details>


### Image, PDF and video parts (7 tests)

Media in user messages and tool results is supported (see "Fixes made while
porting"). What is left: assistant-authored media (generated images replayed),
per-part provider options (Anthropic `file_id`, citations, transformations),
and video beyond URLs and inline parts on Chat, Responses and Gemini.

_OpenAI Responses 2, Anthropic Messages 4, Gemini 1._

<details><summary>Tests</summary>

- `openai-responses` — lowers Responses video by provider and preserves ordered tool results
- `openai-responses` — replays assistant media parts as portable user image input
- `anthropic-messages` — lowers per-part Anthropic media options from provider metadata
- `anthropic-messages` — rejects invalid per-part Anthropic media options
- `anthropic-messages` — serializes Messages video inputs
- `anthropic-messages` — moves Meta tool result videos after consecutive results
- `gemini` — replays generated assistant media as model inline data

</details>


### Billed cost reporting (7 tests)

Provider-reported cost (`cost`, OpenCode allowance, GitHub Copilot nano-AIU billing) is not read into `Usage`.

_OpenAI Chat 3, OpenAI Responses 1, Anthropic Messages 2, Gemini 1._

<details><summary>Tests</summary>

- `openai-chat` — reports the cost the endpoint billed
- `openai-chat` — converts the GitHub Copilot billing beside usage to USD
- `openai-chat` — drops a malformed billed cost without failing the stream
- `openai-responses` — reports the cost the endpoint billed
- `anthropic-messages` — reports the cost the endpoint billed
- `anthropic-messages` — converts the GitHub Copilot billing on the message delta to USD
- `gemini` — reports the cost the endpoint billed

</details>


### Endpoint and auth configuration (7 tests)

Native query params (e.g. Azure `api-version`), Azure `api-key` auth, default auth from config. CodeSmith appends the path to `base_url` and sends bearer/x-api-key/x-goog-api-key.

_OpenAI Chat 3, OpenAI Responses 4._

<details><summary>Tests</summary>

- `openai-chat` — adds native query params to the Chat Completions URL
- `openai-chat` — uses Azure api-key header for static OpenAI Chat keys
- `openai-compatible-chat` — posts to the configured compatible endpoint and parses text usage
- `openai-responses` — adds native query params to the Responses URL
- `openai-responses` — uses Azure api-key header for static OpenAI Responses keys
- `openai-responses` — loads OpenAI default auth from Effect Config
- `openai-responses` — lets explicit auth override OpenAI default API key auth

</details>


### Normalized finish reasons (6 tests)

CodeSmith passes provider finish reasons through raw (they are informational; control flow looks only at tool calls). The common reason algebra (`end`→stop, `pause_turn`, `model_context_window_exceeded`→length, content-filter categories and refusal explanations) is not modelled.

_OpenAI Chat 3, Anthropic Messages 3._

<details><summary>Tests</summary>

- `openai-compatible-chat` — normalizes the end finish reason to stop
- `openai-compatible-chat` — preserves content-filter finishes in the common reason algebra
- `openai-compatible-chat` — names the content filter that blocked a response
- `anthropic-messages` — maps context-window truncation to length
- `anthropic-messages` — preserves pause_turn while normalizing it to stop
- `anthropic-messages` — carries a refusal's category and explanation on the content-filter finish

</details>


### Tool namespaces (4 tests)

Responses tool namespaces (grouped tools) are not used.

_OpenAI Responses 4._

<details><summary>Tests</summary>

- `openai-responses` — lowers tool namespaces without flattening leaf names
- `openai-responses` — flattens nested levels within a native tool namespace
- `openai-responses` — defaults tool namespace descriptions
- `openai-responses` — preserves tool namespaces through streaming and history replay

</details>


### Usage details (3 tests)

Separate reasoning-token and total counts, raw usage in provider metadata.

_OpenAI Chat 2, Gemini 1._

<details><summary>Tests</summary>

- `openai-chat` — uses the configured provider metadata namespace for reasoning and usage
- `openai-chat` — falls back to the selected provider for the metadata namespace
- `gemini` — leaves total usage undefined when component counts are missing

</details>


### Per-model compatibility overrides (2 tests)

Explicit overrides of detected behaviour (`maxTokensField`, `streamUsage: false`, `requireReasoning`, `sanitizer`, `requireAssistantAfterTool`). CodeSmith detects from the endpoint and model id only.

_OpenAI Chat 2._

<details><summary>Tests</summary>

- `openai-chat` — omits stream usage options when compatibility disables them
- `openai-compatible-chat` — configures the max tokens request field

</details>


### Request body compression (2 tests)

gzip/deflate request bodies.

_Shared 2._

<details><summary>Tests</summary>

- `request-encoding` — compresses the JSON body with the requested encoding
- `request-encoding` — sends plain JSON when unset or none

</details>

## By design or not applicable


### Strict stream validation (16 tests)

OpenCode rejects malformed or out-of-spec events (missing item ids, wrong types, content after finish, nameless tool calls). CodeSmith is lenient on purpose: it ignores what it does not understand and keeps going.

_OpenAI Chat 4, OpenAI Responses 7, Anthropic Messages 4, Gemini 1._

<details><summary>Tests</summary>

- `openai-chat` — fails when a buffered tool call never receives a function name
- `openai-chat` — fails on malformed stream events
- `openai-compatible-chat` — rejects a stream without a required finish reason
- `openai-compatible-chat` — rejects tool changes after a terminal chunk
- `openai-responses` — rejects malformed refusal events
- `openai-responses` — rejects output text events without the spec-required item id
- `openai-responses` — requires item ids even when their output index is known
- `openai-responses` — ignores deltas without a matching output item
- `openai-responses` — rejects function argument events without the spec-required item id
- `openai-responses` — rejects reasoning events without the spec-required item id
- `openai-responses` — rejects a reasoning item that starts before the previous item ends
- `anthropic-messages` — rejects malformed recognized content block variants
- `anthropic-messages` — rejects malformed recognized content delta variants
- `anthropic-messages` — rejects malformed payloads on unrelated stream events
- `anthropic-messages` — rejects malformed recognized SSE events
- `gemini` — rejects malformed recognized response parts

</details>


### Not applicable: Effect Schema tool definitions (7 tests)

Tests of converting Effect `Schema.Struct` tool inputs to JSON Schema. CodeSmith tools are plain JSON Schema already.

_Shared 7._

<details><summary>Tests</summary>

- `tool-schema-projection` — only normalizes typed empty input structs, preserving raw schemas and output schemas
- `tool-schema-projection` — retains empty input descriptions and preserves named outputs and explicit raw schemas
- `tool-schema-projection` — normalizes inputs whose encoded side is an empty struct
- `tool-schema-projection` — ${scenario.route.id} prepares empty Effect Struct tools alongside nonempty and raw schemas (×4)

</details>


### Deliberate differences (2 tests)

Gemini prompt-safety blocks (`promptFeedback.blockReason` without candidates) are an error in CodeSmith (the user sees why), not a content-filter finish; `OpenResponses.lowerConversation` is an OpenCode compaction helper with no counterpart.

_Gemini 1, Shared 1._

<details><summary>Tests</summary>

- `gemini` — preserves candidate-less prompt safety blocks as content-filter outcomes
- `conversation-lowering` — conversation lowering excludes generation settings and tool definitions

</details>


### Not applicable: lone surrogates (2 tests)

Rust strings are always valid UTF-8, so the surrogate sanitizer has nothing to do.

_Shared 2._

<details><summary>Tests</summary>

- `sanitize` — replaces lone surrogates in nested message text, tool input, and tool output
- `sanitize` — keeps clean requests equal and shares unchanged nested values

</details>
