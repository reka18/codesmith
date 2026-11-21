# Configuration

CodeSmith works with no config file at all for the hosted providers that read
a key from the environment, but most setups want at least a default `model`.
This page covers where config lives, how files combine, and every key.

- [Files and precedence](#files-and-precedence)
- [Substitution: `{env:}` and `{file:}`](#substitution-env-and-file)
- [Key reference](#key-reference)
- [Providers and models](#providers-and-models)
- [Session titles](#session-titles)
- [Instructions](#instructions)
- [References](#references)
- [Media limits](#media-limits)
- [Compaction](#compaction)
- [Prompt-cache warming](#prompt-cache-warming)
- [Tool output](#tool-output)
- [Web search](#web-search)
- [The terminal UI: `tui`](#the-terminal-ui-tui)
- [File watching](#file-watching)
- [Environment variables](#environment-variables)

## Files and precedence

CodeSmith reads `codesmith.json` or `codesmith.jsonc`. Both accept comments
and trailing commas. Files load in this order, and later files win:

1. the global config, `~/.config/codesmith/codesmith.json[c]`
   (`$CODESMITH_CONFIG_DIR`, else `$XDG_CONFIG_HOME/codesmith`)
2. the file named by `$CODESMITH_CONFIG`
3. each folder's `codesmith.json[c]`, from the project root (the nearest
   folder with `.git` or a Mercurial `.hg`, Git winning in a folder with
   both) down to the working directory
4. then each folder's `.codesmith/codesmith.json[c]`, in the same order
5. inline JSON in `$CODESMITH_CONFIG_CONTENT`

Set `$CODESMITH_DISABLE_PROJECT_CONFIG` to skip steps 3 and 4, and
`$CODESMITH_DISABLE_GLOBAL_CONFIG` to skip step 1 together with every
other global source: the global folder's agents, commands, skills and
`AGENTS.md`, and the home folder's `.claude` and `.agents` folders (and
`AGENTS.md`), even when the project walk passes through the global or home
folder, as OpenCode's `global: false` does. Steps 2 and 5 still load.

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
| `model` | none | Default model as `provider/model` (or `provider/model#variant`), or `{"providerID": "...", "model": "...", "variant": "..."}`. Required unless you pass `-m`. |
| `default_agent` | `build` | Primary agent for new sessions. |
| `small_model` | the session's model | The model that writes session titles, as `provider/model[#variant]`. See [Session titles](#session-titles). |
| `title` | `true` | `false` names sessions after their first line, with no model request. See [Session titles](#session-titles). |
| `shell` | `$SHELL`, else `/bin/zsh` on macOS and Bash (else `/bin/sh`) elsewhere | Shell the `shell` tool runs commands with (`<shell> -c`) and terminals start. A bare name is looked up on `PATH`; a shell that cannot be found falls back to the default, and so do Fish and Nushell for the `shell` tool, which needs POSIX `-c` scripts (terminals still start them). |
| `providers` | built-ins | Providers and their models. See [Providers and models](#providers-and-models). |
| `policies` | `[]` | Deny-only `tool.use` statements over the rules; an earlier file has the final say. See [policies](permissions.md#policies). |
| `agents` | built-ins | Add or override agents. See [agents](agents-and-subagents.md#configuring-agents). |
| `permissions` | see [defaults](permissions.md#defaults) | Ordered rules applied to every agent. They can't widen a built-in agent's own restrictions, and `agents.<id>.permissions` override them. See [where rules come from](permissions.md#where-rules-come-from). |
| `instructions` | `[]` | Extra instruction files or globs. See [Instructions](#instructions). |
| `references` | `{}` | Other folders or Git repositories the agent may consult, by name. See [References](#references). |
| `skills` | `[]` | Extra folders of skills (relative to the project root). See [skills](skills-and-commands.md#skills). |
| `builtin_skills` | `false` | Offer the skills shipped with CodeSmith (`codesmith`, `report`). See [built-in skills](skills-and-commands.md#built-in-skills). |
| `commands` | `{}` | Inline commands: `{"name": {"template": "...", "description", "agent", "model"}}`. See [commands](skills-and-commands.md#commands). |
| `snapshots` | `true` | Snapshot files at each turn so turns can be undone. See [undo](sessions.md#undo-and-redo). |
| `subagent_depth` | `1` | How deep subagents may nest. `1` lets the main session start subagents, but a subagent cannot start its own. |
| `tool_output` | `{"max_lines": 2000, "max_bytes": 51200}` | How much tool output goes to the model before it is truncated. See [Tool output](#tool-output). |
| `compaction` | auto | When to summarize old history. See [Compaction](#compaction). |
| `warming` | off | Keep-alive requests that keep an idle session's prompt cache warm. See [Prompt-cache warming](#prompt-cache-warming). |
| `experimental` | `{}` | `image_generate` and `video_generate`: `false` turns that tool off, `{"model": "provider/model"}` names the model it prefers. Video generation is off unless set. See [generating images and videos](tools.md#generating-images-and-videos). |
| `media` | see below | Limits that images are fitted to, and whether media goes to the blob store. See [Media limits](#media-limits). |
| `formatter` | off | `true`, or an object of overrides. See [formatters](language-servers-and-formatters.md#formatters). |
| `lsp` | off | `true`, or an object of overrides. See [language servers](language-servers-and-formatters.md#language-servers). |
| `mcp` | `{}` | `{"servers": {...}, "timeout": {...}}`. See [MCP](mcp.md). |
| `codemode` | `true` | `false` offers every MCP tool as a direct tool call instead of through [Code Mode](mcp.md#code-mode). |
| `full_codemode` | `false` | Hide glob, grep and webfetch (programs use the [`fetch`](mcp.md#fetch) global), and let GPT and Claude models call the other built-ins from inside `execute`. See [built-in tools in Code Mode](mcp.md#built-in-tools-in-code-mode-full_codemode). |
| `openapi` | `{}` | `{"<name>": {"spec", "base_url", "auth", "headers", "timeout", "enabled"}}`: OpenAPI specs whose operations are Code Mode tools. See [OpenAPI tools](mcp.md#openapi-tools). |
| `websearch` | off | `{"url", "engines", "results"}`. See [Web search](#web-search). |
| `review` | see below | `/review` and `/calibrate` settings. See [code review](code-review.md#settings). |
| `worktree` | | `{"directory", "start"}`. See [worktrees](sessions.md#worktrees). |
| `automations` | `true` | `false` keeps this process from running automations. See [automations](automations.md). |
| `automation` | `{}` | `{"permissions": [...]}`, rules for unattended runs. See [automations](automations.md#permissions-for-unattended-runs). |
| `catalog` | no fetching | `{"fetch": true, "url": "..."}` opts in to fetching the models.dev catalog. See [the model catalog](#the-model-catalog-modelsdev). |
| `update` | `"disable"` | Global config only: `"notify"` checks for a new release once a day and says so after the UI exits. See [update checks](cli.md#update-checks). |
| `plugins` | all on | `{"<id>": {...options}}`, or `false` to turn a compiled-in plugin off. See [plugins](plugins.md). |
| `tui` | `{}` | The terminal UI: theme, key bindings, cursor, reasoning display, transcript verbosity and session tabs. See [The terminal UI](#the-terminal-ui-tui). |
| `watcher` | `{}` | `{"ignore": [...]}`: paths or globs, relative to the project, that are not watched. See [File watching](#file-watching). |

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
| `azure` | OpenAI Responses | `AZURE_OPENAI_API_KEY`, `AZURE_API_KEY`, `AZURE_COGNITIVE_SERVICES_API_KEY`, or an Azure CLI login (needs the resource name or `settings.baseURL`; see [Azure](#azure-openai)) |
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
| `vllm` | OpenAI Chat | none (`http://127.0.0.1:8000/v1`) |
| `amazon-bedrock` | Bedrock Converse (Mantle models: Responses, Chat or Messages) | `AWS_BEARER_TOKEN_BEDROCK`, else SigV4 with AWS credentials; see [Amazon Bedrock](#amazon-bedrock) |
| `google-vertex` | Gemini, Anthropic Messages for Claude, OpenAI Chat for partner models | Application Default Credentials, or `GOOGLE_VERTEX_API_KEY` (Express Mode); see [Google Vertex AI](#google-vertex-ai) |
| `google-vertex-anthropic` | Anthropic Messages | as `google-vertex` |
| `github-copilot` | per model: Anthropic Messages, OpenAI Responses or Chat | `codesmith auth login github-copilot`, or `COPILOT_GITHUB_TOKEN`; see [GitHub Copilot](#github-copilot) |
| `databricks` | OpenAI Chat | `DATABRICKS_TOKEN` (and `DATABRICKS_HOST`, with or without `https://`) |
| `cloudflare-ai-gateway` | per model: OpenAI Responses, Anthropic Messages or OpenAI Chat | `CLOUDFLARE_API_TOKEN`, `CF_AIG_TOKEN` (and the account and gateway ids); see [Cloudflare](#cloudflare-ai-gateway-and-workers-ai) |
| `cloudflare-workers-ai` | OpenAI Chat | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_WORKERS_AI_TOKEN`, `CLOUDFLARE_API_TOKEN` (and the account id) |
| `poe` | OpenAI Chat | `POE_API_KEY`, or `codesmith auth login poe --oauth`; see [Poe](#poe) |
| `digitalocean` | per model: OpenAI Chat, Responses or Anthropic Messages | `DIGITALOCEAN_ACCESS_TOKEN`, or `codesmith auth login digitalocean --oauth`; see [DigitalOcean](#digitalocean) |
| `snowflake-cortex` | per model: OpenAI Chat or Anthropic Messages | `SNOWFLAKE_CORTEX_TOKEN`, `SNOWFLAKE_CORTEX_PAT` (and `SNOWFLAKE_ACCOUNT`), or a login; see [Snowflake Cortex](#snowflake-cortex) |
| `sap-ai-core` | OpenAI Chat | `AICORE_SERVICE_KEY` (the service key JSON) and the deployment; see [SAP AI Core](#sap-ai-core) |
| `claude-agent` | your installed Claude Code | none: Claude Code's own login; see [Claude Agent](#claude-agent-your-claude-code-login) |

A key is looked up in this order: `settings.apiKey`, then the `env`
variables in order, then the saved key. So config and environment variables
take precedence over `codesmith auth login`.

Any model id the provider accepts works, for example
`-m anthropic/claude-sonnet-4-5` or `-m ollama/qwen3-coder`. What a model can
do, its limits and its prices come from the [model catalog](#the-model-catalog-modelsdev)
when it lists the model; configure a model under its provider only to set or
override them, or for a model the catalog doesn't know. The model id is
everything after the first `/`, so ids that contain slashes work
(`openrouter/qwen/qwen3-coder`).

### The model catalog (models.dev)

As in OpenCode, CodeSmith knows the [models.dev](https://models.dev) catalog:
every provider it lists, with each model's input and output modalities, tool
support, reasoning controls, context and output limits, and prices. A copy is
bundled with the binary, so this works offline and with no network request at
all. A model resolves from it like this:

- **Limits**: `limit.context` and `limit.output` come from the catalog unless
  configured, so compaction knows the context window of any listed model. The
  output limit asked for is the catalog's, capped at 256k tokens.
- **Costs**: the catalog's default price is used for the session cost and
  `codesmith stats` unless the model has a `cost` (context tiers above the
  default are kept in the catalog but not applied).
- **Modalities**: the catalog's input modalities decide whether images, PDFs,
  audio and video are sent, unless `modalities`, `attachment` or
  `capabilities.input` say otherwise.
- **Variants**: the reasoning variants (`provider/model#high`) are generated
  from the catalog's reasoning controls (its effort levels, thinking toggle or
  budget), spelled for the model's API. A model moved to another `protocol`
  by config gets them respelled; configured `variants` merge into generated
  ones of the same name.
- **Request extras**: the catalog's per-model base URL (when the catalog
  speaks the model's protocol there), reasoning field and token-limit field
  apply unless configured. Its *modes* are models of their own
  (`openai/gpt-5-fast`, for example), sent as the base model with the mode's
  body and headers.
- **Providers**: a catalog provider whose API CodeSmith speaks (OpenAI Chat
  or Responses, Anthropic Messages, Gemini) works without a `providers` entry,
  on that API and with the key variables the catalog lists (`-m
  moonshotai/kimi-k3` with `MOONSHOT_API_KEY` set, for example). `${VAR}` in a
  catalog URL is filled from the environment, but only for variables that
  say where a provider lives (names ending in `_ACCOUNT`, `_ACCOUNT_ID`,
  `_GATEWAY_ID`, `_PRODUCT_ID`, `_PROJECT`, `_PROJECT_ID`, `_LOCATION`,
  `_REGION`, `_HOST`, `_ENDPOINT`, `_RESOURCE_NAME`, `_DEPLOYMENT_URL`,
  `_BASE_URL` or `_WORKSPACE`, and none containing `KEY`, `TOKEN`, `SECRET`,
  `PASSWORD`, `CREDENTIAL` or `AUTH`); a URL naming any other variable is not
  used. A catalog URL must be `https` (`http` only to this machine).
- **Catalog key variables are limited.** A provider that only the catalog
  defines may not use a key variable a built-in provider reads
  (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `FIREWORKS_API_KEY`, ...) or one
  starting `AWS_`, `AZURE_`, `GOOGLE_`, `GEMINI_`, `ANTHROPIC_`, `OPENAI_`,
  `GITHUB_`, `GH_` or `CODESMITH_`: a fetched catalog chooses where that
  provider's requests go. To use one anyway, name it in the provider's `env`
  in config (`"providers": {"fireworks-ai": {"env": ["FIREWORKS_API_KEY"]}}`),
  or use the built-in provider.
- **Configured providers keep their protocol.** An entry under `providers`
  with the same id as a catalog provider never takes the catalog's protocol:
  it uses its own `protocol`, `openai-chat` when unset, as any custom
  provider does. The catalog still supplies its name, key variables and
  model metadata, and its base URL only when the catalog speaks that same
  protocol there.

Configured values always win, field by field. Deprecated models, chat models
that don't take or make text, and models of other types (image, video,
decision) are not offered for sessions.

**Where it comes from.** Without fetching (the default), the catalog is the
file named by `$CODESMITH_MODELS_PATH` (a models.dev v1 or v2 catalog) if
set, else the copy cached by an earlier fetch in
`$XDG_CACHE_HOME/codesmith/models/` (else `~/.cache/codesmith/models/`),
else the bundled snapshot. Reading it never waits for the network.

**Fetching (opt-in).** CodeSmith fetches the catalog only when asked to:

- `codesmith models --refresh` fetches it once into the cache, then lists
  the models;
- the `catalog` config key, `{"fetch": true}` (optionally with `"url"`), or
  `CODESMITH_MODELS_FETCH=1`, turns on the background refresh: the
  interactive UI, `codesmith run`, `serve` and `acp` then refresh the cache
  when it is more than five minutes old, and every five minutes while they
  run, as OpenCode's server does. Naming a server (`catalog.url` or
  `CODESMITH_MODELS_URL`) turns it on too, unless `fetch` is `false`.

```jsonc
{ "catalog": { "fetch": true } }   // from https://models.dev/api.json
```

The default server is the public models.dev catalog, read as its v1
`/api.json`. A mirror is asked for the v2 API
(`/experimental/v2.0/api.json`) first and its `/api.json` if it has none.
A refresh revalidates with the cached ETag; an unchanged catalog only records
the check, and a failed one keeps the cached copy. A fetched catalog is
cached only when it is at most 32 MiB and reads in full; a cached copy that
no longer reads is dropped for the bundled snapshot. A fetch (retries included)
gives up after ten seconds. The request sends a generic `User-Agent`
(`codesmith/<version>`) and nothing about you, your project or your config.
`CODESMITH_DISABLE_MODELS_FETCH=1` forbids every fetch, `--refresh` included.

| Setting | Effect |
|---|---|
| `catalog.fetch` | `true` turns the background refresh on; `false` keeps it off even with a URL. Default off. |
| `catalog.url` | The catalog server (default `https://models.dev`); setting it turns fetching on unless `fetch` is `false`. |
| `CODESMITH_MODELS_FETCH` | `1`/`true` or `0`/`false`, over `catalog.fetch`. |
| `CODESMITH_MODELS_URL` | The catalog server, over `catalog.url`; turns fetching on like it. |
| `CODESMITH_MODELS_PATH` | Read the catalog from this file instead of the cache (fetches still go to the cache). |
| `CODESMITH_DISABLE_MODELS_FETCH` | `1` or `true`: never fetch, whatever else says. |

`scripts/models-snapshot.py` rebuilds the bundled snapshot.

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
| `local` | Whether the server is your own and serves one request at a time. Unset, a loopback, private-network (including Tailscale's `100.64.0.0/10`), `.local`, `.lan`, `.internal`, `.home.arpa`, `.ts.net` or single-label host is local, and only a loopback one is asked for its models unprompted. Set, it decides both. See [Using a local server](#using-a-local-server-omlx-lm-studio-llamacpp-ollama). |
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
| `compaction` | How every model of this provider [compacts](#native-compaction): `{"type": "native"}` for the provider's own compaction. |
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
| `compaction` | `{"type": "native"}`: [the provider compacts](#native-compaction) this model's sessions. Overrides the provider's. |
| `compatibility` | [Compatibility overrides](#compatibility-overrides) for this model, over the provider's. |
| `capabilities` | OpenCode's capabilities, merged field by field over the catalog's: `input` and `output` modalities, `tools`, `temperature`, the `freeform` tool formats the model accepts (`["text", "lark"]`; see [freeform tools](tools.md#freeform-tools)), and `reasoning` (`{"supported": true, "effort": ["low", "high"]}`, `"toggle": true`, `"budget": {"min": 1024}`), which regenerates the reasoning variants. |
| `type` | `chat` (the default), or `image`, `video` or `decision`; only chat models are offered for sessions. Unset keeps the catalog's. |
| `interleaved` | OpenCode's older spelling of `compatibility.reasoningField`: the field name, or `{"field": name}`. |

Unset `limit`, `cost`, `modalities` and variants come from the
[model catalog](#the-model-catalog-modelsdev) when it lists the model.

`codesmith debug model provider/model[#variant]` shows what a model resolves
to: the URL, protocol, limits, extra body, the provider options sent with each
request, the selected variant, and every variant the model has with its
options. `codesmith models` lists the providers (built-in, configured, and
catalog providers you have a key for) with their configured and catalog chat
models, `codesmith models --refresh` fetches the model catalog first, and
`codesmith models <provider>` asks that provider's API which models it has.

### Using a local server (oMLX, LM Studio, llama.cpp, Ollama)

A server on your own machine or network usually runs one model and works
on one request at a time, and the slow part of a request is reading the
prompt (prefill). CodeSmith is built to keep that cost down:

- **Titles wait.** A session's title is normally written beside the first
  turn. When the title would go to a local server the turn also uses, it
  waits until the turn's first response is in, and finishes before the
  session's next request, so it never slows the reply you are waiting for.
  It is capped at 64 tokens and asks for no thinking (see
  [Session titles](#session-titles)).
- **Background summaries wait for idle time.** As a session nears
  [compaction](#compaction), its summary is normally written in the
  background, beside the turn. On a local server it starts only once the
  turn is over (and its title written), and when you send the next
  message before it is done, it is stopped before the first step goes
  out; the next idle time starts it again. A compaction that is due still
  waits for a summary being written rather than starting another.
- **The prompt prefix stays the same.** The system prompt, the tool
  definitions and everything before your first message are byte for byte
  the same on every step and every turn of a session, and across sessions
  of the same project and across days, so a server with a prefix cache
  reads them once. When instruction files, skills or MCP servers change
  during a session, the change is sent as an update after the history
  instead of rewriting the system prompt. The date is not in the system
  prompt (where hosted providers get it, as in OpenCode): many chat
  templates, Qwen's among them, put the tool definitions after the system
  text, so a date there would make the first session of each day read the
  tools again. It is sent as a short update right after that prefix,
  before the conversation, and a session that runs past midnight is told
  the new date as an update too.
- **No cache fields it doesn't need.** Local servers cache by prefix on
  their own; they get no `prompt_cache_key`, no cache lifetime and no
  breakpoints unless you ask for them.

Recommended settings:

- Set `limit.context` to the context the server is started with, so
  CodeSmith compacts before a request overflows it, and `limit.output` to
  what you need in one reply (a few thousand to 16k tokens): it is sent as
  `max_tokens` with each request.
- Thinking: Qwen 3 and GLM templates think unless told otherwise. To choose
  per session, give the model a `none` variant that turns thinking off and
  pick it with `-m provider/model#none` (or `/variant`). Titles use the
  `none` variant automatically when the model has one; without one, a
  Qwen 3 or GLM model on an OpenAI-compatible server (or any model with
  `chat_template_kwargs` in its `body`) gets
  `chat_template_kwargs.enable_thinking: false` for the title.
- With a second, small model loaded somewhere, set `small_model` to it; a
  title on another server runs beside the turn. Or set `"title": false`.
- Keep the prefix cached: turn on the server's prefix (or prompt) cache and
  give it room for the system prompt and tools (about 10k tokens with the
  built-in tools; MCP tools add more, so keep them in
  [Code Mode](mcp.md#code-mode), which offers them as one tool). Switching
  agents changes the system prompt and tools, so the first request after a
  switch reads them again. Edits to `AGENTS.md` or skills reach a running
  session as updates, and new sessions start from the new text.
- Leave [prompt-cache warming](#prompt-cache-warming) off: its keep-alive
  requests would compete with your turns.

An OpenAI-compatible server on another machine:

```jsonc
{
  "model": "box/qwen3.5",
  "providers": {
    "box": {
      "name": "Mac cluster",
      "protocol": "openai-chat",
      "settings": { "baseURL": "http://192.168.1.20:8000/v1" },
      "models": {
        "qwen3.5": {
          "id": "the-id-the-server-lists",              // `codesmith models box` shows them
          "limit": { "context": 131072, "output": 16384 },
          "variants": {
            "none": { "body": { "chat_template_kwargs": { "enable_thinking": false } } }
          }
        }
      }
    }
  }
}
```

`codesmith debug model box/qwen3.5` prints `local: yes` for a provider
treated as local. With `"discovery": "openai"` (and `"local": true` for a
server on another machine), `codesmith models` and the model picker
(`/model`) list what the server serves; the picker shows the list kept
from the last time at once and asks the server again in the background,
for at most three seconds, each time it opens (see
[Local model discovery](#local-model-discovery-ollama-lm-studio-vllm-llamacpp)).

CodeSmith asks two different questions of a server's address:

- **Is it a local, single-request server?** Then side requests (titles,
  background summaries) wait for the turn. Unset, a loopback or
  private-network address, a Tailscale address or `.ts.net` name, a
  `.local`, `.lan`, `.internal` or `.home.arpa` name, or a single-label
  host is.
- **May it be asked for its models without being asked?** `codesmith
  models` and the model picker list what a local provider with
  `settings.discovery` serves right now. Unset, only a server on this
  machine (`localhost`, `127.0.0.0/8`, `::1`) is contacted this way, so
  nothing on your network is contacted unless you ask
  (`codesmith models box` always asks).

`"local"` on a provider answers both. `"local": true` says the server is
yours: titles and summaries wait for the turn, and a server on another
machine (the box above, with `"discovery": "openai"`) is listed in
`codesmith models` and the model picker. `"local": false` says it serves
many requests at once (a vLLM cluster behind a gateway, a proxy to a
hosted service on `localhost`): side requests run beside the turn and its
models are listed only when asked.

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

The resource name comes from the saved login, else `settings.resourceName`,
else `AZURE_RESOURCE_NAME` (or the older
`AZURE_COGNITIVE_SERVICES_RESOURCE_NAME`), and fills `${AZURE_RESOURCE_NAME}`
in catalog and configured URLs. `codesmith auth login azure` asks for it
after the key unless config names it (`--field resourceName=…` answers
without asking).

**Azure CLI (Microsoft Entra ID).** `codesmith auth login azure --cli
[--field resourceName=…]` saves no secret: each request is signed with a
token from the Azure CLI's own sign-in (`az login`), for its audience
(`https://ai.azure.com` for Foundry endpoints on `*.services.ai.azure.com`,
Cognitive Services otherwise), fetched with `az account get-access-token`
and reused until five minutes before it expires. It needs `az` on `PATH`.
Responses then go over HTTP rather than WebSockets.

**Deployments.** `codesmith models azure` lists the resource's deployments
and keeps them (see [Live model lists](#live-model-lists-modal-github-copilot-azure)):
with an Azure CLI login through Azure Resource Manager (finding the resource
across your subscriptions with Resource Graph, every page of the list, and
never following a page link off `management.azure.com`), falling back to
the resource's own list, which is what a key reads. Each deployment becomes
a model named after it (`azure/prod-mini`), with the limits, prices and
routes of the catalog model it deploys; deployments of models the catalog
does not know need a `models` entry. A custom `settings.baseURL` keeps the
catalog's models.

### Amazon Bedrock

`amazon-bedrock` runs models on the Converse API
(`https://bedrock-runtime.<region>.amazonaws.com`), and models the catalog
lists only on Bedrock Mantle on its Responses, Chat or Messages API
(`https://bedrock-mantle.<region>.api.aws`), as OpenCode does.

- **Region**: `settings.region`, else `AWS_REGION`, else `AWS_DEFAULT_REGION`,
  else `us-east-1`. `settings.endpoint` (a private or VPC endpoint) becomes the
  base URL; `settings.baseURL` beats it.
- **A Bedrock API key** (`AWS_BEARER_TOKEN_BEDROCK`, `settings.apiKey`, or
  `codesmith auth login amazon-bedrock`) is sent as a bearer token and wins
  over AWS credentials.
- **AWS credentials** otherwise sign each request with SigV4, from the
  standard chain, read afresh for every request: `AWS_ACCESS_KEY_ID` /
  `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` (unless a profile is named),
  then the profile (`settings.profile`, a saved login, `AWS_PROFILE`, or
  `default`) in `~/.aws/credentials` and `~/.aws/config` (or
  `AWS_SHARED_CREDENTIALS_FILE` / `AWS_CONFIG_FILE`): static keys,
  `credential_process`, `web_identity_token_file`, and for SSO,
  `role_arn` and console-login profiles the AWS CLI
  (`aws configure export-credentials`), then `AWS_WEB_IDENTITY_TOKEN_FILE`
  with `AWS_ROLE_ARN`, then ECS/EKS container credentials. Credentials that
  say when they expire are reused until five minutes before then.
  CodeSmith does not query the EC2 instance metadata service.
- `settings.auth`: `"sigv4"` ignores an ambient `AWS_BEARER_TOKEN_BEDROCK`,
  `"bearer"` requires a key.
- `codesmith auth login amazon-bedrock --profile work` saves a profile to
  sign with (it also ignores an ambient bearer token).

Model ids go to Bedrock as given, so cross-region inference profiles
(`us.anthropic.claude-sonnet-4-5-20250929-v1:0`, `eu.`, `apac.`, `global.`)
and inference profile ARNs work. A model Bedrock serves only through a
profile answers its bare id with an error that names the profile ids to use.

```jsonc
"providers": { "amazon-bedrock": { "settings": { "region": "eu-west-1", "profile": "work" } } }
```

### Google Vertex AI

`google-vertex` runs Gemini models on `generateContent`, Claude models on
Anthropic Messages (`:streamRawPredict`), and the catalog's partner models
on Vertex's OpenAI-compatible endpoints. `google-vertex-anthropic` is the
older id for Claude on Vertex.

- **Project**: a saved login, `settings.project`, then
  `GOOGLE_VERTEX_PROJECT`, `GOOGLE_CLOUD_PROJECT`, `GCP_PROJECT`,
  `GCLOUD_PROJECT`.
- **Location**: a saved login, `settings.location`, then
  `GOOGLE_VERTEX_LOCATION`, `GOOGLE_CLOUD_LOCATION`, `VERTEX_LOCATION`, then
  `global`. `global` uses `aiplatform.googleapis.com`, `eu` and `us` their
  multi-region hosts (`aiplatform.eu.rep.googleapis.com`), and a region its
  own (`us-east5-aiplatform.googleapis.com`).
- **Credentials**: Application Default Credentials: the file
  `GOOGLE_APPLICATION_CREDENTIALS` names, else gcloud's
  `application_default_credentials.json` (under `CLOUDSDK_CONFIG` when set),
  else the metadata server on Google Cloud. Service-account keys and
  `gcloud auth application-default login` user credentials are exchanged
  for an access token by CodeSmith (and the user credentials' quota project
  is billed); other types (workload identity federation, impersonation) go
  through `gcloud auth application-default print-access-token`. Tokens are
  reused until a minute before they expire. `settings.accessToken` uses a
  token of your own instead.
- **Express Mode**: `GOOGLE_VERTEX_API_KEY` (or `settings.apiKey`) serves
  Gemini without a project, unless a project or location is configured.
- `codesmith auth login google-vertex --project my-project [--location eu]`
  saves the project and location.

### GitHub Copilot

`codesmith auth login github-copilot` signs in with GitHub's device flow
(`--enterprise company.ghe.com` for GitHub Enterprise), checks that the
account has Copilot, and saves the login. The device flow runs as a GitHub
OAuth app, and CodeSmith ships none of its own (nor borrows another
project's): register one at
[github.com/settings/applications/new](https://github.com/settings/applications/new)
(any homepage and callback URL) with **Enable Device Flow** ticked, and give
its client id with `--client-id`, `settings.clientId` on the
`github-copilot` provider, or `CODESMITH_COPILOT_CLIENT_ID`, in that order.
Without one the login stops and says so.

`COPILOT_GITHUB_TOKEN` works instead of a login (not `GITHUB_TOKEN`, which
git and CI usually set without Copilot access). Requests identify
themselves as `codesmith/<version>` in `User-Agent`; Copilot needs no
editor identification headers. Each model goes to the API Copilot serves it on: the catalog's
(Anthropic Messages, OpenAI Responses or Chat Completions), else, as in
OpenCode, Responses for GPT-5 and later (but GPT-5 mini), Grok and MAI Code,
and Chat for the rest. Requests go to the account's own API endpoint, looked
up once per run (the one saved at login until then), with Copilot's headers:
who started the request (`x-initiator`), whether it carries images, and the
kind of step (agent, subagent, title, compaction).

`codesmith models github-copilot` fetches the account's model list and keeps
it (see [Live model lists](#live-model-lists-modal-github-copilot-azure)): models then resolve
with the API, limits, prices and reasoning variants Copilot advertises, the
catalog's Copilot models stay only while Copilot still lists them, and
`github-copilot/auto` appears when the account has an Auto pool. With a kept
list that has Copilot's free utility model (`gpt-4o-mini`), sessions on
Copilot are named with it (marked as session-name generation, as Copilot's
own client does), and only fall back to the billed title request when that
fails.

A session on `github-copilot/auto` asks Copilot's Auto router (`POST
<api>/auto`, with the latest prompt, through the account's own Copilot
credentials) which model of the account's Auto pool serves it, and sends
its requests to that model, with the `Copilot-Session-Token` the router
issued (it unlocks the Auto discount; Copilot rejects it for any other
model). The pick holds until its token is about to expire, the session is
compacted (the compaction itself still uses it), or the kept list is for
another account. When the router fails, or picks a model the kept list
doesn't have, the account's chat fallback serves for five minutes before
the router is asked again. Titles never route. Auto needs the kept list
(`codesmith models github-copilot`).

### Cloudflare AI Gateway and Workers AI

Both need your Cloudflare account id, and AI Gateway its gateway id (else
the gateway named `default`). `codesmith auth login cloudflare-ai-gateway`
(or `cloudflare-workers-ai`) asks for the token and then for the ids the
environment and config do not already give (`--field accountId=…
--field gatewayId=…` answers without asking), and saves them with the token.
The saved answers win over `settings.accountId` / `settings.gatewayId`,
which win over `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_GATEWAY_ID`.

AI Gateway sends `openai/…` models to the gateway's OpenAI passthrough
(Responses) and `anthropic/…` models to its Anthropic passthrough
(Messages, with the model id's dots as dashes), with the token in
`cf-aig-authorization`; every other model goes to the account's
OpenAI-compatible endpoint
(`https://api.cloudflare.com/client/v4/accounts/<account>/ai/v1`) with the
gateway in `cf-aig-gateway-id` (a `workers-ai/` prefix is dropped). Workers
AI uses that endpoint with the token as a bearer token. A configured
`settings.baseURL` is used for every model as given.

### Poe

`poe` takes an API key (`POE_API_KEY`, or `codesmith auth login poe`), or
signs in in the browser with `codesmith auth login poe --oauth`, which has
Poe issue an API key for CodeSmith (OAuth with PKCE, scope
`apikey:create`). The issued key is used until it expires; then the login
says so and you log in again. If the browser runs on another machine, paste
the URL it lands on into the terminal; a URL from another login is refused
and the login keeps waiting.

The browser login runs as an OAuth client registered with Poe, and
CodeSmith ships none of its own (nor borrows another project's): register a
public client (PKCE, scope `apikey:create`, redirect URI
`http://127.0.0.1/callback` on any port) and give its client id with
`--client-id`, `settings.clientId` on the `poe` provider, or
`CODESMITH_POE_CLIENT_ID`, in that order.

### DigitalOcean

`digitalocean` uses the Gradient serverless inference API
(`https://inference.do-ai.run/v1`) with a model access key
(`DIGITALOCEAN_ACCESS_TOKEN`, or `codesmith auth login digitalocean`), or
signs in in the browser with `codesmith auth login digitalocean --oauth`
(DigitalOcean OAuth, scopes `genai:read inference:query`). The browser
login also lists the account's inference routers, which then resolve as
`digitalocean/router:<name>`; `codesmith models digitalocean` lists them
again. A login lasts as long as DigitalOcean's token (30 days unless it
says otherwise); then log in again.

The login runs as an OAuth application you register with DigitalOcean
([cloud.digitalocean.com/account/api/applications](https://cloud.digitalocean.com/account/api/applications))
with the callback URL `http://localhost:1456/auth/callback`; CodeSmith
ships none of its own (nor borrows another project's). Give its client id
with `--client-id`, `settings.clientId` on the `digitalocean` provider, or
`CODESMITH_DIGITALOCEAN_CLIENT_ID`. The login listens on port 1456 on both
`127.0.0.1` and `[::1]` (a browser may take `localhost` to either) and
says so if another program holds it on either. A token post whose state
is not this login's is refused and the login keeps waiting.

### GitLab sign-in

`codesmith auth login gitlab --oauth [--field instanceUrl=https://gitlab.example.com]`
signs in to gitlab.com (or `GITLAB_INSTANCE_URL`, or the instance given,
which may sit under a relative URL root) with OAuth and PKCE (scope `api`),
and saves the login with its instance and application. The access token is
refreshed with the application that issued it (GitLab binds refresh tokens
to it and rotates them on each refresh; concurrent refreshes share one
exchange). A revoked or expired refresh token asks you to sign in again.

The login runs as an OAuth application you register on the instance (User
settings → Applications) with redirect URI `http://127.0.0.1:8080/callback`,
scope `api`, and **Confidential** unticked; CodeSmith ships none of its own
(nor borrows another project's). Give its application id with
`--client-id`, `settings.clientId` on the `gitlab` provider, or
`GITLAB_OAUTH_CLIENT_ID`. The login listens on port 8080 and says so if
another program holds it.

GitLab Duo's models run through GitLab's AI gateway (agentic chat and Duo
workflows), which CodeSmith does not speak yet, so `gitlab/duo-*` models
cannot be used for sessions yet; the login is kept for when they can.

### Sign in with ChatGPT

`codesmith auth login openai --oauth` signs in with ChatGPT, so a ChatGPT
subscription pays for OpenAI requests through OpenAI's token sharing
instead of an API key. It's OAuth with PKCE in the browser (a redirect to
`127.0.0.1`; paste the final URL if the browser runs elsewhere), asking for
the `chatgpt.tokens.use.direct` scope for the OpenAI API.

The sign-in runs as an OAuth client of your own: CodeSmith ships none, and
never uses another product's (not OpenCode's, not Codex CLI's). Give its id
with `--client-id`, `settings.clientId` on the `openai` provider, or
`CODESMITH_CHATGPT_CLIENT_ID`; without one the sign-in stops and says so.
Giving `dynamic_agent_client` instead uses OpenAI's agent registration: the
sign-in registers a client owned by you, named CodeSmith, and OpenAI
returns its id on the callback. Whether OpenAI accepts that registration
for CodeSmith is up to OpenAI. The client is saved with the login; signing
in again reuses it, and a callback that names a different client is
refused. The ID token must be signed by OpenAI's keys, for that client,
unexpired, and carry the sign-in's nonce.

Signed in (and with no API key, which wins), OpenAI requests go to
`api.openai.com/v1` over HTTP rather than the Responses WebSocket, with the
login's access token, refreshed with the same client shortly before it
expires (a `baseURL` on another host is refused rather than sent the
token). Only the models ChatGPT serves are listed, at no per-token cost;
their context is capped as ChatGPT plans cap it (272K of input, 500K from
gpt-6.1 on, 272K for Astra models); compaction writes a summary rather
than using OpenAI's native compaction. Failures retrying can't fix (a
usage limit, an ineligible account, an unsupported request) aren't
retried, and the error starts with what to do about them.

Codex CLI's own browser and device sign-ins are not offered: they work
only as Codex CLI's OAuth client.

### xAI SuperGrok login

`codesmith auth login xai --oauth` signs in with a SuperGrok subscription
through xAI's OAuth device flow (open the URL it shows on any device and
enter the code); `codesmith auth login xai` saves an API key instead. The
device flow runs as an OAuth client you register with xAI: CodeSmith ships
none and never borrows another product's. Give its id with `--client-id`,
`settings.clientId` on the `xai` provider, or `XAI_OAUTH_CLIENT_ID`; without
one the login stops and says so. The login asks for `openid profile email
offline_access api:access` unless `settings.oauthScope` says otherwise.

The login is saved with its client id, and its access token is sent as a
bearer token to the xAI API; it is refreshed at xAI's token endpoint, with
that same client, shortly before it expires; a `baseURL` on another host is
refused rather than sent the token. An API key (`XAI_API_KEY` or a saved
key) wins over the login. A login saved by an earlier browser method
is moved to the device method. xAI's Responses steps go over a
[WebSocket](#responses-over-websocket) by default.

### Snowflake Cortex

`snowflake-cortex` uses the account's Cortex REST API
(`https://<account>.snowflakecomputing.com/api/v2/cortex/v1`). The account
comes from `SNOWFLAKE_ACCOUNT`, else the saved login, else
`settings.account` (an account identifier such as `myorg-myaccount`, or its
URL). The token comes from `SNOWFLAKE_CORTEX_TOKEN` or `SNOWFLAKE_CORTEX_PAT`,
else `settings.token`, else the saved key or login; an environment token wins
even over an expired login.

- `codesmith auth login snowflake-cortex` saves a programmatic access token
  (or other bearer token) and asks for the account.
- `codesmith auth login snowflake-cortex --oauth [--field account=… --field role=…]`
  signs in in the browser with Snowflake OAuth (PKCE, scope `refresh_token`,
  and `session:role:<role>` when a role is given). The access token is
  refreshed when it expires, or once when Cortex rejects it, and the
  rotated tokens are saved. The login uses Snowflake's built-in OAuth client
  for local applications, `LOCAL_APPLICATION`, as Snowflake's own drivers
  do; `--client-id` or `settings.clientId` names a security integration of
  your own instead.

Models get `max_completion_tokens` for the output limit, and Claude models
explicit Anthropic-style cache breakpoints (`cacheFormat: anthropic`), which
is how Cortex caches them.

### SAP AI Core

`sap-ai-core` sends requests to a deployment of SAP's generative AI hub with
the service instance's key. The service key (the JSON SAP BTP shows for the
AI Core instance) comes from `AICORE_SERVICE_KEY`, else
`settings.serviceKey`. CodeSmith gets OAuth tokens with the key's client
credentials (`<url>/oauth/token`) and sends the resource group in
`AI-Resource-Group` (`settings.resourceGroup`, `AICORE_RESOURCE_GROUP`, or
`default`). Requests go to `settings.baseURL`, else `AICORE_DEPLOYMENT_URL`,
else `<AI_API_URL>/v2/inference/deployments/<id>` with the id from
`settings.deploymentId` or `AICORE_DEPLOYMENT_ID`.

```jsonc
"providers": { "sap-ai-core": {
  "settings": { "deploymentId": "d1234567890", "resourceGroup": "default",
                "queryParams": { "api-version": "2024-10-21" } }
} }
```

OpenCode drives SAP's AI SDK here; CodeSmith speaks Chat Completions to the
deployment itself, which suits deployments that serve that API (OpenAI
models; set `"protocol"` for others). Without a service key the deployment
settings are not used and `settings.baseURL` takes a plain key.

### Claude Agent (your Claude Code login)

`claude-agent` runs the `claude` you installed and signed in to (Claude
Code, found on `PATH`, or set `CODESMITH_CLAUDE_PATH`), as OpenCode's
Claude Agent provider does. CodeSmith speaks to it the stream-json protocol
the Claude Agent SDK uses: one long-running process per session, which
stops after five minutes idle and resumes its Claude session next time.
Any login Claude Code has works (a subscription, an API key, a cloud
account). CodeSmith never reads, stores or passes a credential for it:

```sh
claude auth login                       # in a terminal, once
codesmith auth login claude-agent       # checks Claude Code and saves its models; saves no key
codesmith models claude-agent           # the same check, then the model list
codesmith -m claude-agent/sonnet#high   # efforts are the variants Claude Code lists
```

What Claude sees:

- **Prompt.** Claude Code's own system prompt, with a note that it runs in
  CodeSmith, the agent's own prompt as its role, and CodeSmith's
  instructions (AGENTS.md files, skills, MCP instructions, references),
  without the date and environment, which Claude Code writes itself.
  Titles use their own prompt.
- **Tools.** Claude Code's built-in tools are off. CodeSmith's tools reach
  Claude as the tools of an MCP server (`codesmith`) that lives in
  CodeSmith; Claude Code may run only those (`--allowedTools
  mcp__codesmith__*`, `--permission-mode dontAsk`: anything else is refused
  without asking). When Claude calls one, CodeSmith holds the call, runs it
  as it runs any model's tool calls, with its own permission rules and
  prompts, and hands the result back. There is one permission system, and
  if the tool server cannot connect the request fails rather than fall
  back to Claude Code's tools.
- **Settings.** None of your Claude Code settings: settings files, CLAUDE.md
  files, hooks, MCP servers, skills, auto compaction, auto memory, away
  summaries, tool search, the advisor tool and the non-streaming and
  refusal fallbacks are off, and `CLAUDE_CODE_SIMPLE`,
  `CLAUDE_CODE_EFFORT_LEVEL`, `MCP_TOOL_TIMEOUT` and
  `CODESMITH_SERVER_TOKEN` are cleared from its environment. Prompts are
  sent verbatim (CodeSmith already expanded commands and file
  references), which needs Claude Code 2.1.248 or later.
- **Identity.** Claude Code is told it is CodeSmith that drives it
  (`CLAUDE_AGENT_SDK_CLIENT_APP=codesmith/<version>`); nothing claims to be
  another program.

CodeSmith's history stays the source of truth: Claude's replies are saved
like any model's, so you can switch models mid-session. Turns Claude did
not see (another model's) reach it once, inside a `<conversation-checkpoint>`
user message; after an edit or undo the Claude session is branched at the
last point both histories agree on (or rebuilt from the history). The
provider keeps each session's Claude session and checkpoints, and the
model list, under `<data>/claude-agent/`. Claude Code already retries API
failures, so CodeSmith does not retry its turns. Cost follows the login
Claude Code reports: none for a subscription, a custom base URL or a
gateway; the catalog's prices of the model it resolves otherwise.

A provider's config reaches Claude Code as its environment:

| CodeSmith | Claude Code |
|---|---|
| `settings.baseURL` | `ANTHROPIC_BASE_URL` |
| `settings.headerTimeout` (ms) | `API_TIMEOUT_MS` |
| `settings.chunkTimeout` (ms) | `CLAUDE_STREAM_IDLE_TIMEOUT_MS` |
| `headers` | `ANTHROPIC_CUSTOM_HEADERS` |
| `body` | `CLAUDE_CODE_EXTRA_BODY` |
| a model's `limit.output` | `CLAUDE_CODE_MAX_OUTPUT_TOKENS` (else Claude Code picks) |

Unlike OpenCode, `settings.apiKey` and `settings.authToken` are not passed:
put a key where Claude Code reads it. Changing any of these, the system
prompt or the tools starts a new process; the model and effort change on
the running one. Temperature and other sampling options do not apply, and
the HTTP hooks of plugins never run, since Claude Code makes the API calls.
`claude` must run on the same machine.

### Live model lists (Modal, GitHub Copilot, Azure)

Some providers serve a model list of their own for your account (Azure:
the resource's deployments; DigitalOcean: its inference routers).
`codesmith models <provider>` fetches it and keeps it in the data folder's
`live-models.json`, and from then on those models resolve with what the
list says (limits, prices, modalities, reasoning variants) over the
catalog's; nothing is fetched in the background. For `modal` the list is the
proxy's `<baseURL>/models` (with `MODAL_PROXY_TOKEN` or a saved key), each
live model built on the catalog model of its base model (`base_model_id`,
else `hugging_face_id`), as in OpenCode. Run it again to pick up changes.

### Local model discovery (Ollama, LM Studio, vLLM, llama.cpp)

The local providers list what their server has loaded, as OpenCode's
provider plugins do: LM Studio's `/api/v1/models` (language and decision
models, with the smallest loaded context), Ollama's `/api/tags` with
`/api/show` for each model (models that complete text, with their tool and
vision support and context length; details are asked again only when a
model's digest changes), vLLM's `/health` then `/v1/models` (the models it
serves itself, with `max_model_len`), and for `llamacpp` the OpenAI
`/v1/models`. A discovered model resolves with what the server said (its
context window, tools, images); what it does not say takes OpenCode's
defaults for a custom model (200,000 tokens of context, no tools).

Only the provider's own endpoint is asked: the configured `settings.baseURL`
(with `settings.apiKey` or the saved key sent as a bearer token), else the
default address in the table above. `codesmith models` (and the model
picker, each time it opens) asks every local provider whose endpoint is on
this machine (`localhost`, `127.0.0.0/8`, `::1`) or whose `local` is
`true`, but none with `local: false`, giving each half a second to
connect, so a server that is not running is skipped; `codesmith models <provider>` asks that provider's
endpoint wherever it is. The list is kept in `live-models.json` for the
endpoint it came from, through outages until the next success; an empty
answer brings the catalog's models back.

Any other OpenAI-compatible server (oMLX, a llama.cpp server on another
port) opts in with `settings.discovery`: `openai` (`GET /v1/models`, reading
`max_model_len`, `context_length` or llama.cpp's `meta.n_ctx_train` for the
context), `lmstudio`, `ollama` or `vllm`:

```jsonc
"providers": {
  "omlx": {
    "settings": { "baseURL": "http://127.0.0.1:8000/v1", "discovery": "openai" }
  }
}
```

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
(carry on after a compaction, answer in text on the last allowed step, and
[instructions that changed](#when-instructions-change)) are
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
and other protocols leave them out. A call whose result never came (the
response ended, or failed, before it) is settled as failed for good,
"Provider did not return a tool result", and sent back in the provider's
own failure shape; only a paused Anthropic turn keeps its call open for the
next request. Anthropic's beta headers for
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
  session (its id; a subagent uses its parent's), so requests go to the
  same cache. An OpenCode-style id (`ses_` and 64 hex digits) loses its
  prefix to fit OpenAI's 64 characters. Every model request also carries
  the session's correlation headers: `x-session-affinity` and
  `X-Session-Id` (the same id), `x-parent-session-id` for a subagent, and
  `x-codesmith-session-id`, `x-codesmith-session` and `x-codesmith-client`.
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
| `{"warming": true}` | Also keep this model's cache warm (or `false` to not); see [Prompt-cache warming](#prompt-cache-warming). |
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
Some providers sign in differently: `codesmith auth login github-copilot`
(GitHub's device flow), `codesmith auth login amazon-bedrock --profile <name>`
and `codesmith auth login google-vertex --project <id>` save a login instead
of a key; `auth logout` removes those too. Some key logins ask for more than the key (the
account and gateway ids of [Cloudflare](#cloudflare-ai-gateway-and-workers-ai)):
they are asked in the terminal after the key, or given with
`--field KEY=VALUE`, and saved with it. `codesmith auth login azure --cli`
saves an [Azure CLI](#azure-openai) login. `--oauth` logs in through a
provider's browser OAuth instead of a key ([Poe](#poe),
[DigitalOcean](#digitalocean), [GitLab](#gitlab-sign-in),
[Snowflake Cortex](#snowflake-cortex), [Sign in with ChatGPT](#sign-in-with-chatgpt)
for `openai`, and [xAI](#xai-supergrok-login)'s device flow).

A provider can have several saved credentials, one of them active: the
active one is the provider's entry in `auth.json`, and the others are kept
beside it under `$credentials`, each with an id, a label and the order they
were saved in. A new login (such as an [xAI](#xai-supergrok-login) one)
becomes active and keeps the one it replaced; removing the active one makes
the newest remaining one active. Plugins and embedders manage them with
`codesmith_core::providers::credentials` (`list_in`, `create_in`,
`activate_in`, `update_in`, `remove_in`). OAuth logins are refreshed when
they are within five minutes of expiry, once for every request that needs
it: a refresh another process already made, or a login replaced meanwhile,
is kept rather than overwritten. Credentials are never logged, and a
login's tokens go only to the service that issued it and the API it is
for.

## Session titles

A new session gets a short title, written by a model from your first
message (OpenCode's title prompt). The model is, in order:

1. `agents.title.model` (OpenCode's hidden `title` agent; `title` is never
   an agent you can pick),
2. `small_model`,
3. the session's own model.

```jsonc
{
  "small_model": "anthropic/claude-haiku-4-5",
  // or turn titles off:
  "title": false                        // also "agents": {"title": {"disable": true}}
}
```

The title request offers no tools and leaves the model's reasoning options
out (a `#variant` in the title model keeps that variant's). Its reply is
capped at 256 tokens, or 64 on a local server, where it also asks for no
thinking and waits for the turn: see
[Using a local server](#using-a-local-server-omlx-lm-studio-llamacpp-ollama).
When the request fails, or titles are off, the first line of the message
names the session.

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

Folders below the working directory can have their own `AGENTS.md` (or
`CLAUDE.md`). When the agent reads a file or lists a folder inside the
project, the `AGENTS.md` files of the folders from there up to the working
directory are given to it after the read, nearest first, as OpenCode does.
Each is given once while it is in the conversation: one dropped by
compaction or undo comes again with the next read under it.

Set `$CODESMITH_DISABLE_CLAUDE_CODE` to stop reading `CLAUDE.md` files and
`.claude/` folders for instructions, skills and commands.
`codesmith debug prompt` prints the full system prompt the build agent (or
the one named with `--agent`) would get in the current folder.

### When instructions change

The system prompt is built from instruction sources: the [Code
Mode](mcp.md#code-mode) catalog, MCP server instructions,
[references](#references), [skills](skills-and-commands.md#skills), the
instruction files above, then today's date and the environment. They are
read again before each model request, as in OpenCode.

The first request of a session records what they said, and the system prompt
is rendered from that baseline. When something changes later (you edit
`AGENTS.md`, a skill or an MCP tool appears, the date rolls over), the change
is recorded in the session and the model sees a short update at that point in
the conversation, as a system update (see [Providers and models](#providers-and-models)): a diff of an
edited file when that is shorter than the new text, the skills or tools that
came or went. The system prompt stays the same, so the provider's prompt
cache keeps working.

A change to instruction files or skills that shows up after the agent ran a
tool that can write files (anything but read, glob, grep, web fetch and
search, todos, skills and questions), or while its background jobs run, may
be the agent's own doing, or text it was steered into writing. CodeSmith
does not give that change system authority: the model gets it as a
user-role note marked as untrusted project content. Changes you make between
turns arrive as system updates as before.

- A file that cannot be read for now keeps the last value it was read with.
  On a session's first request it stops the request with an error instead,
  so the session never starts from instructions it could not read.
- [Compaction](#compaction) starts a new baseline from the newest values,
  and the updates before it are folded into the system prompt.
- [Undo](sessions.md#undo-and-redo) that removes an update starts a fresh
  baseline at the next request, from what the sources say then.
- A forked session starts from its source's newest values.
- Sessions recorded before CodeSmith tracked instructions start a baseline
  the next time they run.

## References

References name other folders the agent may consult: documentation, a
library's source, a sibling repository. Each is a local path or a Git
repository:

```jsonc
"references": {
  "docs": "../product-docs",
  "sdk": "owner/repo",
  "api": { "repository": "gitlab.com/team/api", "branch": "v2", "description": "Use for the API server's code" },
  "notes": { "path": "~/notes", "description": "Use for my notes on this project" }
}
```

- A string that starts with `.`, `/` or `~` is a local path; any other
  string is a repository. A relative path is relative to the project root
  (OpenCode resolves it against the folder of the config file that names
  it), and `~/` is your home folder.
- A repository is GitHub `owner/repo` (or `github:owner/repo`), `host/path`
  (`gitlab.com/group/repo`), scp-style `git@host:path`, or an `https://` or
  `ssh://` URL. Local `file://` repositories are not supported; name the
  folder as a path instead. `branch` tracks that branch instead of the
  remote's default one.
- Names may not contain `/`, `,`, `` ` `` or whitespace; an invalid entry is
  skipped.
- References **with a `description`** are listed to the model, with their
  folder and description, so it knows when to look there. The others are
  still configured but not mentioned. `hidden` is accepted for OpenCode
  configs; CodeSmith does not use it.

CodeSmith keeps a checkout of each repository under the data folder, in
`repos/<host>/<path>` (`repos/<host>/<path>@<branch>` for a branch), and the
reference's folder is that checkout. When a session starts, a background
`git clone` (shallow) fetches a checkout that is missing; one that exists is
refreshed with `git fetch` and a hard reset to the tracked branch at most once
a day, failed attempts included. Only `git` reaches the network, each command
for at most two minutes and without prompting for credentials, so private
repositories need credentials `git` finds on its own (an SSH agent or a
credential helper). A checkout whose remote no longer matches is cloned
again. `$CODESMITH_REPO_CLONE_GITHUB_BASE_URL` replaces `https://github.com/`
for GitHub repositories (a mirror, say).

Reading files in a reference outside the project asks for
[`external_directory`](permissions.md) like any other outside folder.

## Media limits

Images are fitted to these limits before they reach the model:

```jsonc
"media": { "image": { "auto_resize": true, "max_width": 2000, "max_height": 2000, "max_base64_bytes": 5242880 } }
```

With `auto_resize: false`, an image that is too large is dropped with a note
to the model instead of scaled down. See [images and
PDFs](tools.md#images-and-pdfs) for the rest of the media behaviour.
xAI decodes only PNG, JPEG and WebP images, so on an `xai` model (or one at
`api.x.ai`) any other image type is replaced by a note telling the model it
could not see it, as media the model cannot take at all is.

Media in a session (attachments, images a tool read, generated images and
videos) is kept once in the session database's blob store, outside the
session's records: the records name each item by the SHA-256 of its bytes,
the same bytes are stored once however often they recur (forks share them),
and an item is deleted when the last session or turn naming it goes. One
item is at most 20 MiB. `"media": {"blob": false}` keeps new media inline in
the records instead. Sessions from before the blob store keep reading as
they are; `codesmith session migrate-media` moves their media over.

## Compaction

When a conversation nears the model's context window (`limit.context`), older
history is replaced by a structured summary, and the newest exchanges are kept
verbatim, so long sessions keep going. By default compaction starts when less
than 15% of the window (and at least 16,000 tokens) is left, and about 15,000
tokens of the newest turns are kept beside the summary.

```jsonc
{ "compaction": { "auto": true, "background": true, "buffer": "20%", "keep": { "tokens": 15000 } } }
```

| Field | Default | Meaning |
|---|---|---|
| `auto` | `true` | `false` leaves compaction to `/compact`, even when the provider rejects a prompt as too long. |
| `buffer` | 15% (min 16,000) | Room kept free below the window: a number of tokens, or a share such as `"20%"`. |
| `keep.tokens` | `15000` | Tokens of recent history kept verbatim. |
| `background` | `true` | Write the summary in the background as the session nears automatic compaction, so compaction swaps it in instead of waiting for one. A summary that is never used still costs tokens. Needs `auto`. On a [local server](#using-a-local-server-omlx-lm-studio-llamacpp-ollama) it is written only while the session is idle. |

The prompt size is the provider's token count for the latest response, plus
an estimate (four characters per token) of what came after it; before the
first response, everything is estimated.

The summary request reuses the session's prompt cache; see [long
sessions](sessions.md#long-sessions-compaction). Its output limit is the
model's `limit.output`, lowered to the room the request leaves in the window.

### Native compaction

OpenAI's Responses API can compact a conversation itself, into an
encrypted checkpoint that only that model reads. Turn it on per model (or
provider) instead of summaries:

```jsonc
"providers": { "openai": { "models": { "gpt-5.4": { "compaction": { "type": "native" } } } } }
```

CodeSmith then sends the conversation, as a step would, with a compaction
trigger, and keeps the newest of your own messages (within `keep.tokens`)
followed by the provider's checkpoint. When the session's steps go over the
[Responses WebSocket](#responses-over-websocket), so does the trigger: it
continues the latest response, sending only the trigger, and the next step
then goes in full since the checkpoint starts a new window; if the socket
can't carry it, the whole conversation goes over HTTP. A reply with
anything but one checkpoint fails the compaction without a retry.
`"mechanism": "endpoint"` uses
`/responses/compact` instead, which returns the whole replacement window
(Azure supports only this one); by default the trigger is used where it is
available. Compaction otherwise works as usual (`/compact`, `auto`, an
overflow), but there are no background summaries. On a route that cannot
compact natively, compaction fails without calling the model. A session
continued with another model replays from the latest summary before the
checkpoint (or from the start), with the messages it stood for.

Without `limit.context`, CodeSmith cannot tell when the window is nearly
full. Compaction then happens only when you run `/compact`, or when the
provider rejects a prompt as too long (unless `auto` is `false`).

## Prompt-cache warming

A provider's prompt cache lasts a few minutes after the request that last
used it (five on most, or the model's `cache.ttl`). Leave a session idle
longer, and the next prompt pays to write the whole conversation again.
Warming sends a small keep-alive request before the cache expires: the
session's latest request again, with a prompt asking for a one-word reply.
It is off by default.

```jsonc
{ "warming": true }
{ "warming": { "interval": "2 minutes", "duration": "1 hour", "prompt": "Reply with exactly: OK" } }
```

| Field | Default | Meaning |
|---|---|---|
| `interval` | `"auto"` | Idle time between keep-alive requests. `"auto"` is 90% of the model's `cache.ttl`, or 4 minutes without one. |
| `duration` | `"30 minutes"` | How long after the latest real request to keep warming. |
| `prompt` | a keep-alive prompt | What the keep-alive request asks. |

A provider's or model's `cache.warming` (`true`, `false`, or the same
object without `prompt`) overrides the top-level setting:
`"cache": {"ttl": "5m", "warming": true}`. With `"auto"`, a cache that
outlives `duration` (`"1h"` against the 30-minute default) is not warmed.
Keep-alive requests wait while a request or compaction runs, stop when one
rebuilt the cache instead of reading it (until the next real request), and
end when the session is closed. Subagents are not warmed. Their tokens count
in the session's usage.

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

## The terminal UI: `tui`

The UI's own settings sit under `tui`, in the shape of OpenCode's TUI
config, and merge across files like the rest (a project can set its own
theme):

```jsonc
{
  "tui": {
    "theme": { "name": "tokyonight", "mode": "system" },
    "keybinds": { "leader": "ctrl+o", "session.list": "ctrl+l", "help.show": "f1" },
    "leader": { "timeout": 1500 },
    "cursor": { "style": "line", "blinking": false },
    "session": { "thinking": "show", "markdown": "rendered", "verbosity": "medium", "grouping": "auto" },
    "diffs": { "source": "working", "view": "auto", "tree": true, "single": false, "fullscreen": false }
  }
}
```

| Key | Default | What it does |
|---|---|---|
| `theme.name` | `opencode` | The theme. See [themes](interactive-ui.md#themes). |
| `theme.mode` | `system` | `system` follows the terminal's background; `dark` or `light` pins one. |
| `keybinds` | OpenCode's | Key bindings by command id. See [key bindings](interactive-ui.md#key-bindings). |
| `leader.timeout` | `2000` | Milliseconds the leader key (and any key sequence) waits for its next key. |
| `cursor.style` | `block` | `block`, `underline`, `line`, or `default` to keep the terminal's. |
| `cursor.blinking` | `true` | Whether the cursor blinks. |
| `session.thinking` | `hide` | `show` shows reasoning from the start (`/thinking` toggles it). |
| `session.markdown` | `rendered` | Reasoning as rendered Markdown, or `source` for its highlighted Markdown text. |
| `session.verbosity` | `low` | How much tool activity the transcript shows: `low` sums up each run of tool calls and thoughts on one row, `medium` shows calls on their own rows with reads and searches grouped, `high` also opens those groups. See [verbosity](interactive-ui.md#verbosity). |
| `prompt.paste` | `compact` | `full` pastes long text in whole rather than as a `[Pasted ~N lines]` chip. |
| `session.tps` | `true` | Show a turn's throughput (output tokens a second) in its footer. |
| `session.new_location` | `launch` | Where `/new` starts a session after the [open menu](interactive-ui.md#projects-worktrees-and-other-folders) moved the UI to another folder: `launch`, the folder CodeSmith was started in, or `inherit`, the folder it is in now. |
| `debug.turn_tokens` | `false` | `true` (or OpenCode's `"verbose"`) adds a token usage row after each turn, with likely cache busts flagged. |
| `session.grouping` | `auto` | `none` draws reads and searches one by one rather than as one `Explored` row. |
| `diffs.source` | `branch` | What the [diff viewer](interactive-ui.md#the-diff-viewer-and-review-panel) opens on: `branch` (the branch's commits and local changes against its base), `committed`, `working` (uncommitted changes) or `turn` (the session's last turn). |
| `diffs.view` | `auto` | `split` or `unified` patches; `auto` splits when the pane is 100 columns wide. |
| `diffs.tree` | `true` | Show the file tree beside the patches (full screen, at 88 columns or more). |
| `diffs.single` | `false` | Show one file at a time. |
| `diffs.fullscreen` | `false` | Open the viewer full screen rather than docked beside the session. |

OpenCode's `terminal.copy` (copy on a mouse selection) has no
counterpart: CodeSmith never captures the mouse, so selecting and copying
with it is the terminal's own (see [keys](interactive-ui.md#editing-the-prompt)).

### Session tabs: `tui.tabs`

The [session tabs](interactive-ui.md#session-tabs), as OpenCode's `tabs`
settings:

```jsonc
{ "tui": { "tabs": { "mode": "on", "scope": "global", "layout": "vertical", "indicators": "numbers" } } }
```

| Key | Default | What it does |
|---|---|---|
| `mode` | `auto` | `on`, `off`, or `auto`: on unless the UI runs inside the herdr multiplexer (`HERDR_ENV=1`), which has tabs of its own. OpenCode's older `enabled: true/false` stands for `on`/`off` when `mode` is unset. |
| `scope` | `cwd` | `cwd` keeps a set of open tabs per folder; `global` shares one. |
| `layout` | `horizontal` | `horizontal`, a strip along the top, or `vertical`, a column on the left where the terminal is wide enough. |
| `indicators` | `status` | `status` shows each tab's state before its title; `numbers` always shows its number. |

The open tabs are kept in `~/.local/share/codesmith/tui-tabs.json`, in
OpenCode's shape (each tab's `sessionID` and `title`, plus when it was last
shown).

### Notifications and sounds: `tui.attention`

How the UI [tells you a session needs you](interactive-ui.md#the-terminal-title-notifications-and-exit),
as OpenCode's `attention` settings; both are off by default:

```jsonc
{ "tui": { "attention": { "notifications": true, "sound": true, "sounds": { "done": "~/sounds/done.wav" } } } }
```

| Key | Default | What it does |
|---|---|---|
| `notifications` | `false` | Show a system notification (through the terminal) while it is out of focus. |
| `sound` | `false` | Play a sound: the terminal's bell, or the file `sounds` names. |
| `volume` | `0.4` | From 0 to 1, for sound files (`0` is silent). |
| `sounds` | `{}` | Sound files by event: `default`, `question`, `permission`, `error`, `done`, `subagent_done`, played with `afplay` on macOS and `paplay` elsewhere (for at most ten seconds). CodeSmith ships no sounds of its own. |

`terminal.title` (default `true`) keeps the terminal window's title on the
session's.

OpenCode's first TUI format (its `tui.json`) works here too and is read as
the current one: `"theme": "name"`, `leader_timeout`, and keybind names
like `session_list`. A name set both ways takes the command id's value.
Settings OpenCode has for features CodeSmith lacks (`mouse`, `scroll`,
`attention.sound_pack`, `mini`, …) are ignored. A value that does not fit,
an unknown command or key, and two configured bindings that clash are
reported when the UI opens; the default is used instead.

The theme picked with `/themes` is remembered (in
`~/.local/share/codesmith/tui-state.json`) and wins over `theme.name`
until `theme.name` itself changes. So are the diff viewer's layout choices
(`tui-diff.json` beside it), until `diffs` changes: OpenCode writes them
back to its config, and CodeSmith's UI never writes the config.

## File watching

As in OpenCode, CodeSmith watches files only where something uses the
changes:

- **Language servers** (when [`lsp`](language-servers-and-formatters.md#language-servers)
  is on): once a server is running, the project is watched so that files
  changed outside the agent (by you, a `git checkout`, a build) reach it.
- **The local server** (`codesmith serve`): the project's branch metadata
  (`.git/HEAD`, or `.hg/branch`) and the files a client asked to watch, sent
  as `filesystem.changed` events. See [file watching](local-server.md#file-watching).

Nothing is watched at startup, and a process with nothing to watch holds no
watch and no thread. The platform's own notifications are used: FSEvents on
macOS and ReadDirectoryChangesW on Windows (one watch for the whole project),
inotify on Linux (one watch per directory, at most half of
`fs.inotify.max_user_watches`, so other tools keep room; past that, deeper
directories go unwatched). A watched project leaves out `node_modules`,
`.git`, `.venv`, `venv` and `__pycache__` anywhere, what the project's
`.gitignore` ignores, and `watcher.ignore`:

```jsonc
{ "watcher": { "ignore": ["dist", "**/*.generated.ts"] } }
```

An entry without wildcards is a path from the project root (`dist` is the
top-level `dist`). Naming `.git` (or `.hg`) also stops the branch watch.
`CODESMITH_DISABLE_FILEWATCHER=1` turns file watching off.

Idle cost is negligible: on a 100,000-file tree, see
[PERFORMANCE.md](../PERFORMANCE.md#file-watching).

## Environment variables

| Variable | Effect |
|---|---|
| `CODESMITH_CONFIG` | An extra config file, loaded after the global one. |
| `CODESMITH_CONFIG_CONTENT` | Inline JSON config, loaded last. |
| `CODESMITH_CONFIG_DIR` | The global config folder (default `$XDG_CONFIG_HOME/codesmith`, else `~/.config/codesmith`). |
| `CODESMITH_DISABLE_PROJECT_CONFIG` | Ignore `codesmith.json[c]` and `.codesmith/` in the project. |
| `CODESMITH_DISABLE_GLOBAL_CONFIG` | Ignore the global config folder and the home folder's `.claude` and `.agents`, however they are reached ([files and precedence](#files-and-precedence)). |
| `CODESMITH_DISABLE_CLAUDE_CODE` | Don't read `CLAUDE.md` or `.claude/` skills and commands. |
| `CODESMITH_DISABLE_CLAUDE_CODE_SKILLS` | Don't read `.claude/skills` only. |
| `CODESMITH_DB` | The session database (default `<data>/codesmith.db`). |
| `CODESMITH_REPO_CLONE_GITHUB_BASE_URL` | Clone GitHub [references](#references) from this base URL instead of `https://github.com/`. |
| `XDG_DATA_HOME` | The data folder is `$XDG_DATA_HOME/codesmith`, else `~/.local/share/codesmith`. |
| `XDG_CACHE_HOME` | The cache folder (the models.dev catalog) is `$XDG_CACHE_HOME/codesmith`, else `~/.cache/codesmith`. |
| `CODESMITH_MODELS_FETCH` | `1`: fetch the [model catalog](#the-model-catalog-modelsdev) in the background (off by default). |
| `CODESMITH_MODELS_URL` | Where the model catalog is fetched from (default `https://models.dev`); turns fetching on. |
| `CODESMITH_RELEASES_URL` | Where [`codesmith upgrade`](cli.md#codesmith-upgrade) and the update check look for releases (default `https://github.com/reka18/codesmith/releases`). |
| `CODESMITH_DISABLE_AUTOUPDATE` | `1` or `true` turns the [update check](cli.md#update-checks) off whatever the config says. |
| `CODESMITH_MODELS_PATH` | A catalog file read instead of the cached one. |
| `CODESMITH_DISABLE_MODELS_FETCH` | `1` or `true`: never fetch the catalog, not even with `models --refresh`. |
| `CODESMITH_SERVER_TOKEN` | The bearer token for [`codesmith serve`](local-server.md). |
| `CODESMITH_CLAUDE_PATH` | The `claude` the [Claude Agent](#claude-agent-your-claude-code-login) provider runs, instead of `claude` on `PATH`. |
| `CODESMITH_SHELL_DAEMON` | `0`: [`codesmith serve`](local-server.md#the-shell-daemon) runs commands and terminals itself instead of in its shell daemon. |
| `CODESMITH_SHELLD_HANDOFF` | The shell daemon handoff a server inherits from the one before it ([restarting](local-server.md#restarting-without-stopping-commands)); taken out of the environment at startup. |
| `CODESMITH_SHELLD_SOCKET_DIR` | Where the shell daemon's socket directory goes instead of your runtime directory (`$XDG_RUNTIME_DIR`, or the per-user temporary directory on macOS) or `/tmp` (absolute, no `..`). |
| `XDG_STATE_HOME` | The shell daemon's runtime directories are under `$XDG_STATE_HOME/codesmith/shelld`, else `~/.local/state/codesmith/shelld`. |
| `CODESMITH_DISABLE_FILEWATCHER` | `1` or `true`: watch no files ([file watching](#file-watching)). |

CodeSmith sets `CODESMITH_SESSION_ID` for commands the `shell` tool runs, and
`CODESMITH_WORKTREE_BASE` and `CODESMITH_WORKTREE_PATH` for a worktree's
`start` command.

`codesmith debug paths` shows the config, data, tool-output, scratch and cache
folders in use.
