# Configuration

CodeSmith works with no config file at all for the hosted providers that read
a key from the environment, but most setups want at least a default `model`.
This page covers where config lives, how files combine, and every key.

- [Files and precedence](#files-and-precedence)
- [Substitution: `{env:}` and `{file:}`](#substitution-env-and-file)
- [Key reference](#key-reference)
- [Providers and models](#providers-and-models)
- [Instructions](#instructions)
- [Media limits](#media-limits)
- [Compaction](#compaction)
- [Tool output](#tool-output)
- [Web search](#web-search)
- [Environment variables](#environment-variables)

## Files and precedence

CodeSmith reads `codesmith.json` or `codesmith.jsonc`. Both accept comments
and trailing commas. Files load in this order, and later files win:

1. the global config, `~/.config/codesmith/codesmith.json[c]`
   (`$CODESMITH_CONFIG_DIR`, else `$XDG_CONFIG_HOME/codesmith`)
2. the file named by `$CODESMITH_CONFIG`
3. each folder's `codesmith.json[c]`, from the project root (the nearest
   folder with `.git`) down to the working directory
4. then each folder's `.codesmith/codesmith.json[c]`, in the same order
5. inline JSON in `$CODESMITH_CONFIG_CONTENT`

Set `$CODESMITH_DISABLE_PROJECT_CONFIG` to skip steps 3 and 4.

Objects merge key by key. `permissions` and `instructions` are the exception:
those lists are appended, so a later file's rules come after an earlier
file's, and so override them (see [permissions](permissions.md)). Other lists
are replaced. If a file does not parse, CodeSmith stops with an error that
names the file. It does not skip the file, because skipping it could silently
drop a `deny` rule.

`codesmith debug config` prints the merged config and the files it came from.

> **Project config is trusted.** As in OpenCode, a `codesmith.json` or
> `.codesmith/` folder inside a repository is applied as if you had written
> it. It can start MCP servers (which run programs), choose the shell, turn on
> the repository's own formatters and language servers, point a provider at
> any URL and send it values from `{env:}` or `{file:}`, and add permission
> rules that come after your global ones and so override them. Before you run
> CodeSmith in a repository you don't trust, read its `codesmith.json[c]` and
> `.codesmith/` first, or run with `CODESMITH_DISABLE_PROJECT_CONFIG=1`.

## Substitution: `{env:}` and `{file:}`

`{env:VAR}` and `{file:path}` are replaced anywhere in a file's text before it
is parsed:

- `{env:VAR}` becomes the variable's value, inserted raw, so
  `"snapshots": {env:FLAG}` works. If inserting it raw breaks the JSON (a
  value with quotes or backslashes, for example), it is inserted as escaped
  string content instead. An unset variable becomes empty.
- `{file:path}` becomes the file's trimmed contents, always escaped as string
  content. A relative path is relative to the config file's folder, and `~/`
  is your home folder. A `{file:}` on a `//` comment line is left alone.

```jsonc
{ "providers": { "anthropic": { "settings": { "apiKey": "{file:~/.secrets/anthropic}" } } } }
```

## Key reference

| Key | Default | What it does |
|---|---|---|
| `$schema` | | Ignored; allowed for editors. |
| `model` | none | Default model as `provider/model`. Required unless you pass `-m`. |
| `default_agent` | `build` | Primary agent for new sessions. |
| `shell` | `$SHELL`, else `/bin/sh` | Shell the `shell` tool runs commands with (`<shell> -c`). |
| `providers` | built-ins | Providers and their models. See [Providers and models](#providers-and-models). |
| `agents` | built-ins | Add or override agents. See [agents](agents-and-subagents.md#configuring-agents). |
| `permissions` | see [defaults](permissions.md#defaults) | Ordered rules applied to every agent. They can't widen a built-in agent's own restrictions, and `agents.<id>.permissions` override them. See [where rules come from](permissions.md#where-rules-come-from). |
| `instructions` | `[]` | Extra instruction files or globs. See [Instructions](#instructions). |
| `skills` | `[]` | Extra folders of skills (relative to the project root). See [skills](skills-and-commands.md#skills). |
| `commands` | `{}` | Inline commands: `{"name": {"template": "...", "description", "agent", "model"}}`. See [commands](skills-and-commands.md#commands). |
| `snapshots` | `true` | Snapshot files at each turn so turns can be undone. See [undo](sessions.md#undo-and-redo). |
| `subagent_depth` | `1` | How deep subagents may nest. `1` lets the main session start subagents, but a subagent cannot start its own. |
| `tool_output` | `{"max_lines": 2000, "max_bytes": 51200}` | How much tool output goes to the model before it is truncated. See [Tool output](#tool-output). |
| `compaction` | auto | When to summarize old history. See [Compaction](#compaction). |
| `media` | see below | Limits that images are fitted to. See [Media limits](#media-limits). |
| `formatter` | off | `true`, or an object of overrides. See [formatters](language-servers-and-formatters.md#formatters). |
| `lsp` | off | `true`, or an object of overrides. See [language servers](language-servers-and-formatters.md#language-servers). |
| `mcp` | `{}` | `{"servers": {...}, "timeout": {...}}`. See [MCP](mcp.md). |
| `codemode` | `true` | `false` offers every MCP tool as a direct tool call instead of through [Code Mode](mcp.md#code-mode). |
| `websearch` | off | `{"url", "engines", "results"}`. See [Web search](#web-search). |
| `review` | see below | `/review` and `/calibrate` settings. See [code review](code-review.md#settings). |
| `worktree` | | `{"directory", "start"}`. See [worktrees](sessions.md#worktrees). |
| `automations` | `true` | `false` keeps this process from running automations. See [automations](automations.md). |
| `automation` | `{}` | `{"permissions": [...]}`, rules for unattended runs. See [automations](automations.md#permissions-for-unattended-runs). |
| `plugins` | all on | `{"<id>": {...options}}`, or `false` to turn a compiled-in plugin off. See [plugins](plugins.md). |

Unknown keys are ignored, so a config written for a newer CodeSmith still
loads.

## Providers and models

### Built-in providers

These work without a `providers` entry. Each reads its key from the
environment variables listed, from a key saved with `codesmith auth login`, or
from `settings.apiKey` in config.

| Id | Protocol | Key variables |
|---|---|---|
| `anthropic` | Anthropic Messages | `ANTHROPIC_API_KEY` |
| `openai` | OpenAI Responses | `OPENAI_API_KEY` |
| `google` | Gemini | `GEMINI_API_KEY`, `GOOGLE_GENERATIVE_AI_API_KEY`, `GOOGLE_API_KEY` |
| `azure` | OpenAI Responses | `AZURE_OPENAI_API_KEY` (needs `settings.resourceName` or `settings.baseURL`; see [Azure](#azure-openai)) |
| `openrouter` | OpenAI Chat | `OPENROUTER_API_KEY` |
| `groq` | OpenAI Chat | `GROQ_API_KEY` |
| `deepseek` | OpenAI Chat | `DEEPSEEK_API_KEY` |
| `mistral` | OpenAI Chat | `MISTRAL_API_KEY` |
| `xai` | OpenAI Chat | `XAI_API_KEY` |
| `together` | OpenAI Chat | `TOGETHER_API_KEY` |
| `fireworks` | OpenAI Chat | `FIREWORKS_API_KEY` |
| `cerebras` | OpenAI Chat | `CEREBRAS_API_KEY` |
| `ollama` | OpenAI Chat | none (`http://127.0.0.1:11434/v1`) |
| `lmstudio` | OpenAI Chat | none (`http://127.0.0.1:1234/v1`) |
| `llamacpp` | OpenAI Chat | none (`http://127.0.0.1:8080/v1`) |

A key is looked up in this order: `settings.apiKey`, then the `env`
variables in order, then the saved key. So config and environment variables
take precedence over `codesmith auth login`.

There is no model catalog. Any model id the provider accepts works, for
example `-m anthropic/claude-sonnet-4-5` or `-m ollama/qwen3-coder`. Configure
a model under its provider only when you want to set limits, prices or other
options. The model id is everything after the first `/`, so ids that contain
slashes work (`openrouter/qwen/qwen3-coder`).

### Adding or overriding a provider

```jsonc
{
  "model": "local/qwen3-coder",
  "providers": {
    "local": {
      "name": "My vLLM box",
      "protocol": "openai-chat",          // openai-chat (default), openai-responses, anthropic-messages, google-gemini
      "settings": { "baseURL": "http://127.0.0.1:8000/v1", "apiKey": "{env:LOCAL_API_KEY}" },
      "env": ["LOCAL_API_KEY"],           // variables that may hold the key, in order
      "headers": { "X-Team": "tools" },   // sent with every request
      "body": { "chat_template_kwargs": { "enable_thinking": false } },
      "models": {
        "qwen3-coder": {
          "id": "Qwen/Qwen3-Coder-30B-A3B-Instruct",   // id sent to the server, if not the key
          "name": "Qwen3 Coder",
          "limit": { "context": 32768, "output": 8192 },
          "temperature": 0.2,
          "body": { "top_k": 20 },
          "cost": { "input": 0, "output": 0 },
          "modalities": { "input": ["text"], "output": ["text"] }
        }
      }
    }
  }
}
```

Any OpenAI-compatible server (oMLX, vLLM, llama.cpp, Ollama, LM Studio,
OpenRouter, and others) works with `"protocol": "openai-chat"`. An entry with
the same id as a built-in provider overlays it: you can change only the
`baseURL`, for example, and keep the built-in key variables.

Provider fields:

| Field | Meaning |
|---|---|
| `name` | Display name. |
| `protocol` | `openai-chat` (the default for custom providers), `openai-responses`, `anthropic-messages` or `google-gemini`. |
| `settings.baseURL` | API base URL (`baseUrl` and `base_url` also work). Required for a custom provider. |
| `settings.apiKey` | The key. Use `{env:}` or `{file:}` rather than writing it in the file. |
| `env` | Environment variables that may hold the key, in order. |
| `headers` | Extra HTTP headers. |
| `settings.queryParams` | Query parameters added to every request URL, e.g. `{"api-version": "2026-01-01"}`. |
| `settings.apiKeyHeader` | Send the key as-is in this header instead of the protocol's own (`Authorization: Bearer`, `x-api-key`, `x-goog-api-key`). A configured `Authorization` header is then not sent. |
| `settings.encoding` | Compress request bodies: `gzip`, `zstd` or `none` (the default). Only for servers that accept a compressed body. |
| `settings.transport` | `websocket` or `http`: how an `openai-responses` provider carries a session's steps. See [Responses over WebSocket](#responses-over-websocket). |
| `compatibility` | [Compatibility overrides](#compatibility-overrides) for every model of this provider. |
| `body` | Extra request-body fields for every model of this provider, merged into each request. |
| `options` | [Provider options](#provider-options-and-variants) for every model of this provider. |
| `cache` | [Prompt caching](#prompt-caching) for every model of this provider (`settings.cache` also works). |
| `models` | Per-model settings, keyed by the id you use in `provider/model`. |

Model fields:

| Field | Meaning |
|---|---|
| `id` | The model id sent to the provider, when it differs from the key. |
| `name` | Display name. |
| `limit.context` | Context window in tokens. Turns on [compaction](#compaction) and the context share in the status line. |
| `limit.output` | Maximum output tokens per response. |
| `temperature` | Sampling temperature. |
| `body` | Extra request-body fields for this model, merged after the provider's `body`. |
| `options` | [Provider options](#provider-options-and-variants) for this model, merged over the provider's. |
| `variants` | Named sets of options, picked with `provider/model#name`. See [variants](#provider-options-and-variants). |
| `cost` | Dollars per million tokens: `input`, `output`, `cache_read`, `cache_write`. Used for the session cost and `codesmith stats --cost` when the provider does not report what it billed (see [Billed cost](#billed-cost)). |
| `modalities` | `{"input": [...], "output": [...]}`, as in OpenCode and models.dev. See [images and PDFs](tools.md#images-and-pdfs). |
| `attachment` | OpenCode's older flag: `false` means text input only. |
| `cache` | [Prompt caching](#prompt-caching) for this model, over the provider's (`options.cache` also works). |
| `compatibility` | [Compatibility overrides](#compatibility-overrides) for this model, over the provider's. |

`codesmith debug model provider/model[#variant]` shows what a model resolves
to: the URL, protocol, limits, extra body, the provider options sent with each
request, the selected variant, and every variant the model has with its
options. `codesmith models` lists providers and
configured models, and `codesmith models <provider>` asks that provider's API
which models it has.

### Azure OpenAI

The built-in `azure` provider takes the resource name or a base URL:

```jsonc
"providers": { "azure": {
  "settings": { "resourceName": "my-resource", "apiVersion": "v1" },
  "models": { "gpt-5": { "id": "my-gpt5-deployment" } }
} }
```

As in OpenCode, a `*.openai.azure.com` base URL (or the one built from
`resourceName`) gets the v1 API under `/openai/v1` with
`?api-version=v1` (`settings.apiVersion` changes it), and the key goes in the
`api-key` header. `settings.useDeploymentBasedUrls: true` uses
`/openai/deployments/<model>` instead. `settings.queryParams` adds or
overrides query parameters. A base URL on another host (a proxy) is used as
given, with only `queryParams`. With `"protocol": "anthropic-messages"`, the
resource's Claude endpoint (`https://<resource>.services.ai.azure.com/anthropic/v1`)
is used with the key in `x-api-key`. Set `"protocol": "openai-chat"` for Chat
Completions.

### Responses over WebSocket

As in OpenCode, an `openai-responses` provider can carry a session's steps
over the Responses WebSocket API instead of one HTTP request each. The
session opens one socket on its first step and reuses it for the steps that
follow; once a response completes, the next step sends only its new input
items (the tool results, a new user message) with `previous_response_id`,
which saves resending the whole conversation every step. A step that changes
anything else (the model, the tools, `tool_choice`, options) or rewrites
earlier history is sent in full on the same socket. Nothing is stored
server-side: requests still go with `store: false`.

`settings.transport` chooses it per provider:

```jsonc
"providers": { "openai": { "settings": { "transport": "http" } } }
```

Like OpenCode, the built-in `openai` provider, `xai` with
`"protocol": "openai-responses"`, and `azure` on its v1 API (a
`resourceName`, or a `*.openai.azure.com` base URL, with no other
`apiVersion` and no deployment URLs) default to `websocket`; every other
provider defaults to `http` and can opt in with `"transport": "websocket"`.
Only session steps use the socket; titles, compaction summaries and other
side requests stay on HTTP. `codesmith debug model` shows `transport: websocket` when
it applies.

The handshake carries the same URL (as `wss://`), query parameters and
headers as the HTTP request, plus `OpenAI-Beta: responses_websockets=2026-02-06`
for OpenAI unless the provider sets its own `OpenAI-Beta` header. A
connection is replaced after 55 minutes (24 for xAI) or when the URL or
headers change. A step falls back to HTTP when the socket cannot carry it:
a failed connect (the session then stays on HTTP), a send that never left,
or a request the provider closes as too large (code 1009, also for the rest
of the session). A socket that drops mid-response fails the step, which is
retried; after five such failures in a row the session stays on HTTP. When
the provider no longer has the previous response (`previous_response_not_found`,
a plain invalid request against an incremental send, or a connection limit),
the step is resent in full on a new socket. xAI continues only stored
responses, so with `store: false` its steps are always sent in full over the
reused socket.

### Compatibility overrides

CodeSmith detects what an OpenAI Chat or Anthropic-compatible server needs
from the provider id, its URL and the model id. `compatibility` on a provider
or a model (the model's wins) overrides it, with OpenCode's names:

| Field | Detected | Override |
|---|---|---|
| `maxTokensField` | `max_completion_tokens` on OpenAI and Azure, else `max_tokens` | `"max_tokens"` or `"max_completion_tokens"` |
| `streamUsage` | `true`: sends `stream_options.include_usage` | `false` for servers that reject it |
| `requireReasoning` | `true` on DeepSeek (the `deepseek` provider, its endpoint or a DeepSeek model) and with a `reasoningField`: every assistant message carries the reasoning field, empty when there was none | `true` or `false` |
| `reasoningField` | Reasoning is read from `reasoning_content`, `reasoning` or `reasoning_text` and sent back under the field it came in | A field name, e.g. `"reasoning"`: read first, and every assistant message's reasoning is written there (structured `reasoning_details` from earlier turns are not sent unless the field is `reasoning_details`). `role`, `content`, `refusal` and `tool_calls` are rejected. |
| `requireFinishReason` | `true`: an OpenAI Chat stream that ends without a `finish_reason` was cut short, and the step is retried | `false` for servers that end streams without one; the finish is then inferred |
| `requireAssistantAfterTool` | `true` on Mistral-family models: an assistant turn between tool results and user text | `true` or `false` |
| `requireSignature` | Anthropic Messages: `true`, unsigned reasoning is dropped; `false` on Kimi, Moonshot, MiMo, Qwen and MiniMax servers, which take it with an empty signature | `true` or `false` |

```jsonc
"models": { "my-model": { "compatibility": { "maxTokensField": "max_completion_tokens", "streamUsage": false } } }
```

### Billed cost

When a provider reports what it charged for a response, CodeSmith uses that
instead of pricing the tokens with `cost`, as OpenCode does: `usage.cost`
(OpenRouter, LiteLLM, OpenCode Zen and other gateways) and GitHub Copilot's
`copilot_usage.total_nano_aiu` credits (converted at $0.01 a credit). Steps
without a reported cost are priced from the model's `cost` as before, so the
session cost in the status line and `codesmith stats --cost` can mix both.

### Provider options and variants

Reasoning effort and other provider options go in a provider's or a model's
`options`, using OpenCode's provider option names (camelCase or snake_case):
`reasoningEffort`, `reasoningSummary`, `textVerbosity`, `serviceTier`,
`include` (Responses), `thinking` and `effort` (Anthropic), `thinkingConfig`
(Gemini), `reasoning` (OpenRouter) and more. Each protocol sends the ones it
knows and ignores the rest.

Named `variants` bundle options under a name, picked with
`provider/model#name` anywhere a model is chosen: config, `-m`, `/model` in
the UI, or ACP model selection. `/variant` cycles through a model's variants
in the UI, and editors over ACP show them as an Effort option (see
[editors](editors-acp.md)). Common models also have built-in variants, such as
`anthropic/claude-sonnet-4-5#high`, `openai/gpt-5#low` and
`google/gemini-3-pro-preview#high`.

Switching between effort variants (`none`, `minimal`, `low`, `medium`,
`high`, `xhigh`, `max`, or back to the default) of the same model in a
running session stores an effort update in the conversation, as OpenCode
does. Models that take it in place (Claude Opus 5, Claude 5.1 and later,
GPT-6 and later on Responses) keep the effort the conversation started with
at the top of the request, so its prompt cache survives the switch; other
models get the new effort for the whole request.

```jsonc
"models": { "gpt-5": {
  "options": { "textVerbosity": "low" },
  "variants": { "deep": { "reasoningEffort": "high" }, "fast": { "serviceTier": "priority" } }
} }
```

Reasoning is sent back with what each provider needs to accept it
(Anthropic thinking signatures and redacted thinking, Responses encrypted
reasoning, Gemini thought signatures), so thinking models keep their
reasoning across tool calls. On Responses, replies also keep their message
item ids and phases (`commentary`, `final_answer`) and tool calls their item
ids, which go back with the history as OpenCode sends them. Sessions that
contain signed reasoning or these ids can't be opened by CodeSmith builds
older than these features.

CodeSmith's own instructions to the model in the middle of a conversation
(carry on after a compaction, answer in text on the last allowed step) are
system updates, sent as OpenCode does: native `system` messages on Claude
Opus 4.8, Claude 5 and later on Anthropic Messages, `developer` messages on
Responses, and `<system-update>` text in a user message elsewhere. Set
`"systemUpdates": true` (or `false`) in a model's `options` to override the
Anthropic Messages detection, for example for a gateway's model alias.

### Hosted tools

Some providers run tools themselves: web search, code execution, image
generation. Turn them on with a `hostedTools` list in a provider's or a
model's `options`. Each entry is a short name or the provider's own tool
declaration (camelCase keys are converted):

| Protocol | Short names | Declarations |
|---|---|---|
| OpenAI Responses | `web_search`, `web_search_preview`, `file_search`, `code_interpreter`, `image_generation` | any `{"type": …}`, e.g. `{"type": "image_generation", "quality": "high", "size": "1024x1024"}` |
| Anthropic Messages | `web_search`, `web_fetch`, `code_execution` | `{"type": "web_search_20250305", "name": "web_search", "maxUses": 5}` |
| Gemini | `google_search`, `code_execution`, `url_context` | a tool object, e.g. `{"googleSearch": {}}` |

```jsonc
"models": {
  "gpt-5": { "options": { "hostedTools": ["web_search", { "type": "image_generation", "quality": "high" }] } },
  "claude-sonnet-4-5": { "options": { "hostedTools": ["web_search", "web_fetch"] } }
}
```

The provider runs these tools; CodeSmith never does. A hosted call shows up
as a tool row that settles when its result arrives (generated images are
shown like tool images), it does not start another step, and it is not
subject to permissions. Its call and result are stored with the session and
sent back the way the provider that ran it expects: Responses items,
Anthropic `server_tool_use` and result blocks, Gemini `executableCode` and
`codeExecutionResult` parts. After switching models, a Responses model sees
results from other providers as user content (generated images as images),
and other protocols leave them out. Anthropic's beta headers for
`web_fetch` and `code_execution` are added for you, and naming a
Responses hosted tool in a tool choice (`image_generation`) forces it. OpenAI
Chat has no hosted tools; the option is ignored there. Invalid declarations
(an unknown short name, bad image generation options) fail before the
request is sent. Sessions with hosted calls can't be opened by CodeSmith
builds older than this feature.

### Prompt caching

Prompt caching is on by default, as in OpenCode, so a long session pays the
cached-input price for the prefix it has already sent:

- **Anthropic Messages** (Anthropic, and Anthropic-format endpoints such as
  Moonshot, MiniMax, Z.ai and DashScope): `cache_control` breakpoints
  on the last tool definition, the system prompt and the last message, at
  most four per request. The breakpoint on the last message moves forward
  each step, so every tool loop step reads the previous one from the cache.
- **OpenAI Chat gateways that take Anthropic's breakpoints**: OpenRouter for
  `anthropic/...` models (and `qwen/...`, without the tool breakpoint),
  DigitalOcean, and DashScope Qwen models. Any other Chat server gets them
  when the model's options declare `"cacheFormat": "anthropic"` (or
  `"alibaba"`, which leaves tools out).
- **OpenAI Responses**: a `prompt_cache_key` that stays the same for the
  session (its id), so requests go to the same cache.
- **OpenAI Chat**: the same `prompt_cache_key` where the server takes it
  (OpenAI, Azure, OpenRouter, Cerebras, DeepInfra, DigitalOcean, Fireworks,
  Moonshot, Venice). xAI gets it as the `x-grok-conv-id` header instead.
  Option `"setCacheKey": true` sends it to any other Chat server, and
  `"setCacheKey": false` stops it on Responses servers that reject it.
- **Gemini** caches repeated prefixes on its own; an explicit cache goes in
  the `cachedContent` option.

The `cache` setting on a provider or a model changes this (a model's wins):

```jsonc
"providers": {
  "anthropic": {
    "cache": { "ttl": "1h" },            // longer-lived cache entries
    "models": { "claude-haiku-4-5": { "cache": false } }   // no caching for this model
  },
  "local": { "cache": false }            // a server that rejects cache fields
}
```

| Value | Meaning |
|---|---|
| `true`, `"auto"` | The default placement above. |
| `false`, `"none"` | No breakpoints, no `prompt_cache_key`, no lifetime. |
| `{"ttl": "1h"}` | The default placement with this cache lifetime: `5m` or `1h` on Anthropic's breakpoints; on OpenAI `24h` is sent as `prompt_cache_retention` and anything else as `prompt_cache_options.ttl`. |
| `{"tools": true, "system": true, "messages": 2}` | Exactly these breakpoints: the last tool, the system prompt, and the last 2 messages (`messages` also takes `{"tail": n}`, `"latest-user-message"` or `"latest-assistant"`). |

Variants can't change caching: the cache belongs to the model. Cache reads
and writes are counted separately in usage, and `cost.cache_read` and
`cost.cache_write` price them (writes cost more than plain input on
Anthropic, reads much less).

### Saved keys

`codesmith auth login <provider>` reads a key from the terminal without
echoing it, or from stdin, and saves it in the data folder's `auth.json`,
which only you can read. `codesmith auth list` shows where each provider's key
comes from, and `codesmith auth logout <provider>` removes a saved key.

## Instructions

Instruction files are added to the system prompt. They load in this order,
each file once:

1. the global `~/.config/codesmith/AGENTS.md`, else `~/.claude/CLAUDE.md`
2. `AGENTS.md` in each folder from the working directory up, nearest first,
   with `CLAUDE.md` as the fallback in each folder. The walk stops at your
   home folder when the working directory is inside it, else at the project
   root.
3. each entry of `instructions`, in order: a path or glob, with `~/` for
   your home folder. A relative entry is looked for in every folder from the
   working directory up to the project root.

```jsonc
{ "instructions": ["docs/STYLE.md", "~/notes/rust-conventions.md", "packages/*/CONVENTIONS.md"] }
```

Set `$CODESMITH_DISABLE_CLAUDE_CODE` to stop reading `CLAUDE.md` files and
`.claude/` folders for instructions, skills and commands.
`codesmith debug prompt` prints the full system prompt the build agent (or
the one named with `--agent`) would get in the current folder.

## Media limits

Images are fitted to these limits before they reach the model:

```jsonc
"media": { "image": { "auto_resize": true, "max_width": 2000, "max_height": 2000, "max_base64_bytes": 5242880 } }
```

With `auto_resize: false`, an image that is too large is dropped with a note
to the model instead of scaled down. See [images and
PDFs](tools.md#images-and-pdfs) for the rest of the media behaviour.

## Compaction

When a conversation nears the model's context window (`limit.context`), older
history is replaced by a structured summary, and the newest exchanges are kept
verbatim, so long sessions keep going. By default compaction starts when less
than 15% of the window (and at least 16,000 tokens) is left, and about 15,000
tokens of the newest turns are kept beside the summary.

```jsonc
{ "compaction": { "auto": true, "buffer": "20%", "keep": { "tokens": 15000 } } }
```

| Field | Default | Meaning |
|---|---|---|
| `auto` | `true` | `false` leaves compaction to `/compact`, even when the provider rejects a prompt as too long. |
| `buffer` | 15% (min 16,000) | Room kept free below the window: a number of tokens, or a share such as `"20%"`. |
| `keep.tokens` | `15000` | Tokens of recent history kept verbatim. |

Without `limit.context`, CodeSmith cannot tell when the window is nearly
full. Compaction then happens only when you run `/compact`, or when the
provider rejects a prompt as too long (unless `auto` is `false`).

## Tool output

```jsonc
{ "tool_output": { "max_lines": 2000, "max_bytes": 51200 } }
```

Output longer than this is truncated before the model sees it. The full output
is saved under the data folder's `tool-output/`, and the model is told where,
so it can read the rest when it needs to.

## Web search

The `websearch` tool uses your own [SearXNG](https://docs.searxng.org/)
instance, never a hosted search API. The tool is offered only when `url` is
set. Turn on SearXNG's JSON format in its `settings.yml`
(`search: formats: [html, json]`), then add:

```jsonc
{ "websearch": { "url": "http://localhost:8888", "results": 8, "engines": "duckduckgo,wikipedia" } }
```

`results` (default 8) caps the results returned to the model, and `engines`
(comma-separated) overrides the instance's default engines.

## Environment variables

| Variable | Effect |
|---|---|
| `CODESMITH_CONFIG` | An extra config file, loaded after the global one. |
| `CODESMITH_CONFIG_CONTENT` | Inline JSON config, loaded last. |
| `CODESMITH_CONFIG_DIR` | The global config folder (default `$XDG_CONFIG_HOME/codesmith`, else `~/.config/codesmith`). |
| `CODESMITH_DISABLE_PROJECT_CONFIG` | Ignore `codesmith.json[c]` and `.codesmith/` in the project. |
| `CODESMITH_DISABLE_CLAUDE_CODE` | Don't read `CLAUDE.md` or `.claude/` skills and commands. |
| `CODESMITH_DISABLE_CLAUDE_CODE_SKILLS` | Don't read `.claude/skills` only. |
| `CODESMITH_DB` | The session database (default `<data>/codesmith.db`). |
| `XDG_DATA_HOME` | The data folder is `$XDG_DATA_HOME/codesmith`, else `~/.local/share/codesmith`. |
| `CODESMITH_SERVER_TOKEN` | The bearer token for [`codesmith serve`](local-server.md). |

CodeSmith sets `CODESMITH_SESSION_ID` for commands the `shell` tool runs, and
`CODESMITH_WORKTREE_BASE` and `CODESMITH_WORKTREE_PATH` for a worktree's
`start` command.

`codesmith debug paths` shows the config, data, tool-output and scratch
folders in use.
