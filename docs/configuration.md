# Configuration

CodeSmith works with no config file at all for the hosted providers that read
a key from the environment, but most setups want at least a default `model`.
This page covers where config lives, how files combine, and every key.

- [Files and precedence](#files-and-precedence)
- [Substitution: `{env:}` and `{file:}`](#substitution-env-and-file)
- [Key reference](#key-reference)
- [Providers and models](#providers-and-models)
- [Instructions](#instructions)
- [References](#references)
- [Media limits](#media-limits)
- [Compaction](#compaction)
- [Tool output](#tool-output)
- [Web search](#web-search)
- [The terminal UI: `tui`](#the-terminal-ui-tui)
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
| `references` | `{}` | Other folders or Git repositories the agent may consult, by name. See [References](#references). |
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
| `catalog` | no fetching | `{"fetch": true, "url": "..."}` opts in to fetching the models.dev catalog. See [the model catalog](#the-model-catalog-modelsdev). |
| `plugins` | all on | `{"<id>": {...options}}`, or `false` to turn a compiled-in plugin off. See [plugins](plugins.md). |
| `tui` | `{}` | The terminal UI: theme, key bindings, cursor, reasoning display and transcript verbosity. See [The terminal UI](#the-terminal-ui-tui). |
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
| `amazon-bedrock` | Bedrock Converse (Mantle models: Responses, Chat or Messages) | `AWS_BEARER_TOKEN_BEDROCK`, else SigV4 with AWS credentials; see [Amazon Bedrock](#amazon-bedrock) |
| `google-vertex` | Gemini, Anthropic Messages for Claude, OpenAI Chat for partner models | Application Default Credentials, or `GOOGLE_VERTEX_API_KEY` (Express Mode); see [Google Vertex AI](#google-vertex-ai) |
| `google-vertex-anthropic` | Anthropic Messages | as `google-vertex` |
| `github-copilot` | per model: Anthropic Messages, OpenAI Responses or Chat | `codesmith auth login github-copilot`, or `COPILOT_GITHUB_TOKEN`; see [GitHub Copilot](#github-copilot) |
| `databricks` | OpenAI Chat | `DATABRICKS_TOKEN` (and `DATABRICKS_HOST`, with or without `https://`) |
| `cloudflare-ai-gateway` | per model: OpenAI Responses, Anthropic Messages or OpenAI Chat | `CLOUDFLARE_API_TOKEN`, `CF_AIG_TOKEN` (and the account and gateway ids); see [Cloudflare](#cloudflare-ai-gateway-and-workers-ai) |
| `cloudflare-workers-ai` | OpenAI Chat | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_WORKERS_AI_TOKEN`, `CLOUDFLARE_API_TOKEN` (and the account id) |

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
| `capabilities` | OpenCode's capabilities, merged field by field over the catalog's: `input` and `output` modalities, `tools`, `temperature`, and `reasoning` (`{"supported": true, "effort": ["low", "high"]}`, `"toggle": true`, `"budget": {"min": 1024}`), which regenerates the reasoning variants. |
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
kind of step (agent, subagent, title, compaction). Copilot's Auto model
router and its free title model are not used.

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
Some providers sign in differently: `codesmith auth login github-copilot`
(GitHub's device flow), `codesmith auth login amazon-bedrock --profile <name>`
and `codesmith auth login google-vertex --project <id>` save a login instead
of a key; `auth logout` removes those too. Some key logins ask for more than the key (the
account and gateway ids of [Cloudflare](#cloudflare-ai-gateway-and-workers-ai)):
they are asked in the terminal after the key, or given with
`--field KEY=VALUE`, and saved with it.

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
| `session.grouping` | `auto` | `none` draws reads and searches one by one rather than as one `Explored` row. |
| `diffs.source` | `branch` | What the [diff viewer](interactive-ui.md#the-diff-viewer-and-review-panel) opens on: `branch` (the branch's commits and local changes against its base), `committed`, `working` (uncommitted changes) or `turn` (the session's last turn). |
| `diffs.view` | `auto` | `split` or `unified` patches; `auto` splits when the pane is 100 columns wide. |
| `diffs.tree` | `true` | Show the file tree beside the patches (full screen, at 88 columns or more). |
| `diffs.single` | `false` | Show one file at a time. |
| `diffs.fullscreen` | `false` | Open the viewer full screen rather than docked beside the session. |

OpenCode's first TUI format (its `tui.json`) works here too and is read as
the current one: `"theme": "name"`, `leader_timeout`, and keybind names
like `session_list`. A name set both ways takes the command id's value.
Settings OpenCode has for features CodeSmith lacks (`mouse`, `scroll`,
`tabs`, `attention`, …) are ignored. A value that does not fit,
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
| `CODESMITH_DISABLE_CLAUDE_CODE` | Don't read `CLAUDE.md` or `.claude/` skills and commands. |
| `CODESMITH_DISABLE_CLAUDE_CODE_SKILLS` | Don't read `.claude/skills` only. |
| `CODESMITH_DB` | The session database (default `<data>/codesmith.db`). |
| `CODESMITH_REPO_CLONE_GITHUB_BASE_URL` | Clone GitHub [references](#references) from this base URL instead of `https://github.com/`. |
| `XDG_DATA_HOME` | The data folder is `$XDG_DATA_HOME/codesmith`, else `~/.local/share/codesmith`. |
| `XDG_CACHE_HOME` | The cache folder (the models.dev catalog) is `$XDG_CACHE_HOME/codesmith`, else `~/.cache/codesmith`. |
| `CODESMITH_MODELS_FETCH` | `1`: fetch the [model catalog](#the-model-catalog-modelsdev) in the background (off by default). |
| `CODESMITH_MODELS_URL` | Where the model catalog is fetched from (default `https://models.dev`); turns fetching on. |
| `CODESMITH_MODELS_PATH` | A catalog file read instead of the cached one. |
| `CODESMITH_DISABLE_MODELS_FETCH` | `1` or `true`: never fetch the catalog, not even with `models --refresh`. |
| `CODESMITH_SERVER_TOKEN` | The bearer token for [`codesmith serve`](local-server.md). |
| `CODESMITH_DISABLE_FILEWATCHER` | `1` or `true`: watch no files ([file watching](#file-watching)). |

CodeSmith sets `CODESMITH_SESSION_ID` for commands the `shell` tool runs, and
`CODESMITH_WORKTREE_BASE` and `CODESMITH_WORKTREE_PATH` for a worktree's
`start` command.

`codesmith debug paths` shows the config, data, tool-output, scratch and cache
folders in use.
