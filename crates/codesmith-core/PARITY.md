# Core parity with OpenCode

CodeSmith's agent core (`codesmith-core`: the session loop, tools, permissions, shell scanning, config, compaction, MCP, automations, skills and commands) is pinned to OpenCode's `packages/core` by porting its unit tests (MIT) as Rust tests. Existing ports are the `oc_*` tests and those that cite an OpenCode test by name: inline in the modules (`src/tool/*.rs`, `src/permission.rs`, `src/config.rs`, `src/compaction.rs`, `src/retry.rs`, `src/runner.rs`, `src/title.rs`, `src/instructions.rs`, `src/skill.rs`, `src/command.rs`, `src/media.rs`), in [`tests/runner_parity.rs`](tests/runner_parity.rs), [`tests/parity.rs`](tests/parity.rs) and [`tests/websocket_session.rs`](tests/websocket_session.rs), and, added with this inventory, in [`tests/oc_tools.rs`](tests/oc_tools.rs), [`tests/oc_runner.rs`](tests/oc_runner.rs), [`tests/oc_compaction.rs`](tests/oc_compaction.rs) and [`tests/oc_config.rs`](tests/oc_config.rs). The provider protocols have their own matrix in [`codesmith-ai/PARITY.md`](../codesmith-ai/PARITY.md).

Every test in `packages/core/test` was examined: 332 files, 3826 test sites, 5588 test cases. Parametrized tests count once per generated case, as in the provider matrix: a `test.each` row, or a pass of a `for` loop over a literal list, is one case. (Cases come from the literal tables; a loop over a computed list counts once.) Each case is in one bucket:

- **ported**: CodeSmith already behaved this way, and a CodeSmith test pins it.
- **ported after a fix**: CodeSmith was wrong in a way that matters; it was fixed and the test ported.
- **gap: missing feature**: the test needs a feature CodeSmith does not have (grouped in the [roadmap](#roadmap)).
- **gap: by design / not applicable**: CodeSmith differs on purpose (a documented deliberate difference), or the test is about OpenCode's own machinery (Effect layers, its SQLite event store, its client/server Locations, hosted services, Windows/PowerShell, npm packages).
- **gap: not yet ported**: CodeSmith has the behaviour (or its equivalent) but no test pins it yet; these need no new feature and are the [porting backlog](#porting-backlog).
- **gap: found bug**: porting showed CodeSmith wrong and it is [recorded](#found-bugs-and-differences) rather than fixed.

How the buckets were decided: a test is **ported** only when a CodeSmith test named for it (or citing it) checks the same behaviour; "adapted" notes say where a port asserts CodeSmith's wording or message model instead of OpenCode's. **Not yet ported** is a judgement that CodeSmith has the behaviour, made by reading its code, not by running the test, so porting a backlog test can still turn up a bug (as the ports in this change did). Where one table mixes PowerShell and POSIX rows, the rows were counted from the table and split between the two buckets.

## Summary

| Area | Tests examined | Ported | Ported after a fix | Gap: missing feature | Gap: by design / n.a. | Gap: not yet ported | Gap: found bug |
|---|---:|---:|---:|---:|---:|---:|---:|
| Session runner | 393 | 70 | 7 | 59 | 190 | 67 | 0 |
| Subagents | 46 | 10 | 0 | 0 | 35 | 1 | 0 |
| Compaction | 51 | 9 | 0 | 29 | 12 | 1 | 0 |
| Titles | 26 | 8 | 0 | 0 | 13 | 5 | 0 |
| Model requests and transport | 68 | 4 | 0 | 23 | 36 | 5 | 0 |
| Snapshots and undo | 15 | 1 | 0 | 0 | 3 | 11 | 0 |
| Sessions and their store | 308 | 10 | 0 | 76 | 168 | 54 | 0 |
| Tool: read | 82 | 65 | 0 | 0 | 1 | 16 | 0 |
| Tool: edit | 22 | 22 | 0 | 0 | 0 | 0 | 0 |
| Tool: write | 19 | 5 | 1 | 1 | 6 | 6 | 0 |
| Tool: apply_patch | 90 | 0 | 0 | 90 | 0 | 0 | 0 |
| Tools: glob and grep | 39 | 21 | 5 | 0 | 11 | 2 | 0 |
| Tool: webfetch | 100 | 100 | 0 | 0 | 0 | 0 | 0 |
| Tool: websearch | 55 | 0 | 0 | 0 | 51 | 4 | 0 |
| Tools: question, todo, skill | 9 | 0 | 0 | 0 | 2 | 7 | 0 |
| Tool output | 17 | 12 | 0 | 0 | 5 | 0 | 0 |
| Tool registry, schemas, placement | 100 | 1 | 0 | 25 | 53 | 21 | 0 |
| Media tools | 39 | 0 | 0 | 24 | 9 | 6 | 0 |
| Tool target environments | 7 | 0 | 0 | 3 | 4 | 0 | 0 |
| Shell tool and runtime | 207 | 87 | 2 | 45 | 58 | 15 | 0 |
| Shell scan and parse | 1465 | 174 | 0 | 0 | 444 | 847 | 0 |
| Permission | 177 | 90 | 0 | 11 | 50 | 26 | 0 |
| Agents | 65 | 34 | 0 | 11 | 14 | 6 | 0 |
| Skills and commands | 59 | 26 | 0 | 1 | 21 | 11 | 0 |
| Instructions and references | 78 | 63 | 0 | 0 | 13 | 2 | 0 |
| Config | 164 | 30 | 0 | 3 | 127 | 4 | 0 |
| MCP | 212 | 1 | 0 | 29 | 61 | 121 | 0 |
| Code Mode | 67 | 26 | 0 | 3 | 19 | 19 | 0 |
| Automations | 107 | 0 | 0 | 15 | 45 | 47 | 0 |
| Formatters | 26 | 14 | 0 | 0 | 3 | 9 | 0 |
| Worktrees, VCS and projects | 158 | 0 | 0 | 27 | 88 | 43 | 0 |
| Providers, models and credentials | 796 | 169 | 0 | 265 | 325 | 37 | 0 |
| Plugins | 90 | 0 | 0 | 23 | 67 | 0 | 0 |
| Storage, state and bus | 189 | 0 | 0 | 0 | 189 | 0 | 0 |
| Runtime, locations and filesystem | 242 | 24 | 0 | 3 | 211 | 4 | 0 |
| **Total** | **5588** | **1076** | **15** | **766** | **2334** | **1397** | **0** |

OpenCode's `packages/core` has no LSP tests (CodeSmith's language-server client is tested on its own in `tests/lsp.rs`). Shell scan and parse is large because its tables expand into many cases; most of what is left there is PowerShell (by design: CodeSmith runs commands with `<shell> -c` and ports only the POSIX grammar) and Bash/Zsh table rows CodeSmith's scanner should already pass but no test runs yet.

## Fixes made while porting

- `ripgrep.test.ts`: "greps lines that are not valid UTF-8" (1 case): grep dropped every line of a file with invalid UTF-8; now decoded lossily. Pinned by `tests/oc_tools.rs::oc_ripgrep_greps_lines_that_are_not_valid_utf8`.
- `session-runner.test.ts`: "explains a compaction blocked by the provider" (1 case): a blocked summary was nudged and retried; it now fails as blocked (adapted: no refusal category). Pinned by `tests/oc_compaction.rs::oc_compaction_explains_a_compaction_blocked_by_the_provider`.
- `session-runner.test.ts`: "rejects compaction ${response} without retrying or committing its draft" (2 cases): the summary's finish reason was ignored, so a summary cut off by the output limit or a content filter replaced the history. Pinned by `tests/oc_compaction.rs::oc_compaction_rejects_a_cut_short_summary_without_retrying_or_committing_it`.
- `session-runner.test.ts`: "publishes the original overflow when recovery summarization fails" (1 case): the failed summary's error was reported instead of the overflow. Pinned by `tests/oc_compaction.rs::oc_compaction_reports_the_original_overflow_when_recovery_fails`.
- `session-runner.test.ts`: "does not recover context overflow after durable assistant output" (1 case): an overflow after partial output was compacted and retried; it now fails the turn. Pinned by `tests/oc_runner.rs::oc_session_does_not_recover_context_overflow_after_output`.
- `session-runner.test.ts`: "retries an unknown finish before output" (1 case): an "unknown" finish ended the turn as a normal stop; it is now retried when no tool ran. Pinned by `tests/oc_runner.rs::oc_session_retries_an_unknown_finish_before_output`.
- `session-runner.test.ts`: "retries an unknown finish cleanly after observable text when no tool ran" (1 case): as above, dropping the text it streamed. Pinned by `tests/oc_runner.rs::oc_session_retries_an_unknown_finish_cleanly_after_text`.
- `shell.test.ts`: "builds command args per shell family" (1 case): POSIX families; Zsh now gets `+o nomatch`. Pinned by `tests/oc_tools.rs::oc_shell_passes_an_unmatched_zsh_glob_through_to_the_command`.
- `shell.test.ts`: "passes an unmatched zsh glob through to the command" (1 case): an unmatched glob failed the whole Zsh line. Pinned by `tests/oc_tools.rs::oc_shell_passes_an_unmatched_zsh_glob_through_to_the_command`.
- `tool-search.test.ts`: "${name} distinguishes a malformed filename glob from an empty search" (2 cases): the glob error read "Invalid glob in pattern error parsing glob ...". Pinned by `tests/oc_tools.rs::oc_search_distinguishes_a_malformed_glob_from_an_empty_search`.
- `tool-search.test.ts`: "${name} reports a missing search path" (2 cases): glob said "not a directory", grep "Unable to grep for ...". Pinned by `tests/oc_tools.rs::oc_search_reports_a_missing_search_path`.
- `tool-write.test.ts`: "preserves exactly one BOM when overwriting existing files" (1 case): overwriting a file dropped its byte order mark. Pinned by `tests/oc_tools.rs::oc_write_preserves_exactly_one_bom`.

## Found bugs and differences

None open: every bug porting found is fixed above.


## Roadmap

Missing features, by how many OpenCode test cases each would unlock (766 cases in 61 groups). Each group lists its tests as `file`: name.

| Feature | Cases | What is missing |
|---|---:|---|
| [Provider Plugins](#provider-plugins) | 97 | GitHub Copilot; Modal; … |
| [Patch Tool (apply_patch)](#patch-tool-applypatch) | 93 | the apply_patch format parser; patch instead of edit/write per model; … |
| [Claude Agent SDK Provider](#claude-agent-sdk-provider) | 82 | Claude Code (Agent SDK) as a provider, with its sessions, tools and sync |
| [Shell Daemon](#shell-daemon) | 39 | the process daemon itself (discovery, registrations, protocol versions, adoption); handing the daemon's processes over to the next server; … |
| [ChatGPT Subscription Login](#chatgpt-subscription-login) | 34 | ChatGPT provider; ChatGPT context caps; … |
| [Image Generation](#image-generation) | 21 | image entrypoint; image_generate tool; … |
| [Media Blob Store](#media-blob-store) | 21 | media stored once as blobs and referenced; media refs |
| [Workspaces](#workspaces) | 21 | OpenCode workspaces (remote/sandboxed environments) |
| [Goal Plugin](#goal-plugin) | 20 | goal watchdog; /goal loop with a reviewer |
| [Prompt-cache Warming](#prompt-cache-warming) | 19 | keeping provider caches warm |
| [Local Model Discovery](#local-model-discovery) | 19 | discovering LM Studio models and context; discovering local Ollama models and metadata; … |
| [OAuth Provider Logins](#oauth-provider-logins) | 18 | refreshable provider credentials; multiple credentials per integration; … |
| [Tool Input Repair](#tool-input-repair) | 17 | repair before validation, other harnesses' tool names, Code Mode calls; schema-guided repair of nested containers, nulls, unions, tuples and dictionaries |
| [MCP Elicitation](#mcp-elicitation) | 16 | forms behind elicitation and questions (conditional fields); elicitation forms and URL elicitations |
| [Native Provider Compaction](#native-provider-compaction) | 16 | routes that cannot compact natively; provider-native compaction endpoints; … |
| [Stateless Generate API](#stateless-generate-api) | 14 | one-shot model calls outside a session with their own retry rules; generate from settled session context without changing history |
| [Compaction Prompt-cache Prefix](#compaction-prompt-cache-prefix) | 14 | summary requests reuse the session's cache prefix and measured input; summary request keeps the session request prefix |
| [Background Compaction Summaries](#background-compaction-summaries) | 14 | precomputed summaries swapped in at compaction |
| [Client-attached MCP Servers](#client-attached-mcp-servers) | 13 | MCP servers attached by a client (ACP/SDK) over the session |
| [Automation Durable Handoffs](#automation-durable-handoffs) | 12 | checkpointed handoffs settled across pauses and expiry; failed checks handed to a quiet child |
| [Plan Mode Plugin (Plan Directory)](#plan-mode-plugin-plan-directory) | 11 | plan files in a Plan directory, enter/leave reminders reconciled with history |
| [Hosted (Provider-executed) Tools in Sessions](#hosted-provider-executed-tools-in-sessions) | 11 | provider-executed tool results in the session; provider-executed results replayed inline; … |
| [Plugin Session API](#plugin-session-api) | 9 | plugins creating child sessions; plugins admitting compaction; … |
| [Session Forking](#session-forking) | 9 | fork a session at a message boundary |
| [User Shell Commands](#user-shell-commands) | 9 | shell command run as a session message; user-run (`!`) background shells entering context via a completion notice; … |
| [Decision Models](#decision-models) | 7 | keyless decision/classifier model calls |
| [Tool-use Policies](#tool-use-policies) | 6 | a deny-only policy layer over permissions (an allow never grants) |
| [Mercurial VCS](#mercurial-vcs) | 6 | Mercurial project markers; Mercurial support |
| [Responses Checkpoint Continuation](#responses-checkpoint-continuation) | 6 | WebSocket continuation checkpoints for compaction triggers; compaction across WebSocket recovery without stale response IDs |
| [Nested Instructions on Read](#nested-instructions-on-read) | 6 | AGENTS.md above a read file injected once |
| [Skill Mentions](#skill-mentions) | 6 | prepared skills attached to a prompt; skill mentions materialized on their prompt |
| [Shell Resolution Rules](#shell-resolution-rules) | 6 | shell name normalization, login shells, fallbacks and blacklists (CodeSmith uses config, then $SHELL, then /bin/sh) |
| [Code Mode Tool Placement Policy](#code-mode-tool-placement-policy) | 5 | per-model direct vs Code Mode tool placement under full_codemode |
| [HTTP Request Hooks](#http-request-hooks) | 4 | plugin hooks on raw HTTP requests/responses |
| [Old Image Eviction](#old-image-eviction) | 4 | oldest images replaced to cap the retained payload |
| [Mid-session Effort Switch](#mid-session-effort-switch) | 4 | recording an effort change as a model-visible update; effort switch recorded as a cache-preserving update |
| [Provider State Across Model Switches](#provider-state-across-model-switches) | 4 | provider metadata kept for the same model, dropped after a switch/abort/failure |
| [Session Read State](#session-read-state) | 4 | viewed/unread watermarks |
| [Automation Event Stream Recovery](#automation-event-stream-recovery) | 3 | reconnect with replay from the last committed cursor, pause after repeated failures |
| [Browser Attachment](#browser-attachment) | 3 | attaching a browser to a session |
| [Config Source Toggles](#config-source-toggles) | 3 | disabling the global config / home-level .claude and agents directories |
| [Session Archiving](#session-archiving) | 3 | archive / unarchive sessions |
| [Attachment Source Paths](#attachment-source-paths) | 3 | local source paths announced next to provider media |
| [Session Context Hooks](#session-context-hooks) | 3 | plugin hook that rewrites the model context |
| [Session Request Headers and Cache Keys](#session-request-headers-and-cache-keys) | 3 | session correlation headers, parent cache affinity, bounded prompt cache keys |
| [OpenCode Self-management Tool](#opencode-self-management-tool) | 3 | the `opencode` tool (models, worktrees, session moves) |
| [Video Generation](#video-generation) | 3 | video_generate tool; video jobs |
| [Voice](#voice) | 3 | voice models / GPT-Live |
| [Repository-scoped External Approvals](#repository-scoped-external-approvals) | 2 | an "always" external approval saved at the enclosing repository; external approval saved at the nearest project directory |
| [Saved Approvals Management](#saved-approvals-management) | 2 | listing, editing and removing saved approvals per project |
| [Plugin Permission API](#plugin-permission-api) | 2 | plugins asserting permissions |
| [Freeform Tool Formats](#freeform-tool-formats) | 2 | custom/freeform tool definitions; execute taking raw code as freeform text |
| [Attachment Deduplication](#attachment-deduplication) | 2 | durable attachment references deduplicate provider media |
| [Code Mode Progress](#code-mode-progress) | 2 | execute progress rows |
| [Built-in Skills](#built-in-skills) | 1 | skills shipped with the app |
| [Compaction Hooks](#compaction-hooks) | 1 | a hook supplies the summary |
| [Per-session Permission Rules](#per-session-permission-rules) | 1 | stored per-session rules inherited by children and forks |
| [Session Subpath Filter](#session-subpath-filter) | 1 | project sessions filtered by subpath |
| [Provider Media Capabilities](#provider-media-capabilities) | 1 | per-provider image formats (xAI) |
| [Directory Attachments](#directory-attachments) | 1 | a directory attached to a prompt, listed in model context |
| [Session Title Search](#session-title-search) | 1 | case-insensitive title search |

### Provider Plugins

97 cases.

- `github-copilot/models.test.ts`: defensively syncs advertised Copilot models
- `github-copilot/models.test.ts`: prices cache reads from either token price spelling
- `modal/models.test.ts`: modal plugin is registered
- `modal/models.test.ts`: maps live Modal models onto catalog templates
- `modal/models.test.ts`: keeps template cost and limits when the proxy omits them
- `modal/models.test.ts`: throws on proxy failure so the plugin can fail soft
- `plugin/provider-azure.test.ts`: hides Azure CLI authentication when the Azure CLI is not installed
- `plugin/provider-azure.test.ts`: asks for the resource name as text without calling Azure
- `plugin/provider-azure.test.ts`: starts without calling Azure, then lists deployments
- `plugin/provider-azure.test.ts`: shows the deployments listed with an API key, named after the deployment
- `plugin/provider-azure.test.ts`: lists Azure CLI deployments through the management API before the resource's own list
- `plugin/provider-azure.test.ts`: falls back to the resource's own list when the Azure CLI cannot read the management API
- `plugin/provider-azure.test.ts`: keeps the catalog for a custom endpoint without listing deployments
- `plugin/provider-azure.test.ts`: publishes the management list only after its last page
- `plugin/provider-azure.test.ts`: keeps the last complete list when a later page fails
- `plugin/provider-azure.test.ts`: reports the management API error when the resource's own list fails too
- `plugin/provider-azure.test.ts`: never sends the management token to a page link outside the management endpoint
- `plugin/provider-azure.test.ts`: keeps a model that is configured explicitly
- `plugin/provider-azure.test.ts`: loads again when the API key changes in place
- `plugin/provider-azure.test.ts`: shows the new account without waiting for the previous account's discovery
- `plugin/provider-azure.test.ts`: authorizes Azure, Foundry, and WebSocket requests with a token for their audience
- `plugin/provider-azure.test.ts`: {item.name} (6 cases)
- `plugin/provider-azure.test.ts`: leaves models unchanged and uncloned without a resource name
- `plugin/provider-azure.test.ts`: prefers WebSockets on the provider for the Azure Responses API only
- `plugin/provider-digitalocean.test.ts`: is registered alongside the other provider plugins
- `plugin/provider-digitalocean.test.ts`: registers browser OAuth alongside generic key and environment methods
- `plugin/provider-digitalocean.test.ts`: loads routers for the signed-in account
- `plugin/provider-digitalocean.test.ts`: loads nothing and logs nothing at startup when the access token has expired
- `plugin/provider-digitalocean.test.ts`: refreshes routers in the background with the latest access token
- `plugin/provider-digitalocean.test.ts`: loads on credential changes and clears the previous account's routers
- `plugin/provider-digitalocean.test.ts`: shows a returning account's cached routers at once and refreshes them in the background
- `plugin/provider-digitalocean.test.ts`: completes browser OAuth independently of router discovery and releases the listener
- `plugin/provider-digitalocean.test.ts`: rejects ${fixture.name} and releases the listener (5 cases)
- `plugin/provider-dynamic.test.ts`: creates an SDK from a provider factory export
- `plugin/provider-dynamic.test.ts`: does not override an SDK already supplied by an earlier plugin
- `plugin/provider-dynamic.test.ts`: loads npm packages through their resolved import entrypoint
- `plugin/provider-dynamic.test.ts`: wraps missing npm entrypoint failures as AISDK init errors
- `plugin/provider-dynamic.test.ts`: wraps dynamic import failures as AISDK init errors
- `plugin/provider-dynamic.test.ts`: wraps missing provider factory exports as AISDK init errors
- `plugin/provider-dynamic.test.ts`: uses the model modelID for the default language model
- `plugin/provider-github-copilot.test.ts`: reloads providers only when a refreshed model list changes
- `plugin/provider-github-copilot.test.ts`: names sessions with the utility model as a free background request
- `plugin/provider-github-copilot.test.ts`: sends the utility title request through the host's fetch
- `plugin/provider-github-copilot.test.ts`: fails the utility title on rate limits so the billable path can run
- `plugin/provider-github-copilot.test.ts`: fails the utility title on an empty completion
- `plugin/provider-github-copilot.test.ts`: routes Auto through Copilot's router and keeps the pick until compaction
- `plugin/provider-gitlab-oauth.test.ts`: is registered alongside the other provider plugins
- `plugin/provider-gitlab-oauth.test.ts`: registers PKCE OAuth alongside the generic key/env methods from ModelsDevPlugin
- `plugin/provider-gitlab-oauth.test.ts`: exchanges a PKCE code for a native GitLab OAuth credential against gitlab.com by default
- `plugin/provider-gitlab-oauth.test.ts`: uses the answered self-managed instance URL, including a relative URL root
- `plugin/provider-gitlab-oauth.test.ts`: fails early for self-managed instances without GITLAB_OAUTH_CLIENT_ID
- `plugin/provider-gitlab-oauth.test.ts`: refreshes an OAuth credential using its stored instance URL and includes redirect_uri
- `plugin/provider-gitlab-oauth.test.ts`: shares one token exchange between concurrent refreshes of the same credential
- `plugin/provider-gitlab-oauth.test.ts`: refreshes credentials without a client ID with the opencode-gitlab-auth application
- `plugin/provider-gitlab-oauth.test.ts`: refreshes only with the recorded client ID when the credential has one
- `plugin/provider-gitlab-oauth.test.ts`: reports a revoked refresh token with a sign-in-again hint instead of authorization-code hints
- `plugin/provider-gitlab-oauth.test.ts`: rejects invalid or denied callbacks (${JSON.stringify(invalid.params)}) (4 cases)
- `plugin/provider-gitlab-oauth.test.ts`: preserves the active connection after a failed token exchange (${response.status}) (3 cases)
- `plugin/provider-gitlab-oauth.test.ts`: reports EADDRINUSE with a clear message when the callback port is occupied
- `plugin/provider-gitlab-oauth.test.ts`: honors GITLAB_OAUTH_CLIENT_ID for self-managed applications
- `plugin/provider-gitlab.test.ts`: discovers workflow models for the location and exposes their refs in the model registry
- `plugin/provider-gitlab.test.ts`: uses OAuth access tokens for discovery
- `plugin/provider-gitlab.test.ts`: sends requests to the instance an OAuth login was issued against
- `plugin/provider-gitlab.test.ts`: does not discover with only an ambient GITLAB_TOKEN or configured apiKey
- `plugin/provider-gitlab.test.ts`: discovers with a stored login without replacing configured model definitions
- `plugin/provider-gitlab.test.ts`: scopes the SDK discovery cache to the active account
- `plugin/provider-gitlab.test.ts`: gives up on unresponsive discovery so later account switches still load
- `plugin/provider-gitlab.test.ts`: skips unauthenticated discovery and refreshes after credential changes, including failures
- `plugin/provider-gitlab.test.ts`: creates SDKs with legacy default instance URL, token env, headers, and feature flags
- `plugin/provider-gitlab.test.ts`: uses GITLAB_INSTANCE_URL when instanceUrl is not configured
- `plugin/provider-gitlab.test.ts`: keeps configured instance URL, apiKey, aiGatewayHeaders, and featureFlags over env/defaults
- `plugin/provider-gitlab.test.ts`: ignores non-GitLab SDK packages
- `plugin/provider-gitlab.test.ts`: uses workflowChat for duo workflow models and preserves selectedModelRef
- `plugin/provider-gitlab.test.ts`: uses exact static workflow model ids when the provider recognizes them
- `plugin/provider-gitlab.test.ts`: uses provider feature flags instead of model settings feature flags
- `plugin/provider-gitlab.test.ts`: uses agenticChat with provider aiGatewayHeaders and feature flags for normal models
- `plugin/provider-openrouter.test.ts`: filters OpenRouter's gpt-5 chat alias
- `plugin/provider-openrouter.test.ts`: does not disable gpt-5-chat-latest for non-OpenRouter providers
- `plugin/provider-sap-ai-core.test.ts`: copies serviceKey option into AICORE_SERVICE_KEY but keeps SDK options to deployment metadata
- `plugin/provider-sap-ai-core.test.ts`: preserves existing AICORE_SERVICE_KEY over serviceKey option
- `plugin/provider-sap-ai-core.test.ts`: omits deployment and resourceGroup SDK options when no service key is available
- `plugin/provider-sap-ai-core.test.ts`: uses the callable SDK for language selection
- `plugin/provider-sap-ai-core.test.ts`: ignores non-SAP AI Core providers

### Patch Tool (apply_patch)

93 cases.

- `patch.test.ts`: parses add, update, and delete hunks
- `patch.test.ts`: parses an empty patch
- `patch.test.ts`: ignores a Codex environment preamble
- `patch.test.ts`: parses an update followed by an add
- `patch.test.ts`: parses a file move
- `patch.test.ts`: identifies the missing patch boundary
- `patch.test.ts`: allows whitespace after the end marker
- `patch.test.ts`: strips a heredoc wrapper
- `patch.test.ts`: strips a heredoc wrapper without cat
- `patch.test.ts`: strips quoted heredoc wrappers
- `patch.test.ts`: rejects malformed heredoc wrappers
- `patch.test.ts`: parses a whitespace-padded hunk header
- `patch.test.ts`: parses leading and trailing whitespace around patch markers
- `patch.test.ts`: parses whitespace on the inner sides of patch marker lines
- `patch.test.ts`: parses relative and absolute hunk paths
- `patch.test.ts`: strips one carriage return from CRLF patch lines
- `patch.test.ts`: preserves an extra carriage return in CRLF patch lines
- `patch.test.ts`: preserves the end-of-file marker
- `patch.test.ts`: allows an end-of-file marker before an explicit chunk
- `patch.test.ts`: allows an end-of-file marker before an implicit chunk and move
- `patch.test.ts`: derives fuzzy line updates while preserving BOM
- `patch.test.ts`: derives multiple update chunks
- `patch.test.ts`: appends a pure-addition chunk to a nonempty file
- `patch.test.ts`: preserves equal-offset insertion order and frozen chunks for %j (2 cases)
- `patch.test.ts`: applies a pure-addition chunk after an earlier replacement
- `patch.test.ts`: applies a deletion-only update chunk
- `patch.test.ts`: updates empty files and adds a trailing newline
- `patch.test.ts`: disambiguates updates with change context
- `patch.test.ts`: matches leading, trailing, and Unicode punctuation differences
- `patch.test.ts`: matches Unicode minus signs and spaces
- `patch.test.ts`: does not normalize ellipses
- `patch.test.ts`: prefers a later exact match over an earlier normalized match
- `patch.test.ts`: matches EOF-anchored chunks from the end
- `patch.test.ts`: does not fall back to a non-EOF match
- `patch.test.ts`: identifies a missing blank line
- `patch.test.ts`: parses an update without an explicit first chunk header
- `patch.test.ts`: keeps indented update markers as context lines
- `patch.test.ts`: keeps indented move and EOF markers as context lines
- `patch.test.ts`: preserves update context indentation
- `patch.test.ts`: preserves bare empty update lines as context
- `patch.test.ts`: rejects invalid add and delete lines
- `patch.test.ts`: rejects an empty update hunk
- `patch.test.ts`: rejects an empty update chunk
- `patch.test.ts`: rejects an invalid update line
- `patch.test.ts`: rejects invalid and pathless hunk headers
- `tool-model-policy.test.ts`: gives GPT-5+ patch and leaves file tools, shell, and websearch in Code Mode when fullCodeMode is on
- `tool-model-policy.test.ts`: keeps edit and write instead of patch for gpt-4 and gpt-oss models
- `tool-model-policy.test.ts`: needs no demotion when fullCodeMode is off while still applying patch and websearch rules
- `tool-patch.test.ts`: registers and sequentially applies add, update, and delete hunks
- `tool-patch.test.ts`: replaces a file with a directory containing an added file
- `tool-patch.test.ts`: counts deleted lines with and without a trailing newline
- `tool-patch.test.ts`: serializes concurrent patch transactions
- `tool-patch.test.ts`: returns file diffs for final formatted content
- `tool-patch.test.ts`: moves and updates a file
- `tool-patch.test.ts`: moves a file over an existing destination
- `tool-patch.test.ts`: moves a file without changing its contents
- `tool-patch.test.ts`: refuses to rewrite a file that is not valid UTF-8 and applies none of the patch
- `tool-patch.test.ts`: renames a file when the move only changes its case
- `tool-patch.test.ts`: moves into a directory whose name differs only in case
- `tool-patch.test.ts`: moves a symlink without deleting its target
- `tool-patch.test.ts`: includes move file info in metadata
- `tool-patch.test.ts`: includes the move destination in edit permission resources
- `tool-patch.test.ts`: uses Location-relative resources for move targets in a nested Location
- `tool-patch.test.ts`: inserts lines with an insert-only hunk
- `tool-patch.test.ts`: rejects deleting a directory
- `tool-patch.test.ts`: rejects a missing second chunk context
- `tool-patch.test.ts`: requires patchText
- `tool-patch.test.ts`: rejects invalid patch format
- `tool-patch.test.ts`: rejects an empty patch
- `tool-patch.test.ts`: rejects an invalid hunk header
- `tool-patch.test.ts`: applies successive update operations to one file
- `tool-patch.test.ts`: preserves files across self-moves and multi-hunk moves in one patch
- `tool-patch.test.ts`: does not invent a first-line diff for BOM files
- `tool-patch.test.ts`: rejects an update with missing context
- `tool-patch.test.ts`: rejects an update when the target file is missing
- `tool-patch.test.ts`: identifies a directory used as an update target
- `tool-patch.test.ts`: identifies a missing delete target
- `tool-patch.test.ts`: reports the failing destination and filesystem error
- `tool-patch.test.ts`: reports the successful prefix and filesystem error
- `tool-patch.test.ts`: reports a destination written before move removal fails
- `tool-patch.test.ts`: approves an external directory before reading and requests edit permission afterward
- `tool-patch.test.ts`: does not inspect an external file when external permission is denied
- `tool-patch.test.ts`: preserves edit permission rejection
- `tool-patch.test.ts`: treats a sibling path inside the project worktree as internal
- `tool-patch.test.ts`: follows an internal symlink to an external file without external permission
- `tool-patch.test.ts`: approves a relative external target before reading and requests edit permission afterward
- `tool-patch.test.ts`: uses canonical external permissions and resources for a move destination
- `tool-patch.test.ts`: approves each external file under the same parent
- `tool-patch.test.ts`: rejects invalid later update before applying an earlier add
- `tool-patch.test.ts`: adds files by overwriting existing targets
- `tool-patch.test.ts`: overwrites an add target that appears during permission approval
- `tool-patch.test.ts`: preserves a later commit defect after earlier sequential applications

### Claude Agent SDK Provider

82 cases.

- `plugin/provider/claude-agent/claude/branches.test.ts`: a failed compaction deletes its temporary branch and keeps the unmodified primary history resumable
- `plugin/provider/claude-agent/claude/branches.test.ts`: an unsupported attachment in changed history fails with its own message before forking
- `plugin/provider/claude-agent/claude/branches.test.ts`: a fork finds its source after reload and rebuilds safely if that transcript has disappeared
- `plugin/provider/claude-agent/claude/branches.test.ts`: undo resends an unchanged question, then branches again using remapped checkpoints
- `plugin/provider/claude-agent/claude/branches.test.ts`: an edited earlier reply forks before it and sends the edited history
- `plugin/provider/claude-agent/claude/branches.test.ts`: cancelling one generate branch leaves another and the primary session usable, and deletes both temporary branches
- `plugin/provider/claude-agent/claude/branches.test.ts`: compaction leaves a parked tool turn running and the next primary request starts from the summary
- `plugin/provider/claude-agent/claude/probe.test.ts`: discovers each model's capacity without sending a prompt
- `plugin/provider/claude-agent/claude/probe.test.ts`: keeps the login and other limits when one model reports an invalid capacity
- `plugin/provider/claude-agent/claude/probe.test.ts`: closes stuck context discovery without turning it into a login failure
- `plugin/provider/claude-agent/claude/probe.test.ts`: times out a stuck startup and closes the process
- `plugin/provider/claude-agent/claude/probe.test.ts`: only 2.1.248 or later is compatible
- `plugin/provider/claude-agent/claude/probe.test.ts`: only a missing credential counts as logged out
- `plugin/provider/claude-agent/claude/recorder.test.ts`: checked-in recordings contain no thinking blocks or deltas
- `plugin/provider/claude-agent/claude/session.test.ts`: exit code %i before a result reports failure and allows cleanup (2 cases)
- `plugin/provider/claude-agent/claude/sessions.test.ts`: streams consecutive turns once, on one process, with per-turn usage
- `plugin/provider/claude-agent/claude/sessions.test.ts`: a missing Claude transcript is rebuilt from history, even after a rejected attachment
- `plugin/provider/claude-agent/claude/sessions.test.ts`: cancellation drains past the interrupt receipt before accepting another turn
- `plugin/provider/claude-agent/claude/sessions.test.ts`: cancelled history resumes after a registry restart without replaying the partial reply
- `plugin/provider/claude-agent/claude/sessions.test.ts`: chat keeps the Claude Code preset when OpenCode's built-in prompt changes
- `plugin/provider/claude-agent/claude/sessions.test.ts`: a turn arriving during idle shutdown waits for the old process, then resumes its session
- `plugin/provider/claude-agent/claude/sessions.test.ts`: cancelling during startup closes it and does not save a nonexistent session
- `plugin/provider/claude-agent/claude/sessions.test.ts`: chat checks compatibility before sending a prompt
- `plugin/provider/claude-agent/claude/sessions.test.ts`: changes model and effort live, and resumes when the output limit changes or is cleared
- `plugin/provider/claude-agent/claude/sessions.test.ts`: titles use their own task prompt in a throwaway session that leaves the chat alone
- `plugin/provider/claude-agent/claude/sessions.test.ts`: conversation branches send the chat's tools and system prompt, and a tool call ends the branch
- `plugin/provider/claude-agent/claude/sessions.test.ts`: closing the registry interrupts and drains a running side call
- `plugin/provider/claude-agent/claude/status.test.ts`: finds claude when it is installed after the plugin starts
- `plugin/provider/claude-agent/claude/status.test.ts`: a reconnect waits for the startup check, then publishes the new login
- `plugin/provider/claude-agent/claude/status.test.ts`: reports a failed startup and recovers on the next check
- `plugin/provider/claude-agent/claude/sync.test.ts`: catches up once, saves transcript boundaries, then continues without replaying history
- `plugin/provider/claude-agent/claude/tools.test.ts`: two tool steps keep one turn alive past the idle timeout and match every result to its call
- `plugin/provider/claude-agent/claude/tools.test.ts`: plugin reload hands over the process with its pending handlers; without a reload it shuts down
- `plugin/provider/claude-agent/claude/tools.test.ts`: closing the registry waits for a running tool call to drain, without answering it
- `plugin/provider/claude-agent/claude/tools.test.ts`: a turn left waiting for tool results stops IDLE_MS after OpenCode stops running the session
- `plugin/provider/claude-agent/claude/tools.test.ts`: a process exit while OpenCode runs tools resumes with their saved results
- `plugin/provider/claude-agent/claude/tools.test.ts`: results that do not answer every waiting call stop the turn and rebuild from the full history
- `plugin/provider/claude-agent/claude/tools.test.ts`: active-turn steering delivers tool feedback and new input through the SDK queue
- `plugin/provider/claude-agent/claude/tools.test.ts`: an aborted tool drains the old turn before sending a fresh user message on the same process
- `plugin/provider/claude-agent/claude/tools.test.ts`: tool images and PDFs reach Claude as image and document content
- `plugin/provider/claude-agent/claude/tools.test.ts`: a last step without tools stops the parked turn and resumes with the saved result
- `plugin/provider/claude-agent/claude/tools.test.ts`: a tool list change lets the parked turn finish and answers calls to removed tools
- `plugin/provider/claude-agent/claude/tools.test.ts`: an unavailable OpenCode server fails closed
- `plugin/provider/claude-agent/language-model/convert.test.ts`: calls to tools we did not offer are left to Claude Code and do not end the step
- `plugin/provider/claude-agent/language-model/convert.test.ts`: a step's finish carries the plan usage Claude Code reported during it
- `plugin/provider/claude-agent/language-model/convert.test.ts`: utility calls keep system instructions separate from conversation content and omit thinking
- `plugin/provider/claude-agent/language-model/convert.test.ts`: preserves byte/base64/URL image and PDF attachments
- `plugin/provider/claude-agent/language-model/error.test.ts`: a recorded failed turn ends in an error after its text, without a finish
- `plugin/provider/claude-agent/language-model/error.test.ts`: a recorded billing failure is an error, not answer text
- `plugin/provider/claude-agent/language-model/error.test.ts`: a rejected weekly window gives its reset date, while allowed windows do not fail
- `plugin/provider/claude-agent/language-model/error.test.ts`: empty successes after API retries report the terminal HTTP status
- `plugin/provider/claude-agent/plugin.test.ts`: skips where opencode cannot start local processes
- `plugin/provider/claude-agent/plugin.test.ts`: starts without models or a claude run, and connecting fills and saves the list
- `plugin/provider/claude-agent/plugin.test.ts`: starts with the saved list and its prices without running claude
- `plugin/provider/claude-agent/plugin.test.ts`: shows no cost for a subscription
- `plugin/provider/claude-agent/plugin.test.ts`: shows Anthropic's prices for an API key login
- `plugin/provider/claude-agent/plugin.test.ts`: shows no cost through a custom base URL, which bills in its own way
- `plugin/provider/claude-agent/plugin.test.ts`: shows a cloud account its cloud's prices
- `plugin/provider/claude-agent/plugin.test.ts`: ignores a malformed saved list and checks claude when already connected
- `plugin/provider/claude-agent/plugin.test.ts`: binds chats to plugin storage, and checks context sizes for models a chat reports without one
- `plugin/provider/claude-agent/plugin.test.ts`: connecting without claude installed asks the user to install it
- `plugin/provider/claude-agent/plugin.test.ts`: connecting while logged out of claude asks the user to sign in
- `plugin/provider/claude-agent/plugin.test.ts`: connecting with an old claude still lists its models
- `plugin/provider/claude-agent/plugin.test.ts`: tags requests by kind, and marks a fork's primary requests with its source
- `plugin/provider/claude-agent/plugin.test.ts`: leaves the output budget to Claude and keeps tool error flags
- `plugin/provider/claude-agent/plugin.test.ts`: passes the agent's role to Claude on every kind of request
- `plugin/provider/claude-agent/plugin.test.ts`: passes opencode's instructions to Claude, except the date and environment Claude Code writes itself
- `plugin/provider/claude-agent/plugin.test.ts`: never retries a turn, since Claude Code already retries API failures
- `plugin/provider/claude-agent/plugin.test.ts`: passes the configured connection to Claude Code, for the login check and for chats
- `plugin/provider/claude-agent/plugin.test.ts`: leaves the user's environment alone when nothing is configured
- `plugin/provider/claude-agent/sync/sync.test.ts`: an interrupted reply continues after the submitted input without hiding earlier edits or missed turns
- `plugin/provider/claude-agent/sync/sync.test.ts`: branches only at a matching checkpoint, never midway through a group of tool calls
- `plugin/provider/claude-agent/sync/sync.test.ts`: chooses all four cases and never resumes a record without history
- `plugin/provider/claude-agent/sync/sync.test.ts`: ignores thinking, metadata and reordered user notes, but notices changed visible text
- `plugin/provider/claude-agent/sync/sync.test.ts`: compares tool inputs independent of key order and keeps tool images with their result
- `plugin/provider/claude-agent/sync/sync.test.ts`: catch-up includes only missed turns and leaves the current request outside the checkpoint
- `plugin/provider/claude-agent/sync/sync.test.ts`: catch-up preserves tool calls, results, images and PDFs, excluding reasoning
- `plugin/provider/claude-agent/tools/tools.test.ts`: names round-trip within Claude's limit, including names that need sanitizing
- `plugin/provider/claude-agent/tools/tools.test.ts`: long subagent descriptions retain the actual OpenCode agent list
- `plugin/provider/claude-agent/tools/tools.test.ts`: execution failures and denied permissions stay errors, and JSON stays intact
- `plugin/provider/claude-agent/tools/tools.test.ts`: tool media remains content blocks and does not consume a newly queued user message

### Shell Daemon

39 cases.

- `persistent-pty-daemon.test.ts`: rediscovers a same-protocol daemon after its registration rotates
- `persistent-pty-daemon.test.ts`: rediscovers a rotated registration when acquiring a subscription
- `persistent-pty-daemon.test.ts`: waits on its own connection until the process exits
- `persistent-pty-daemon.test.ts`: closes the wait connection when the wait is interrupted
- `persistent-pty-daemon.test.ts`: reports a daemon that goes away during a wait as unreachable
- `persistent-pty-daemon.test.ts`: retries a start-required request when connection fails before dispatch
- `persistent-pty-daemon.test.ts`: does not replay a dispatched mutating request when its response is lost
- `persistent-pty-daemon.test.ts`: rejects incompatible daemons without replacing or killing them
- `persistent-pty-daemon.test.ts`: starts fresh instead of failing when an inherited registration is gone or expired
- `persistent-pty-daemon.test.ts`: stops a discovered inherited daemon when adoption fails
- `persistent-pty-daemon.test.ts`: gives up at once on a daemon binary that speaks another protocol, and never starts it again
- `persistent-pty-session.test.ts`: knows a handoff is prepared until a shutdown or until its ticket expires
- `persistent-pty-session.test.ts`: creates, inspects, signals and waits for a pipe process
- `persistent-pty-session.test.ts`: keeps pipe processes and terminals apart
- `persistent-pty-session.test.ts`: reports daemon rejections as request errors and a terminated wait as one
- `persistent-pty-session.test.ts`: prepares a handoff more than once on the same owner connection
- `shell-daemon.test.ts`: runs a command with the server's environment, an absolute cwd and its metadata
- `shell-daemon.test.ts`: pages shell output at UTF-8 boundaries with byte cursors
- `shell-daemon.test.ts`: retries an incomplete live character and consumes malformed bytes at exit
- `shell-daemon.test.ts`: counts lines like the spawner and reads only what reached the file after a write error
- `shell-daemon.test.ts`: bounds what it reads to number lines the daemon has not counted yet
- `shell-daemon.test.ts`: reports a command lost with the daemon as killed, with what it wrote
- `shell-daemon.test.ts`: records successful and failed exits without signalling a finished command
- `shell-daemon.test.ts`: recreates the output directory removed while the Location is open
- `shell-daemon.test.ts`: fails a command the daemon rejected instead of spawning it
- `shell-daemon.test.ts`: stops a timed-out command with SIGTERM, then SIGKILL after the grace period, and keeps it
- `shell-daemon.test.ts`: forgets a command in the daemon before deleting its file, on remove and on eviction
- `shell-daemon.test.ts`: takes its commands back after a restart and leaves other Locations' alone
- `shell-daemon.test.ts`: a Location closed while the server runs keeps enforcing its commands' timeouts
- `shell-daemon.test.ts`: a closed Location and its replacement see the same commands, with one timer and one report each
- `shell-daemon.test.ts`: finishes taking a Location's commands back even if the call that started it is interrupted
- `shell-daemon.test.ts`: the sweep keeps a running command's file, and deletes a finished one's file and forgets it
- `shell-daemon.test.ts`: the spawner recreates the output directory removed while the Location is open
- `shell-daemon.test.ts`: starts the daemon on the first command, never on boot or sweep, and runs it with the server's environment
- `shell-restart.test.ts`: settles a foreground call ${finished ? "whose command finished during the restart" : "whose command still runs"} and resumes its session (2 cases)
- `shell-restart.test.ts`: stops a command ${starting ? "still being created" : "a foreground call waits on"} when the server shuts down without a handoff (2 cases)
- `shell-retention.test.ts`: eviction makes progress past an already-removed shell

### ChatGPT Subscription Login

34 cases.

- `plugin/provider-chatgpt.test.ts`: loads only visible API-supported ChatGPT models
- `plugin/provider-chatgpt.test.ts`: verifies a signed ID token against OpenAI's issuer, client ID, expiry, and sign-in nonce
- `plugin/provider-chatgpt.test.ts`: registers the ChatGPT OAuth method
- `plugin/provider-chatgpt.test.ts`: keeps the Codex browser and headless methods alongside token sharing
- `plugin/provider-chatgpt.test.ts`: stops retries for ${code} from ${source} failures (3 cases)
- `plugin/provider-chatgpt.test.ts`: leaves API-key OpenAI retries alone
- `plugin/provider-chatgpt.test.ts`: stops deterministic ChatGPT stream failures while retrying temporary unavailability
- `plugin/provider-chatgpt.test.ts`: registers a new ChatGPT agent on first sign-in with a loopback callback
- `plugin/provider-chatgpt.test.ts`: waits to show browser success until the sign-in callback succeeds
- `plugin/provider-chatgpt.test.ts`: reauthorizes with the client ID issued to an existing ChatGPT connection
- `plugin/provider-chatgpt.test.ts`: rejects a reauthorization callback that returns a different client ID
- `plugin/provider-chatgpt.test.ts`: moves only token-sharing credentials to the new method ID and caches their saved models
- `plugin/provider-chatgpt.test.ts`: routes ChatGPT over HTTP with only the listed catalog fallback models
- `plugin/provider-chatgpt.test.ts`: keeps the token-sharing safeguards when the token cannot be refreshed at load
- `plugin/provider-chatgpt.test.ts`: keeps the full OpenAI catalog under an API key connection
- `plugin/provider-chatgpt.test.ts`: switches between token-sharing and Codex credentials without mixing their routes
- `plugin/provider-chatgpt.test.ts`: omits the default output limit from OpenAI steps and compaction
- `plugin/provider-chatgpt.test.ts`: ${connection} compaction uses plugin defaults but allows later model and variant overrides (2 cases)
- `plugin/provider-chatgpt.test.ts`: ${connection} sends mid-session reasoning effort switches in a form its backend accepts (2 cases)
- `plugin/provider-chatgpt.test.ts`: selects WebSocket only from explicit policy
- `plugin/provider-openai-context-caps.test.ts`: caps OpenAI models under ${methodID} (3 cases)
- `plugin/provider-openai-context-caps.test.ts`: keeps catalog limits under an API key
- `plugin/provider-openai.test.ts`: registers browser and headless ChatGPT OAuth methods
- `plugin/provider-openai.test.ts`: filters the OpenAI catalog to codex-eligible models under a ChatGPT connection
- `plugin/provider-openai.test.ts`: keeps the full OpenAI catalog under an API key connection
- `plugin/provider-openai.test.ts`: reloads providers on a switch only when the ChatGPT login changes
- `session-error.test.ts`: types the ChatGPT usage limit whichever way the failure arrives
- `session-error.test.ts`: types the ChatGPT workspace usage limit

### Image Generation

21 cases.

- `generate.test.ts`: generates images through the model's image entrypoint with the provider key
- `generate.test.ts`: keeps credential metadata out of image requests
- `generate.test.ts`: downloads URL images and detects their type from the bytes
- `tool-image-generate.test.ts`: is registered only while an image model is available
- `tool-image-generate.test.ts`: is on by default and off with experimental.image_generate: false
- `tool-image-generate.test.ts`: uses the configured model unless the agent picks one, and falls back with a note
- `tool-image-generate.test.ts`: reads input images, asks permission, and saves the result, overwriting an existing file
- `tool-image-generate.test.ts`: saves with the extension of the image's real format
- `tool-image-generate.test.ts`: reports a failed generation in the completion notice
- `tool-image-generate.test.ts`: cancels a running generation through its job
- `tool-image-generate.test.ts`: starts no job when a permission is denied
- `tool-image-generate.test.ts`: validates an explicit model and its support for input images
- `tool-image-generate.test.ts`: passes a model variant through, from the input or from config
- `tool-image-generate.test.ts`: asks webfetch permission before downloading an image URL
- `tool-image-generate.test.ts`: rejects input files that are not images
- `tool-image-generate.test.ts`: lists image models through the models tool type filter
- `tool-image-generate.test.ts`: prefers the session's provider, then the newest
- `tool-image-generate.test.ts`: only considers models that accept input images when images are passed
- `tool-media-target-environment.test.ts`: reads its input and saves its output in the resolved target's environment, not the session's
- `tool-media-target-environment.test.ts`: asks only the deny rules before replacing the file its real extension renames
- `tool-media-target-environment.test.ts`: reads its input and saves its output in the resolved target's environment, not the session's

### Media Blob Store

21 cases.

- `session-projector.test.ts`: hydrates media:&lt;hash> refs on message and inbox reads
- `session-projector.test.ts`: replaces refs to missing blobs with an unavailable placeholder on read
- `session-projector.test.ts`: names ref attachments in kept text without reading their blobs
- `session-projector.test.ts`: keeps media inline unless experimental.media_blob is on
- `session-projector.test.ts`: stores media once and refs it from inbox, message, and tool rows when experimental.media_blob is on
- `session-projector.test.ts`: deletes a cancelled inbox row's blob refs and blobs
- `session-projector.test.ts`: deletes reverted messages' blob refs and the blobs only they referenced
- `session-projector.test.ts`: keeps a blob while a fork references it and deletes it with the last session
- `session-projector.test.ts`: keeps a tool part's blob while a fork references it
- `session-projector.test.ts`: releases a tool's blob when a content update drops its ref
- `session-projector.test.ts`: splits inline media out of a session_part tool row for offline blob backfills
- `session-projector.test.ts`: stores a fork's copies of inline parent media as blobs and leaves the parent's rows inline
- `session-projector.test.ts`: copies inline parent media inline into a fork unless the parent's location stores media as blobs
- `session-projector.test.ts`: moves a legacy inline media message's rewritten content to session_part with its media as a blob
- `session-prompt.test.ts`: stores media refs at admission when the session's config enables media_blob
- `session-prompt.test.ts`: forks a session's inline media into blobs when its location enables media_blob
- `session-prompt.test.ts`: forks inline media inline when the parent's location doesn't enable media_blob, whatever the caller's
- `session-prompt.test.ts`: forks and imports with media inline when the location's instance can't boot
- `session-prompt.test.ts`: records refs on import only for the blobs it took out of its own inline media
- `session-prompt.test.ts`: admits and imports media into blobs by default on a dev-channel server with no config
- `session-prompt.test.ts`: imports media into blobs only where the location enables media_blob

### Workspaces

21 cases.

- `workspace.test.ts`: rejects unregistered workspace providers
- `workspace.test.ts`: creates and persists an ID without provisioning
- `workspace.test.ts`: creates a workspace with a caller-supplied ID
- `workspace.test.ts`: reuses a caller-supplied ID with the same provider
- `workspace.test.ts`: rejects a caller-supplied ID already assigned to another provider
- `workspace.test.ts`: resolves an existing caller-supplied ID before provider lookup
- `workspace.test.ts`: destroys an unprovisioned workspace through the driver with a null binding
- `workspace.test.ts`: succeeds without calling the driver when the workspace does not exist
- `workspace.test.ts`: reports whether destroy removed an existing workspace
- `workspace.test.ts`: starts eager provisioning in the background and lets callers join it
- `workspace.test.ts`: starts lazy provisioning on the first spawn
- `workspace.test.ts`: shares provisioning between concurrent first spawns
- `workspace.test.ts`: keeps shared provisioning alive when a waiter is interrupted
- `workspace.test.ts`: interrupts in-flight provisioning on destroy and fails waiters with NotFound
- `workspace.test.ts`: shares a failed attempt and retries the same workspace ID
- `workspace.test.ts`: persists the workspace lifecycle and reconnects after idle suspension
- `workspace.test.ts`: surfaces wake failures through the spawn error channel
- `workspace.test.ts`: routes file overrides through the workspace connection without spawning processes
- `workspace.test.ts`: falls back to execDefaults over the connection spawner for unprovided overrides
- `workspace.test.ts`: tracks active operations and lastActivity across file overrides
- `workspace.test.ts`: surfaces wake failures during file overrides as Environment.Failed

### Goal Plugin

20 cases.

- `plugin/goal-watchdog.test.ts`: pauses a goal an earlier server process left active when the plugin starts
- `plugin/goal-watchdog.test.ts`: pauses a goal that sits idle past the limit, and not while the Session keeps running
- `plugin/goal-watchdog.test.ts`: keeps waiting while a descendant Session keeps running steps
- `plugin/goal-watchdog.test.ts`: never overwrites what the user did after the loop decided
- `plugin/goal-watchdog.test.ts`: serves the goal over RPC and publishes every change
- `plugin/goal.test.ts`: keeps working in one execution until the reviewer finds the goal met
- `plugin/goal.test.ts`: pauses when ${scenario.name} (2 cases)
- `plugin/goal.test.ts`: keeps going while each round calls tools and finds a new reason
- `plugin/goal.test.ts`: pauses when the reviewer gives no usable verdict twice
- `plugin/goal.test.ts`: waits for a background subagent instead of judging
- `plugin/goal.test.ts`: does not wait on work a descendant owns, since only the descendant is woken
- `plugin/goal.test.ts`: drops the reviewer's verdict when the goal is paused while it runs
- `plugin/goal.test.ts`: reminds the model of the goal after a compaction until a continuation restates it
- `plugin/goal.test.ts`: compacts and asks again when the reviewer's request is too large
- `plugin/goal.test.ts`: pauses when the reviewer's request is too large even after compacting
- `plugin/goal.test.ts`: pauses an active goal when the user stops the session
- `plugin/goal.test.ts`: pauses a goal set by an earlier server process instead of continuing it
- `plugin/goal.test.ts`: does not give a child session its parent's goal
- `plugin/goal.test.ts`: pauses, resumes and clears a goal through its subcommands

### Prompt-cache Warming

19 cases.

- `config/warming.test.ts`: accepts boolean enablement
- `config/warming.test.ts`: decodes custom durations
- `plugin/warming.test.ts`: warms every 4 minutes when the model has no cache.ttl
- `plugin/warming.test.ts`: warms at 90% of the model's cache.ttl by default
- `plugin/warming.test.ts`: doesn't warm a cache that outlives the warming window
- `plugin/warming.test.ts`: keeps an explicit interval
- `plugin/warming.test.ts`: keeps warming after a ping hits the cache
- `plugin/warming.test.ts`: stops warming after a ping misses the cache
- `plugin/warming.test.ts`: resumes warming on the next real request after a miss
- `plugin/warming.test.ts`: doesn't count a ping without cache usage as a miss
- `plugin/warming.test.ts`: keeps warming when a request is sent while a missed ping responds
- `plugin/warming.test.ts`: counts the interval from when the request was sent, not when its response ended
- `plugin/warming.test.ts`: counts the next interval from when a ping was sent
- `plugin/warming.test.ts`: waits for a request running at the due time, then pings while the cache lasts
- `plugin/warming.test.ts`: stops warming when a request outlasts the cache, until the next request
- `plugin/warming.test.ts`: keeps one loop when warming is turned off and on again before the next ping
- `plugin/warming.test.ts`: doesn't ping during a running compaction
- `plugin/warming.test.ts`: stops warming a deleted session
- `plugin/warming.test.ts`: stops every loop when the plugin closes

### Local Model Discovery

19 cases.

- `plugin/provider-lmstudio.test.ts`: is registered as a built-in provider plugin
- `plugin/provider-lmstudio.test.ts`: discovers local language models with their capabilities and effective context
- `plugin/provider-lmstudio.test.ts`: refreshes the catalog when LM Studio models change
- `plugin/provider-lmstudio.test.ts`: discovers from configured endpoints with bearer authentication
- `plugin/provider-lmstudio.test.ts`: discovers with a key saved through /connect as soon as it is saved
- `plugin/provider-lmstudio.test.ts`: keeps sending the integration's env key after models load
- `plugin/provider-lmstudio.test.ts`: shares discovery requests across plugin instances
- `plugin/provider-lmstudio.test.ts`: replaces the credential-gated Models.dev catalog when discovery succeeds
- `plugin/provider-ollama.test.ts`: skips unconfigured default local discovery when the location has no local execution plane
- `plugin/provider-ollama.test.ts`: discovers local completion models and native metadata
- `plugin/provider-ollama.test.ts`: refreshes changed digests and retains inventory through transient failures
- `plugin/provider-ollama.test.ts`: replaces and restores the same-ID Models.dev provider
- `plugin/provider-ollama.test.ts`: reloads layered endpoint and bearer authentication settings
- `plugin/provider-vllm.test.ts`: waits for readiness and discovers official vLLM model metadata
- `plugin/provider-vllm.test.ts`: waits for a slow endpoint instead of dropping its models
- `plugin/provider-vllm.test.ts`: does not report a replaced endpoint's failure against the new one
- `plugin/provider-vllm.test.ts`: refreshes inventory while retaining the last success through transient failures
- `plugin/provider-vllm.test.ts`: replaces and restores same-ID Models.dev entries after an empty success
- `plugin/provider-vllm.test.ts`: reloads layered custom endpoint and bearer authentication settings

### OAuth Provider Logins

18 cases.

- `credential-refresh.test.ts`: saves the refreshed value
- `credential-refresh.test.ts`: returns the stored row when another caller already refreshed it
- `credential-refresh.test.ts`: keeps a row that changed while the refresh ran
- `credential-refresh.test.ts`: resolves undefined for a missing credential
- `credential-refresh.test.ts`: shares failures but allows a later refresh
- `credential-refresh.test.ts`: is due within five minutes of expiry
- `credential-refresh.test.ts`: never expires a token without a lifetime
- `credential.test.ts`: reads key form answers stored in configuration before v2.1 as metadata
- `credential.test.ts`: publishes global events only for observable credential mutations
- `credential.test.ts`: promotes the newest remaining legacy credential when the effective selection is removed
- `credential.test.ts`: activates older credentials without affecting other integrations
- `credential.test.ts`: creates credentials with a requested ID without taking over the current selection
- `oauth-page.test.ts`: escapes bootstrap options embedded in the inline script
- `plugin.test.ts`: refreshes expired OAuth credentials through the context during activation
- `plugin.test.ts`: activates a saved credential through the context
- `plugin/provider-xai.test.ts`: registers device OAuth and API key methods
- `plugin/provider-xai.test.ts`: migrates browser OAuth credentials to the device method
- `plugin/provider-xai.test.ts`: enables xAI Responses WebSockets

### Tool Input Repair

17 cases.

- `plugin/tool-input-repair-integration.test.ts`: repairs tool input before validating its original schema
- `plugin/tool-input-repair-integration.test.ts`: runs a call to another harness's tool name as the matching tool and says so
- `plugin/tool-input-repair-integration.test.ts`: repairs namespaced inner tool input called from Code Mode
- `plugin/tool-input-repair-integration.test.ts`: repairs nested Code Mode calls against the Code Mode input schema
- `plugin/tool-input-repair.test.ts`: preserves valid input identity, nested containers, and unknown properties
- `plugin/tool-input-repair.test.ts`: removes extras only from explicitly closed objects without mutating inputs
- `plugin/tool-input-repair.test.ts`: preserves unknown keys when patterned ownership cannot be determined
- `plugin/tool-input-repair.test.ts`: preserves properties that may belong to composed object schemas
- `plugin/tool-input-repair.test.ts`: removes only optional nonnullable nulls and non-object empty placeholders
- `plugin/tool-input-repair.test.ts`: preserves nulls whose validity is hidden inside compositions
- `plugin/tool-input-repair.test.ts`: preserves optional-looking nulls when the parent composes requirements
- `plugin/tool-input-repair.test.ts`: wraps compatible scalars after repairing array items
- `plugin/tool-input-repair.test.ts`: repairs nested question-like inputs without mutating original containers
- `plugin/tool-input-repair.test.ts`: repairs unique nullable alternatives while preserving accepted union values
- `plugin/tool-input-repair.test.ts`: repairs tuple positions and rest items while preserving valid array identity
- `plugin/tool-input-repair.test.ts`: repairs typed dictionaries and straightforward local references
- `plugin/tool-input-repair.test.ts`: leaves ambiguous unions, compositions, and unsupported roots unchanged

### MCP Elicitation

16 cases.

- `form.test.ts`: validates absolute URI formats without restricting schemes
- `form.test.ts`: returns a terminal cancelled state from ask
- `form.test.ts`: supports the temporary global mcp elicitation owner
- `form.test.ts`: gates required fields and rejects inactive answers via when
- `form.test.ts`: evaluates when against multiselect answers as inclusion
- `form.test.ts`: requires every when condition to match and treats empty when as active
- `form.test.ts`: evaluates neq against multiselect answers as non-inclusion
- `form.test.ts`: treats unanswered when references as false and cascades inactivity
- `form.test.ts`: rejects invalid when definitions at creation
- `form.test.ts`: requires external field acknowledgements
- `form.test.ts`: cleans up created forms when event publication fails
- `form.test.ts`: keeps forms pending when reply event publication fails
- `mcp.test.ts`: accepts empty MCP elicitations without creating forms
- `mcp.test.ts`: acknowledges completed MCP URL elicitations without returning internal content
- `mcp.test.ts`: cancels MCP elicitation forms when the server gives up on them
- `mcp.test.ts`: settles modern MCP URL elicitations when the user confirms

### Native Provider Compaction

16 cases.

- `session-compaction.test.ts`: native compaction fails without a model call on a route that cannot compact
- `session-native-compaction.test.ts`: manual trigger persists and continues, retains earlier users repeatedly, and preserves context on failure/cancellation
- `session-native-compaction.test.ts`: manual and automatic endpoint compaction keep the provider replacement unchanged
- `session-native-compaction.test.ts`: automatic native failures, interruptions, and overflows retain the checkpoint
- `session-native-compaction.test.ts`: compaction hooks supply the summary instead of provider compaction
- `session-native-compaction.test.ts`: retained user budget counts attachments and drops whole oldest messages
- `session-native.test.ts`: reads pre-2.0.15 checkpoint media without rewriting the stored payload
- `session-native.test.ts`: reads mixed legacy and current media without overriding an existing asset
- `session-native.test.ts`: loads and replays a pre-v2.1 providerContext checkpoint with pre-2.0.15 media
- `session-native.test.ts`: a native window round-trips tools, opaque checkpoints and binary media through JSON
- `session-native.test.ts`: advances the native instruction epoch and omits superseded chronological updates after durable replay and provider switches
- `session-native.test.ts`: falls back to an earlier compatible native or local checkpoint
- `session-native.test.ts`: skips a malformed native window and replays the history before it
- `session-runner.test.ts`: restores installed native context with auto disabled and preserves it across fork and revert
- `session-runner.test.ts`: automatically persists native windows, retains earlier users, and waits for fresh usage
- `session-runner.test.ts`: recovers an overflowing native window with another native compaction

### Stateless Generate API

14 cases.

- `generate.test.ts`: retries a stateless call the provider never received
- `generate.test.ts`: does not retry a stateless ${reason._tag} failure the provider may have run (3 cases)
- `generate.test.ts`: does not retry stateless ${reason._tag} failures (3 cases)
- `generate.test.ts`: honors provider backoff and cancels without another generation
- `generate.test.ts`: retries an overloaded provider at most three times
- `generate.test.ts`: attributes each stateless completion without creating a stored session
- `session-generate.test.ts`: generates from fresh settled Session context and records only its usage
- `session-generate.test.ts`: runs generate hooks instead of context hooks
- `session-generate.test.ts`: regenerates a failed partial answer without mutating Session history
- `session-generate.test.ts`: blocks unavailable initial instructions before generation

### Compaction Prompt-cache Prefix

14 cases.

- `session-compaction.test.ts`: forked session compaction reuses the fork root prompt cache key
- `session-compaction.test.ts`: summary cache prefix uses measured input with old reasoning (background=${background}) (2 cases)
- `session-compaction.test.ts`: summary cache prefix still flattens genuinely oversized measured input
- `session-compaction.test.ts`: summary measured recovery shrinks from the rejected measured size
- `session-compaction.test.ts`: summary cache prefix survives hooks that shape every request alike
- `session-compaction.test.ts`: summary resends as text when a hook makes the request larger than the measured count
- `session-compaction.test.ts`: summary measured recovery reuses usage only for the matching agent
- `session-runner.test.ts`: preserves the session request prefix during ${reason} compaction (${route.id}) (6 cases)

### Background Compaction Summaries

14 cases.

- `session-compaction.test.ts`: background summaries stay off when disabled
- `session-compaction.test.ts`: automatic compaction swaps in the background summary without another model call
- `session-compaction.test.ts`: automatic compaction waits for a background summary still being written
- `session-compaction.test.ts`: a model switch waits for the new model's first response before summarizing in the background
- `session-compaction.test.ts`: a background summary that fell too far behind is replaced from a newer response
- `session-compaction.test.ts`: a failed background summary is retried from a newer response, then left until compaction
- `session-compaction.test.ts`: compaction summarizes as usual when the background summary no longer fits the conversation
- `session-compaction.test.ts`: a background summary is swapped in for a different model
- `session-compaction.test.ts`: an overflow compaction swaps in the background summary
- `session-compaction.test.ts`: a background summary starts when the latest response already passed the threshold
- `session-compaction.test.ts`: a background summary keeps a tail longer than twice keep while it stays under 40% of the budget
- `session-compaction.test.ts`: a background summary starts a fixed lead before the threshold, not at a share of it
- `session-compaction.test.ts`: a background summary starts on a window whose threshold is under 64k
- `session-compaction.test.ts`: a background summary waits for a response whose prompt is still cached

### Client-attached MCP Servers

13 cases.

- `mcp-attach.test.ts`: attaches a client MCP server and routes its tools with the session
- `mcp-attach.test.ts`: removes an attached entry when its scope closes
- `mcp-attach.test.ts`: fails an attach whose initial tool list fails and closes its transport
- `mcp-attach.test.ts`: routes an unknown tool on an attached-only name to the client
- `mcp-attach.test.ts`: removes an attached entry when the client closes the connection
- `mcp-attach.test.ts`: stacks a second attach under the same name and keeps the first connected
- `mcp-attach.test.ts`: falls back to the previous connection when the top one closes
- `mcp-attach.test.ts`: falls back to a connection with the same tools without publishing a change
- `mcp-attach.test.ts`: keeps the stored tools when the fallback re-list fails
- `mcp-attach.test.ts`: refreshes the top connection's tools when it reports a change
- `mcp-attach.test.ts`: closing a connection below the top changes nothing visible
- `mcp-attach.test.ts`: removes the entry once every stacked connection closes
- `mcp-attach.test.ts`: hides attached entries from server status and keeps them across config reconciles

### Automation Durable Handoffs

12 cases.

- `automation-event.test.ts`: settles a durable handoff before expiry: ${outcome} (3 cases)
- `automation-event.test.ts`: checkpoints an accepted handoff across a ${pause} pause (2 cases)
- `automation-event.test.ts`: does not checkpoint a rejected handoff or commit its terminal state
- `automation.test.ts`: hands a failed check to one quiet child with the creator's permissions
- `automation.test.ts`: keeps the handoff visible and logs within the UTF-8 output budget
- `automation.test.ts`: bounds failed code output including the pause reason in UTF-8 bytes
- `automation.test.ts`: bad targets and session or agent policy never deliver or commit the rest of the result
- `automation.test.ts`: retries failed admission without committing state or done, then settles the handoff once
- `automation.test.ts`: pauses on a missing handoff directory without rerunning the check

### Plan Mode Plugin (Plan Directory)

11 cases.

- `plugin/plan.test.ts`: reconciles a missing enter reminder into the request and persists it
- `plugin/plan.test.ts`: does nothing when the transcript already has a live enter reminder
- `plugin/plan.test.ts`: reconciles a stale enter reminder with a leave reminder
- `plugin/plan.test.ts`: does nothing for non-plan sessions without plan history
- `plugin/plan.test.ts`: does nothing when a leave reminder already follows the enter reminder
- `plugin/plan.test.ts`: treats reminder text quoted inside a larger message as not live
- `plugin/plan.test.ts`: does not create the Plan directory during activation
- `plugin/plan.test.ts`: allows edits only inside the Plan directory
- `plugin/plan.test.ts`: allows external directories without asking
- `plugin/plan.test.ts`: rewrites blocked mutation failures with the Plan directory
- `plugin/plan.test.ts`: preserves mutation failures unrelated to permissions

### Hosted (Provider-executed) Tools in Sessions

11 cases.

- `session-runner-tool-events.test.ts`: provider-executed success derives content and retains provider result state
- `session-runner-tool-events.test.ts`: commits a hosted tool result when cancellation races with the aggregate lock
- `session-runner-tool-events.test.ts`: provider-executed tool metadata is flattened using the route key
- `session-runner-tool-events.test.ts`: success event data can carry provider-executed native data
- `session-runner.test.ts`: replays durable provider-executed tool results inline in the next request
- `session-runner.test.ts`: records malformed provider-executed input as executed
- `session-runner.test.ts`: durably fails a hosted tool when its provider errors before returning a result
- `session-runner.test.ts`: durably fails a hosted tool left unresolved at normal provider EOF
- `session-runner.test.ts`: fails an unresolved hosted tool before one clean step end
- `session-runner.test.ts`: settles unresolved local and hosted tools before one raw provider failure
- `session-runner.test.ts`: durably fails a hosted tool left unresolved by a raw provider stream failure

### Plugin Session API

9 cases.

- `plugin.test.ts`: creates child sessions from a plugin at the parent's location
- `plugin/session-compact.test.ts`: Effect plugins can admit session compaction
- `plugin/session-compact.test.ts`: Promise plugins can admit session compaction
- `plugin/session-instructions.test.ts`: Effect plugins can put, list, and remove session instruction entries seen by the next model request
- `plugin/session-instructions.test.ts`: Promise plugins can put, list, and remove session instruction entries seen by the next model request
- `plugin/session-list.test.ts`: Effect plugins list sessions with the same pages as Core
- `plugin/session-list.test.ts`: Promise plugins list sessions and follow cursors
- `plugin/session-remove.test.ts`: Effect plugins can remove sessions and their children
- `plugin/session-remove.test.ts`: Promise plugins can remove sessions and their children

### Session Forking

9 cases.

- `session-create.test.ts`: forks a session by replaying a durable fork event into copied projected rows
- `session-create.test.ts`: queues a fork notice after copied history and before the first prompt
- `session-create.test.ts`: keeps a fork untitled when its parent is untitled
- `session-create.test.ts`: replays a fork with stable projected identities
- `session-create.test.ts`: inherits instruction entries when forking
- `session-create.test.ts`: does not copy a running assistant into a fork
- `session-create.test.ts`: copies only settled shell messages into forks
- `session-create.test.ts`: rejects forking an empty session
- `session-create.test.ts`: forks before the selected boundary message

### User Shell Commands

9 cases.

- `session-create.test.ts`: runs a shell command and projects the started/ended shell message
- `session-create.test.ts`: still emits shell ended for a failing command
- `session-runner-message.test.ts`: background user shells enter model context only through their completion notification
- `session-shell.test.ts`: records output and admits one completion without waking the model after exit ${exit} (2 cases)
- `session-shell.test.ts`: records a ${outcome.status} shell and admits its completion without waking the model (2 cases)
- `session-shell.test.ts`: admits a spawn failure without waking the model before failing the caller
- `session-shell.test.ts`: keeps the shell and completion alive when the waiting caller is interrupted

### Decision Models

7 cases.

- `generate.test.ts`: sends keyless local decisions without auth, with images at the body root
- `generate.test.ts`: answers decision questions and reports the provider's cost
- `generate.test.ts`: places decision media where the catalog's media_input says
- `generate.test.ts`: routes a gemini-decisions catalog entry to the Google decisions entrypoint
- `generate.test.ts`: sends a variant's reasoning setting and maps provider rejections
- `generate.test.ts`: returns a typed error for an invalid request instead of crashing
- `generate.test.ts`: rejects models of another type and unknown models

### Tool-use Policies

6 cases.

- `config/policy.test.ts`: preserves last-match precedence for tool.use without granting approvals
- `config/policy.test.ts`: matches tool.use file policies without case where file systems ignore it by default
- `config/policy.test.ts`: denies permissions matched as action:resource
- `config/policy.test.ts`: turns an ask into a deny but never grants
- `config/policy.test.ts`: lets a later allow lift an earlier broad deny
- `config/policy.test.ts`: denies every permission with a bare wildcard

### Mercurial VCS

6 cases.

- `project.test.ts`: prefers git when both git and mercurial metadata exist
- `project.test.ts`: prefers the nearest mercurial marker over an outer git repository
- `project.test.ts`: prefers the nearest git marker over an outer mercurial repository
- `project.test.ts`: returns global id for unreadable mercurial metadata
- `vcs-hg.test.ts`: Mercurial rejects unsupported review modes and bases without requiring hg
- `vcs-hg.test.ts`: Vcs mercurial

### Responses Checkpoint Continuation

6 cases.

- `session-checkpoint-transport.test.ts`: trigger reuses the append baseline and clears it before the next generation
- `session-checkpoint-transport.test.ts`: invalid trigger output does not retry or commit a continuation checkpoint
- `session-checkpoint-transport.test.ts`: trigger recovers over SSE with complete input after the old socket closes and reconnect is rejected
- `session-compaction-transport.test.ts`: compaction survives ${recovery} without stale response IDs (3 cases)

### Nested Instructions on Read

6 cases.

- `session-instructions.test.ts`: injects AGENTS.md files above a read, excludes the Location root, and dedups across reads
- `session-instructions.test.ts`: falls back to CLAUDE.md above a read where a directory has no AGENTS.md
- `session-instructions.test.ts`: does not re-inject paths already recorded in durable session history
- `session-instructions.test.ts`: discovers AGENTS.md on a directory listing, including the listed directory's own, and dedups with a later file read
- `session-instructions.test.ts`: re-injects nested instructions dropped from history by compaction
- `session-instructions.test.ts`: listing the Location root directory injects no instructions

### Skill Mentions

6 cases.

- `session-runner-message.test.ts`: lowers each prepared skill once before the prompt
- `session-runner-message.test.ts`: does not inject skill content for reference-only attachments
- `session-skill.test.ts`: refuses blocked skill mentions and standalone activation without writing their content
- `session-skill.test.ts`: materializes mentioned skills on their owning prompt
- `session-skill.test.ts`: excludes mentioned skills when forking before their prompt
- `session-skill.test.ts`: publishes raw standalone content under the caller-supplied ID without inbox admission

### Shell Resolution Rules

6 cases.

- `shell.test.ts`: normalizes shell names
- `shell.test.ts`: detects login shells
- `shell.test.ts`: falls back when configured shell cannot be resolved
- `shell.test.ts`: falls back for terminal-only shells when compatibility is required
- `shell.test.ts`: resolves the environment shell without retaining a removed executable
- `shell.test.ts`: rejects blacklisted shells case-insensitively

### Code Mode Tool Placement Policy

5 cases.

- `tool-model-policy.test.ts`: treats Anthropic, Claude, and claude-agent models as strong when fullCodeMode is on
- `tool-model-policy.test.ts`: hides glob, grep, and webfetch for every model when fullCodeMode is on
- `tool-model-policy.test.ts`: keeps non-strong and absent models direct when fullCodeMode is on, hiding websearch when unavailable
- `tool-model-policy.test.ts`: keeps strong-model tools direct when full_codemode is off
- `tool-model-policy.test.ts`: places tools per model, propagates Code Mode to MCP instructions, and uses configured ref fallback

### HTTP Request Hooks

4 cases.

- `session-model-request-hooks.test.ts`: tags every Session request kind on http.request and http.response
- `session-model-request-hooks.test.ts`: sends a kept request's original body and a replaced request's new body
- `session-model-request-hooks.test.ts`: keeps hook edits to a tool schema out of later requests
- `session-model-request-hooks.test.ts`: offers the WebSocket executor alongside HTTP hooks and routes the WebSocket hooks

### Old Image Eviction

4 cases.

- `session-model-request.test.ts`: preserves images below the trigger
- `session-model-request.test.ts`: replaces oldest images until the retained payload reaches the target
- `session-model-request.test.ts`: keeps the replaced prefix stable as new images arrive, so the prompt cache survives
- `session-model-request.test.ts`: replaces images nested in tool results

### Mid-session Effort Switch

4 cases.

- `session-runner-message.test.ts`: records a same-model effort switch as an effort update
- `session-runner-message.test.ts`: maps the default variant and no variant to the model default effort
- `session-runner-message.test.ts`: ignores switches that are not effort changes on the requested model
- `session-runner.test.ts`: records a same-model effort switch as a cache-preserving effort update

### Provider State Across Model Switches

4 cases.

- `session-runner-message.test.ts`: drops model-scoped continuation metadata after a model switch but keeps hosted result payloads
- `session-runner-message.test.ts`: preserves provider metadata for a catalog alias with a different API model ID
- `session-runner-message.test.ts`: drops assistant text provider state across model changes, aborts, and provider failures
- `session-runner-message.test.ts`: preserves assistant text provider state for the same model

### Session Read State

4 cases.

- `session-view.test.ts`: copies the latest idle time without changing session recency
- `session-view.test.ts`: keeps a newer completion unread when the viewed watermark is stale
- `session-view.test.ts`: rejects an unknown session
- `session-view.test.ts`: replays viewed state into a fresh database

### Automation Event Stream Recovery

3 cases.

- `automation-event.test.ts`: a broken stream opens on the replacement connection, replaying from the last committed event
- `automation-event.test.ts`: reopens after four healthy streams disconnect, each from its last committed cursor
- `automation-event.test.ts`: pauses after three consecutive reconnects without confirmed stream output

### Browser Attachment

3 cases.

- `browser-attach.test.ts`: a replaced attachment's revoke never removes its replacement's grant
- `browser-attach.test.ts`: disposing the plugin revokes the browser rules before the location finishes closing
- `browser-idle.test.ts`: idle eviction ends the browser attachment and permits a fresh attachment

### Config Source Toggles

3 cases.

- `config/config.test.ts`: excludes home-level claude and agents directories when global is disabled
- `config/config.test.ts`: excludes global config reached through the project walk when global is disabled
- `config/config.test.ts`: excludes missing files under symlinked global roots when global is disabled

### Session Archiving

3 cases.

- `session-archive.test.ts`: archives and unarchives through one durable event each
- `session-archive.test.ts`: filters listed sessions by archive state
- `session-archive.test.ts`: rejects an unknown session

### Attachment Source Paths

3 cases.

- `session-runner-message.test.ts`: exposes admitted local image source paths before provider media
- `session-runner-message.test.ts`: falls back to attachment names for invalid local source paths
- `session-runner-message.test.ts`: does not add attachment location text for non-local provider media

### Session Context Hooks

3 cases.

- `session-runner.test.ts`: executes a tool renamed by a session context hook
- `session-runner.test.ts`: passes stored message metadata to the context hook but not to the model request
- `session-runner.test.ts`: keeps context hook edits to messages without stored metadata

### Session Request Headers and Cache Keys

3 cases.

- `session-runner.test.ts`: adds session correlation headers to model requests
- `session-runner.test.ts`: uses parent cache affinity for child model requests
- `session-runner.test.ts`: bounds 64-character session prompt cache keys

### OpenCode Self-management Tool

3 cases.

- `tool-opencode.test.ts`: groups available models by provider with paging
- `tool-opencode.test.ts`: creates, lists, and removes worktrees for the plugin's project
- `tool-opencode.test.ts`: adapts the worktree session_move hint to execute, direct tools, or neither

### Video Generation

3 cases.

- `tool-video-generate.test.ts`: starts the job, saves every video it returns and lists them all in the notice
- `video-generation.test.ts`: keeps video jobs across restarts and forgets them a day after they finish
- `video-generation.test.ts`: waits for a job to finish and cancels a running one

### Voice

3 cases.

- `voice.test.ts`: uses the first key or env connection Integration lists and never a ChatGPT sign-in
- `voice.test.ts`: starts GPT-Live on OpenCode's hosted endpoint with an OpenCode Console connection only without an OpenAI key
- `voice.test.ts`: lists voice models with availability and default without resolving credentials

### Repository-scoped External Approvals

2 cases.

- `file-access-path.test.ts`: saves external approval at the enclosing repository on the host filesystem
- `tool-write.test.ts`: saves external directory approval at the nearest project directory

### Saved Approvals Management

2 cases.

- `permission.test.ts`: stores and removes saved resources for a project
- `permission.test.ts`: edits saved approvals and enforces the edited rule

### Plugin Permission API

2 cases.

- `plugin/permission-assert.test.ts`: Effect plugins assert permissions and receive outcomes as tool errors
- `plugin/permission-assert.test.ts`: Promise plugins assert permissions with plain input

### Freeform Tool Formats

2 cases.

- `session-model-request-hooks.test.ts`: advertises only supported freeform formats without removing tools or changing the shared definitions
- `tool-execute.test.ts`: execute takes raw code as freeform text, and Code Mode calls freeform tools with a string

### Attachment Deduplication

2 cases.

- `session-runner-message.test.ts`: deduplicates provider media while preserving durable attachment references
- `session-runner-message.test.ts`: preserves provider media with distinct labels or URI sources

### Code Mode Progress

2 cases.

- `tool-execute.test.ts`: execute sends at most one progress update per 100 ms with the latest rows
- `tool-execute.test.ts`: interrupted execute sends its latest progress rows before it settles

### Built-in Skills

1 cases.

- `plugin/skill.test.ts`: registers built-in skills

### Compaction Hooks

1 cases.

- `session-compaction.test.ts`: compaction hooks can supply the summary instead of the model

### Per-session Permission Rules

1 cases.

- `session-create.test.ts`: stores permission rules, inherits them through children and forks, and replaces them

### Session Subpath Filter

1 cases.

- `session-create.test.ts`: filters project sessions by subpath

### Provider Media Capabilities

1 cases.

- `session-model-request.test.ts`: replaces images xAI cannot decode and keeps png, jpeg and webp

### Directory Attachments

1 cases.

- `session-runner-message.test.ts`: exposes admitted reference directory source paths in model context

### Session Title Search

1 cases.

- `session-store.test.ts`: searches titles case-insensitively beyond ASCII and keeps LIKE wildcard semantics

## Porting backlog

Tests for behaviour CodeSmith already has but no test pins (1397 cases), by area, with what to drive and assert. Sizes: S a few lines on an existing harness, M a new fixture or scripted model.

### Session runner (67)

- accepted transport reads retry, accepted writes and rejected requests do not (S) — `retry::classify`
  - `session-error.test.ts`: retries accepted transport reads but not accepted writes or rejected requests
- running tool progress reaches the front end until the call settles (S) — `tool::Progress via Sink`
  - `session-live-tool-metadata.test.ts`: adds progress to running tool parts until the call settles
- step cost from the matching context tier; malformed cost fields ignored; no tier without base price (S) — `stats / usage cost`
  - `session-runner.test.ts`: calculates step cost using the matching context tier
  - `session-runner.test.ts`: ignores malformed model cost fields
  - `session-runner.test.ts`: does not apply an ineligible tier without base pricing
- the title is generated while the first step runs, once per session (M) — `tests/parity.rs`
  - `session-runner.test.ts`: generates the title while the first model step is still running
  - `session-runner.test.ts`: coalesces title generation while a request is active
- an existing title is not replaced (S) — `tests/parity.rs`
  - `session-runner.test.ts`: does not automatically replace an existing session title
- the idle hook can admit input and runs once the session would go idle (M) — `plugin::Plugin::idle`
  - `session-runner.test.ts`: continues instead of going idle when a session idle hook admits input (resume: ${resume}) (2 cases)
  - `session-runner.test.ts`: runs the session idle hook only once the session would go idle
  - `session-runner.test.ts`: passes background work of the session and its descendants to the idle hook
  - `session-runner.test.ts`: runs the idle hook again when a background notice wakes the session
- the model-family prompt is used when the agent has no (or an empty) prompt override (S) — `prompt::system`
  - `session-runner.test.ts`: uses the selected model family prompt when the agent does not override it
  - `session-runner.test.ts`: uses the selected model family prompt when the agent system override is empty
- the selected/default agent's system prompt comes before durable context (S) — `prompt::system`
  - `session-runner.test.ts`: includes the effective default agent system before durable context
  - `session-runner.test.ts`: uses the configured default agent system for omitted-agent sessions
  - `session-runner.test.ts`: uses an explicitly selected non-build agent system
- skills listed for the newly selected agent after an agent switch (M) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: updates selected-agent skill instructions after an agent switch
- compaction stops after the one template reminder fails (S) — `tests/parity.rs`
  - `session-runner.test.ts`: compaction ${summary ? "recovers" : "stops"} after one template reminder for ${response} (3 cases)
- compaction drafts retried after transient failures, bounded across a template correction (M) — `tests/parity.rs`
  - `session-runner.test.ts`: restarts compaction drafts after transient failures and unsuccessful finishes
  - `session-runner.test.ts`: bounds compaction network retries across a template correction
- cancelled/failed manual compaction recorded without a model call (S) — `tests/parity.rs`
  - `session-runner.test.ts`: records cancelled manual compaction without surfacing an internal failure
  - `session-runner.test.ts`: settles an admitted manual compaction when pre-start resolution throws
  - `session-runner.test.ts`: records manual compaction model resolution failures without calling the model
- the advertised output limit alone does not trigger compaction (S) — `compaction::should_compact`
  - `session-runner.test.ts`: does not compact immediately when the advertised output limit fills the context
- a required automatic compaction that fails stops the turn (S) — `tests/parity.rs`
  - `session-runner.test.ts`: stops after required automatic compaction fails
- overflow recovery without a configured limit, before an undersized limit, once only, from a raw overflow (M) — `tests/parity.rs`
  - `session-runner.test.ts`: compacts before requesting an undersized configured context limit
- output limit fitted to the prompt / capped / summary keeps its room (S) — `compaction`
  - `session-runner.test.ts`: fits the output limit to the prompt size
  - `session-runner.test.ts`: caps the output limit a large model advertises
  - `session-runner.test.ts`: gives the summary its full output limit when the conversation overshot the threshold
  - `session-runner.test.ts`: keeps the summary its room on a small window
- the original overflow is reported when recovery fails; interrupting recovery (M) — `tests/parity.rs`
  - `session-runner.test.ts`: interrupts overflow recovery while the summary provider is running
- continue after settling a local tool; full stream consumed before tools settle (S) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: continues with reloaded history after durably settling one local tool call
  - `session-runner.test.ts`: reloads a model switch before a tool-driven continuation step
  - `session-runner.test.ts`: consumes the full provider stream before recording its boundary and settling local tools
  - `session-runner.test.ts`: starts recorded local tools eagerly and awaits settlement before continuing
- reasoning/text provider metadata replayed in the next request (M) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: restores durable reasoning provider metadata in the next request
  - `session-runner.test.ts`: keeps one durable reasoning part when reasoning closes after text
  - `session-runner.test.ts`: restores durable text provider metadata in the next request
- repeated tool-call IDs settle against their own assistant message (M) — `runner history`
  - `session-runner.test.ts`: settles repeated provider-local tool call IDs against their owning assistant messages
- hosted tools / pending tool input left by a prior process are failed before continuing (M) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: durably fails hosted tools left running by a prior process before continuing inline
  - `session-runner.test.ts`: durably fails pending tool input left by a prior process before continuing
- tool defects and policy blocks returned to the model, which continues (S) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: returns tool-wrapped policy blocks to the model and continues
- provider errors (before or after step start, raw stream failures) end the step as a visible failure (S) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: persists raw finish reasons and provider state
  - `session-runner.test.ts`: projects raw provider stream failures as terminal assistant step failures
- no new attempt after interruption during backoff (M) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: does not start another physical attempt after interruption during retry backoff
- unknown finishes, incomplete streams and mid-stream failures retried cleanly from the prompt (S) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: retries cleanly after ${failure.name} during reasoning (2 cases)
- retries share one retry allowance and one assistant identity (M) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: shares retry accounting and assistant identity across transparent retries and retries after output
- malformed streamed tool input settles before the provider failure (S) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: settles malformed streamed tool input before the provider failure
- a valid sibling call runs before malformed input is recovered (S) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: settles a valid sibling before recovering malformed tool input
  - `session-runner.test.ts`: does not recover malformed input after sibling execution is interrupted
- provider failure after malformed input or a local tool call is recorded, no automatic continuation (S) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: records a provider failure after malformed input
- a tool defect / output persistence failure is kept alongside the provider failure (M) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: preserves a tool defect before provider failure settlement
  - `session-runner.test.ts`: preserves the provider failure when tool output persistence also fails
- partial text/reasoning/tool input closed when the stream is interrupted (M) — `tests/runner_parity.rs`
  - `session-runner.test.ts`: durably closes partial ${kind} when the provider stream is interrupted (3 cases)
- background shells run alongside the model and finish on their own (M) — `tool::shell::Jobs`
  - `session-shell.test.ts`: runs shells concurrently with an active model and waits for each shell's own completion
  - `session-shell.test.ts`: does not suppress a normal prompt wake while a shell is running or wake again when it completes
- each finish reason settles with the requested tool choice (S) — `tests/runner_parity.rs`
  - `session-step.test.ts`: settles ${fixture.finish} with tool choice ${fixture.toolChoice ?? "default"} (3 cases)
- the endpoint-reported cost wins over the catalog price (S) — `stats`
  - `session-step.test.ts`: records the cost the endpoint reports instead of the catalog price

### Subagents (1)

- the child runs on an explicitly requested model (S) — `tests/runner_parity.rs`
  - `tool-subagent.test.ts`: runs the child on an explicitly requested model

### Compaction (1)

- tool media described without base64 (S) — `compaction::transcript`
  - `session-compaction.test.ts`: compaction describes tool media without embedding base64

### Titles (5)

- commentary (non-final response phase) excluded from titles (S) — `title`
  - `session-title.test.ts`: excludes commentary from titles when the response phase arrives at ${timing} (2 cases)
- explicitly requested child sessions get titles (S) — `tests/parity.rs`
  - `session-title.test.ts`: generates a title for an explicitly requested child session
- no title when the title agent is disabled (S) — `agent config`
  - `session-title.test.ts`: does not generate when the title agent is removed
- a failed title stream does not rename (S) — `tests/parity.rs`
  - `session-title.test.ts`: does not rename after a failed title stream

### Model requests and transport (5)

- unsupported media replaced with a visible error, including in tool results (M) — `runner history`
  - `session-model-request.test.ts`: replaces unsupported user media with a visible error
  - `session-model-request.test.ts`: replaces unsupported media nested in tool results
  - `session-model-request.test.ts`: preserves supported media
  - `session-model-request.test.ts`: returns the same messages when nothing is unsupported
  - `session-model-request.test.ts`: rebuilds only messages with unsupported media

### Snapshots and undo (11)

- diff of the busy period containing a user message (M) — `snapshot::Snapshots::diff`
  - `session-diff.test.ts`: diffs the busy period containing a user message and ranges across later turns
- capture/restore, restoring many files and removing absent ones, partial failure, unavailable outside Git (S) — `snapshot::Snapshots`
  - `snapshot.test.ts`: captures and restores Location-scoped changes
  - `snapshot.test.ts`: restores many files from several trees and removes paths absent from them
  - `snapshot.test.ts`: restores the other files when removing one path fails
  - `snapshot.test.ts`: treats capture outside Git as unavailable
- corrupt index / stale lock, odd directory names, symlinked or case-different paths, long selections, per-worktree indexes (M) — `snapshot::Snapshots`
  - `snapshot.test.ts`: recovers from a corrupt index and ignores a stale index lock
  - `snapshot.test.ts`: captures a Location in a directory whose name starts with two dots
  - `snapshot.test.ts`: captures and restores a Location opened through a symlinked path
  - `snapshot.test.ts`: captures a Location opened with different letter case
  - `snapshot.test.ts`: restores and diffs a selection too long for one command line
  - `snapshot.test.ts`: isolates snapshot indexes by canonical Git worktree

### Sessions and their store (54)

- a missing title stays unset until generated or supplied (S) — `store::Store`
  - `session-create.test.ts`: persists a missing title until one is generated or supplied
- fresh ID when omitted; a retried ID returns the original (S) — `store::Store::create`
  - `session-create.test.ts`: creates a fresh projected session when the ID is omitted
  - `session-create.test.ts`: returns the original session when the ID is retried
  - `session-create.test.ts`: returns the existing Session when one ID is reused with different create arguments
  - `session-create.test.ts`: returns one recorded session to concurrent exact retries
- a trailing-slash directory lists with the same sessions (S) — `store::Store::list`
  - `session-create.test.ts`: stores a trailing-slash directory in the same session list
- children inherit the parent's directory; a missing parent is rejected (S) — `store::Store`
  - `session-create.test.ts`: inherits location from an existing parent when omitted
  - `session-create.test.ts`: rejects child creation when the parent does not exist
- root sessions filtered before the page limit; direct children by parent (S) — `store::Store::list`
  - `session-create.test.ts`: filters root sessions before applying the page limit
  - `session-create.test.ts`: filters direct child sessions by parent ID
- sessions ordered by latest activity (S) — `store::Store::list`
  - `session-create.test.ts`: orders sessions by their latest prompt
- agent/model switches stored; unchanged model ignored; missing session rejected (S) — `store::Store::set_selection`
  - `session-create.test.ts`: switches the selected agent through the durable Session event
  - `session-create.test.ts`: rejects an agent switch for a missing Session
  - `session-create.test.ts`: switches the selected model through the durable Session event
  - `session-create.test.ts`: ignores a model switch when the selected model is unchanged
  - `session-create.test.ts`: treats an omitted variant as the default variant
  - `session-create.test.ts`: rejects a model switch for a missing Session
- only settled messages exported and imported (S) — `store::Store::export / import`
  - `session-create.test.ts`: exports only settled projected messages
  - `session-create.test.ts`: imports only settled projected messages
- importing over an existing session ID is rejected without changing it (S) — `store::Store::import`
  - `session-create.test.ts`: rejects an existing session ID without changing its transcript
- instructions load without any read (S) — `instructions::load`
  - `session-instructions.test.ts`: loads instructions directly without a read
- a new prompt after undo drops the undone turns (M) — `tests/runner_parity.rs`
  - `session-prompt.test.ts`: commits a staged revert before admitting a new prompt
  - `session-prompt.test.ts`: holds synthetic input behind a staged revert and discards it when committed
- attachment MIME, file content and images materialized at admission (M) — `session prompt with attachments`
  - `session-prompt.test.ts`: resolves attachment MIME before admission
  - `session-prompt.test.ts`: materializes selected source file content
  - `session-prompt.test.ts`: materializes directories as directory attachments
  - `session-prompt.test.ts`: materializes local image content before admission
  - `session-prompt.test.ts`: admits captured image bytes with their source path after the source disappears
- large images normalized; undecodable images passed through; data URL content sniffed (S) — `media::prepare_image`
  - `session-prompt.test.ts`: normalizes large image content before validating persisted Base64
  - `session-prompt.test.ts`: passes images the resizer cannot decode through unchanged
  - `session-prompt.test.ts`: sniffs data URL content instead of trusting its declared MIME
- malformed or oversized inline attachments rejected without echoing bytes (S) — `media`
  - `session-prompt.test.ts`: rejects malformed base64 data URLs
  - `session-prompt.test.ts`: rejects oversized inline attachments without echoing their bytes
- prompting an unknown session fails (S) — `session`
  - `session-prompt.test.ts`: fails for an unknown session
- deleting a session removes its children (S) — `store::Store::delete`
  - `session-remove.test.ts`: removes a session and its children
- deleting a missing session reports it (S) — `store::Store::delete`
  - `session-remove.test.ts`: fails when the session does not exist
- text attachments follow the prompt in one user message, order kept, empty prompt text omitted (S) — `session prompt with attachments`
  - `session-runner-message.test.ts`: lowers text attachments after the prompt in one user message
  - `session-runner-message.test.ts`: preserves attachment order after the prompt
  - `session-runner-message.test.ts`: omits empty prompt text before an attachment
- inline text attachment content is decoded (S) — `session prompt with attachments`
  - `session-runner-message.test.ts`: decodes inline text attachment content
- image/PDF attachments and tool media sent as provider media, not base64 text (M) — `tests/media_session.rs`
  - `session-runner-message.test.ts`: uses materialized image and PDF data as provider media
  - `session-runner-message.test.ts`: replays durable tool media into canonical tool messages without structured base64
- OpenAI encrypted reasoning metadata replayed on the next request (M) — `runner history`
  - `session-runner-message.test.ts`: restores OpenAI encrypted reasoning metadata
- the error names the network when the host is unreachable (S) — `retry::message`
  - `session-runner-retry.test.ts`: names the network when the provider's host can't be reached
- tool media serialized once (S) — `tests/media_session.rs`
  - `session-runner-tool-events.test.ts`: local tool success serializes media base64 once through canonical content
- an interrupted subagent's failure names its child session; other tools' progress IDs are not exposed (M) — `tests/runner_parity.rs`
  - `session-runner-tool-events.test.ts`: interrupted subagent failures expose their existing child session to the model
  - `session-runner-tool-events.test.ts`: interrupted non-subagent failures do not expose their progress session IDs
- a content-filter finish keeps the partial text and fails the step (S) — `tests/runner_parity.rs`
  - `session-runner-tool-events.test.ts`: content-filter finish retains failure evidence until step closeout
  - `session-runner-tool-events.test.ts`: content-filter finish preserves partial streamed text and never ends the step successfully
- activity and tool reliability aggregated over a range (M) — `stats`
  - `session-stats.test.ts`: aggregates activity and tool reliability without reading message payloads outside the range
- renders the default system prompt (S) — `prompt::system`
  - `session-system-prompt.test.ts`: renders the default system prompt instructions
- catalog price when the endpoint reports none; reported cost wins; reported zero kept (S) — `stats / usage cost`
  - `session-usage.test.ts`: prices the tokens from the catalog when the endpoint reports no cost
  - `session-usage.test.ts`: records the cost the endpoint reports instead of the catalog price
  - `session-usage.test.ts`: keeps a reported zero cost, as for a request a subscription covered

### Tool: read (16)

- paging, past-end and negative offsets, long lines, byte budgets, NUL detection, lossy UTF-8, symlinks, directory listings (S) — `tool::read`
  - `tool-read-filesystem.test.ts`: applies the default line limit to small files at physical line boundaries
  - `tool-read-filesystem.test.ts`: applies the byte limit to the returned content of small files
  - `tool-read-filesystem.test.ts`: reads extensionless and renamed GIFs as images
  - `tool-read-filesystem.test.ts`: returns a listing when read reports a directory
  - `tool-read-filesystem.test.ts`: lists unresolved symlinks, including broken and escaping links
  - `tool-read-filesystem.test.ts`: reads a symlinked directory as a listing
  - `tool-read-filesystem.test.ts`: rejects a negative offset for a directory
  - `tool-read-filesystem.test.ts`: enforces line and byte budgets with continuation offsets
  - `tool-read-filesystem.test.ts`: sorts and pages directory entries
  - `tool-read-filesystem.test.ts`: stops checking for null bytes after the requested page
  - `tool-read-filesystem.test.ts`: ignores a NUL after a chunked page but rejects it for a past-end offset
  - `tool-read-filesystem.test.ts`: keeps a split byte order mark in a past-end tail and strips it at a negative offset
  - `tool-read-filesystem.test.ts`: checks skipped lines for null bytes
  - `tool-read-filesystem.test.ts`: reads page two after fetching more than the first 256KB range
  - `tool-read-filesystem.test.ts`: reads after a newline at the first chunk boundary
  - `tool-read-filesystem.test.ts`: preserves the media ingestion limit message

### Tool: write (6)

- writes create parents, keep one BOM, external targets, concurrent writes serialize (S) — `tool::write`
  - `file-mutation.test.ts`: writes an existing internal file and returns a stable result
  - `file-mutation.test.ts`: writes a prospective internal file and creates parent directories
  - `file-mutation.test.ts`: preserves exactly one BOM for text writes and normalizes created text
  - `file-mutation.test.ts`: writes an explicitly resolved external target
  - `file-mutation.test.ts`: serializes concurrent writes to the same absolute target
- the committed file is formatted (S) — `tool::write + format`
  - `tool-write.test.ts`: formats the committed file

### Tools: glob and grep (2)

- wildcard directory rules from .gitignore are honoured (S) — `tool::glob`
  - `filesystem/search.test.ts`: honors wildcard directory rules from .gitignore
- a scan from home is bounded (S) — `tool::glob`
  - `filesystem/search.test.ts`: bounds a home scan even when home is detected as a repository

### Tool: websearch (4)

- permission asked before searching (S) — `tool::websearch`
  - `tool-websearch.test.ts`: asserts permission before delegating to WebSearch
- flat results (S) — `tool::websearch`
  - `tool-websearch.test.ts`: returns flat results and keeps the provider in metadata
- the no-results fallback text (S) — `tool::websearch`
  - `tool-websearch.test.ts`: uses the concise no-results fallback
- HTTP failures reported safely (S) — `tool::websearch`
  - `tool-websearch.test.ts`: reports safe HTTP failures with the attempted provider

### Tools: question, todo, skill (7)

- one item schema for the questions array (S) — `question schema`
  - `tool-question.test.ts`: emits one item schema for the nonempty questions array
- a denied question is not offered (S) — `permission + question`
  - `tool-question.test.ts`: omits a catalog-denied question and enforces its leaf permission
- answers returned to the model without a permission prompt (S) — `question`
  - `tool-question.test.ts`: registers question and projects user answers without a permission assertion
- dismissed questions stay out of model-facing output (S) — `question`
  - `tool-question.test.ts`: keeps dismissed questions out of model-facing output
- a denied skill is refused before loading (S) — `tool skill + permission`
  - `tool-skill.test.ts`: refuses a policy-blocked skill before authorization or content preparation
- lists skills and loads the chosen one's content (S) — `tool skill`
  - `tool-skill.test.ts`: lists available skills, authorizes the selected ID, and loads model-facing content
- todowrite summarizes the update (S) — `tool::todo`
  - `tool-todo.test.ts`: registers todowrite on the native tool list and summarizes the update

### Tool registry, schemas, placement (21)

- a stringified root object parsed (S) — `tool::repair`
  - `plugin/tool-input-repair.test.ts`: parses root objects and repairs nested stringified containers
- numeric and boolean strings coerced (S) — `tool::repair`
  - `plugin/tool-input-repair.test.ts`: coerces numeric and boolean strings while preserving invalid and existing values
- execute offered only with Code Mode; tools direct when execute is denied (S) — `tool registry + codemode`
  - `tool-registry.test.ts`: are not available without full_codemode
  - `tool-registry.test.ts`: are available with full_codemode
  - `tool-registry.test.ts`: keeps execute available without Code Mode tools unless explicitly denied
  - `tool-registry.test.ts`: exposes Code Mode tools directly when execute is denied
- disabled tools with edit aliases and ordered wildcard precedence (S) — `tool registry`
  - `tool-registry.test.ts`: filters disabled tools with edit aliases and ordered wildcard precedence
- a hook renaming a call, including inside Code Mode (M) — `plugin::Plugin::tool_before`
  - `tool-registry.test.ts`: normalizes ${input.name} after the final tool hook (4 cases)
  - `tool-registry.test.ts`: a hook can rename a call inside a Code Mode program to another of its tools
  - `tool-registry.test.ts`: a call renamed inside a Code Mode program returns the renamed tool's structured output
  - `tool-registry.test.ts`: hooks see one ID per call inside a Code Mode program
- JSON-schema validation of tool input and readable failures (S) — `tool schema validation`
  - `tool-schema.test.ts`: portable schemas validate and describe typed tools
  - `tool-schema.test.ts`: portable schema failures become tool failures
  - `tool-schema.test.ts`: input error prompts limit normalized issues
  - `tool-schema.test.ts`: raw JSON schemas validate and decode tool input
  - `tool-schema.test.ts`: raw JSON schemas resolve draft-07 definitions
  - `tool-schema.test.ts`: raw JSON schemas pass input through when they cannot be imported
  - `tool-schema.test.ts`: missing external input schemas fall back to an empty schema

### Media tools (6)

- byte sniffing: text fallback, BM text, BMP prefixes, ISO-BMFF brands (S) — `media::detect`
  - `mime.test.ts`: leaves samples without media headers to the text fallback: %j (3 cases)
  - `mime.test.ts`: keeps ordinary text starting with BM as text
  - `mime.test.ts`: detects Windows and OS/2 bitmaps from a prefix
  - `mime.test.ts`: tells ISO-BMFF brands apart

### Shell tool and runtime (15)

- expired output files removed across projects (S) — `tool::shell output files`
  - `shell-cleanup.test.ts`: removes expired output files across projects
- output file and shell named, truncated tail notice, no-output wording, killed without exit code (S) — `tool::shell::notice / result`
  - `shell-result.test.ts`: names the output file and the shell even when the output fits
  - `shell-result.test.ts`: keeps a truncated tail's own notice
  - `shell-result.test.ts`: says once that there was no output, and that a command still stopping is not done
  - `shell-result.test.ts`: reports a command killed without an exit code or signal of its own
- parallel shells from one Code Mode program (M) — `codemode + tool::shell`
  - `tool-shell.test.ts`: returns both parallel CodeMode shell results
  - `tool-shell.test.ts`: runs parallel Code Mode shells with the long default and notifies after a shorter timeout
- a user stop ends the call with the tail, file and shell ID, before or while it starts; SIGTERM-ignoring commands (M) — `tool::shell::Jobs::cancel`
  - `tool-shell.test.ts`: stops a command the user stopped, keeps its file and fails the call with the tail, file and shell ID
  - `tool-shell.test.ts`: ends a call the user stopped before its command started, without starting it
  - `tool-shell.test.ts`: ends a call the user stopped while its working directory resolved, without starting it
  - `tool-shell.test.ts`: says once that a stopped command wrote nothing
  - `tool-shell.test.ts`: settles a stopped call without waiting for a command that ignores SIGTERM
  - `tool-shell.test.ts`: stops a command the user stopped while it was being created
  - `tool-shell.test.ts`: stops a command whose call an interrupt cut off, with no handoff to take it over
- an after hook replaces a stopped call's failure (S) — `plugin::Plugin::tool_after`
  - `tool-shell.test.ts`: lets an execute.after hook replace a stopped call's failure

### Shell scan and parse (847)

- resources, saved prefixes and directories table (POSIX rows) (M) — `shell_scan::scan`
  - `shell-parse-compatibility.test.ts`: extracts resources, saved prefixes, and directories: %s %s %j (28 cases)
- prefix for long argument lists (S) — `shell_scan::scan`
  - `shell-parse-compatibility.test.ts`: derives the prefix for long argument lists
- former tree-sitter gaps table (S) — `shell_scan::scan`
  - `shell-parse-compatibility.test.ts`: {fixture.name} (11 cases)
- the shared directory command set (S) — `shell_scan::scan`
  - `shell-parse-compatibility.test.ts`: retains the shared directory command set: %s (3 cases)
- generated supported syntax has valid saved prefixes (M) — `shell_scan::scan`
  - `shell-parse-parity.test.ts`: scans generated supported syntax with valid saved prefixes and no fallback
- resources/prefixes/directories in bash and zsh (M) — `shell_scan::scan`
  - `shell-parse-parity.test.ts`: extracts resources, saved prefixes, and directories in %s: %s (22 cases)
- commandless redirects keep matching saved prefixes (S) — `shell_scan::scan`
  - `shell-parse-parity.test.ts`: retains commandless redirects with matching saved prefixes: %s (2 cases)
- bash/zsh ordinary syntax table extracts commands (M) — `shell_scan::scan`
  - `shell-parse-syntax.test.ts`: ${shell} extracts commands without rejecting ordinary syntax: %s (70 cases)
- heredoc bodies and spans stay with their commands (S) — `shell_scan::scan`
  - `shell-parse-syntax.test.ts`: does not invent commands from a quoted second heredoc body
  - `shell-parse-syntax.test.ts`: keeps heredoc and here-string spans attached to their own commands across lists and pipelines
- zsh glob qualifiers / parameter flags / disown; code qualifiers rejected (S) — `shell_scan::scan`
  - `shell-parse-syntax.test.ts`: scans Zsh glob qualifiers, indirect parameter flags, and disown operators while rejecting code qualifiers
- variable sinks with substitution-built subscripts rejected (S) — `shell_scan::scan`
  - `shell-parse-syntax.test.ts`: %s rejects variable sinks when a substitution can build the subscript: %s (5 cases)
- visible Bash command positions table (M) — `shell_scan::scan`
  - `shell-scan/adversarial.test.ts`: scans visible Bash command positions: %s (32 cases)
- repeated zsh q flags in bounded time (S) — `shell_scan::scan`
  - `shell-scan/adversarial.test.ts`: scans repeated zsh q flags in bounded time
- arithmetic tables: kept unevaluated, explicit commands found, malformed rejected, bounded (M) — `shell_scan::scan`
  - `shell-scan/arithmetic.test.ts`: preserves arithmetic without evaluating it: %s (14 cases)
  - `shell-scan/arithmetic.test.ts`: reports explicit commands inside arithmetic: %s (6 cases)
  - `shell-scan/arithmetic.test.ts`: recognizes arithmetic in existing shell contexts: %s (5 cases)
  - `shell-scan/arithmetic.test.ts`: does not expand arithmetic inside single quotes
  - `shell-scan/arithmetic.test.ts`: rejects malformed arithmetic syntax: %s (5 cases)
  - `shell-scan/arithmetic.test.ts`: bounds arithmetic nesting, input size, and repeated conditional work
- arithmetic rows checked against real bash/zsh (M) — `tests/shell_differential.rs`
  - `shell-scan/arithmetic.test.ts`: ${shell} evaluates %s independently (20 cases)
- Bash safety tables: whitespace, fd prefixes, commandless redirects, group redirects, delimiter confusion (M) — `shell_scan::scan`
  - `shell-scan/bash-safety.test.ts`: ${shell} command syntax is visible: %s (2 cases)
  - `shell-scan/bash-safety.test.ts`: recognizes shell whitespace %j (2 cases)
  - `shell-scan/bash-safety.test.ts`: does not normalize non-shell whitespace %j (5 cases)
  - `shell-scan/bash-safety.test.ts`: does not consume quoted command names as fd prefixes: %s (3 cases)
  - `shell-scan/bash-safety.test.ts`: retains commandless redirects as explicit permission resources: %s (6 cases)
  - `shell-scan/bash-safety.test.ts`: still requires redirect targets: %s (6 cases)
  - `shell-scan/bash-safety.test.ts`: retains file redirects on compound groups and trailing assignments: %s (5 cases)
  - `shell-scan/bash-safety.test.ts`: does not invent redirect commands for pure group descriptor duplications: %s (3 cases)
  - `shell-scan/bash-safety.test.ts`: retains both redirect resources and explicit substitutions in commandless redirects: %s (4 cases)
  - `shell-scan/bash-safety.test.ts`: scans a list following a group redirect suffix
  - `shell-scan/bash-safety.test.ts`: retains explicit colon commands and their redirects after a group
  - `shell-scan/bash-safety.test.ts`: does not repeatedly rescan nested conditionals
  - `shell-scan/bash-safety.test.ts`: does not lose commands through delimiter or quote confusion: %s (4 cases)
  - `shell-scan/bash-safety.test.ts`: ${shell}: %s (3 cases)
  - `shell-scan/bash-safety.test.ts`: ${shell} preserves commands around redirect-only syntax: %s (12 cases)
- Bash/Zsh syntax tables: command nodes, ANSI-C quoting, heredoc delimiters, dialect differences (M) — `shell_scan::scan`
  - `shell-scan/bash-syntax.test.ts`: extracts actual command nodes: %s (70 cases)
  - `shell-scan/bash-syntax.test.ts`: rejects incomplete syntax: %s (7 cases)
  - `shell-scan/bash-syntax.test.ts`: scans a Zsh brace group closed after a redirect
  - `shell-scan/bash-syntax.test.ts`: preserves raw lexical spelling of ANSI-C and locale quoted words
  - `shell-scan/bash-syntax.test.ts`: decodes unknown ANSI-C escapes like %s: %s (6 cases)
  - `shell-scan/bash-syntax.test.ts`: decodes unknown escapes in ANSI-C heredoc delimiters like each dialect
  - `shell-scan/bash-syntax.test.ts`: rejects ANSI-C escapes whose value differs between shells in %s: %s (7 cases)
  - `shell-scan/bash-syntax.test.ts`: retains explicit substitutions without evaluating expressions: %s (6 cases)
  - `shell-scan/bash-syntax.test.ts`: preserves heredoc permission resources and saved prefixes: %s (4 cases)
  - `shell-scan/bash-syntax.test.ts`: does not reinterpret the tail of a here-string operator: %s (4 cases)
  - `shell-scan/bash-syntax.test.ts`: uses identical delimiter decoding in every command context: %j (12 cases)
  - `shell-scan/bash-syntax.test.ts`: posix reports both readings of a double parenthesis
  - `shell-scan/bash-syntax.test.ts`: reads Bash and Zsh syntax precisely where Dash diverges: %s (2 cases)
  - `shell-scan/bash-syntax.test.ts`: reads a reserved word after a redirect as a Zsh keyword
  - `shell-scan/bash-syntax.test.ts`: scans safe Zsh parameter flags, Zsh repeat loops, and Bash extglob arguments
  - `shell-scan/bash-syntax.test.ts`: derives the dialect from the shell executable: %s (5 cases)
- the fixtures run in real bash/zsh (add them to the differential corpus) (M) — `tests/shell_differential.rs`
  - `shell-scan/bash-syntax.test.ts`: ${shell} accepts the source grammar: ${source} (140 cases)
  - `shell-scan/bash-syntax.test.ts`: ${shell} runs only the explicitly found probes: %s (22 cases)
  - `shell-scan/bash-syntax.test.ts`: real Bash agrees with delimiter quoting: %s (8 cases)
- single quotes suppress active syntax (S) — `shell_scan::scan`
  - `shell-scan/closure.test.ts`: {source} (8 cases)
  - `shell-scan/closure.test.ts`: single quotes suppress active syntax: %s
  - `shell-scan/closure.test.ts`: {source} (16 cases)
- function bodies and shorthand loops in every context (M) — `shell_scan::scan`
  - `shell-scan/compound-parity.test.ts`: ${shell}: ${source} (160 cases)
  - `shell-scan/compound-parity.test.ts`: keeps compound function bodies inside command substitutions: %s (4 cases)
  - `shell-scan/compound-parity.test.ts`: {source} (5 cases)
  - `shell-scan/compound-parity.test.ts`: retains commands in nested shorthand loops: %s (7 cases)
- loops checked against real bash/zsh (M) — `tests/shell_differential.rs`
  - `shell-scan/compound-parity.test.ts`: ${shell}: %s (6 cases)
- generated combinations of commands and separators; malformed mutations fail closed (M) — `shell_scan::scan`
  - `shell-scan/generated.test.ts`: decomposes every combination of static commands and separators
  - `shell-scan/generated.test.ts`: keeps quoted and escaped separators in arguments
  - `shell-scan/generated.test.ts`: fails closed when valid commands are mutated with malformed syntax
  - `shell-scan/generated.test.ts`: retains generated dynamic command heads without resolving them
  - `shell-scan/generated.test.ts`: keeps wrappers and shell evaluators at their delegated boundary
- regression table: function definitions, continuations, comments, zsh anonymous functions, process substitutions (M) — `shell_scan::scan`
  - `shell-scan/parity-regressions.test.ts`: preserves deeply indented function definitions: %s (2 cases)
  - `shell-scan/parity-regressions.test.ts`: retains conditional process substitutions and permission resources: %s (2 cases)
  - `shell-scan/parity-regressions.test.ts`: ${shell} preserves line continuations at function boundaries: %s (8 cases)
  - `shell-scan/parity-regressions.test.ts`: ${shell} preserves comments between a function head and its body: %s (6 cases)
  - `shell-scan/parity-regressions.test.ts`: ${shell} preserves function resources, saved prefixes, and directories: %s (2 cases)
  - `shell-scan/parity-regressions.test.ts`: preserves Zsh anonymous functions and parenthesized loop permissions: %s (5 cases)
  - `shell-scan/parity-regressions.test.ts`: accepts anonymous-function compositions without recovery artifacts: $source (3 cases)
  - `shell-scan/parity-regressions.test.ts`: does not turn quoted process-substitution text into commands: %s (5 cases)
- the extracted command really executes (M) — `tests/shell_differential.rs`
  - `shell-scan/parity-regressions.test.ts`: ${shell} really executes the extracted command: %s (14 cases)

### Permission (26)

- path resolution (relative, prospective, symlinked, tilde) and which targets need external_directory (S) — `tool read/write/edit external_directory checks`
  - `file-access-path.test.ts`: resolves an active relative existing file target
  - `file-access-path.test.ts`: resolves an active relative prospective file target
  - `file-access-path.test.ts`: requires external-directory authorization for a relative lexical escape
  - `file-access-path.test.ts`: allows a relative path outside the Location but inside the project worktree
  - `file-access-path.test.ts`: does not treat a filesystem-root project sentinel as an internal boundary
  - `file-access-path.test.ts`: resolves a prospective target below an external symlink lexically
  - `file-access-path.test.ts`: follows an in-location symlink using ordinary filesystem semantics
  - `file-access-path.test.ts`: accepts an explicit absolute in-location target without external approval
  - `file-access-path.test.ts`: requires external-directory authorization for an explicit external absolute target
  - `file-access-path.test.ts`: resolves an existing external file target
  - `file-access-path.test.ts`: uses an explicit file kind without treating an existing directory as the target boundary
  - `file-access-path.test.ts`: authorizes prospective external descendants at their lexical parent
  - `file-access-path.test.ts`: expands a leading tilde against the home directory
  - `file-access-path.test.ts`: resolves a tilde path as an external home target
  - `file-access-path.test.ts`: treats a tilde path as in-location when the location is home
- external directory authorized before the file's own rules; denial stops; first-seen order (S) — `tool external_directory checks`
  - `file-access.test.ts`: authorizes an external directory before the file's read rules
  - `file-access.test.ts`: propagates ${action} denial without continuing authorization (2 cases)
  - `file-access.test.ts`: reuses a sibling's directory approval only for the supplied recovery call
  - `file-access.test.ts`: checks the external directory for a target that is not a sibling
  - `file-access.test.ts`: batches external resources in first-seen order and preserves broader repository saves
- omitted agent uses build's rules; no default agent denies (S) — `permission + agent`
  - `permission.test.ts`: uses build permissions when the Session agent is omitted
  - `permission.test.ts`: denies omitted-agent permissions when no primary default agent exists
- a rejection also rejects sibling requests of the same step (M) — `session`
  - `permission.test.ts`: rejects sibling requests in the same step and leaves another step's request pending
- pending auto-approval skipped after an "always" for a guarded request (S) — `permission`
  - `permission.test.ts`: skips pending auto-approval after always for ${guard} (2 cases)

### Agents (6)

- no custom agents / runtime defaults for created agents (S) — `agent::resolve`
  - `agent.test.ts`: starts without agents
  - `agent.test.ts`: creates agents with runtime defaults and supports direct removal
- the selected default agent is listed first (S) — `agent::list`
  - `agent.test.ts`: lists the selected default agent first
- home-relative permission paths; global permissions before agent-specific (S) — `agent::resolve`
  - `config/agent.test.ts`: matches POSIX paths against home-relative permissions
- plan reminder injected on switching to plan (S) — `agent::PLAN_REMINDER`
  - `plugin/plan.test.ts`: injects enter and leave reminders on agent switches
- OpenAI Responses verbosity defaults (S) — `codesmith_ai options`
  - `plugin/verbosity.test.ts`: sets known OpenAI Responses defaults without overriding configured or unknown models

### Skills and commands (11)

- plain content without frontmatter (S) — `skill frontmatter`
  - `config/markdown.test.ts`: parses plain content without frontmatter
- config entry types map to skill directories; a worktree copy beats the parent checkout's (S) — `skill::discover`
  - `config/skill.test.ts`: maps config entry types to skill directories
  - `config/skill.test.ts`: prefers a worktree skill over the parent checkout copy
- built-in init and review commands registered (S) — `command::builtin`
  - `plugin/command.test.ts`: registers built-in init and review commands
- denied skills hidden and refused (S) — `skill + permission`
  - `skill.test.ts`: hides and refuses denied skills while organization policy overrides local allows
  - `skill.test.ts`: filters values by agent permissions
- described skills rendered into the prompt (S) — `skill::instructions`
  - `skill/instructions.test.ts`: renders described agent skills and updates the complete available list
- skill list follows the agent's skill rules (S) — `skill + permission`
  - `skill/instructions.test.ts`: omits instructions when the selected agent denies all skills
  - `skill/instructions.test.ts`: omits instructions when a resource-specific denial follows the global denial
  - `skill/instructions.test.ts`: retains specifically allowed skills after a global denial
  - `skill/instructions.test.ts`: omits instructions when a specifically allowed skill is denied again

### Instructions and references (2)

- one failing source does not fail the rest (S) — `instructions::load`
  - `instruction-discovery.test.ts`: isolates source failure without failing activation
- canonical boundaries and project opt-out (S) — `instructions::load`
  - `instruction-discovery.test.ts`: canonicalizes boundaries and honors project opt-out

### Config (4)

- .claude directories skipped when Claude Code compatibility is off (S) — `config::load`
  - `config/config.test.ts`: excludes claude directories when claude is disabled
- unreadable discovery roots treated as absent (S) — `config::load`
  - `config/config.test.ts`: treats unreadable discovery roots as absent
- global ecosystem directories found twice load once (S) — `config::load`
  - `config/config.test.ts`: deduplicates global ecosystem directories found during upward discovery
- malformed selections and reserved fragments rejected (S) — `config::split_variant`
  - `config/model.test.ts`: rejects malformed selections and reserved fragments

### MCP (121)

- event extension discovery, ordered streams, termination, cancellation, idle timeout, no events offered (M) — `mcp::events`
  - `mcp-events.test.ts`: declares the extension and lists every page of event types
  - `mcp-events.test.ts`: delivers every message of a stream in order and ends when the server closes it
  - `mcp-events.test.ts`: fails a stream the server terminates, and cancels its request
  - `mcp-events.test.ts`: routes concurrent streams by subscription id
  - `mcp-events.test.ts`: cancels the request when the consumer stops reading
  - `mcp-events.test.ts`: fails a stream that sends nothing for a minute
  - `mcp-events.test.ts`: offers no events when the server does not, or when the connection does not declare them
- server instructions rendered; Code Mode vs direct listings (S) — `mcp instructions`
  - `mcp-instructions.test.ts`: renders instructions for servers with at least one reachable snapshot tool
  - `mcp-instructions.test.ts`: lists Code Mode servers as direct tools when the snapshot has no Code Mode catalog
  - `mcp-instructions.test.ts`: keeps MCP instructions when Code Mode is disabled for the server and execute is denied
  - `mcp-instructions.test.ts`: replaces a server entry when Code Mode is disabled for it
- loopback callback, pasted redirects, discovery via 401/metadata, CIMD/dynamic registration, refresh and sign-out rules (M) — `mcp::oauth`
  - `mcp-oauth.test.ts`: completes interactive authorization through the loopback callback
  - `mcp-oauth.test.ts`: keeps waiting for the redirect after callback requests without OAuth parameters
  - `mcp-oauth.test.ts`: completes from a pasted redirect URL and keeps waiting after a wrong paste
  - `mcp-oauth.test.ts`: reports a rejected token exchange on the callback page
  - `mcp-oauth.test.ts`: generates a loopback redirect URL when none is configured
  - `mcp-oauth.test.ts`: preserves a configured redirect URL and fixed port
  - `mcp-oauth.test.ts`: rejects an invalid redirect URL
  - `mcp-oauth.test.ts`: rejects an authorization endpoint that is not http or https
  - `mcp-oauth.test.ts`: finds resource metadata through the 401 header when the well-known path is not served
  - `mcp-oauth.test.ts`: uses configured authorization server metadata when the resource publishes none
  - `mcp-oauth.test.ts`: requests offline_access without forcing a consent prompt
  - `mcp-oauth.test.ts`: forwards iss from the redirect so issuer-advertising servers can complete
  - `mcp-oauth.test.ts`: uses the client metadata document when the server supports public CIMD clients
  - `mcp-oauth.test.ts`: registers dynamically when the server does not accept public clients
  - `mcp-oauth.test.ts`: redeems the code with the localhost redirect a server sent the browser back to
  - `mcp-oauth.test.ts`: reuses the client registered by an earlier login
  - `mcp-oauth.test.ts`: registers dynamically when a custom redirect_uri is configured
  - `mcp-oauth.test.ts`: refreshes a refused token and retries with the new one
  - `mcp-oauth.test.ts`: refreshes a token that expires within 30 seconds with the saved discovery before sending it
  - `mcp-oauth.test.ts`: sends the current token when the refresh fails
  - `mcp-oauth.test.ts`: leaves tokens with time left, without a lifetime, or saved without discovery to the 401
  - `mcp-oauth.test.ts`: refreshes the credential it was made with, not the one signed in since
  - `mcp-oauth.test.ts`: reports needs_auth for an unauthorized server without a credential or any OAuth request
  - `mcp-oauth.test.ts`: sends the configured URL when the server publishes no metadata
  - `mcp-oauth.test.ts`: keeps the configured URL when metadata echoes the dialed query
  - `mcp-oauth.test.ts`: discovers the resource for a login saved without one
  - `mcp-oauth.test.ts`: follows the 401 to the resource for a login saved without discovery, and saves it
  - `mcp-oauth.test.ts`: refreshes through configured authorization server metadata
  - `mcp-oauth.test.ts`: sends one refresh when two instances refresh the same credential at once
  - `mcp-oauth.test.ts`: uses the stored tokens instead of spending a refresh token another instance already spent
  - `mcp-oauth.test.ts`: needs auth without a request when the credential was signed out
  - `mcp-oauth.test.ts`: fails without signing out when the token endpoint answers 503
  - `mcp-oauth.test.ts`: fails without signing out when the token endpoint is unreachable
  - `mcp-oauth.test.ts`: signs out when the token endpoint answers invalid_grant
  - `mcp-oauth.test.ts`: keeps the credential when the refresh times out
  - `mcp-oauth.test.ts`: sends nothing to an authorization server other than the one that issued the credential
  - `mcp-oauth.test.ts`: names only the error code of a token endpoint that answers without an OAuth error
  - `mcp-oauth.test.ts`: signs out a credential without client information
  - `mcp-oauth.test.ts`: tries the next metadata URL when one answers 502
  - `mcp-oauth.test.ts`: reports needs_auth when the server asks for more scope
- cwd resolved, ~ expanded (S) — `mcp stdio`
  - `mcp-stdio.test.ts`: resolves cwd from the workspace and expands ~ to the home directory
- frames up to 16 MiB, CRLF/blank lines/split UTF-8, oversize line closes (S) — `mcp stdio framing`
  - `mcp-stdio.test.ts`: receives a frame between 10 and 16 MiB split into pipe-sized chunks
  - `mcp-stdio.test.ts`: frames CRLF, blank lines, several messages per chunk and split UTF-8
  - `mcp-stdio.test.ts`: accepts a frame of exactly 16 MiB
  - `mcp-stdio.test.ts`: closes the connection on a line one byte over 16 MiB
- tool naming, pagination, output schemas, prompts, timeouts, resources, session expiry reconnects, runtime add/disconnect, Code Mode results, isError, media, permissions, events (M) — `mcp`
  - `mcp.test.ts`: expose useful messages
  - `mcp.test.ts`: MCP tool names match V1 sanitization
  - `mcp.test.ts`: passes session IDs as MCP request metadata
  - `mcp.test.ts`: preserves output schema validation across paginated tool discovery
  - `mcp.test.ts`: retains output schemas across paginated MCP discovery
  - `mcp.test.ts`: accepts legacy MCP tools whose output schema is not a root object
  - `mcp.test.ts`: lists paginated prompts and invokes them through the MCP client
  - `mcp.test.ts`: rejects sends before the stdio transport is started
  - `mcp.test.ts`: joins concurrent stdio transport closes
  - `mcp.test.ts`: terminates MCP descendants after the wrapper exits successfully
  - `mcp.test.ts`: closes a stdio process that finishes spawning after close
  - `mcp.test.ts`: falls back to legacy when a stdio server ignores the version probe
  - `mcp.test.ts`: applies the configured MCP catalog timeout
  - `mcp.test.ts`: applies the configured MCP execution timeout
  - `mcp.test.ts`: applies the configured MCP execution timeout to prompts
  - `mcp.test.ts`: applies configured MCP timeouts to resource operations
  - `mcp.test.ts`: reconnects and retries a tool call after the MCP session expires
  - `mcp.test.ts`: exposes every connection operation
  - `mcp.test.ts`: skips unusable tools and keeps the rest of the server's tools
  - `mcp.test.ts`: returns results that miss or break the called tool's output schema
  - `mcp.test.ts`: retries a modern MCP tool call whose listed header declaration went stale
  - `mcp.test.ts`: lists, reads, and reports MCP resource changes
  - `mcp.test.ts`: does not reconnect an SSE stream after a JSON-RPC error response
  - `mcp.test.ts`: skips MCP resource requests when the capability is absent
  - `mcp.test.ts`: loads and reads MCP resources
  - `mcp.test.ts`: discovers and reads MCP resources through Code Mode
  - `mcp.test.ts`: adds, disconnects, and reconnects MCP servers at runtime
  - `mcp.test.ts`: marks a remote MCP server with an invalid URL as failed
  - `mcp.test.ts`: merges MCP defaults into the winning configured server without changing runtime overrides
  - `mcp.test.ts`: flush waits for a server that connects within its startup window
  - `mcp.test.ts`: flush stops waiting for a hung server when its startup window ends
  - `mcp.test.ts`: advertises MCP output schemas to Code Mode
  - `mcp.test.ts`: forwards the invoking session through direct and Code Mode MCP tools
  - `mcp.test.ts`: returns content-only MCP results through Code Mode
  - `mcp.test.ts`: parses JSON text results from MCP tools without an output schema
  - `mcp.test.ts`: advertises MCP tools directly when Code Mode is disabled for the server
  - `mcp.test.ts`: fails the call when MCP reports isError
  - `mcp.test.ts`: preserves MCP text and media content for the model
  - `mcp.test.ts`: decodes text blobs and saves other MCP blobs instead of attaching them
  - `mcp.test.ts`: waits for permission before calling an MCP tool
  - `mcp.test.ts`: does not call MCP when permission is blocked
  - `mcp.test.ts`: returns permission feedback instead of calling MCP
  - `mcp.test.ts`: streams a server's events over Streamable HTTP, naming the event in Mcp-Name
  - `mcp.test.ts`: publishes a subscription's events on the bus and follows its status
  - `mcp.test.ts`: backs off failed opens, resets after active, and resumes with a replay gap once
  - `mcp.test.ts`: ends a subscription the server terminates or rejects, without reopening it
  - `mcp.test.ts`: reopens without a replay gap when cursorless, and ends when disabled
- remote MCP Code Mode preference table (S) — `mcp config`
  - `mcp.test.ts`: remote MCP code mode preference: ${entry.name} (4 cases)
- no retry beyond the query fallback (S) — `mcp`
  - `mcp.test.ts`: does not retry MCP beyond the query fallback: ${entry.name} (11 cases)
- Code Mode defaults for known servers unless configured (S) — `mcp config`
  - `plugin/mcp-codemode-defaults.test.ts`: applies Code Mode defaults for known MCP servers unless codemode is configured
- list a server's push events after permission; fail for a server without events (S) — `mcp events tool`
  - `tool-mcp-events.test.ts`: lists one server's push events after asking permission
  - `tool-mcp-events.test.ts`: fails for a server that sends no events

### Code Mode (19)

- a nested call's stop does not replace the execute result (S) — `codemode::execute`
  - `codemode.test.ts`: never hands a stop to a nested call, so its error cannot replace the execute call's result
- Code Mode program semantics: description, typed outputs, failures, denied permissions, log/warn/result output, fetch through webfetch rules, namespaces, nested call IDs (M) — `codemode::execute`
  - `tool-execute.test.ts`: execute describes invariant Code Mode behavior
  - `tool-execute.test.ts`: canonical execution distinguishes declared, model-only, and raw schema outputs
  - `tool-execute.test.ts`: declared outputs cannot bypass validation and raw outputs stay JSON-compatible
  - `tool-execute.test.ts`: foreign typed failures settle as Tool.Error at the untrusted boundary
  - `tool-execute.test.ts`: execute rejects filesystem fetches
  - `tool-execute.test.ts`: a denied permission reaches the program and changes nothing
  - `tool-execute.test.ts`: a program that only logs shows just its logs
  - `tool-execute.test.ts`: a program that only warns shows just its warnings
  - `tool-execute.test.ts`: a null result with nothing else still shows null
  - `tool-execute.test.ts`: a returned value keeps its line before the logs
  - `tool-execute.test.ts`: interrupting execute aborts a pending request
  - `tool-execute.test.ts`: fetch asks the webfetch permission for its URL as the execute call
  - `tool-execute.test.ts`: a redirect to a URL denied for webfetch is never requested
  - `tool-execute.test.ts`: fetch matches rules without the fragment and rejects credentials in the URL
  - `tool-execute.test.ts`: a rejection with feedback reaches the program as the error message
  - `tool-execute.test.ts`: a URL denied for webfetch is never requested and rejects like a failed fetch
  - `tool-execute.test.ts`: execute supports callable namespace tools
  - `tool-execute.test.ts`: each call in a program gets its own ID and is marked as nested in execute

### Automations (47)

- MCP event watches: terminal events, timers on a running watch, nonterminal errors, pause/resume/delete, argument and permission validation, quoted untrusted fields, oversized prompts, stop reasons (M) — `tests/automation.rs`
  - `automation-event.test.ts`: pushes pending then ${terminal} through one automation, with no timer or tool poll (2 cases)
  - `automation-event.test.ts`: a timer added to a running watch fires without reopening its stream
  - `automation-event.test.ts`: continues on the same stream after a nonterminal producer error
  - `automation-event.test.ts`: pause, restart, resume and delete cancel and reacquire exactly one stream
  - `automation-event.test.ts`: validates permission, push capability and arguments before creating a watch
  - `automation-event.test.ts`: plain prompt events stay distinct and quote untrusted fields; oversized input does not run
  - `automation-event.test.ts`: rejects an oversized rendered event prompt before creating a child
  - `automation-event.test.ts`: stops the stream without a wake: ${stop} (6 cases)
- at/cron scheduling, zones and DST, create/list/pause/resume/delete, targets, child sessions, expiry, late runs after downtime, skip while unconsumed, backoff, program state, permissions extracted at creation (M) — `tests/automation.rs`
  - `automation.test.ts`: runs an at automation once, into the creating Session by default
  - `automation.test.ts`: parses at times with offsets or in the automation's zone
  - `automation.test.ts`: creates, lists, pauses, resumes and deletes
  - `automation.test.ts`: sends a prompt to another existing Session
  - `automation.test.ts`: starts a child of the creating Session with the agent on each run, without waking the parent
  - `automation.test.ts`: allows any agent that can run as a subagent, and rejects hidden and primary-only agents
  - `automation.test.ts`: runs without a Session in the given directory, and needs a target for a prompt
  - `automation.test.ts`: stops at expires
  - `automation.test.ts`: never runs a slot that only comes up after expires, after downtime or on resume
  - `automation.test.ts`: runs once after downtime, marked late, and keeps the phase
  - `automation.test.ts`: skips a run while the previous prompt is unconsumed
  - `automation.test.ts`: keeps automations when their creating Session is deleted, and pauses when a target is gone
  - `automation.test.ts`: keeps the run count when paused while a prompt is being admitted
  - `automation.test.ts`: does not start queued timers that were paused, deleted or rescheduled
  - `automation.test.ts`: pauses with a reason when its directory disappears
  - `automation.test.ts`: run sleeps until the next automation is due
  - `automation.test.ts`: backs off for five minutes when a prompt fails, then runs once
  - `automation.test.ts`: keeps cron at the same wall-clock time across DST changes
  - `automation.test.ts`: keeps state across runs, stays quiet without a return, and stops when done
  - `automation.test.ts`: a program that returns nothing leaves state unchanged
  - `automation.test.ts`: keeps state on failure, retries a minute later, and pauses after 3 failed runs
  - `automation.test.ts`: pauses at once when a run returns an invalid shape
  - `automation.test.ts`: stays quiet while pending, then admits both a progress and a terminal send to the creator
  - `automation.test.ts`: extracts tools at creation, asks once without save, and runs unattended after
  - `automation.test.ts`: asks for resource-scoped ask rules at creation, and still needs a person for them at run time
  - `automation.test.ts`: asks for execute when its tools are already allowed
  - `automation.test.ts`: rejects dynamic tool access, unknown tools, shell, recursive automation tools and denied permissions
- automation tool: event-watch call, create/list/pause/resume/delete, /automation command, validation, listing text, other projects' automations missing (M) — `tests/automation.rs`
  - `tool-automation.test.ts`: passes the originating tool call to the service when creating an event watch
  - `tool-automation.test.ts`: creates, lists, pauses, resumes and deletes this project's automations
  - `tool-automation.test.ts`: /automation hands the request to the model with the automation instructions
  - `tool-automation.test.ts`: rejects a malformed trigger, target or empty name
  - `tool-automation.test.ts`: puts each automation's id, trigger, status, next run and action in the text without Code Mode
  - `tool-automation.test.ts`: treats another project's automation as missing

### Formatters (9)

- per-formatter detection table (S) — `format::builtins`
  - `formatter.test.ts`: {entry.name} (9 cases)

### Worktrees, VCS and projects (43)

- create/list/remove linked worktrees; unborn repositories (S) — `worktree`
  - `git.test.ts`: creates, lists, and removes linked worktrees
  - `git.test.ts`: reads no remote and no root commits from an unborn repository
- project identity from the repository (root commit / remote), nested dirs resolve to the root, linked worktrees (S) — `worktree::project_id`
  - `project.test.ts`: returns git global for repo with no commits and no remote
  - `project.test.ts`: falls back to root commit when origin is missing and the cached id is corrupt
  - `project.test.ts`: prefers normalized origin over root commit
  - `project.test.ts`: normalizes ssh and https remotes to the same id
  - `project.test.ts`: ignores file remotes and falls back to root commit
  - `project.test.ts`: returns previous cached id from common dir
  - `project.test.ts`: resolves from nested directories to repo root
  - `project.test.ts`: linked worktree returns opened worktree directory and previous from common dir
- review diffs: working copy, before first commit, feature branch vs default, staged/untracked, explicit and missing bases (M) — `review::target::resolve`
  - `vcs.test.ts`: lists local branches by recent activity
  - `vcs.test.ts`: lists remote branches without remote HEAD symrefs
  - `vcs.test.ts`: reports modified, deleted, and untracked files
  - `vcs.test.ts`: diffs the working copy against HEAD with patches
  - `vcs.test.ts`: respects the repository's line-ending conversion
  - `vcs.test.ts`: respects the repository's symlink setting
  - `vcs.test.ts`: respects the context option
  - `vcs.test.ts`: diffs before the first commit
  - `vcs.test.ts`: diffs a feature branch against the default branch
  - `vcs.test.ts`: separates committed, combined, and staged/unstaged/untracked working changes
  - `vcs.test.ts`: keeps a locally undone commit visible only in committed and working modes
  - `vcs.test.ts`: shows local WIP on the default branch
  - `vcs.test.ts`: keeps unpushed default-branch commits in the resolved review
  - `vcs.test.ts`: scopes committed diffs to nested paths and retains binary, rename, and deletion metadata
  - `vcs.test.ts`: uses an explicit diff base without treating upstream or Git config as a parent
  - `vcs.test.ts`: reports missing explicit bases and missing default bases without calling them clean
  - `vcs.test.ts`: diffs explicit remote-only bases without borrowing another fork's branch
  - `vcs.test.ts`: reports ${scenario.name} base metadata as unavailable rather than absent (2 cases)
  - `vcs.test.ts`: reviews every feature commit and local changes against inferred v2, not the unrelated default
  - `vcs.test.ts`: ignores stale, deleted, renamed, and self-mirroring creation hints
  - `vcs.test.ts`: creates detached worktrees without review metadata and requires an explicit base without a named hint
  - `vcs.test.ts`: rejects a branch creation anchor removed by rebase
  - `vcs.test.ts`: does not infer beyond the bounded creation-reflog window
- create and remove, default directory and suffixes, from a branch, branch not read as an option, missing source, dirty needs force, conflicts (S) — `worktree::create / remove`
  - `worktree.test.ts`: creates and removes a git worktree directory
  - `worktree.test.ts`: defaults to the TUI worktree directory and suffixes duplicate names
  - `worktree.test.ts`: creates a git worktree from a selected branch
  - `worktree.test.ts`: does not interpret a branch as a git option
  - `worktree.test.ts`: rejects a missing source directory
  - `worktree.test.ts`: requires force to remove a dirty git worktree
  - `worktree.test.ts`: adds a numeric suffix when a worktree directory already exists
  - `worktree.test.ts`: fails after ten worktree directory conflicts
- the setup (start) command runs with worktree paths (S) — `worktree::create`
  - `worktree.test.ts`: runs the project setup script with worktree paths

### Providers, models and credentials (37)

- default model when none is configured; a disabled provider's default ignored (S) — `config::resolve_model`
  - `catalog.test.ts`: falls back to newest available model when no default is configured
  - `catalog.test.ts`: ignores a configured default on a disabled provider
- provider/model/variant options split into settings and body (S) — `codesmith_ai options`
  - `config/provider-options.test.ts`: splits provider overlays without changing package settings
  - `config/provider-options.test.ts`: moves openai-compatible options the package does not read into the body
  - `config/provider-options.test.ts`: keeps model and variant options unchanged
  - `config/provider-options.test.ts`: uses mechanical lowering for custom provider options
- provider and model compatibility precedence and later overrides (S) — `config providers`
  - `config/provider.test.ts`: keeps provider and model compatibility precedence across config documents
  - `config/provider.test.ts`: lets configured provider compatibility override built-in provider defaults
  - `config/provider.test.ts`: applies provider compatibility to every provider model with model overrides
  - `config/provider.test.ts`: passes configured stream usage compatibility to the runtime model
  - `config/provider.test.ts`: loads configured providers and applies later model overrides
- key auth for custom providers, default custom model metadata, model types (S) — `config providers`
  - `config/provider.test.ts`: adds key auth for custom providers without env credentials
  - `config/provider.test.ts`: defaults custom model metadata
  - `config/provider.test.ts`: lists a custom model by its configured type and an untyped one as chat
  - `config/provider.test.ts`: resolves a config model with only input/output capabilities using default tools
- a configured baseURL overrides model URLs (S) — `config providers`
  - `config/provider.test.ts`: lets a configured provider baseURL override catalog model baseURLs
- configured variant bodies kept unchanged (S) — `config variants`
  - `config/provider.test.ts`: keeps configured model variant bodies unchanged
  - `config/provider.test.ts`: keeps layered model variant bodies unchanged
- store, update, list and remove saved keys (S) — `auth`
  - `credential.test.ts`: stores, updates, lists, and removes credentials
- Azure deployment routing (S) — `config providers (azure)`
  - `model-resolver.test.ts`: constructs native Azure requests with deployment IDs and projected resource URLs
  - `model-resolver.test.ts`: lets Azure CLI requests past package auth for the Azure plugin to authorize
- API model IDs, empty keys omitted, env templates, unresolved variables rejected, variant overlays, stored vs configured credentials (S) — `config::resolve_model`
  - `model-resolver.test.ts`: uses the API modelID instead of the catalog ID for native OpenAI routes
  - `model-resolver.test.ts`: treats an empty configured API key as omitted
  - `model-resolver.test.ts`: uses merged API settings for OpenAI-compatible auth and request defaults
  - `model-resolver.test.ts`: resolves provider URLs from environment without mutating the catalog model
  - `model-resolver.test.ts`: rejects unresolved variables in constructed provider routes
  - `model-resolver.test.ts`: overlays selected OpenAI variant settings and bodies
  - `model-resolver.test.ts`: overlays selected OpenAI-compatible variant bodies
  - `model-resolver.test.ts`: rejects an explicit unavailable variant during model resolution
  - `model-resolver.test.ts`: overlays selected Anthropic variant settings
  - `model-resolver.test.ts`: uses resolved credentials for bearer auth
  - `model-resolver.test.ts`: prefers stored credentials over configured auth
- xAI Responses effort with reasoning summaries (S) — `codesmith_ai options`
  - `model-resolver.test.ts`: sends xAI Responses effort variants with reasoning summaries
- OpenRouter / Mistral headers and body (S) — `config providers`
  - `model-resolver.test.ts`: merges mapped OpenRouter headers and body with catalog overlays
  - `model-resolver.test.ts`: merges mapped Mistral headers and body with catalog overlays
- provider and model resource URLs expanded (S) — `config providers (azure)`
  - `plugin/provider-azure.test.ts`: expands provider and model resource URLs
- no default output limit sent to OpenAI (S) — `session request`
  - `plugin/provider-openai.test.ts`: omits the default output limit from OpenAI steps and compaction

### Runtime, locations and filesystem (4)

- PATH lookup (S) — `format::which`
  - `util/which.test.ts`: returns null when command is missing
  - `util/which.test.ts`: finds a command from PATH override
  - `util/which.test.ts`: uses first PATH match
  - `util/which.test.ts`: returns null for non-executable file on unix

## By design or not applicable

| Reason | Cases |
|---|---:|
| Windows, PowerShell, Bun/Node and npm/AI SDK packages | 684 |
| Deliberate differences, documented in the module headers (no steers, queues or session moves; subagents run inside the tool call; no live config reload or v1 config migration; compiled-in plugins; SearXNG-only web search; ...) | 538 |
| OpenCode's SQLite/drizzle event store, projector rows and bus (CodeSmith has its own session store) | 325 |
| OpenCode internals with no counterpart: Effect layers, services, fibers and schema types | 313 |
| OpenCode's client/server architecture: Locations, instances, workspaces, RPC and API shapes | 220 |
| Hosted OpenCode services: Zen, Console, free models, organizations, integrations, well-known config | 160 |
| Other (each says why) | 94 |

## Per-test inventory

The tests not listed in the roadmap or the backlog, by file: `P` ported, `F` ported after a fix, `D` by design / n.a., `X` found bug; `×n` is the number of cases. Consecutive by-design tests with the same reason share a line.

<details><summary>Session runner (267 of 393)</summary>


**`generate.test.ts`**

- D loads dynamic AI SDK models · resolves dynamic models with their catalog metadata — platform: dynamic AI SDK models

**`session-error.test.ts`**

- D maps every AI error reason to the open wire type — client-server: maps AI error reasons onto OpenCode's wire error schema
- P separates the free and Go usage limits from paid rate limits, credit limits, and other providers' 429s — `src/retry.rs::oc_session_error_separates_quota_caps_from_rate_limits` (quota vs rate limit only; the OpenCode Free/Go plan limits are cloud)
- D tells a budget on the requested model from one pausing all model use — cloud: OpenCode Console model budgets
- D preserves the permission rejection type without exposing internal fields · preserves provider HTTP status without exposing runtime diagnostics · preserves provider response body without HTTP status · maps token-sharing failures while preserving HTTP and stream bodies · preserves unresolved provider endpoint errors · preserves provider configuration and initialization errors — client-server: which error fields OpenCode's API exposes
- P retries rate limits, provider-internal, transport, and unrecognized failures — `src/retry.rs::oc_session_error_retries_rate_limits_provider_internal_transport_and_unrecognized_failures`
- D honors provider retry header overrides — deliberate: retry.rs: retry-after / x-should-retry headers are not kept, so not honoured

**`session-execution.test.ts`**

- D classifies success and typed failure terminals · defaults owner-scope interruption to shutdown and preserves explicit reasons — effect-internal: execution terminal classification and owner-scope interruption
- D resumes a directly driven child after execution teardown · leaves a child parked when its parent is gone or its restart budget is exhausted · claims at execution start, releases on completion, and preserves through teardown · a user interrupt releases the claim so the turn never resurrects · tells the drain once the user stopped it · reports an idle interrupt as a no-op · does not resume a user-cancelled background child whose notification was not admitted · honors a job cancellation persisted before its child could release the claim · starts every claimed execution without waiting for earlier drains to finish · resumes each claimed Session at most once · terminalizes a turn that exhausts its resume budget instead of crash-looping · counts every resume durably and never consumes the claim it recovers · the sweep leaves Sessions already draining in this process untouched · keeps shell owners idle until a user prompt delivers recovered notices exactly once · counts a suspended parent's restart attempt before its shell-woken child can wake it · watches shell jobs whose commands survived the restart · settles a shell call a killed server left running as moved to the background · reattaches once a ${exited ? "finished" : "running"} shell a killed server backgrounded without settling its call ×2 · settles a recovered child's unsettled shell call before the child resumes · preserves locally running background work · wakes the owner for a silent shell failure persisted before its completion notification · delivers an image outcome persisted before its notification, and cancels one still running · waits again for a running video after a restart, then saves it and delivers the notice · does not duplicate a shell notification already ${delivered ? "delivered" : "admitted"} ×2 · acknowledges recovery markers when their owning session is deleted · delivers cancellation at the resumed parent's next step · does not bypass a claimed owner's restart budget for a shell notice · recovers a child's shell ${shellFirst ? "before" : "after"} its job record without bypassing attempt ${resumeAttempts + 1} ×4 · resumes a background subagent and notifies its parent exactly once · delivers a subagent result persisted before restart without rerunning the child · delivers a completed outbox row at boot without touching the child · wakes the parent of an admitted report if the restart sweep fails afterward · reconciles an outbox row whose notification was already delivered · retains a subagent completion marker when synthetic admission conflicts · honors a suspended parent's restart budget after ${resumeAttempts} attempts before notifying it ×2 · terminalizes a recovered subagent that exhausts its resume budget · charges a suspended parent's restart attempt before a fast child can wake it — deliberate: session.rs: subagents run inside the tool call; no background-subagent reports, job claims or restart recovery
- D resumes only steering input after an interrupt with continue · stays parked after an interrupt with continue when only queued work remains · an idle interrupt with continue resumes pending steers · an interrupt with continue resumes a queued compaction next in line · keeps a control item parked behind a queued prompt on continue — deliberate: session.rs: no steers or queues

**`session-run-coordinator.test.ts`**

- D joins concurrent resumes for one key · joins a wake-started execution without forcing a successor · starts execution when woken while idle · snapshots only active executions · cleans active executions after failure and defect · preserves settlement hook defects while releasing ownership · cleans active executions when its scope closes · coalesces wakes received during active execution · runs again when woken during the follow-up · does nothing when interrupted while idle · does not attach a late interrupt reason after terminal settlement starts · a settlement-window wake starts a fresh execution with its own scope · interrupts active execution and clears its pending wake · tells why an execution is being stopped while it cleans up · runs a wake registered during interruption cleanup · coalesces drain scopes with input taking precedence · does not carry a completed input scope into a steer drain · a cleanup-era wake starts a successor with its own scope · starts a resume registered during interruption cleanup · starts one follow-up when a wake races with failure · does not cancel execution when a joined waiter is interrupted · runs different keys concurrently · settles once per execution across coalesced drains · settles interrupted executions before waiters resolve · acknowledges interruption before cleanup settles · an interrupt during terminal settlement claims the recorded wake · trampolines synchronous self-waking execution — effect-internal: execution coordinator (wake/join/interrupt fibers)

**`session-runner.test.ts`**

- D retries title generation from the first prompt after title and execution failures — deliberate: title.rs: a failed title falls back to the first line instead of retrying
- D advertises and executes a location registered tool — client-server: Location tool registry / registry reload
- D runs the idle hook after a move delivered at the idle boundary · keeps idle hook input pending across a compaction admitted with it · does not run the session idle hook after manual compaction of an idle session — deliberate: session.rs: no moves or queued input
- D executes the tool advertised before a registry reload — client-server: Location tool registry / registry reload
- D starts a real runner step after default prompt recording · runs a follow-up when synthetic input arrives during an active continuation — storage-model: default prompt recording / synthetic input
- D streams one request with registry definitions from chronological user history — client-server: Location tool registry / registry reload
- P marks the initial instruction sync as baseline metadata — `tests/oc_instruction_state.rs::oc_session_marks_the_initial_instruction_sync_as_the_baseline`
- P retries the first request after system context becomes available — `tests/oc_instruction_state.rs::oc_session_retries_the_first_request_once_context_is_readable` (adapted: the prompt fails before anything is recorded and is sent again)
- D preserves instruction state and interrupts the source Location runner after a Session moves · settles compaction and delivers a move while initial instructions are unavailable · delivers a queued move atomically at the idle boundary · preserves a tool continuation and step allowance across chained moves (${delivery}) ×2 · keeps queued input parked across a mid-turn move · runs a queued control on Location entry before a carried continuation — client-server: a Session moving between Locations
- P seeds a fork with the parent's newest instruction values — `tests/oc_instruction_state.rs::oc_session_seeds_a_fork_with_the_newest_values` (adapted: Store::fork copies the whole session)
- P keeps nested forks self-contained — `tests/oc_instruction_state.rs::oc_session_keeps_nested_forks_self_contained`
- P re-establishes a fresh baseline when instruction state is missing — `tests/oc_instruction_state.rs::oc_session_makes_a_fresh_baseline_when_the_state_is_missing` (adapted: a log without instruction records)
- P keeps the initial instructions stable and derives a chronological update from values — `tests/oc_instruction_state.rs::oc_session_keeps_the_baseline_stable_and_narrates_an_update`
- P keeps the system prompt stable across a committed revert — `tests/oc_instruction_state.rs::oc_session_keeps_the_system_prompt_across_an_undo` (adapted: undo of the newest turn)
- P resyncs the instruction state when a committed revert removes an update — `tests/oc_instruction_state.rs::oc_session_resyncs_when_an_undo_removes_an_update` (adapted: two undos)
- P hides an update a revert left behind once the source is removed — `tests/oc_instruction_state.rs::oc_session_hides_an_update_an_undo_left_behind_once_the_source_is_gone` (adapted: two undos)
- P fails before the model request when the selected agent is unavailable — `tests/oc_runner.rs::oc_session_fails_before_the_model_request_when_the_agent_is_unavailable`
- D keeps the sampled agent when selection changes during observation · keeps the sampled model when selection changes during model resolution — effect-internal: selection sampled once during observation
- P admits removed context as a chronological System message — `tests/oc_instruction_state.rs::oc_session_admits_removed_context_as_a_system_message`
- P renders API context entries through add, change, and removal — `tests/oc_instruction_state.rs::oc_session_renders_context_entries_through_add_change_and_removal` (adapted: Store::put_context, no per-source request metadata)
- P retains JSON null API entries as values — `tests/oc_instruction_state.rs::oc_session_retains_null_context_entries_as_values`
- P accepts API instruction entries up to 256 KiB — `tests/oc_instruction_state.rs::oc_store_accepts_context_entries_up_to_256_kib`
- P rejects API instruction entries larger than 256 KiB — `tests/oc_instruction_state.rs::oc_store_rejects_context_entries_over_256_kib`
- P keeps initial instructions and chronological updates after a model switch — `tests/oc_instruction_state.rs::oc_session_keeps_instructions_and_updates_after_a_model_switch` (adapted: the session reopened on another model)
- P preserves instruction values while a source is temporarily unavailable — `tests/oc_instruction_state.rs::oc_session_keeps_values_while_a_source_is_unreadable`
- P narrates only the Code Mode tools that change, through compaction and fork — `tests/oc_instruction_state.rs::oc_session_narrates_only_the_code_mode_tools_that_change` (adapted: an MCP server that grows a tool when one is called)
- P moves the epoch at compaction and narrates later changes — `tests/oc_instruction_state.rs::oc_session_moves_the_epoch_at_compaction_and_narrates_later_changes`
- D prioritizes manual compaction admitted ${order} two steers at the safe boundary ×3 · waits for active tools before prioritizing compaction over pending steers · rechecks compaction admitted during boundary context preparation · preserves both earlier steers when prioritized compaction is ${outcome} ×2 · keeps steers durable across interrupted priority compaction and replay · does not pull compaction across an earlier move · runs steers before queued compaction and later queued input · releases queued prompts when durable compaction fails — deliberate: session.rs: no steers or queues, so no compaction priority against them
- P explains when manual compaction has no history — `tests/parity.rs::oc_compaction_nothing_to_compact_in_an_empty_session`
- D delivers steered manual compaction when the model has no context limit · runs manual compaction at the next step boundary before queued prompts · runs manual compaction before the continuation of an active tool turn — deliberate: session.rs: manual compaction runs between turns, not at a step boundary
- P preserves provider errors from manual compaction — `tests/oc_compaction.rs::oc_compaction_preserves_provider_errors_from_manual_compaction`
- P compaction ${summary ? "recovers" : "stops"} after one template reminder for ${response} ×3 — `tests/parity.rs::oc_compaction_nudges_a_reply_without_the_template`
- D waits for plugin activation before each step and tool call — effect-internal: plugin activation waits
- F explains a compaction blocked by the provider — `tests/oc_compaction.rs::oc_compaction_explains_a_compaction_blocked_by_the_provider` (a blocked summary was nudged and retried; it now fails as blocked (adapted: no refusal category))
- D stops compaction retries through the ${header ? "provider header" : "retry hook"} ×2 — deliberate: retry.rs: retry headers and retry hooks not honoured
- F rejects compaction ${response} without retrying or committing its draft ×2 — `tests/oc_compaction.rs::oc_compaction_rejects_a_cut_short_summary_without_retrying_or_committing_it` (the summary's finish reason was ignored, so a summary cut off by the output limit or a content filter replaced the history)
- D stops after four smaller compaction inputs overflow · resends compaction as text after payload too large, then shrinks later rejections · shortens the only exchange's user text when it does not fit · shrinks after payload too large on a compaction already sent as text · does not resend a rejected conversation unchanged when compacting · aims the first overflow compaction below the rejected context · serializes history without tools or media after a summary input overflow · drops tool output in serialized compaction history before omitting exchanges · fits serialized compaction history by omitting oldest exchanges — deliberate: compaction.rs: a summary request is not shrunk and retried
- P automatically compacts into a completed summary and retained recent turn — `tests/parity.rs::oc_compaction_auto_compacts_past_the_buffered_ceiling`
- P forces one compaction and retries after provider context overflow — `tests/parity.rs::oc_compaction_an_overflow_compacts_and_retries`
- D refreshes preparation after overflow compaction without promoting new input — deliberate: session.rs: no queued input to promote
- P does not recover provider context overflow when automatic compaction is disabled — `tests/parity.rs::oc_compaction_overflow_is_left_to_the_user_when_auto_is_disabled`
- P recovers from provider context overflow without a configured context limit — `tests/parity.rs::oc_compaction_an_overflow_compacts_and_retries` (the scripted model has no configured limit and fails with a raw overflow)
- P persists a second context overflow after one recovery — `tests/oc_compaction.rs::oc_compaction_a_second_overflow_after_one_recovery_fails_the_turn`
- P recovers once from a raw context overflow failure — `tests/parity.rs::oc_compaction_an_overflow_compacts_and_retries` (the scripted model has no configured limit and fails with a raw overflow)
- F publishes the original overflow when recovery summarization fails — `tests/oc_compaction.rs::oc_compaction_reports_the_original_overflow_when_recovery_fails` (the failed summary's error was reported instead of the overflow)
- P uses epoch values after compaction while a source is unavailable — `tests/oc_instruction_state.rs::oc_session_uses_the_epoch_values_after_compaction_while_unreadable`
- D projects reasoning and tool events without executing or continuing tools — storage-model: projection-only replay of events
- D joins concurrent resume calls into one active provider run — effect-internal: concurrent resume joining / run fan-out
- D steers an active step with newly recorded prompts · promotes queued input after continuation ends · keeps queued input parked when a steer is cancelled during preparation · dispatches a queued move when a steer is cancelled during preparation · preserves durable queued input for a later wake after interruption · preserves durable steering input for a later resume after interruption · promotes queued inputs one at a time in FIFO order · stops a steer-scoped drain before queued input · a steer-scoped drain runs a queued manual compaction next in line · a steer-scoped drain leaves a compaction parked behind a queued prompt · promotes queued input after steering continuation ends · promotes steers before the next queued input · coalesces multiple active steering prompts into one continuation step · runs steering input accepted while the active step fails — deliberate: session.rs: no steers, queues or inbox
- D settles abandoned compactions before continuing after a process crash · settles an abandoned compaction before delivering another manual compaction — deliberate: session.rs: no restart recovery of compactions
- P durably fails local tools left running by a prior process before continuing — `tests/runner_parity.rs::oc_session_durably_fails_local_tools_left_running_by_a_prior_process_before_continuing`
- D preserves a stale subagent child session in its model-visible failure — deliberate: session.rs: subagents run inside the tool call
- D settles only unfinished tools after the latest compaction after a process crash — deliberate: session.rs: no restart recovery of compactions
- D promotes the first queued input when woken while idle · retries inbox input after prompt projection rolls back · does not strand a committed promotion when a post-commit listener defects — deliberate: session.rs: no steers, queues or inbox
- D runs different sessions concurrently · fans out one failed run and allows a later retry — effect-internal: concurrent resume joining / run fan-out
- P durably settles local tool failures before continuing — `tests/runner_parity.rs::oc_session_durably_settles_local_tool_failures_before_continuing`
- P returns unexpected local tool defects to the model and continues — `tests/oc_runner.rs::oc_session_returns_tool_failures_to_the_model_and_continues` (adapted: a failing tool call stands in for a tool defect)
- D preserves each parallel Promise hook failure — effect-internal: Promise hook failures
- P interrupts runner continuation on a decline after settling an ordinary tool error — `tests/runner_parity.rs::oc_session_interrupts_runner_continuation_on_a_decline_after_settling_an_ordinary_tool_error`
- P returns permission corrections to the model and continues — `tests/runner_parity.rs::oc_session_returns_permission_corrections_to_the_model_and_continues`
- P returns configured permission denials to the model and continues — `tests/runner_parity.rs::oc_session_returns_configured_permission_denials_to_the_model_and_continues`
- P interrupts runner continuation when a question is cancelled — `tests/runner_parity.rs::oc_session_interrupts_runner_continuation_when_a_question_is_cancelled`
- D awaits started local tools before surfacing provider stream failure · durably fails blocked local tools when a step is interrupted · interrupts a blocked step without local tool execution · durably fails blocked local tools when interrupted while awaiting settlement · keeps closed blocks and drops uncalled tool inputs when interrupted mid-stream — deliberate: runner.rs: tools run after the stream ends, so none is in flight when it fails or is interrupted
- P forces a text response on an agent's configured final step — `tests/runner_parity.rs::oc_session_forces_a_text_response_on_an_agents_configured_final_step`
- D resets the configured step allowance when steering input promotes — deliberate: session.rs: no steers, queues or inbox
- P projects provider errors as terminal assistant step failures — `tests/oc_runner.rs::oc_session_projects_provider_errors_as_terminal_failures` (adapted: the prompt stays in history for the next turn)
- P projects provider errors emitted before assistant step start — `tests/oc_runner.rs::oc_session_projects_provider_errors_as_terminal_failures` (adapted: the prompt stays in history for the next turn)
- P projects content-filter finishes as visible terminal failures — `tests/runner_parity.rs::oc_session_projects_content_filter_finishes_as_visible_terminal_failures`
- P settles a local tool before one content-filter step failure — `tests/oc_runner.rs::oc_session_settles_a_local_tool_before_a_content_filter_failure`
- F does not recover context overflow after durable assistant output — `tests/oc_runner.rs::oc_session_does_not_recover_context_overflow_after_output` (an overflow after partial output was compacted and retried; it now fails the turn)
- P bounds jittered exponential backoff before output for ${failure.name} ×2 — `src/retry.rs::oc_session_retry_keeps_the_general_allowance_for_other_transient_failures` (backoff schedule bounds)
- D immediately rebuilds once after explicit continuation rejection · bounds repeated continuation rejection to one immediate recovery — other: explicit continuation rejection is a Responses-WebSocket transport detail (tests/websocket_session.rs covers the transport)
- P retries an incomplete stream before output — `tests/runner_parity.rs::oc_session_retries_an_incomplete_stream_before_output`
- D marks only the retried attempt span with its attempt number ${failed.name} ×2 — effect-internal: tracing span attributes
- F retries an unknown finish before output — `tests/oc_runner.rs::oc_session_retries_an_unknown_finish_before_output` (an "unknown" finish ended the turn as a normal stop; it is now retried when no tool ran)
- D uses a larger provider retry-after delay · caps an excessive provider retry-after delay at fifteen minutes — deliberate: retry.rs: retry-after not honoured
- P continues ${type} after an output token limit ×2 — `tests/runner_parity.rs::oc_session_continues_text_after_an_output_token_limit`
- P compacts when the provider cuts an oversized prompt to its window and replies with nothing — `tests/runner_parity.rs::oc_session_compacts_when_the_provider_cuts_an_oversized_prompt_to_its_window_and_replies_with_nothing`
- P caps output token limit continuations and resets for new input (${tools}) ×2 — `tests/runner_parity.rs::oc_session_caps_output_token_limit_continuations_and_resets_for_new_input`
- P retries an incomplete stream cleanly from the prompt when no tool ran — `tests/oc_runner.rs::oc_session_retries_an_incomplete_stream_cleanly_from_the_prompt`
- F retries an unknown finish cleanly after observable text when no tool ran — `tests/oc_runner.rs::oc_session_retries_an_unknown_finish_cleanly_after_text` (as above, dropping the text it streamed)
- D retries after a mid-stream rate limit honoring retry-after without partial text — deliberate: retry.rs: retry-after not honoured
- P retries after an unrecognized mid-stream provider failure when no tool ran — `tests/oc_runner.rs::oc_session_retries_after_an_unrecognized_mid_stream_failure`
- P drops interrupted reasoning and retries cleanly without continuation prompt — `tests/oc_runner.rs::oc_session_drops_interrupted_reasoning_and_retries_cleanly`
- D retries an incomplete stream from a settled local tool · retries an incomplete stream from a settled local tool defect — deliberate: runner.rs: a failed stream is retried from the prompt, never resumed from a tool
- P stops incomplete stream retries once the retry allowance is spent — `tests/oc_runner.rs::oc_session_stops_incomplete_stream_retries_once_the_allowance_is_spent`
- P stops once the retry allowance is spent — `tests/runner_parity.rs::oc_session_stops_once_the_retry_allowance_is_spent`
- P retries a model call without consuming the logical agent step — `tests/runner_parity.rs::oc_session_retries_a_model_call_without_consuming_the_logical_agent_step`
- P does not retry non-eligible provider failures — `tests/runner_parity.rs::oc_session_does_not_retry_non_eligible_provider_failures`
- P records OpenCode Console's Go usage limit 429 as a typed step failure without retrying — `tests/runner_parity.rs::oc_session_records_a_usage_limit_429_without_retrying` (adapted: a provider usage limit, not OpenCode Console's)
- P settles unfinished tool input after an output limit — `tests/runner_parity.rs::oc_session_settles_unfinished_tool_input_after_an_output_limit`
- P continues malformed local tool input with a bounded excerpt (${finish}) ×2 — `src/runner.rs::oc_session_invalid_tool_input_explains_and_quotes_the_input` (the excerpt text; tests/runner_parity.rs covers the continuation)
- P continues after repeated malformed tool input — `tests/runner_parity.rs::oc_session_continues_after_repeated_malformed_tool_input`
- P does not continue malformed tool input past the agent step limit — `tests/runner_parity.rs::oc_session_does_not_continue_malformed_tool_input_past_the_agent_step_limit`
- P does not continue automatically after a provider error follows a local tool call — `tests/oc_runner.rs::oc_session_does_not_continue_after_a_provider_error_follows_a_tool_call`
- D rejects a second text start before the open fragment ends · projects sequential text fragments as separate content parts · broadcasts pending text while the provider stream is paused — storage-model: streamed part ordering in OpenCode's event model
- P durably closes partial ${kind} when the provider stream fails ×3 — `tests/runner_parity.rs::oc_session_durably_closes_partial_text_when_the_provider_stream_fails` (text, reasoning and tool input cases)
- D rejects duplicate streamed text starts · transitions streamed raw tool input to parsed called input · rejects malformed streamed tool input ordering — storage-model: streamed part ordering in OpenCode's event model

**`session-shell.test.ts`**

- D routes shell completion to the moved Session while retaining output from its original Location — deliberate: session.rs: no session moves
- D names the session, a truncated command and the exit on shell spans outside any turn — effect-internal: tracing spans

**`session-step.test.ts`**

- D lets a tool the user stopped fail with its own error and interrupts the others at once${locationClosed ? ", after a Location reload" : ""} ×2 · classifies what a tool raises while the user's stop settles it: a decline · classifies what a tool raises while the user's stop settles it: a defect · settles every call as interrupted on ${reason === "shutdown" ? "a server shutdown" : "an unexplained interrupt"} without a handoff ×2 · leaves an interrupted step as a crash would while the server hands its daemon over — deliberate: runner.rs: tools run after the stream; stop/shutdown hand-off semantics of OpenCode's daemon

**`session-tool-progress.test.ts`**

- D keeps progress live-only and terminal settlements durable — storage-model: live-only progress vs durable settlement rows

</details>

<details><summary>Subagents (45 of 46)</summary>


**`session-background.test.ts`**

- D includes active descendants with their agent and title · does not duplicate running background children or include itself and unrelated sessions · skips cancelled descendants when job state is available — deliberate: session.rs: no background subagents

**`subagent-completion.test.ts`**

- P leaves the report body unchanged when no background work runs — `src/tool/subagent.rs::oc_subagent_report_format`
- D formats shell, image, and subagent items with descendant session context · labels a completed report with pending background work as running, only in the model text · preserves a pre-formatted cancelled body so restart replay keeps running-work context — deliberate: session.rs: no background work runs past a subagent's report

**`subagent-idle-e2e.test.ts`**

- D a slow script returns a receipt, then wakes its session with the result · reports immediately with running background work and again after the shell exits · reports every directly prompted busy period on a finished background child · reports once for each background follow-up to an idle child · nested subagents report one level at a time · replays the report committed before a crash without running the child again · resumes a background child killed mid-turn and reports exactly once · deleting a parent mid-run settles cleanly without leftover job rows · deleting a child mid-run settles cleanly and clears its job — deliberate: session.rs: subagents run inside the tool call (no background reports or restart replay)

**`subagent-lifetime.test.ts`**

- D notifies an idle parent exactly once after its moved child ${outcome} (evict=${evict}) ×3 — deliberate: session.rs: no session moves or background subagents

**`subagent-report.test.ts`**

- P reports a directly prompted child when it goes idle — `tests/runner_parity.rs::oc_subagent_starts_a_child_session_and_returns_its_report` (adapted: reported when the tool call returns)
- D keeps an automation run's child quiet unless the parent followed up through a Job · reports immediately with still-running background work and skips non-running descendants · reports each later busy period with its own boundary and without stale running work · keeps control-only periods quiet while background jobs still report their own completion — deliberate: session.rs: subagents run inside the tool call (no idle reports, claims or outbox)
- P reports failures and user interruptions with running work preserved — `tests/runner_parity.rs::oc_subagent_reports_failures`
- D reports a creation-tick cancellation exactly once without a bound started event · sends nothing on shutdown and preserves the claim and running job marker · rolls back the idle projection and claim release when the report insert fails · commits the report and release before delivery, then acknowledges and wakes the parent · acknowledges a parent deleted before or after commit without crashing · retains a conflicted marker and keeps the settled hook alive — deliberate: session.rs: subagents run inside the tool call (no idle reports, claims or outbox)

**`tool-subagent.test.ts`**

- D admits one durable completion across live delivery and restart replay — deliberate: session.rs: subagents run inside the tool call (no background steering or notices)
- D registers globally while resolving agents from the caller location — client-server: global registration across Locations
- P prevents subagents from launching subagents by default — `tests/runner_parity.rs::oc_subagent_prevents_subagents_from_launching_subagents_by_default`
- P allows nested subagents up to the configured depth — `tests/runner_parity.rs::oc_subagent_allows_nested_subagents_up_to_the_configured_depth`
- P starts a child session and returns the running handle — `tests/runner_parity.rs::oc_subagent_starts_a_child_session_and_returns_its_report` (adapted: returns the report, not a running handle)
- P continues an existing child session — `tests/runner_parity.rs::oc_subagent_continues_an_existing_child_session`
- D steers a running child session in the background · follow-ups to a running child return immediately and notify once — deliberate: session.rs: subagents run inside the tool call (no background steering or notices)
- P rejects unrelated children and switches agents on continuation — `tests/runner_parity.rs::oc_subagent_rejects_unrelated_children`
- D notifies once when background work completes — deliberate: session.rs: subagents run inside the tool call (no background steering or notices)
- P reports the child's final answer to the parent — `tests/runner_parity.rs::oc_subagent_starts_a_child_session_and_returns_its_report`
- D reports only the current busy period when an idle child is continued — deliberate: session.rs: subagents run inside the tool call (no background steering or notices)
- P reports a child runner failure to the parent — `tests/runner_parity.rs::oc_subagent_reports_failures`
- D interrupts the child only for an explicit cancel and preserves shutdown claims — deliberate: session.rs: subagents run inside the tool call (no background steering or notices)

</details>

<details><summary>Compaction (21 of 51)</summary>


**`config/compaction.test.ts`**

- P merges settings and reloads changed config — `src/config.rs::oc_config_compaction_buffer_shares_and_invalid_values` (merge half; reload is not supported (no live reload))

**`session-compact.test.ts`**

- D durably coalesces manual compaction · coalesces concurrent manual compaction · commits a staged revert before admitting manual compaction — storage-model: coalescing manual compaction requests in the inbox

**`session-compaction.test.ts`**

- P compaction prompt preserves detailed work state and relevant files — `src/compaction.rs::oc_compaction_prompt_preserves_detailed_work_state_and_relevant_files`
- P compaction truncation does not split surrogate pairs — `src/compaction.rs::oc_compaction_truncation_does_not_split_characters`
- D summaries written before kept-message references keep their stored recent text · forks rebuild the recent text of a summary they copied · ignores non-message keepID strings when loading history or forking · history from the latest completed compaction matches the full transcript cut there · history never decodes rows before the latest compaction or its kept tail — deliberate: compaction.rs: the kept tail stays as real messages (no kept-message references or forks)
- P compaction prompt requires the checkpoint headings in order — `src/compaction.rs::oc_compaction_prompt_requires_the_checkpoint_headings_in_order`
- P compaction update prompt rewrites legacy checkpoints only when asked — `src/compaction.rs::oc_compaction_update_prompt_has_no_legacy_rewrite` (deliberate: no legacy rewrite variant)
- P compaction prompts prohibit task execution — `src/compaction.rs::oc_compaction_prompts_prohibit_task_execution`
- P auto compaction estimates current content against the buffered prompt ceiling — `src/compaction.rs::oc_compaction_auto_ceiling_and_buffers` (cases without input-only limits)
- P manual compaction summarizes short context instead of no-op — `tests/parity.rs::oc_compaction_manual_compaction_summarizes_short_context`
- P summary updates include the retained tail in compaction hooks and the model request — `tests/parity.rs::oc_compaction_updates_an_existing_checkpoint` (model-request half; no compaction hooks)
- D summary compaction past the ceiling keeps the conversation unchanged while the window fits · summary compaction sends the history before the tail unchanged when the whole conversation does not fit · compaction lowers the output limit when it sends a request it could not shrink · compaction keeps the output limit for a request that fits its target — deliberate: compaction.rs: a summary request is not shrunk and retried; the kept tail is real messages

</details>

<details><summary>Titles (21 of 26)</summary>


**`session-title.test.ts`**

- P generates a title from the sole user message and renames the session — `tests/parity.rs::oc_title_generates_a_title_from_the_sole_user_message`
- P runs title hooks instead of context hooks — `tests/parity.rs::oc_title_uses_a_given_title_without_a_model_request` (adapted: a caller-given title, no title hooks)
- P uses a hook-provided title without a model request — `tests/parity.rs::oc_title_uses_a_given_title_without_a_model_request` (adapted: a caller-given title, no title hooks)
- D uses a small model from the primary provider · falls back to the primary model when the small model fails · uses the primary model at its lowest variant without a small model · falls back to the selected primary variant when the lowest variant fails · uses the selected primary variant when no lower variant exists · keeps the primary default variant instead of a low variant that may enable reasoning · uses the primary none variant from its default variant — deliberate: title.rs: titles use the session's own model (no small model or variant selection)
- P generates from the first user message after later messages exist — `tests/parity.rs::oc_title_generates_from_the_first_user_message_only_once`
- D retries a legacy persisted fallback title — storage-model: legacy persisted fallback titles
- D regenerates an existing title using the title agent · bounds regeneration context while preserving the original request and recent conversation · keeps the clipped regeneration context well formed — deliberate: title.rs: no regeneration from later history
- P clips a long generated title without splitting an emoji after ${prefix} letters ×3 — `src/title.rs::oc_title_clips_a_long_title_without_splitting_an_emoji`
- D preserves the existing title when regeneration fails — deliberate: title.rs: no regeneration from later history
- P retries after a failed title request — `tests/parity.rs::oc_title_a_failed_request_falls_back_to_the_first_line` (deliberate: falls back instead of retrying)
- D preserves a manual rename completed while generation is in flight — deliberate: title.rs: no regeneration from later history

</details>

<details><summary>Model requests and transport (40 of 68)</summary>


**`session-model-transport-live.test.ts`**

- P continues a real Responses connection with only the appended input — `tests/websocket_session.rs` (one connection for the whole turn (see also codesmith-ai src/parity_ws.rs))
- D clears a rejected continuation and keeps one provider request per attempt — other: WebSocket edge cases are covered at the provider layer (codesmith-ai/src/parity_ws.rs) or not reproduced
- P reuses one real connection with handshake headers and ping/pong — `tests/websocket_session.rs` (one connection for the whole turn (see also codesmith-ai src/parity_ws.rs))
- P closes a real active connection on cancellation — `tests/websocket_session.rs` (one connection for the whole turn (see also codesmith-ai src/parity_ws.rs))
- D poisons a real connection after an unsupported binary frame · closes a real connection after an oversized frame · falls back once after a real rejected upgrade — other: WebSocket edge cases are covered at the provider layer (codesmith-ai/src/parity_ws.rs) or not reproduced

**`session-model-transport.test.ts`**

- D exposes response metadata once the lazy connection opens · commits checkpoints only after successful outer completion · selects the connection from the handshake and reopens when it changes · sends the frame the send tap returns and observes the frame the receive tap returns · does not carry a checkpoint across physical connection rotation · clears a rejected checkpoint before the runner retries full · rotates after the provider rejects the connection generation · closes the connection after a provider error frame so the next call reconnects — other: WebSocket connection pool internals (affinity, rotation, poisoning); the provider layer covers the protocol in codesmith-ai/src/parity_ws.rs
- P reuses one physical connection for sequential Session calls — `tests/websocket_session.rs` (one physical connection for sequential calls)
- D serializes concurrent calls for one Session · isolates connections and permits concurrency across Sessions · cancels a queued call without affecting the active exchange · closes the connection when an active exchange is interrupted · classifies active and queued close (observed: %s) ×2 · times out a hanging connect and falls back to http · times out an idle accepted request and poisons its socket · disables the idle timeout when chunkTimeout is false · closes a newly opened connection when request creation is interrupted · closes a connection returned after its owner closes during setup · falls back when connection setup fails and keeps the Session on HTTP · keeps the Session on HTTP after repeated mid-stream socket losses · falls back to HTTP after close code 1009 and keeps the Session on HTTP · preserves ${fromConnection ? "connection" : "error"} HTTP context on an ambiguous send failure without fallback ×2 · rotates when refreshed authorization changes handshake affinity · rotates when the connection exceeds its requested age limit · poisons a socket that receives data while idle · buffers a synchronous burst of frames larger than any fixed capacity · poisons unsupported binary frames after provider observation · closes individual and all owned connections · closes owned connections when the Location scope ends · records metadata-only lifecycle metrics — other: WebSocket connection pool internals (affinity, rotation, poisoning); the provider layer covers the protocol in codesmith-ai/src/parity_ws.rs

</details>

<details><summary>Snapshots and undo (4 of 15)</summary>


**`session-revert.test.ts`**

- P undoes and restores a file rename without losing either path — `tests/runner_parity.rs::oc_session_revert_undoes_and_restores_a_file_rename_without_losing_either_path`

**`snapshot.test.ts`**

- D keeps lazy repository discovery after the first caller is interrupted · captures concurrently from independent processes · applies availability transforms — effect-internal: lazy discovery, cross-process capture, availability transforms

</details>

<details><summary>Sessions and their store (178 of 308)</summary>


**`bus-session-routing.test.ts`**

- D delivers workspace-only moves to both owners without duplicating same-location moves · routes forks through their parent before the child exists · routes existing Sessions without changing public events or global delivery · applies the same routing to typed and multi-type subscriptions · snapshots routing across creation and moves for slow subscribers · routes a cold Session deletion before its projector removes ownership · preserves routing through a batch that moves and deletes a Session · does not change ownership when single or batched moves roll back · updates cached ownership on silent replay and filters published replay — storage-model: OpenCode's event bus routing of sessions between Locations

**`session-activation.test.ts`**

- D runs prompt hooks from a cold Location before admitting a prompt · resolves plugin commands from a cold Location — client-server: cold Location activation

**`session-create.test.ts`**

- D preserves the project canonical directory when creating a session in another clone · repeat Session.create on the same location spawns no git · follows the directory's project identity established after creation · does not adopt nested repositories or sessions in another workspace — client-server: project identity across clones/workspaces
- D stores supplied immutable create attributes · stores creation metadata and inherits it through children and forks — storage-model: immutable create attributes and creation metadata
- D returns the current Session projection after updates · persists creation through the current created event · persists caller-ID creation through the existing created event · includes current creation rows in the Session event stream · replays one prompt lifecycle into a fresh target database · does not mask unrelated created projector defects — storage-model: projector created events and replay
- D imports projected messages and reserves their aggregate sequence · clamps imported busy and viewed watermarks to their idle transition · imports a long transcript in statements under 100 bound parameters — storage-model: aggregate sequences, watermarks and bound-parameter limits

**`session-environment.test.ts`**

- P stores replacement snapshots by session — `src/tool/shell.rs::oc_session_environment_stores_replacement_snapshots_by_session`

**`session-instructions.test.ts`**

- D toLLMMessages does not forward synthetic metadata to the provider — storage-model: synthetic metadata stripping

**`session-list.test.ts`**

- D pages through the filtered store results in both directions · returns 50 sessions when no limit is given · rejects an invalid cursor — storage-model: cursor paging of the session list (CodeSmith lists in full)

**`session-log.test.ts`**

- D replays public session events and marks synced at the aggregate watermark · continues with live public events when following · fails with NotFound for an unknown session · reads across undecodable gaps in aggregate order and marks the true log position · completes with a bare synced marker for a migrated Session with no event sequence — storage-model: public session event log replay

**`session-move.test.ts`**

- D moves through the bound service without depending on the Session facade · delegates to the provided move service · moves an idle session from ${broken ? "broken" : "healthy"} source configuration ×2 · recovers an idle session from an unreadable source directory · uses the ${broken ? "broken" : "healthy discovery-disabled"} selected instance rather than the default Location ×2 · does not recover or enqueue a move when the ${interrupt} interrupts the probe ×2 · does not recover if execution starts during the source probe · recovers a missing source without initializing its instance and retains destination workspace identity · allows inbox cancellation during a source probe and rejects stale ${changed} recovery ×2 · rejects an unavailable destination before admitting the move · applies a move immediately when the source directory no longer exists · defers an active move when the source directory no longer exists · keeps a moved session out of its former directory's new identity — deliberate: session.rs: no moving sessions between Locations

**`session-owned.test.ts`**

- D owns state changes and message reads without caller services or Location acquisition · acquires Location only for new prompt preparation and persists before waking · keeps the first admission across handles, including delivered retries and identity conflicts · reads fresh placement through an existing handle after a projected move · activates skills through detached handles using fresh placement and ambient publication context · checks Session existence before skill lookup and leaves missing activations untouched · publishes skills before detached resumes and owns those resumes in the host scope · keeps prompt wakes independent of shell work across handles · mutates only this handle's pending inbox and preserves public conflict tags · joins same-ID resumes without transferring execution ownership to a cancelled caller · keeps preparation interruptible without admitting input or committing a staged revert · selects the destination's snapshot service after a move · prepares repeatable input without admitting it · captures snapshots when staging and restores them when clearing · captures the provided Inbox service when constructing Session · captures the host dependencies for detached commands · returns checked user and synthetic admissions and typed pending or delivered conflicts · checks the winner of concurrent admissions before returning it · exposes failed pending transitions as typed conflicts and rolls back their events · does not turn unrelated projector defects into conflicts — client-server: session handles, inbox ownership and Location placement

**`session-projector.test.ts`**

- D does not settle a pending manual compaction on an auto failure · loads legacy revert storage into canonical state · projects staged, cleared, and committed reverts · orders projected messages and context by durable aggregate sequence · maps malformed persisted rows consistently while single-message lookup defects · checks session existence before resolving a missing message cursor · consumes the pending row and projects the message at promotion · projects durable context messages supported by the updater · rejects distinct creator events that reuse one projected message ID · projects retry state and clears it at the next step or execution terminal · does not infer restart continuation from lifecycle history · records each execution's start, including a resume after shutdown · updates only the newest incomplete assistant projection · reads and replays native data stored under its pre-v2.1 names · merges tool result native data into the call's native data · projects ended and failed step terminal state · does not revive a stale incomplete assistant projection · bumps session time_updated on step lifecycle events · leaves a settled inline assistant message untouched when a new step starts · stores interleaved assistant parts in session_part by ordinal across out-of-order tool completions and forks · appends inline content after existing session_part rows when splitting an inline assistant row · keeps projecting into session_part rows a newer release wrote for an in-progress message · forks a settled message whose parts are in session_part with its content · keeps session_part rows when an older build left inline content on the same message · reads a split message an older build added inline content to as its parts then that content · forks a split message an older build added inline media to · deletes a reverted message's session_part rows and the blobs only they referenced · forks and reads a long session in statements under 100 bound parameters · hydrates parts when envelope content is omitted or non-array, and fails to decode non-array content without parts · writes whole messages inline with experimental.part_rows off · reads the same reply when experimental.part_rows is switched on, off and on again mid-session — storage-model: OpenCode's event projector rows

**`session-prompt.test.ts`**

- D exposes the execution registry · delegates execution continuation through SessionExecution · delegates process-local interruption through SessionExecution · forwards interrupt continuation policy · delegates interruption without requiring a recorded Session — effect-internal: execution delegation
- D durably admits one user message before transcript promotion — storage-model: inbox admission before promotion
- D streams durable Session events after an aggregate sequence · resumes through a recorded message without appending another prompt · records distinct messages when the ID is omitted · returns the original recorded message when the ID is retried · reconciles an exact retry from the promoted message without admission history · ignores delivery when retrying a promoted message · wakes execution when an exact prompt retry recovers a committed message · keeps the first admission when one ID is reused with a different prompt · keeps the first admission's delivery mode when one ID is reused with another · returns one recorded message to concurrent exact retries · promotes one message once under concurrent promotion attempts · reprojects pending inbox input without scheduling execution · rejects reuse of one globally unique message ID across sessions · rejects a prompt ID already used by visible Session history — storage-model: idempotent prompt admission and event streams
- D starts execution by default after recording the prompt · starts execution when resume is explicitly true · only records the prompt when resume is false — client-server: resume flag on the prompt API
- D keeps the first admission's metadata when one ID is reused with other metadata — storage-model: idempotent prompt admission and event streams
- D durably admits synthetic input before transcript promotion — storage-model: inbox admission before promotion
- D reconciles synthetic retries from the promoted message regardless of payload — storage-model: idempotent prompt admission and event streams
- D keeps queued input pending until the idle boundary · promotes prompt and synthetic steers in admission order · lists admitted work in admission order until promotion · lists an unhandled compaction until it is cancelled · cancels pending input and allows its ID to be admitted again · moves pending input between steer and queue delivery — deliberate: session.rs: no steers or queues

**`session-remove.test.ts`**

- D removes unloaded sessions and children without initializing an instance — client-server: removal without an instance

**`session-runner-message.test.ts`**

- P omits empty assistant turns — `src/runner.rs::oc_session_message_omits_empty_assistant_turns`
- D maps every top-level Session message type — storage-model: maps OpenCode's stored message types
- D leaves out the recent context of a checkpoint that kept none — deliberate: compaction.rs: the kept tail stays as real messages
- D replays flat state under an OpenCode hosted model's route key — cloud: OpenCode hosted model route key
- D retries a failed stream from its last local call that ran — deliberate: runner.rs: tools run after the stream ends, a failed stream is retried from the prompt
- P omits unfinished reasoning from an interrupted assistant message — `src/runner.rs::oc_session_message_omits_reasoning_only_assistant_turns`
- D replays tools migrated from V1, which never recorded when they ran — storage-model: V1-migrated tool rows

**`session-runner-recorded.test.ts`**

- D executes one recorded prompt through the recorded HTTP transport — other: OpenCode's recorded-HTTP transport harness (CodeSmith replays recordings in codesmith-ai/src/replay_tests.rs)

**`session-runner-retry.test.ts`**

- P stops retrying transport timeouts after three attempts — `src/retry.rs::oc_session_retry_stops_retrying_transport_timeouts_after_three_attempts`
- P keeps the general allowance for other transient failures — `src/retry.rs::oc_session_retry_keeps_the_general_allowance_for_other_transient_failures`
- P keeps retrying an unreachable host for an hour with gaps of at most 30 seconds — `src/retry.rs::oc_session_retry_keeps_retrying_an_unreachable_host_for_an_hour_with_gaps_of_at_most_30_seconds`
- P keeps a refused connection on the general allowance — `src/retry.rs::oc_session_retry_keeps_a_refused_connection_on_the_general_allowance`
- P doesn't spend the general allowance on an unreachable host — `src/retry.rs::oc_session_retry_doesnt_spend_the_general_allowance_on_an_unreachable_host`
- P doesn't replay a call whose response was cut off by the network going down — `src/retry.rs::oc_model_retry_one_shot_doesnt_replay_a_call_cut_off_by_the_network_going_down`

**`session-runner-tool-events.test.ts`**

- D interrupted progress metadata remains in the terminal failure snapshot · local failure metadata completes the progress snapshot · failure snapshot retains canonical progress above the default byte limit · failure before progress omits partial output fields — storage-model: progress/failure snapshots of OpenCode's tool parts
- D provider metadata is flattened using the route key · reasoning state from start, empty delta, and end is merged — storage-model: provider metadata flattening and reasoning-state merging in event rows
- D publishes the trailing ${kind} chunk while the provider is paused ×2 · batches text deltas and flushes pending text before the terminal event · publishes batched deltas before the next block starts · retains new chunks and orders text-end behind an in-flight timer publication · batches reasoning deltas and flushes pending reasoning before the terminal event — storage-model: event batching/publishing order of the projector
- D authoritative end values replace accumulated deltas in the durable ended events — storage-model: provider metadata flattening and reasoning-state merging in event rows
- D tool input deltas are accumulated without being published — storage-model: event batching/publishing order of the projector
- D binary failure emits no success event — storage-model: binary failure events
- D step start waits for the pending start snapshot · step finish records settlement without publishing step ended — storage-model: event batching/publishing order of the projector
- P content-filter failure explains the refusal when the provider gives a reason — `src/runner.rs::oc_content_filter_failure_explains_the_refusal`

**`session-stats.test.ts`**

- D counts tool calls in both session_part rows and inline content an older build left on one message · aggregates compaction usage across more than 100 sessions under the 100-parameter limit — storage-model: OpenCode's part-row layouts and parameter limits

**`session-store.test.ts`**

- D lists by updated time and ID with exclusive two-item pages in either direction · pages messages by durable sequence, not timestamp or ID, and scopes cursor lookup · hydrates and settles assistant messages across session_part rows and legacy inline content rows · lastAssistantAfter returns the newest completed error-free assistant after the boundary with parts hydrated · lastAssistantAfter pages past more than twenty incomplete or errored assistant messages · lastAssistantAfter returns undefined without a qualifying assistant after the boundary — storage-model: OpenCode's message paging and part-row hydration

**`session-wait.test.ts`**

- D delegates to SessionExecution.awaitIdle — effect-internal: awaitIdle delegation

</details>

<details><summary>Tool: read (66 of 82)</summary>


**`tool-read-filesystem.test.ts`**

- P {input.name} ×15 — `tests/oc_tools.rs::oc_read_text_serialization`
- D preserves the environment not-found error — client-server: Environment not-found error
- P reads malformed UTF-8 lossily and still rejects null-byte binary content — `tests/oc_tools.rs::oc_read_reads_malformed_utf8_lossily_and_rejects_binary`
- P reads text despite a binary-associated extension — `tests/oc_tools.rs::oc_read_reads_text_despite_a_binary_extension`
- P returns the last lines when the offset is past the end — `tests/oc_tools.rs::oc_read_returns_the_last_lines_when_the_offset_is_past_the_end`
- P returns the last lines of a file larger than the first chunk when the offset is past the end — `tests/oc_tools.rs::oc_read_returns_the_last_lines_of_a_large_file_past_the_end`
- P starts a negative offset that many lines before the end — `tests/oc_tools.rs::oc_read_starts_a_negative_offset_that_many_lines_before_the_end`
- P starts a negative offset from the end of a file larger than the first chunk — `tests/oc_tools.rs::oc_read_starts_a_negative_offset_from_the_end_of_a_large_file` (without the 300 KB last-line case)
- P reads the end of a file many chunks long — `tests/oc_tools.rs::oc_read_reads_the_end_of_a_file_many_chunks_long`
- P pages text with one-based offsets — `tests/oc_tools.rs::oc_read_pages_text_with_one_based_offsets`
- P truncates long lines — `tests/oc_tools.rs::oc_read_truncates_long_lines`
- P reads PDFs as bounded media — `src/tool/read.rs::oc_read_pdfs_as_media`

**`tool-read.test.ts`**

- P registers, authorizes, and reads through the location filesystem — `src/tool/read.rs::oc_read_registers_authorizes_and_reads`
- P places read in Code Mode with full_codemode unless demoted to direct — `src/tool/read.rs::oc_read_code_mode_placement`
- P rejects a zero limit before reading — `src/tool/read.rs::oc_read_rejects_zero_limit_before_reading`
- P asks for external_directory approval before reading an external absolute path — `src/tool/read.rs::oc_read_asks_external_directory_before_external_read`
- P returns a small PNG as native media instead of durable base64 text — `src/tool/read.rs::oc_read_small_png_as_media`
- P preserves a PNG above the generic text limit as native media — `src/tool/read.rs::oc_read_png_above_text_limit_as_media`
- P returns the original image when the resizer is unavailable — `src/tool/read.rs::oc_read_original_image_when_resizer_unavailable`
- P drops undecodable image data from the outcome — `src/tool/read.rs::oc_read_drops_undecodable_image_data`
- P drops oversized images from the outcome when resizing is disabled — `src/media.rs::oc_image_drops_oversized_images_when_resizing_is_disabled, src/tool/read.rs::oc_read_drops_oversized_images_without_resizing`
- P resizes images to configured dimensions before returning media — `src/media.rs::oc_image_resizes_to_configured_dimensions, src/tool/read.rs::oc_read_resizes_images_to_configured_dimensions`
- P stores large opaque PNG screenshots as JPEG — `src/media.rs::oc_image_stores_large_opaque_png_screenshots_as_jpeg, src/tool/read.rs::oc_read_large_opaque_png_screenshots_as_jpeg`
- P keeps fitting PNGs that are small, transparent, or smaller than their JPEG — `src/media.rs::oc_image_keeps_fitting_pngs_that_are_small_transparent_or_smaller_than_their_jpeg, src/tool/read.rs::oc_read_keeps_fitting_pngs`
- P accepts PNG candidates at the exact base64 limit and skips them one byte below — `src/media.rs::oc_image_accepts_png_candidates_at_the_exact_base64_limit_and_skips_them_one_byte_below, src/tool/read.rs::oc_read_png_candidates_at_base64_limit`
- P drops images that cannot fit max base64 bytes after resize attempts — `src/tool/read.rs::oc_read_drops_images_that_cannot_fit_max_bytes`
- P returns supported image contents despite a misleading binary extension — `src/tool/read.rs::oc_read_image_despite_misleading_extension`
- P returns PDFs as native media — `src/tool/read.rs::oc_read_pdfs_as_media`
- P attaches real image and PDF files without their bytes in the structured output — `src/tool/read.rs::oc_read_real_image_and_pdf_files`
- P returns expected filesystem failures to the model — `src/tool/read.rs::oc_read_binary_failure_returned_to_model`
- P preserves actionable read failure messages — `src/tool/read.rs::oc_read_preserves_actionable_failure_messages`
- P preserves unexpected filesystem defects — `src/tool/read.rs::oc_read_unexpected_filesystem_failures`
- P does not read when permission is denied — `src/tool/read.rs::oc_read_denied_permission_does_not_read`
- P returns missing paths as model-visible tool failures — `src/tool/read.rs::oc_read_missing_paths_are_tool_failures`
- P names the first missing folder and its nearest existing parent — `src/tool/read.rs::oc_read_names_first_missing_folder_and_nearest_parent`
- P trusts the environment, not this host, about which folders exist — `src/tool/read.rs::oc_read_trusts_the_filesystem_about_folders`
- P suggests close names from the same folder — `src/tool/read.rs::oc_read_suggests_close_names_from_same_folder`
- P ranks same-folder suggestions and shows at most three — `src/tool/read.rs::oc_read_ranks_suggestions_and_shows_three`
- P suggests a path written from the parent of the location first — `src/tool/read.rs::oc_read_suggests_path_from_parent_of_location_first`
- P suggests close folder names from the nearest existing folder — `src/tool/read.rs::oc_read_suggests_close_folder_names`
- P recovers a unique file whose non-breaking spaces differ — `src/tool/read.rs::oc_read_recovers_file_whose_nonbreaking_spaces_differ`
- P recovers an external filename without repeating directory approval — `src/tool/read.rs::oc_read_recovers_external_name_without_repeating_directory_approval`
- P does not read a recovered filename denied by its own read rules — `src/tool/read.rs::oc_read_recovered_name_denied_by_own_rules`
- P does not recover ambiguous files — `src/tool/read.rs::oc_read_does_not_recover_ambiguous_files`
- P does not recover a directory — `src/tool/read.rs::oc_read_does_not_recover_a_directory`
- P lists a bounded directory page through read — `src/tool/read.rs::oc_read_lists_bounded_directory_page`
- P normalizes a zero directory offset in the model heading — `src/tool/read.rs::oc_read_zero_directory_offset_heading`
- P does not list a directory when permission is denied — `src/tool/read.rs::oc_read_denied_directory_is_not_listed`
- P forwards pagination and returns bounded text pages with continuation — `src/tool/read.rs::oc_read_pagination_with_continuation`
- P forwards a negative offset and shows the real line numbers and continuation — `src/tool/read.rs::oc_read_negative_offset_shows_real_line_numbers`
- P returns the negative directory offset error to the model — `src/tool/read.rs::oc_read_negative_directory_offset_error`
- P reads virtual skill:// files and directories from Skill.Service without external_directory approval — `src/tool/read.rs::oc_read_virtual_skill_paths`

</details>

<details><summary>Tool: edit (22 of 22)</summary>


**`tool-edit.test.ts`**

- P registers and replaces relative exact text through FileMutation once — `src/tool/edit.rs::oc_edit_replaces_relative_exact_text_once`
- P returns the diff for final formatted content — `src/tool/edit.rs::oc_edit_reports_final_formatted_content`
- P accepts an absolute file path inside the active Location — `src/tool/edit.rs::oc_edit_accepts_absolute_path_inside_location`
- P edits an external symlink target with only its in-location permission — `src/tool/edit.rs::oc_edit_external_symlink_target_needs_external_approval`
- P approves an explicit external absolute path before edit — `src/tool/edit.rs::oc_edit_approves_external_absolute_path_before_edit`
- P does not write when external_directory or edit approval is denied — `src/tool/edit.rs::oc_edit_no_write_when_external_or_edit_denied`
- P denied edit does not disclose whether oldString matches — `src/tool/edit.rs::oc_edit_denied_edit_does_not_disclose_match`
- P rejects no-op, empty, missing, and ambiguous exact replacements — `src/tool/edit.rs::oc_edit_rejects_noop_empty_missing_and_ambiguous`
- P points a missed oldString at the closest lines, or at newString already applied, when read is allowed — `src/tool/edit.rs::oc_edit_miss_points_at_closest_lines_or_applied_new_string`
- P returns specific missing file and directory errors — `src/tool/edit.rs::oc_edit_missing_file_and_directory_errors`
- P replaces every exact occurrence when replaceAll is true — `src/tool/edit.rs::oc_edit_replace_all_replaces_every_occurrence`
- P normalizes Unicode typography only after exact matching fails — `src/tool/edit.rs::oc_edit_normalizes_typography_only_after_exact_fails`
- P ignores trailing whitespace while preserving untouched lines — `src/tool/edit.rs::oc_edit_ignores_trailing_whitespace_preserving_untouched_lines`
- P uses non-overlapping trailing-whitespace matches and preserves CRLF — `src/tool/edit.rs::oc_edit_non_overlapping_trailing_matches_and_crlf`
- P preserves BOM and CRLF line endings — `src/tool/edit.rs::oc_edit_preserves_bom_and_crlf`
- P refuses to rewrite a file that is not valid UTF-8 — `src/tool/edit.rs::oc_edit_refuses_invalid_utf8`
- P matches across mixed line endings and keeps each region's endings — `src/tool/edit.rs::oc_edit_mixed_endings_keep_each_regions_endings`
- P keeps the endings of unchanged and in-place lines inside a multi-line match — `src/tool/edit.rs::oc_edit_keeps_endings_of_unchanged_and_in_place_lines`
- P applies edits that differ only in \r to the exact bytes — `src/tool/edit.rs::oc_edit_carriage_return_only_edits_apply_to_bytes`
- P fails instead of reporting success when the edit leaves the file unchanged — `src/tool/edit.rs::oc_edit_fails_when_edit_leaves_file_unchanged`
- P serializes concurrent edit transactions — `src/tool/edit.rs::oc_edit_serializes_concurrent_edits`
- P applies the edit when content changes after matching — `src/tool/edit.rs::oc_edit_applies_when_content_changes_after_matching`

</details>

<details><summary>Tool: write (12 of 19)</summary>


**`file-mutation.test.ts`**

- D appends bytes to a file, creating it when missing — other: append API of OpenCode's FileMutation service (no tool appends)
- D shares transaction locks across Location service instances · allows transaction locks for distinct resolved paths to proceed independently · allows distinct absolute targets to proceed independently — effect-internal: transaction locks across service instances

**`tool-write.test.ts`**

- P registers and creates a relative file through FileMutation once — `tests/oc_tools.rs::oc_write_creates_a_relative_file_and_its_parents`
- P overwrites a relative existing file and reports that it wrote the file — `tests/oc_tools.rs::oc_write_overwrites_an_existing_file`
- F preserves exactly one BOM when overwriting existing files — `tests/oc_tools.rs::oc_write_preserves_exactly_one_bom` (overwriting a file dropped its byte order mark)
- P accepts an absolute file path inside the active Location — `tests/oc_tools.rs::oc_write_accepts_an_absolute_path_inside_the_project`
- D writes an external symlink target with only its in-location permission — deliberate: CodeSmith follows the link and asks external_directory for its target (tests/oc_tools.rs::oc_write_external_symlink_target_needs_external_approval pins it)
- P approves an explicit external absolute path before edit — `tests/oc_tools.rs::oc_write_approves_an_external_path_before_edit` (adapted: names the file, saves its folder)
- P does not write when external_directory or edit approval is denied — `tests/oc_tools.rs::oc_write_does_not_write_when_denied`
- D creates a new unformatted file without span failures or post-write reads — effect-internal: tracing spans and post-write reads

</details>

<details><summary>Tools: glob and grep (37 of 39)</summary>


**`filesystem/search.test.ts`**

- D creates the fff picker on first search and waits for its initial scan · returns partial fff results once the initial scan wait times out · keeps the fff picker owned by the layer when the first search is interrupted · real fff finds files on first search · selects the ripgrep layer for workspace-backed locations even when vcs would pick fff · refreshes a stale ripgrep index atomically without blocking search · reuses location-owned fuzzy targets across index refreshes — other: OpenCode's fff/ripgrep index layers; CodeSmith walks the tree per call

**`ripgrep.test.ts`**

- P keeps matching files when an ignore glob is malformed and a symlink is broken — `tests/oc_tools.rs::oc_ripgrep_keeps_matches_despite_a_malformed_ignore_glob_and_a_broken_symlink`
- P glob honors hidden=${hidden} before limit=${limit} ×3 — `tests/oc_tools.rs::oc_search_glob_honors_hidden_before_limit` (through the glob tool)
- P globs files as an array — `tests/oc_tools.rs::oc_ripgrep_globs_files`
- P greps files with include filtering — `tests/oc_tools.rs::oc_ripgrep_greps_files_with_include_filtering`
- F greps lines that are not valid UTF-8 — `tests/oc_tools.rs::oc_ripgrep_greps_lines_that_are_not_valid_utf8` (grep dropped every line of a file with invalid UTF-8; now decoded lossily)
- P keeps ignored files out of catch-all find results — `tests/oc_tools.rs::oc_ripgrep_keeps_ignored_files_out_of_catch_all_results`
- P never includes git metadata — `tests/oc_tools.rs::oc_ripgrep_never_includes_git_metadata`
- D excludes protected directory trees from catch-all find results — other: the `exclude` option of OpenCode's internal find (home scans); the tools have no such input
- P returns a bounded preview for matches on oversized lines — `tests/oc_tools.rs::oc_ripgrep_bounds_previews_of_oversized_lines_on_whole_characters` (adapted: 2,000 characters and "…")
- P ends oversized line previews on a whole character — `tests/oc_tools.rs::oc_ripgrep_bounds_previews_of_oversized_lines_on_whole_characters` (adapted: 2,000 characters and "…")
- D accepts a download matching the pinned hash · refuses a download whose hash does not match — platform: downloading a pinned ripgrep binary (CodeSmith links ripgrep's crates)

**`tool-search.test.ts`**

- F ${name} distinguishes a malformed filename glob from an empty search ×2 — `tests/oc_tools.rs::oc_search_distinguishes_a_malformed_glob_from_an_empty_search` (the glob error read "Invalid glob in pattern error parsing glob ...")
- P glob honors hidden=${hidden} before limit=${limit} ×3 — `tests/oc_tools.rs::oc_search_glob_honors_hidden_before_limit`
- P bounds omitted glob and grep limits — `tests/oc_tools.rs::oc_search_bounds_omitted_glob_and_grep_limits`
- P rejects an empty grep pattern — `tests/oc_tools.rs::oc_search_rejects_an_empty_grep_pattern` (adapted: no argument-validation envelope)
- P handles explicit grep file and directory paths — `tests/oc_tools.rs::oc_search_handles_explicit_grep_file_and_directory_paths`
- P reports no grep matches — `tests/oc_tools.rs::oc_search_reports_no_grep_matches`
- P reports invalid grep regex details — `tests/oc_tools.rs::oc_search_reports_invalid_grep_regex_details`
- P requires external_directory approval for external grep files and directories — `tests/oc_tools.rs::oc_search_external_grep_needs_external_directory_approval` (adapted: a file is named, its folder saved)
- F ${name} reports a missing search path ×2 — `tests/oc_tools.rs::oc_search_reports_a_missing_search_path` (glob said "not a directory", grep "Unable to grep for ...")
- P reports a file used as the glob search path — `tests/oc_tools.rs::oc_search_reports_a_file_used_as_the_glob_search_path`
- P requires external_directory approval for an explicit external glob path — `tests/oc_tools.rs::oc_search_external_glob_path_needs_external_directory_approval`
- D globs through an in-location external symlink without external approval — deliberate: CodeSmith follows the link and asks external_directory for its target (tests/oc_tools.rs::oc_search_globs_through_an_in_location_symlink_after_external_approval pins it)

</details>

<details><summary>Tool: webfetch (100 of 100)</summary>


**`tool-html-markdown-budget.test.ts`**

- P finishes bounded code conversion: %s ×9 — `src/tool/html_markdown.rs::oc_html_markdown_budget_finishes_bounded_code_conversion`

**`tool-webfetch.test.ts`**

- P defaults format and rejects invalid timeout controls — `src/tool/webfetch.rs::oc_webfetch_defaults_format_and_rejects_invalid_timeouts`
- P ports HTML text and markdown conversions without active content — `src/tool/html_markdown.rs::oc_webfetch_ports_text_and_markdown_without_active_content`
- P renders headings, inline semantics, links, images, breaks, and thematic breaks — `src/tool/html_markdown.rs::oc_webfetch_renders_headings_links_images_breaks_and_rules`
- P preserves inline and preformatted code verbatim with safe fences — `src/tool/html_markdown.rs::oc_webfetch_preserves_inline_and_preformatted_code`
- P preserves inline code boundaries for %j ×14 — `src/tool/html_markdown.rs::oc_webfetch_preserves_inline_code_boundaries`
- P fits inline code to its emitted boundaries: %s ×17 — `src/tool/html_markdown.rs::oc_webfetch_fits_inline_code_to_its_emitted_boundaries`
- P keeps nested ordered and unordered lists structurally readable — `src/tool/html_markdown.rs::oc_webfetch_keeps_nested_lists_readable`
- P renders blockquotes and tables as readable Markdown — `src/tool/html_markdown.rs::oc_webfetch_renders_blockquotes_and_tables`
- P decodes entities and normalizes prose whitespace without joining words — `src/tool/html_markdown.rs::oc_webfetch_decodes_entities_and_normalizes_whitespace`
- P omits active and fallback content while retaining surrounding prose — `src/tool/html_markdown.rs::oc_webfetch_omits_active_and_fallback_content`
- P is deterministic and bounded for malformed input across parser chunks — `src/tool/html_markdown.rs::oc_webfetch_is_deterministic_and_bounded_for_malformed_input`
- P defaults to the production byte budget with room for closing syntax — `src/tool/html_markdown.rs::oc_webfetch_defaults_to_the_production_byte_budget`
- P preserves UTF-8 boundaries at the content limit for %s ×3 — `src/tool/html_markdown.rs::oc_webfetch_preserves_utf8_boundaries_at_the_content_limit`
- P bounds deeply nested list output and fragmented code fences — `src/tool/html_markdown.rs::oc_webfetch_bounds_deep_lists_and_fragmented_fences`
- P escapes prose that would otherwise become Markdown structure — `src/tool/html_markdown.rs::oc_webfetch_escapes_prose_that_would_become_structure`
- P preserves code whitespace and quotes every line of multiline blocks — `src/tool/html_markdown.rs::oc_webfetch_quotes_every_line_of_multiline_blocks`
- P keeps nested blockquotes inside their outer quote — `src/tool/html_markdown.rs::oc_webfetch_keeps_nested_blockquotes_inside_the_outer_quote`
- P keeps visible whitespace around inline emphasis — `src/tool/html_markdown.rs::oc_webfetch_keeps_whitespace_around_inline_emphasis`
- P captures formatting elements inside preformatted content as code only — `src/tool/html_markdown.rs::oc_webfetch_formatting_inside_pre_is_code_only`
- P normalizes multiline table cells without changing their columns — `src/tool/html_markdown.rs::oc_webfetch_normalizes_multiline_table_cells`
- P flattens nested tables without corrupting the outer table — `src/tool/html_markdown.rs::oc_webfetch_flattens_nested_tables`
- P preserves loose text around malformed table rows — `src/tool/html_markdown.rs::oc_webfetch_preserves_loose_text_around_malformed_rows`
- P escapes tilde fences and removes empty emphasis markers — `src/tool/html_markdown.rs::oc_webfetch_escapes_tilde_fences_and_drops_empty_emphasis`
- P does not confuse source NUL text with buffered code — `src/tool/html_markdown.rs::oc_webfetch_does_not_confuse_nul_text_with_code`
- P preserves multiline inline code verbatim — `src/tool/html_markdown.rs::oc_webfetch_preserves_multiline_inline_code`
- P prefixes inline code at the start of a blockquote line — `src/tool/html_markdown.rs::oc_webfetch_prefixes_inline_code_at_a_blockquote_line_start`
- P keeps links nested in inline code associated with their text — `src/tool/html_markdown.rs::oc_webfetch_keeps_links_in_inline_code_with_their_text`
- P indents nested list continuations and preserves ordered numbering — `src/tool/html_markdown.rs::oc_webfetch_indents_list_continuations_and_numbering`
- P renders block content outside link syntax — `src/tool/html_markdown.rs::oc_webfetch_renders_block_content_outside_link_syntax`
- P recovers nested anchors without unmatched Markdown syntax — `src/tool/html_markdown.rs::oc_webfetch_recovers_nested_anchors`
- P keeps emphasis whitespace through neutral wrappers — `src/tool/html_markdown.rs::oc_webfetch_keeps_emphasis_whitespace_through_wrappers`
- P flattens preformatted content inside table cells — `src/tool/html_markdown.rs::oc_webfetch_flattens_pre_inside_table_cells`
- P preserves Unicode in inline constructs — `src/tool/html_markdown.rs::oc_webfetch_preserves_unicode_in_inline_constructs`
- P preserves block content and following lists — `src/tool/html_markdown.rs::oc_webfetch_preserves_block_content_and_following_lists`
- P keeps quoted code with long delimiter runs inside a safe closed fence — `src/tool/html_markdown.rs::oc_webfetch_long_delimiter_runs_stay_in_a_closed_fence`
- P separates reconstructed tables from adjacent inline and quoted content — `src/tool/html_markdown.rs::oc_webfetch_separates_tables_from_adjacent_content`
- P keeps multiline quoted code closed before following prose — `src/tool/html_markdown.rs::oc_webfetch_closes_multiline_quoted_code_before_prose`
- P keeps active content suppressed when depth fallback begins — `src/tool/html_markdown.rs::oc_webfetch_active_content_stays_suppressed_in_depth_fallback`
- P keeps visible text after depth fallback begins inside preformatted content — `src/tool/html_markdown.rs::oc_webfetch_visible_text_survives_depth_fallback_inside_pre`
- P resumes links around every block structure — `src/tool/html_markdown.rs::oc_webfetch_resumes_links_around_every_block`
- P indents child lists from the actual parent marker width — `src/tool/html_markdown.rs::oc_webfetch_indents_child_lists_from_the_marker_width`
- P renders captions and definition lists with readable boundaries — `src/tool/html_markdown.rs::oc_webfetch_renders_captions_and_definition_lists`
- P falls back to row-oriented text for table spans — `src/tool/html_markdown.rs::oc_webfetch_falls_back_to_rows_for_table_spans`
- P suppresses head and hidden subtrees while retaining visible body content — `src/tool/html_markdown.rs::oc_webfetch_suppresses_head_and_hidden_subtrees`
- P preserves pre breaks and normalizes multiline link titles — `src/tool/html_markdown.rs::oc_webfetch_preserves_pre_breaks_and_normalizes_link_titles`
- P renders closed and open details according to visibility — `src/tool/html_markdown.rs::oc_webfetch_renders_details_by_visibility`
- P registers and fetches an ordinary hostname HTTP URL without rewriting it — `src/tool/webfetch.rs::oc_webfetch_fetches_a_hostname_url_without_rewriting_it`
- P accepts localhost URLs with the same requested-URL permission check — `src/tool/webfetch.rs::oc_webfetch_accepts_localhost_urls_with_the_same_check`
- P follows redirects, checking each target against the rules without asking — `src/tool/webfetch.rs::oc_webfetch_follows_redirects_checking_targets_without_asking`
- P stops at a redirect target the rules deny — `src/tool/webfetch.rs::oc_webfetch_stops_at_a_denied_redirect_target`
- P matches rules against the URL as written without the fragment and rejects credentials — `src/tool/webfetch.rs::oc_webfetch_matches_the_url_as_written_and_rejects_credentials`
- P rejects redirects to non-HTTP schemes and endless redirect chains — `src/tool/webfetch.rs::oc_webfetch_rejects_non_http_redirects_and_endless_chains`
- P rejects non-HTTP schemes before permission or transport — `src/tool/webfetch.rs::oc_webfetch_rejects_non_http_schemes_before_permission`
- P converts HTML to requested markdown and text — `src/tool/webfetch.rs::oc_webfetch_converts_html_to_markdown_and_text`
- P converts deeply nested HTML without overflowing — `src/tool/webfetch.rs::oc_webfetch_converts_deeply_nested_html`
- P rejects declared and streamed oversized bodies — `src/tool/webfetch.rs::oc_webfetch_rejects_declared_and_streamed_oversized_bodies`
- P keeps images and files unsupported until typed outcomes can carry attachments — `src/tool/webfetch.rs::oc_webfetch_keeps_images_and_files_unsupported`
- P retries Cloudflare challenges with an honest user agent — `src/tool/webfetch.rs::oc_webfetch_retries_cloudflare_challenges_with_an_honest_agent`
- P does not retry ordinary 403 responses — `src/tool/webfetch.rs::oc_webfetch_does_not_retry_ordinary_403s`
- P times out stalled requests — `src/tool/webfetch.rs::oc_webfetch_times_out_stalled_requests`

</details>

<details><summary>Tool: websearch (51 of 55)</summary>


**`plugin/websearch.test.ts`**

- D releases rate-limited HTTP requests for ${plugin.id} before caching their errors ×5 · registers a provider without an integration · registers Firecrawl with the standard key method · registers Exa with its MCP schema · registers Parallel and keeps its credential in the authorization header · registers Tavily with keyless and keyed Search API access · registers TinyFish with keyless and keyed MCP search access — deliberate: tool/websearch.rs: self-hosted SearXNG only, no hosted search providers

**`tool-websearch.test.ts`**

- D asks once and uses the default provider when web search is first enabled · honors automatic consent when the configured provider is unavailable · asks a second form when choosing another provider · shares provider consent across concurrent searches · persists the choice to disable web search · keeps provider progress, output, and metadata accurate across automatic failover · does not reopen consent when all automatic providers are cooling down — deliberate: tool/websearch.rs: no provider consent forms, provider choice or failover

**`websearch.test.ts`**

- D shares the normal and test interfaces without installing live providers · executes an explicit provider without changing the default · requires a provider when no default is set · uses the default set by a transform · reloads active transforms from their current source · persists the selected provider in KV · keeps config transforms above the persisted selection · keeps the random provider across queries, default lookups, and reloads · preserves persisted random selection and keeps its provider · fails over on rate limits with random and keeps the replacement after cooldown · reselects when a concurrent query cools down the provider while progress is pending · fails promptly when all providers are cooling down without asking for a provider · tries each provider only once per query even with a zero cooldown · respects Retry-After ${JSON.stringify(header)} and recovers after cooldown ×6 · does not rotate or cool down providers for other failures · does not fail over fixed or explicitly requested providers · reselects when the active provider is removed · uses updated registrations for the sticky provider · keeps independent session affinities across parallel initial and subsequent searches · does not overwrite peer or Location affinity when a session switches providers · shares cooldowns without sending a peer back to the rate-limited provider · converges overlapping session failures and ignores a late success on the old provider · keeps fixed and explicit providers pinned with session context · forgets affinity on session ${operation} without retaining it through an in-flight query ×2 · fails when web search is explicitly disabled · falls back when the configured default is unavailable · removes scoped provider registrations — deliberate: tool/websearch.rs: one SearXNG backend, no provider registry, consent or failover

</details>

<details><summary>Tools: question, todo, skill (2 of 9)</summary>


**`tool-question.test.ts`**

- D does not invent tool ownership metadata without a durable registry source — storage-model: tool ownership metadata

**`tool-todo.test.ts`**

- D is absent unless experimental.todo is enabled — deliberate: todowrite is always on in CodeSmith

</details>

<details><summary>Tool output (17 of 17)</summary>


**`tool-output.test.ts`**

- P writes oversized text and returns a bounded preview — `src/tool/mod.rs::oc_tool_output_writes_oversized_text_and_returns_a_preview`
- P keeps the preview when the full output cannot be saved — `src/tool/mod.rs::oc_tool_output_keeps_the_preview_when_saving_fails`
- P cuts at the byte limit, inside a line when it falls there — `src/tool/mod.rs::oc_tool_output_cuts_at_the_byte_limit_inside_a_line`
- P shows the start of a single line longer than the byte limit — `src/tool/mod.rs::oc_tool_output_shows_the_start_of_a_long_single_line`
- P never splits a character or an emoji at the cut — `src/tool/mod.rs::oc_tool_output_never_splits_a_character_or_emoji`
- P keeps flags and long clusters whole however long the text before them — `src/tool/mod.rs::oc_tool_output_keeps_flags_and_long_clusters_whole`
- D cuts inside a later text part and leaves out the parts after it — other: multi-part tool results (CodeSmith tool output is one string)
- P ends a line cut before its line break — `src/tool/mod.rs::oc_tool_output_ends_a_line_cut_before_its_line_break`
- P counts a blank last line of the preview — `src/tool/mod.rs::oc_tool_output_counts_a_blank_last_line_of_the_preview`
- P shows nothing under a zero line limit — `src/tool/mod.rs::oc_tool_output_shows_nothing_under_a_zero_line_limit`
- D joins text parts with a line break only where one is missing · preserves mixed content ordering · skips results that report a truncation state · marks results that fit without changing their content — other: multi-part tool results (CodeSmith tool output is one string)
- P does not count a trailing newline as another line — `src/tool/mod.rs::oc_tool_output_does_not_count_a_trailing_newline_as_a_line`
- P reports a trailing newline omitted by the byte limit — `src/tool/mod.rs::oc_tool_output_reports_a_trailing_newline_cut_by_the_byte_limit`
- P uses file modification time when IDs wrap — `src/tool/mod.rs::oc_tool_output_cleans_up_by_modification_time`

</details>

<details><summary>Tool registry, schemas, placement (54 of 100)</summary>


**`tool-registry.test.ts`**

- D reads the current editor tools by effective name · replays mutations on refreshed sources and restores tools on disposal and scope cleanup · updates schemas and executors without renaming tools and applies removal in order · converts schemas at registration and reuses them in every snapshot · skips invalid updates without dropping the existing definition · replays empty sources on reload and keeps advertised snapshots · disposes overlays once and replays remaining transforms in order · reads pending tools inside a batch and suppresses terminal teardown replay · uses the last valid addition on replay and restores earlier transforms on disposal · logs and skips invalid dotted namespaces · skips invalid and reserved names while letting the last normalized name win · executes native tools without requiring letter-leading names or namespace segments · executes Code Mode tools without requiring letter-leading names or namespace segments · keeps healthy tools when another namespace is invalid · keeps namespace descriptions beside catalog tools · retains namespace descriptions in executable snapshots after appended transforms · preserves a top-level tool that also has child tools · logs invalid tool definitions without dropping healthy tools · skipped registrations leave existing tools and scoped cleanup intact · canonicalizes effective definitions and keeps Code Mode last · snapshots external tools with missing input schemas · places tools with options.codemode overrides in Code Mode by default, honors direct policy demotion, and resolves pinned · caches the 5 most recently used Code Mode catalogs and invalidates on registry changes · keeps permission options isolated between registrations · removes a scoped registration · preserves an interrupted registration until its scope closes · returns model errors without swallowing interruption or defects · exposes execution only through a snapshot · passes complete call identity to tool execution · lists registered tools by effective name — effect-internal: OpenCode's tool registry overlays, transforms, scopes and codecs
- P normalizes image tool output once and drops unresizable images — `src/tool/read.rs::oc_read_drops_undecodable_image_data` (image normalization through read)
- D publishes progress metadata unchanged · enforces transformed codecs at execution and projection boundaries · registers, advertises, and executes a Zod tool · executes the tool advertised in a model request · reveals the previous registration after an overlay closes · executes and reports progress for codemode tools advertised in a model request — effect-internal: OpenCode's tool registry overlays, transforms, scopes and codecs

**`tool-schema.test.ts`**

- D tools are structural values · Effect tool schemas use exact optional keys and flatten compatible constraints · empty $name Effect input structs describe callable object parameters ×2 · empty input normalization preserves explicit Effect overrides and raw JSON schemas · Effect inputs delegate empty-input projection to the shared helper · empty input normalization leaves nested empty structs unchanged · Effect tool schemas inline named child schemas · Effect tool schemas resolve escaped definition names · definitions keep their pinned serialization · Effect tool schemas inline URI-encoded definition %s ×3 · Zod schemas validate, transform, and describe typed tools · Effect schema failures use normalized input issues · canonical results carry metadata with typed output · raw JSON schema validation does not keep the caller alive — effect-internal: Effect/Zod schema projection

</details>

<details><summary>Media tools (9 of 39)</summary>


**`image-photon.test.ts`**

- D resizes in a worker to the same bytes as before · shares one resizer across the process · falls back to resizing in-process when the worker cannot start, warning once · queues concurrent calls on one worker · fails only the request a crash interrupts, then runs the queue on a fresh worker · terminates a worker when interrupted between acquiring it and sending the request · terminates the worker once it has been idle · an idle worker does not keep the process alive · terminates a worker whose startup is interrupted — platform: Photon WASM worker pool (CodeSmith resizes in-process with the image crate)

</details>

<details><summary>Tool target environments (4 of 7)</summary>


**`tool-search-target-environment.test.ts`**

- D check and search the resolved target's environment, not the session's — client-server: target environments

**`tool-shell-target-environment.test.ts`**

- D runs its command in the resolved working directory's environment, not the session's — client-server: target environments

**`tool-target-environment.test.ts`**

- D read and write the resolved target's environment, not the session's · reads, writes and deletes in the resolved target's environment, not the session's — client-server: target environments

</details>

<details><summary>Shell tool and runtime (147 of 207)</summary>


**`persistent-pty-session.test.ts`**

- P removes a deleted session's terminals — `tests/oc_pty.rs::oc_persistent_pty_removes_a_deleted_sessions_terminals`
- D keeps cleaning up later deletions after a terminal fails to stop — deliberate: removing an in-process terminal cannot fail, so nothing is left stuck to block later cleanups
- P renames a terminal, publishes the update and clears the user title — `tests/oc_pty.rs::oc_persistent_pty_renames_a_terminal_publishes_the_update_and_clears_the_user_title` (adapted: the update event carries the terminal, whose sessionId names the session)

**`process/process.test.ts`**

- D captures stdout and exit code zero · maps command setup failures to AppProcessError · captures stdout and stderr in emission order · non-zero exit returns RunResult; caller can require success · requireSuccess fails on non-zero exit · requireSuccess succeeds on exit 0 · requireExitIn allowlists multiple exit codes · truncates stdout when maxOutputBytes is set · truncates stderr when maxErrorBytes is set · result includes command description · timeout cleans up the scoped child process · fiber interruption cleans up the scoped child process after readiness · string returns stdout as string · lines returns the platform's array of lines · feeds a string to stdin and returns it on stdout · feeds a Uint8Array to stdin · feeds a Stream of Uint8Array chunks to stdin · completes correctly with empty input · carries existing Command options like env · carries existing Command options like cwd · maps streaming command setup failures to AppProcessError · emits lines incrementally and ends cleanly on exit 0 · okExitCodes determines whether a non-zero exit fails the stream · includes stderr in output while retaining capped failure diagnostics · without okExitCodes, never fails on exit code · AbortSignal interrupts the stream · returns the platform ChildProcessHandle for advanced use — effect-internal: OpenCode's Effect process wrapper (CodeSmith uses tokio::process directly)

**`pty/info-schema.test.ts`**

- P accepts pid 0 (Windows ConPTY assigns the pid asynchronously) — `tests/oc_pty.rs::oc_info_accepts_pid_0`
- P accepts a positive pid — `tests/oc_pty.rs::oc_info_accepts_a_positive_pid`
- P rejects a negative pid — `tests/oc_pty.rs::oc_info_rejects_a_negative_pid`
- P accepts an exit code for retained exited sessions — `tests/oc_pty.rs::oc_info_accepts_an_exit_code_for_retained_exited_sessions`

**`pty/protocol.test.ts`**

- P drops invalid binary input frames and decodes valid ones — `tests/oc_pty.rs::oc_protocol_drops_invalid_binary_input_frames_and_decodes_valid_ones`
- P encodes the cursor as a 0x00-prefixed JSON control frame — `tests/oc_pty.rs::oc_protocol_encodes_the_cursor_as_a_0x00_prefixed_json_control_frame`
- P splits replay into bounded frames — `tests/oc_pty.rs::oc_protocol_splits_replay_into_bounded_frames` (adapted: frames are at most 64 KiB of UTF-8, cut on character boundaries)

**`pty/pty-session.test.ts`**

- P returns typed not found errors for missing sessions — `tests/oc_pty.rs::oc_pty_returns_typed_not_found_errors_for_missing_sessions`
- P retains exited sessions until removed — `tests/oc_pty.rs::oc_pty_retains_exited_sessions_until_removed`
- P replays buffered output and streams live output to attachments — `tests/oc_pty.rs::oc_pty_replays_buffered_output_and_streams_live_output_to_attachments`
- P stops delivering output after detach — `tests/oc_pty.rs::oc_pty_stops_delivering_output_after_detach` (adapted: detaching consumes the attachment, so the check is that output still flows to others)
- P isolates output between sessions — `tests/oc_pty.rs::oc_pty_isolates_output_between_sessions`
- P notifies attachments with the exit code and rejects attach after exit — `tests/oc_pty.rs::oc_pty_notifies_attachments_with_the_exit_code_and_rejects_attach_after_exit`
- P defaults command, login args, and cwd from shell selection and location — `tests/oc_pty.rs::oc_pty_defaults_command_login_args_and_cwd_from_shell_selection_and_location`

**`pty/ticket.test.ts`**

- P consumes tickets once — `tests/oc_pty.rs::oc_ticket_consumes_tickets_once`
- P rejects tickets scoped to a different request — `tests/oc_pty.rs::oc_ticket_rejects_tickets_scoped_to_a_different_request`
- P rejects tickets after the TTL elapses — `tests/oc_pty.rs::oc_ticket_rejects_tickets_after_the_ttl_elapses`
- P rejects tickets scoped to a different workspace — `tests/oc_pty.rs::oc_ticket_rejects_tickets_scoped_to_a_different_workspace`
- P keeps PTY and client MCP targets apart — `tests/oc_pty.rs::oc_ticket_keeps_pty_and_client_mcp_targets_apart`

**`shell-result.test.ts`**

- D keeps a removed command's result unchanged — storage-model: removed command rows

**`shell-retention.test.ts`**

- D does not inherit host process.env when creating a shell in a workspace-backed location — client-server: workspace-backed Locations

**`shell.test.ts`**

- F builds command args per shell family — `tests/oc_tools.rs::oc_shell_passes_an_unmatched_zsh_glob_through_to_the_command` (POSIX families; Zsh now gets `+o nomatch`)
- F passes an unmatched zsh glob through to the command — `tests/oc_tools.rs::oc_shell_passes_an_unmatched_zsh_glob_through_to_the_command` (an unmatched glob failed the whole Zsh line)
- D normalizes Git Bash shell paths from env · resolves /usr/bin/bash from env to Git Bash · resolves bare bash to Git Bash before PATH · resolves bare PowerShell shells · resolves a configured app execution alias — platform: Windows / Git Bash / PowerShell / app execution aliases

**`tool-shell.test.ts`**

- P declarations reuse approvals while substitutions retain reject/once/always behavior — `src/tool/shell.rs::oc_shell_declarations_reuse_approvals_while_substitutions_ask`
- P pipeline redirect keeps the redirect target visible to exact approval and denial — `src/tool/shell.rs::oc_shell_pipeline_redirect_target_stays_visible`
- P external-directory rejection stops execution before a workspace marker is written — `src/tool/shell.rs::oc_shell_external_directory_rejection_stops_execution`
- P a numeric symlink operand still reaches outside without an external-directory prompt — `src/tool/shell.rs::oc_shell_numeric_symlink_operand_enforces_external_deny`
- P a continued directory operand resolves to the destination and enforces external-directory deny — `src/tool/shell.rs::oc_shell_continued_directory_operand_enforces_external_deny`
- P a nested deny prevents the substitution from running — `src/tool/shell.rs::oc_shell_conditional_substitution_nested_deny`
- P conditional substitutions respect ${reply} ×3 — `src/tool/shell.rs::oc_shell_conditional_substitutions_respect_reject, src/tool/shell.rs::oc_shell_conditional_substitutions_respect_once, src/tool/shell.rs::oc_shell_conditional_substitutions_respect_always`
- P ${fixture.shell}: ${fixture.command} ×5 — `src/tool/shell.rs::oc_shell_compound_syntax_approval_compatibility`
- P ${shell}: ${fixture.name} reuses existing approvals ×10 — `src/tool/shell.rs::oc_shell_ordinary_syntax_reuses_existing_approvals` (bash and zsh rows)
- D ${shell}: ${fixture.name} reuses existing approvals ×4 — platform: PowerShell rows
- P ${shell}: a loop body deny prevents execution ×2 — `src/tool/shell.rs::oc_shell_loop_body_deny_prevents_execution`
- D PowerShell: ordinary syntax reuses approvals: ${command} ×8 · PowerShell: always allow covers repeat execution and preserves exact deny: ${command} ×3 — platform: PowerShell
- P registers and returns real successful output from the active Location — `src/tool/shell.rs::oc_shell_registers_and_returns_real_output`
- P uses the session environment instead of the server environment — `src/tool/shell.rs::oc_shell_uses_the_session_environment_instead_of_the_process_one`
- P identifies agent shell commands without inheriting a stale session ID — `src/tool/shell.rs::oc_shell_identifies_agent_commands, src/tool/shell.rs::oc_shell_identifies_agent_commands_without_a_stale_session_id`
- P preserves an outer AI_AGENT marker in agent shell commands — `src/tool/shell.rs::oc_shell_preserves_an_outer_ai_agent_marker`
- P resolves a relative workdir from the active Location — `src/tool/shell.rs::oc_shell_resolves_a_relative_workdir`
- P reports a missing workdir — `src/tool/shell.rs::oc_shell_reports_a_missing_workdir`
- P permissions compound commands separately — `src/tool/shell.rs::oc_shell_permissions_compound_commands_separately`
- P captures stderr-only and mixed stdout/stderr output — `src/tool/shell.rs::oc_shell_captures_stderr_only_and_mixed_output`
- P rejects a workdir that stops being a directory during approval — `src/tool/shell.rs::oc_shell_rejects_a_workdir_that_stops_being_a_directory`
- P approves an explicit external workdir before shell execution — `src/tool/shell.rs::oc_shell_approves_an_explicit_external_workdir_first`
- P deduplicates external directory approvals across workdir and directory-change commands — `src/tool/shell.rs::oc_shell_deduplicates_external_approvals_across_workdir_and_cd`
- P approves an expanded external home directory — `src/tool/shell.rs::oc_shell_approves_an_expanded_external_home`
- P does not execute after external-directory or shell denial — `src/tool/shell.rs::oc_shell_does_not_execute_after_denial`
- P rejects malformed syntax across channels without partial execution — `src/tool/shell.rs::oc_shell_rejects_malformed_syntax_without_partial_execution`
- P preserves arithmetic and directory permissions in ${shell} ×2 — `src/tool/shell.rs::oc_shell_preserves_arithmetic_and_directory_permissions`
- P keeps non-zero exits useful — `src/tool/shell.rs::oc_shell_keeps_non_zero_exits_useful`
- P puts the exit status after the truncation marker when overflowing output fails — `src/tool/shell.rs::oc_shell_exit_status_follows_the_truncation_marker`
- P moves a foreground timeout to the background and delivers its original process result once — `src/tool/shell.rs::oc_shell_foreground_timeout_moves_to_background_and_reports_once`
- P returns a fast foreground result directly — `src/tool/shell.rs::oc_shell_returns_a_fast_foreground_result_directly`
- P preserves a timeout-promoted command's eventual failure — `src/tool/shell.rs::oc_shell_preserves_a_timeout_promoted_failure`
- P treats a foreground timeout of zero as the default timeout — `src/tool/shell.rs::oc_shell_foreground_timeout_of_zero_is_the_default`
- P still cancels a timeout-promoted shell through its existing job handle — `src/tool/shell.rs::oc_shell_still_cancels_a_timeout_promoted_command_through_its_job`
- P stops a background command when its explicit timeout expires — `src/tool/shell.rs::oc_shell_stops_a_background_command_at_its_timeout`
- P truncates the model view and points at the saved output file when output overflows — `src/tool/shell.rs::oc_shell_truncates_overflowing_output_from_the_end`
- P uses configured line limits — `src/tool/shell.rs::oc_shell_uses_configured_line_limits`
- P drops the partial first line so the truncation note counts only whole lines — `src/tool/shell.rs::oc_shell_drops_the_partial_first_line`
- P says how much of a single long line is shown — `src/tool/shell.rs::oc_shell_says_how_much_of_a_long_line_is_shown`
- P starts the truncated view on a whole UTF-8 character — `src/tool/shell.rs::oc_shell_truncated_view_starts_on_a_whole_character`
- P numbers lines exactly when the line limit truncates — `src/tool/shell.rs::oc_shell_numbers_lines_exactly_at_the_line_limit`
- P reports the shell ID for a running command — `src/tool/shell.rs::oc_shell_reports_the_shell_id_for_a_running_command`
- P reports shell ID progress once — `src/tool/shell.rs::oc_shell_reports_shell_id_progress_once`
- D names the tool call and background on the spans of the shell it starts — effect-internal: tracing spans
- D keeps a background daemon command's job across a Location reload and reports how it ended — client-server: Location reload / daemon hand-off
- P returns the shell id and PID for a background command — `src/tool/shell.rs::oc_shell_returns_the_shell_id_and_pid_for_a_background_command`
- P preserves a background command's non-zero exit — `src/tool/shell.rs::oc_shell_preserves_a_background_non_zero_exit`
- P persists a silent command that finishes before backgrounding — `src/tool/shell.rs::oc_shell_reports_a_silent_background_command`
- P updates and clears a running shell timeout — `src/tool/shell.rs::oc_shell_updates_and_clears_a_running_timeout`
- P escalates a shell timeout when the ready process ignores SIGTERM — `src/tool/shell.rs::oc_shell_escalates_a_timeout_when_sigterm_is_ignored`
- P words the status as an exit code — `src/tool/shell.rs::oc_shell_words_the_status_as_an_exit_code`
- P settles a shell terminated by an external signal — `src/tool/shell.rs::oc_shell_settles_a_command_ended_by_an_external_signal`
- P backgrounds a foreground command when the session is signaled — `src/tool/shell.rs::oc_shell_backgrounds_a_foreground_command_when_the_session_is_signaled`
- D stops a command the user stopped after its Location reloaded — client-server: Location reload / daemon hand-off
- D ${handoff ? "leaves a command running" : "stops a command"} when an interrupt ${handoff ? "during a handoff " : ""}cuts its call off while it ${starting ? "starts" : "runs"} ×4 — deliberate: tool/shell.rs: commands never outlive the session (no daemon hand-off)
- D records which call waits on a daemon command, and keeps no timeout for a restart — client-server: Location reload / daemon hand-off

</details>

<details><summary>Shell scan and parse (618 of 1465)</summary>


**`shell-parse-compatibility.test.ts`**

- D extracts resources, saved prefixes, and directories: %s %s %j ×10 — platform: PowerShell rows
- D extracts inline PowerShell directory flags with case-insensitive names and quoted values · retains statement-head ForEach-Object cmdlet calls when non-scriptblock parameters are present — platform: PowerShell

**`shell-parse-parity.test.ts`**

- D extracts resources, saved prefixes, and directories in %s: %s ×5 — platform: PowerShell rows
- P returns an explicit empty result for pure variable assignments without redirects — `src/tool/shell_scan.rs::oc_scan_assignment_only_input_has_no_command`
- P fails explicitly for malformed %s syntax: %s ×6 — `src/tool/shell_scan.rs::oc_scan_malformed_structure_is_opaque` (POSIX rows)
- D fails explicitly for malformed %s syntax: %s ×3 — platform: PowerShell rows

**`shell-parse-syntax.test.ts`**

- D PowerShell invocation approvals include the operator instead of saving an ineffective prefix · PowerShell scriptblock callers preserve permission resources and usable approvals: %s ×8 · PowerShell extracts commands without rejecting ordinary syntax: %s ×14 — platform: PowerShell

**`shell-parse.test.ts`**

- P splits bash commands and derives reusable prefixes — `src/tool/shell_scan.rs::oc_shell_parse_splits_commands_and_derives_prefixes`
- P rejects malformed syntax with an explicit scanner error — `src/tool/shell_scan.rs::oc_shell_parse_rejects_malformed_syntax`
- P preserves command resources and directories across compound forms — `src/tool/shell_scan.rs::oc_shell_parse_preserves_resources_across_compound_forms`
- P handles heredocs with the command permission resource — `src/tool/shell_scan.rs::oc_shell_parse_heredoc_resource_is_the_whole_command`
- P keeps source-shaped command heads under shell authorization: %s ×4 — `src/tool/shell_scan.rs::oc_shell_parse_source_shaped_heads_stay_under_shell_rules`
- P preserves declaration permission behavior for %s without hiding nested commands ×7 — `src/tool/shell_scan.rs::oc_shell_parse_declarations_keep_nested_commands`
- P declaration filtering retains directory checks inside command substitutions — `src/tool/shell_scan.rs::oc_shell_parse_declarations_keep_directory_checks_in_substitutions`
- D does not treat PowerShell commands as Bash declarations · splits PowerShell commands case-insensitively — platform: PowerShell
- P does not permission directory changes separately — `src/tool/shell_scan.rs::oc_shell_parse_directory_changes_are_not_shell_resources`
- P extracts static directory targets across escapes, end-of-options, literal dollars, and precommands — `src/tool/shell_scan.rs::oc_shell_parse_static_directory_targets`
- P retains commandless redirects, redirected declarations, and PowerShell method calls as permission resources — `src/tool/shell_scan.rs::oc_shell_parse_commandless_and_redirected_declarations`
- D extracts PowerShell directory parameters — platform: PowerShell
- P expands deterministic directory variables — `src/tool/shell_scan.rs::oc_shell_parse_expands_deterministic_directories`

**`shell-scan/adversarial.test.ts`**

- P scans subscript-shaped text outside shell sinks: %s ×9 — `src/tool/shell_scan.rs::oc_scan_adversarial_subscript_text_outside_sinks`
- P keeps structurally uncertain Bash input opaque: %s ×4 — `src/tool/shell_scan.rs::oc_scan_adversarial_uncertain_input_is_opaque`
- P scans adversarial Bash input in bounded time: %s ×6 — `src/tool/shell_scan.rs::oc_scan_adversarial_input_in_bounded_time`
- D scans visible PowerShell command positions: %s ×14 · reports incomplete PowerShell input: %s ×2 — platform: PowerShell

**`shell-scan/bash-safety.test.ts`**

- P rejects malformed command positions: %s ×7 — `src/tool/shell_scan.rs::oc_scan_adversarial_command_positions`
- P keeps unsupported group positions and empty command names opaque: %s ×6 — `src/tool/shell_scan.rs::oc_scan_keeps_dynamic_command_names`

**`shell-scan/generated.test.ts`**

- D decomposes every combination of static commands and separators · keeps quoted and escaped separators in arguments · fails closed when valid commands are mutated with malformed syntax · distinguishes variable expressions from delegated execution · extracts nested commands in directory expressions without evaluating variables — platform: PowerShell

**`shell-scan/mutation.test.ts`**

- P keeps malformed Bash input opaque: %s ×4 — `src/tool/shell_scan.rs::oc_scan_malformed_structure_is_opaque` (malformed Bash input stays opaque)
- D keeps incomplete PowerShell input opaque: %s ×3 — platform: PowerShell

**`shell-scan/oracle.test.ts`**

- P discovers at least bash on PATH — `tests/shell_differential.rs` (real-shell oracle (CodeSmith's differential runner checks the scanner against Bash, Zsh, sh, Dash and Bash 3.2))
- P reports or rejects real-shell execution: %j ×2 — `tests/shell_differential.rs` (real-shell oracle (CodeSmith's differential runner checks the scanner against Bash, Zsh, sh, Dash and Bash 3.2))
- P reports or rejects in %s: %j ×8 — `tests/shell_differential.rs` (real-shell oracle (CodeSmith's differential runner checks the scanner against Bash, Zsh, sh, Dash and Bash 3.2))
- P reports the directory a real shell changes to: %s ×4 — `tests/shell_differential.rs` (real-shell oracle (CodeSmith's differential runner checks the scanner against Bash, Zsh, sh, Dash and Bash 3.2))

**`shell-scan/powershell-runtime.test.ts`**

- D accepts supported PowerShell syntax without an opaque escape hatch: %s ×24 · successful PowerShell scans cover real parser boundaries and executed probe calls — platform: PowerShell

**`shell-scan/powershell-safety.test.ts`**

- D backticks in single quotes cannot hide subsequent executions · recognizes comments after escaped whitespace at a token boundary %j ×11 · keeps backtick whitespace inside strings literal: %j ×8 · keeps embedded greater-than text distinct from redirects: %s ×2 · recognizes expression-mode comments without inventing command heads: %s ×16 · ends nested block comments at %j ×3 · keeps block delimiters inside strings: %s ×5 · preserves doubled double quotes in argument values · recognizes comments after complete literal tokens: %s ×3 · keeps generic-token quotes distinct from standalone strings · block matching agrees with tokenization after a merging redirect · refuses unsupported lexical modes: %s ×3 · ends stop-parsing at %j even adjacent to completed tokens ×3 · reports malformed or unsupported lexical syntax: %s ×21 · preserves commands with directory variables for Core policy through %s ×7 · preserves directory command syntax without deciding directory policy: %s ×11 · keeps command aliases visible: %s ×5 · retains command names and paths without resolving them: %s ×6 · recursively extracts commands from nested script blocks · keeps hashes within words out of block-comment detection · never invents a command after a trailing %j ×5 · does not clear a dangling pipeline with a new empty statement: %s ×4 · scans valid redirects without consuming command arguments · bounds script block nesting and input size · scans compound subexpressions with expression-mode comments inside expandable strings · recognizes block comments immediately after expression tokens: %s ×7 · rejects mid-token here-string openers and preserves subsequent commands · does not hide commands across lexical mode boundaries: %s ×108 — platform: PowerShell

**`shell-scan/powershell-syntax.test.ts`**

- D extracts command resources from %s ×50 · distinguishes standalone continuations from escapes inside tokens: %j ×3 · decodes literal backtick escapes without evaluating expressions · preserves word offsets relative to each raw command, including nested invocations · retains caller and lexical statement-head context: %s ×9 · bounds and reports incomplete lexical structures: %s ×8 · bounds mixed expression, string, and block recursion — platform: PowerShell

**`shell-scan/redirect.test.ts`**

- P keeps redirects visible across lists and pipelines: %s ×8 — `src/tool/shell_scan.rs::oc_scan_redirects_stay_visible_across_lists_and_pipelines`
- P scans redirect positions across generated list and pipeline boundaries — `src/tool/shell_scan.rs::oc_scan_redirect_positions_across_generated_boundaries`
- P keeps full redirect resource, lexical words, and nested redirect-target commands — `src/tool/shell_scan.rs::oc_scan_redirect_keeps_full_resource_and_nested_target_commands`
- P retains trailing redirect span after a continuation on pipeline RHS — `src/tool/shell_scan.rs::oc_scan_redirect_after_continuation_on_pipeline_rhs`
- P saved prefixes cover their standalone continuation command: %j ×6 — `src/tool/shell_scan.rs::oc_scan_saved_prefixes_cover_standalone_continuations`
- P preserves meaningful raw syntax before an ignored trailing continuation: %j ×4 — `src/tool/shell_scan.rs::oc_scan_meaningful_raw_syntax_before_trailing_continuation`
- P assignment then redirect on a pipeline RHS retains the full command and trailing redirect — `src/tool/shell_scan.rs::oc_scan_assignment_then_redirect_on_pipeline_rhs`
- P redirect before cd operand retains the target directory — `src/tool/shell_scan.rs::oc_scan_redirect_before_cd_operand_keeps_the_directory`

**`shell-scan/scan.test.ts`**

- P accepts empty scripts and blank lines: %j ×6 — `src/tool/shell_scan.rs::oc_scan_accepts_empty_scripts_and_blank_lines`
- P does not treat blank lines or comments as missing commands: %j ×3 — `src/tool/shell_scan.rs::oc_scan_blank_lines_and_comments_are_not_missing_commands`
- P retains required operands across line breaks after %s ×4 — `src/tool/shell_scan.rs::oc_scan_retains_required_operands_across_line_breaks`
- P scans a static command — `src/tool/shell_scan.rs::oc_scan_scans_a_static_command`
- P scans every command in lists and pipelines — `src/tool/shell_scan.rs::oc_scan_scans_every_command_in_lists_and_pipelines`
- P does not split operators inside quoted or escaped arguments — `src/tool/shell_scan.rs::oc_scan_does_not_split_operators_inside_quotes_or_escapes`
- P scans commands substituted into an argument — `src/tool/shell_scan.rs::oc_scan_scans_commands_substituted_into_an_argument`
- P scans substitutions in assignment values and redirect targets — `src/tool/shell_scan.rs::oc_scan_scans_substitutions_in_assignments_and_redirect_targets`
- P scans substitutions inside parameter operators — `src/tool/shell_scan.rs::oc_scan_scans_substitutions_inside_parameter_operators`
- P recursively scans substitutions and preserves shell quote rules — `src/tool/shell_scan.rs::oc_scan_recursively_scans_substitutions_with_quote_rules`
- P makes the whole result opaque when a nested scan is opaque: %s ×2 — `src/tool/shell_scan.rs::oc_scan_whole_result_is_opaque_when_a_nested_scan_is`
- P bounds substitution nesting and input size — `src/tool/shell_scan.rs::oc_scan_bounds_substitution_nesting_and_input_size`
- P preserves dynamic command syntax without resolving its name — `src/tool/shell_scan.rs::oc_scan_keeps_dynamic_command_names`
- P finds the command after static assignment prefixes — `src/tool/shell_scan.rs::oc_scan_finds_the_command_after_assignment_prefixes`
- P keeps delegated execution at the invoked command boundary: %s ×5 — `src/tool/shell_scan.rs::oc_scan_keeps_delegated_execution_at_the_invoked_command`
- P scans common compound execution: %s ×8 — `src/tool/shell_scan.rs::oc_scan_scans_common_compound_execution`
- P returns opaque for malformed conditionals: %s ×3 — `src/tool/shell_scan.rs::oc_scan_malformed_conditionals_are_opaque`
- P keeps redirects in resources across list operators — `src/tool/shell_scan.rs::oc_scan_keeps_redirects_in_resources_across_lists`
- P recognizes redirects without surrounding whitespace — `src/tool/shell_scan.rs::oc_scan_recognizes_redirects_without_whitespace`
- P returns opaque for malformed command structure: %s ×7 — `src/tool/shell_scan.rs::oc_scan_malformed_structure_is_opaque`
- P ignores comments outside words — `src/tool/shell_scan.rs::oc_scan_ignores_comments_outside_words`
- P scans heredoc commands and expansions: %s ×2 — `src/tool/shell_scan.rs::oc_scan_scans_heredoc_commands_and_expansions`
- P does not invent a command for assignment-only input — `src/tool/shell_scan.rs::oc_scan_assignment_only_input_has_no_command`
- P scans assignment-only boundaries without evaluating their effects: %s ×7 — `src/tool/shell_scan.rs::oc_scan_assignment_only_boundaries`
- P scans parameter operator grammar without interpreting values: %s ×2 — `src/tool/shell_scan.rs::oc_scan_parameter_operator_grammar`
- P scans Bash lexical forms without interpreting shell values: %s ×6 — `src/tool/shell_scan.rs::oc_scan_bash_lexical_forms`
- P bounds nested parameter expansions — `src/tool/shell_scan.rs::oc_scan_bounds_nested_parameter_expansions`
- P retains Bash quotes and escapes while excluding assignment prefixes and redirects — `src/tool/shell_scan.rs::oc_scan_lexical_provenance_quotes_assignments_and_redirects`
- P does not mistake quoted assignment-like words or fd numbers for shell syntax — `src/tool/shell_scan.rs::oc_scan_quoted_assignment_like_words_and_fd_numbers_are_words`
- P retains nested Bash substitutions and continuations from the original token — `src/tool/shell_scan.rs::oc_scan_retains_nested_substitutions_and_continuations`
- D retains PowerShell invocation quotes, escaped words, and empty arguments · keeps separate PowerShell literal tokens and nested script block spans · keeps adjacent invocation operators in resources · does not carry redirect state through comments · scans module-qualified commands · splits carriage-return statement separators · splits CRLF statement separators · ends comments at carriage returns · scans static commands and pipelines · keeps separators inside strings · keeps escaped command separators in the argument · keeps escaped newlines in a started generic token · uses PowerShell quote escaping rules · does not treat backticks as escapes in verbatim strings · fails closed for PowerShell smart quotes · excludes PowerShell redirects and their targets from words · returns opaque for malformed PowerShell tokens: %s ×2 · keeps delegated PowerShell execution at the invoked command boundary: %s ×11 · recursively scans PowerShell script blocks · scans PowerShell commands separated by the background operator · ignores braces in PowerShell script-block comments · ignores comments and keeps redirects in resources · accepts empty PowerShell statements: %s ×3 · reports incomplete PowerShell syntax: %s ×2 — platform: PowerShell

</details>

<details><summary>Permission (140 of 177)</summary>


**`config/policy.test.ts`**

- D organization tool.use policy overrides authored policy · filters plugin-provided providers with ordered wildcard policies — cloud: organization / Console / integration policies
- D prevents project policy from overriding user-global policy — deliberate: config.rs: project rules come after (and so override) global ones
- D reloads changed policies · lets organization provider statements override authored documents · lets an organization allow restore a provider denied by the user · names the organization when its permission statement decides: ${input.organization} ×2 · rebuilds the catalog when the organization's policy changes · requires one Console organization through MDM policy alone · keeps an integration's account out of a bare-ID allow · decides a well-known origin's providers by the origin: ${effect} ×2 · removes built-in and plugin web search providers that websearch.use denies · converts legacy statements in Console config before enforcement sees them — cloud: organization / Console / integration policies

**`file-access-path.test.ts`**

- D stats and finds the repository through the Location's Environment driver — client-server: Location Environment driver
- D ignores unknown path input fields — effect-internal: schema input decoding
- D normalizes Windows shell drive path %s before resolution ×4 — platform: Windows drive paths

**`file-access.test.ts`**

- D returns an absolute target and preserves invocation identity on the read assertion — effect-internal: invocation identity on the assertion

**`permission.test.ts`**

- P returns the evaluated effect and only queues prompts — `src/permission.rs::oc_permission_returns_the_evaluated_effect_and_only_asks_when_needed`
- P evaluates against an explicit agent — `src/permission.rs::oc_permission_evaluates_against_an_explicit_agent`
- P allows and denies from explicit rules without asking — `src/permission.rs::oc_permission_allows_and_denies_from_explicit_rules_without_asking`
- P checks for a deny without asking — `src/permission.rs::oc_permission_checks_for_a_deny_without_asking`
- P reports whether a request is already allowed without asking — `src/permission.rs::oc_permission_reports_whether_a_request_is_already_allowed`
- P matches Windows rule resources against slash-normalized file access resources — `src/permission.rs::oc_permission_matches_windows_rules_against_slash_normalized_resources`
- P matches file rules without case where file systems ignore it by default — `src/permission.rs::oc_permission_matches_file_rules_without_case_where_files_ignore_it`
- P allows managed output reads without granting external directory access — `src/permission.rs::oc_permission_allows_managed_output_reads_without_external_access`
- P evaluates bash with the normal configured-rule semantics — `src/permission.rs::oc_permission_evaluates_shell_with_normal_rule_semantics`
- P merges session rules after agent rules and before saved approvals — `src/permission.rs::oc_permission_session_rules_after_agent_rules_before_saved_approvals`
- P uses saved bash approvals while preserving configured deny precedence — `src/permission.rs::oc_permission_saved_approvals_keep_configured_deny_precedence`
- P resolves an asked permission once — `src/permission.rs::oc_permission_resolves_an_asked_permission_once`
- D shows a nested call's request on its parent's row until the scope closes · drops undefined metadata values so pending requests encode as JSON — client-server: pending-request rows and JSON encoding
- P defects when an asked permission is declined — `src/permission.rs::oc_permission_declining_fails_the_request`
- P ${fixture.name}: ${scenario.name} ×72 — `src/permission.rs::oc_permission_shell_scanner_impact` (Bash/Zsh fixtures)
- D ${fixture.name}: ${scenario.name} ×24 — platform: PowerShell fixtures
- P ${fixture.name}: always allow covers repeat execution and narrows prefix scope ×5 — `src/permission.rs::oc_permission_always_covers_repeats_and_narrows_prefix_scope` (Bash fixtures)
- D ${fixture.name}: always allow covers repeat execution and narrows prefix scope ×2 — platform: PowerShell fixtures

</details>

<details><summary>Agents (48 of 65)</summary>


**`agent.test.ts`**

- D publishes an updated event after agent changes · materializes replayable agent transforms · rebuilds state when a transform is replaced · removes a transform when its scope closes · applies direct agent updates — effect-internal: agent service events and replayable transforms
- P allows external directories without opting built-in agents into bash — `tests/oc_config.rs::oc_agent_explore_is_read_only_and_asks_for_env_files` (the explore rules; CodeSmith has no compaction/summary/title agents)
- P denies the subagent tool for built-in subagents — `tests/oc_config.rs::oc_agent_built_in_subagents_deny_the_subagent_tool`

**`config/agent.test.ts`**

- P ignores named agent color tokens — `tests/oc_agents.rs::oc_agent_ignores_named_agent_color_tokens`
- P keeps schema fields and name out of legacy agent options — `tests/oc_agents.rs::oc_agent_keeps_schema_fields_and_name_out_of_legacy_agent_options` (adapted: the options are request body fields, with temperature and top_p)
- P loads Markdown ${item.name}${native ? " with native request and permissions" : ""} ×16 — `tests/oc_agents.rs::oc_agent_loads_markdown_model_selections_with_and_without_native_fields` (adapted: model references are provider/model#variant specs; no request settings)
- P keeps Markdown legacy permission precedence in source order — `tests/oc_agents.rs::oc_agent_keeps_markdown_legacy_permission_precedence_in_source_order`
- P loads Markdown legacy temperature ${variant ? "with" : "without"} a separate variant ×2 — `tests/oc_agents.rs::oc_agent_loads_markdown_legacy_temperature_with_and_without_a_variant`
- D matches Windows paths against home-relative permissions — platform: Windows paths
- D applies remote permission defaults before explicit global and build rules — cloud: remote (well-known) permission defaults
- P applies all global permissions before agent-specific permissions — `tests/oc_config.rs::oc_agent_global_permissions_come_before_agent_specific_ones` (without the later-document removal and model fields)
- P maps configured agent fields and preserves an unspecified model variant — `tests/oc_agents.rs::oc_agent_maps_configured_agent_fields_and_merges_request_maps` (adapted: two config files)
- P removes a built-in agent disabled by configuration — `tests/oc_config.rs::oc_agent_a_built_in_agent_disabled_by_config_is_removed`
- P loads legacy file-based agents from config directories — `tests/oc_agents.rs::oc_agent_loads_legacy_file_based_agents_from_config_directories` (adapted: the global config folder and its codesmith.json)
- P reads agent files that use V2 fields as V2 — `tests/oc_agents.rs::oc_agent_reads_agent_files_that_use_v2_fields_as_v2`
- D rebuilds agents when a source file is ${testCase.name} · coalesces updates inside the debounce window into one rebuild · rebuilds on a config update published immediately after startup · ignores updates outside agent source directories — deliberate: config.rs: no live reload or file watching

**`plugin/optimize.test.ts`**

- D enables prompt plugins without model-specific tool optimization — deliberate: prompt.rs: the model prompts are built in, not plugins that can be listed or enabled one by one
- P selects model-lab prompts through session context hooks — `tests/oc_prompts.rs::oc_prompt_selects_model_lab_prompts_by_model_id` (adapted: rebranded prompts chosen by prompt::base_named)
- P renders the OpenAI prompt without changing tools or project instructions — `tests/oc_prompts.rs::oc_prompt_renders_the_openai_prompt_with_tool_guidance`
- P appends the Anthropic prompt to the baseline without changing tools or project instructions — `tests/oc_prompts.rs::oc_prompt_appends_the_anthropic_prompt_to_the_baseline`
- P uses catalog names in Meta prompts for Muse model IDs — `tests/oc_prompts.rs::oc_prompt_uses_model_names_in_meta_prompts_for_muse_models` (adapted: names from config (or the catalog))
- P preserves tools and an explicit agent system prompt by default — `tests/oc_prompts.rs::oc_prompt_preserves_an_explicit_agent_system_prompt`
- D allows one model-lab optimization plugin to be enabled independently — deliberate: prompt.rs: the model prompts are built in, not plugins that can be listed or enabled one by one
- P preserves tools for model aliases and catalog-ID prompt selection by default — `tests/oc_prompts.rs::oc_prompt_selects_by_the_configured_model_id_not_the_api_model` (adapted: CodeSmith reads no model family)
- D lists models once per step across prompt and identity plugins — effect-internal: one model-catalog lookup per step shared across prompt and identity plugins

</details>

<details><summary>Skills and commands (47 of 59)</summary>


**`command.test.ts`**

- P registers and executes callback commands — `tests/oc_plugin_commands.rs::oc_command_registers_and_executes_callback_commands` (a plugin's Command)
- P replaces commands with later definitions — `tests/oc_commands.rs::oc_command_replaces_commands_with_later_definitions` (adapted: config layers)
- P returns callback error messages without stack traces — `tests/oc_plugin_commands.rs::oc_command_registers_and_executes_callback_commands` (in the same test (the plugin registry is process-wide))

**`config/command-subagent.test.ts`**

- P runs ${fixture.name} in the background without switching the parent ×3 — `tests/oc_commands.rs::oc_command_runs_subagent_commands_in_a_child_without_switching_the_parent` (adapted: the child runs to completion and its report joins the parent as an exchange)
- P subagent: false overrides subagent mode and the legacy alias — `tests/oc_commands.rs::oc_command_subagent_false_overrides_subagent_mode_and_the_legacy_alias` (adapted: the front end switches the session through Command::switches)

**`config/command.test.ts`**

- P interpolates ${JSON.stringify(item.template)} with literal input ${JSON.stringify(item.input)} ×7 — `src/command.rs::oc_command_interpolates_literal_input`
- P loads inline and file-based commands in config order — `tests/oc_commands.rs::oc_command_loads_inline_and_file_based_commands_in_config_order` (adapted: a project codesmith.json below its .codesmith/ folder; no live reload)
- D rebuilds commands when a source file is ${testCase.name} · coalesces updates inside the debounce window into one rebuild · rebuilds on a config update published immediately after startup · ignores updates outside command source directories · ConfigCommandPlugin native watcher — deliberate: config.rs: no live reload or file watching

**`config/markdown.test.ts`**

- P recovers unquoted-colon frontmatter via the sanitize fallback — `src/skill.rs::oc_skill_frontmatter_values_with_colons_and_brackets`
- P recovers a frontmatter value that starts with a bracketed tag — `src/skill.rs::oc_skill_frontmatter_values_with_colons_and_brackets`
- D recovers the same content again after a previous failed parse · keeps throwing for the same unparseable content on every call — deliberate: skill.rs: frontmatter is read as flat key: value lines, so nothing fails to parse

**`config/skill.test.ts`**

- D reads disable-model-invocation as autoinvoke — deliberate: skill.rs: disable-model-invocation not supported
- P parses root and nested skill ids and metadata flags — `src/skill.rs::oc_skill_ids_come_from_the_folder_and_name_defaults_to_it`
- P loads directory and individual downloaded skill roots with later-source precedence — `tests/oc_skill_remote.rs::oc_skill_loads_directory_and_downloaded_skill_roots_with_later_source_precedence` (adapted: against a local catalog)
- P keeps directory skills when a URL source fails — `tests/oc_skill_remote.rs::oc_skill_keeps_directory_skills_when_a_url_source_fails` (adapted: an unreachable catalog)
- D rescans directory sources when watched files change · watches canonical files behind symlinked skills · reloads symlinked sources when their target changes · follows missing source directories as their parents appear · ignores unrelated config updates and coalesces compatibility bursts into one reload — deliberate: config.rs: no file watching

**`plugin/skill.test.ts`**

- D reports canonical configured plugin sources with existing labels and ordering — effect-internal: plugin source labels

**`skill-discovery.test.ts`**

- P downloads safe nested files under the skill root — `tests/oc_skill_remote.rs::oc_skill_discovery_downloads_safe_nested_files_under_the_skill_root`
- P refreshes cached files when the version changes — `tests/oc_skill_remote.rs::oc_skill_discovery_refreshes_cached_files_when_the_version_changes`
- P publishes complete updates and removes stale files — `tests/oc_skill_remote.rs::oc_skill_discovery_publishes_complete_updates_and_removes_stale_files`

**`skill.test.ts`**

- D applies configuration precedence to skills without changing their registered values — effect-internal: skill registry transforms and events
- D publishes catalog updates when organization policy changes — cloud: organization policy
- D reads the current editor entry by ID · registers values with last-write-wins precedence · updates and removes registered values · restores earlier values when an updating transform is disposed · publishes updates after committed values are visible — effect-internal: skill registry transforms and events
- P stores virtual skill files, notifies when they change, and prepares skill:// output without disk scans — `src/tool/read.rs::oc_read_virtual_skill_paths`

**`skill/instructions.test.ts`**

- P announces added and removed skills as deltas without restating the list — `tests/oc_instruction_state.rs::oc_skills_announce_added_and_removed_skills_as_deltas`
- P renders only the changed skill when a description changes — `tests/oc_instruction_state.rs::oc_skills_render_only_the_changed_skill`

</details>

<details><summary>Instructions and references (76 of 78)</summary>


**`instruction-discovery.test.ts`**

- P stores ordered values with last-write-wins precedence — `tests/oc_instruction_state.rs::oc_instruction_files_store_ordered_values_with_the_latest_content` (adapted: discovery lists each file once at its first position, with the content it has when read)
- P preserves admitted values while the source is unavailable — `tests/oc_instruction_state.rs::oc_instruction_files_keep_admitted_values_while_unreadable` (adapted: an unreadable file)
- P renders granular instruction updates — `tests/oc_instruction_state.rs::oc_instruction_files_render_granular_updates`
- P loads global and upward project files and rescans them on change — `src/instructions.rs::oc_instructions_loads_global_and_upward_project_files`
- P loads configured files, globs, home paths, and URLs alongside AGENTS.md — `src/instructions.rs::oc_instructions_loads_configured_files_globs_and_home_paths`
- D reloads configured sources on config updates and keeps the last fetched URL — deliberate: instructions.rs: configured URLs are not fetched
- P falls back to CLAUDE.md where no AGENTS.md exists — `src/instructions.rs::oc_instructions_falls_back_to_claude_md_where_no_agents_md_exists`
- P keeps an empty AGENTS.md as available context — `src/instructions.rs::oc_instructions_keeps_an_empty_agents_md`
- P discovers a newly created instruction file above the project root — `src/instructions.rs::oc_instructions_finds_a_file_above_the_project_root_inside_home`
- P stops instruction candidates at the project root outside home — `src/instructions.rs::oc_instructions_stop_at_the_project_root_outside_home`
- D stops at the project root when the location drive letter differs in case — platform: Windows drive letters
- P marks a discovered file that disappears before read as unavailable — `tests/oc_instruction_state.rs::oc_instruction_files_that_vanish_before_the_read_are_unavailable`
- D skips discovered files a sandbox denies and keeps the rest available — client-server: sandbox-denied files

**`instruction-state.test.ts`**

- D observes each source once without publishing events or inserting blobs · commits initial metadata and changed and removed deltas without rereading sources · records reordered instructions without emitting update text · keeps no-op observations free of events and blobs — storage-model: instruction_state.rs: the state is folded from the session's record log, not projected rows, events and blobs
- P treats a missing state row as a fresh baseline without repairing it — `tests/oc_instruction_state.rs::oc_session_makes_a_fresh_baseline_when_the_state_is_missing` (adapted: a log without instruction records)
- D trusts the projected state without consulting durable events · previews changed and removed instructions from observed blobs — storage-model: instruction_state.rs: the state is folded from the session's record log, not projected rows, events and blobs
- P persists chronological updates as system messages — `tests/oc_instruction_state.rs::oc_session_keeps_the_baseline_stable_and_narrates_an_update` (adapted: through a session; the update is a record the history shows as a system message)
- D assembles initial instructions without persisting a baseline — storage-model: instruction_state.rs: the state is folded from the session's record log, not projected rows, events and blobs
- P retains a committed value when fresh instructions are unavailable — `tests/oc_instruction_state.rs::oc_session_keeps_values_while_a_source_is_unreadable` (adapted: through a session)
- P blocks an unavailable initial instruction without persisting a baseline — `tests/oc_instruction_state.rs::oc_session_retries_the_first_request_once_context_is_readable` (adapted: through a session)
- D keeps prepare equivalent to observe followed by commit — storage-model: instruction_state.rs: the state is folded from the session's record log, not projected rows, events and blobs

**`instructions/builtins.test.ts`**

- P loads location-scoped environment and host-local date instructions — `tests/oc_instruction_state.rs::oc_builtins_load_the_environment_and_date`
- P updates the date without repeating unchanged environment instructions — `tests/oc_instruction_state.rs::oc_builtins_update_the_date_without_repeating_the_environment`
- P does not update again within the same local calendar day — `tests/oc_instruction_state.rs::oc_builtins_do_not_update_again_the_same_day`

**`instructions/index.test.ts`**

- P reads each source once and derives the initial delta and text — `tests/oc_instruction_state.rs::oc_instructions_reads_each_source_once_and_derives_the_initial_delta_and_text`
- P derives no delta when the encoded value is unchanged — `tests/oc_instruction_state.rs::oc_instructions_derives_no_delta_when_the_value_is_unchanged`
- P renders a changed value from stored values — `tests/oc_instruction_state.rs::oc_instructions_renders_a_changed_value_from_stored_values`
- P admits and renders an observed removal — `tests/oc_instruction_state.rs::oc_instructions_admits_and_renders_an_observed_removal`
- P treats JSON null as a value rather than a removal — `tests/oc_instruction_state.rs::oc_instructions_treats_json_null_as_a_value`
- P blocks the initial delta while any source is unavailable — `tests/oc_instruction_state.rs::oc_instructions_blocks_the_initial_delta_while_a_source_is_unavailable`
- P keeps the stored value while a source is unavailable mid-session — `tests/oc_instruction_state.rs::oc_instructions_keeps_the_stored_value_while_unavailable_mid_session`
- P does not infer removal when a source is absent from the current version — `tests/oc_instruction_state.rs::oc_instructions_does_not_infer_removal_for_an_absent_source`
- P renders a newly added source with its initial renderer — `tests/oc_instruction_state.rs::oc_instructions_renders_a_new_source_with_its_initial_renderer`
- P hashes objects independently of key order — `tests/oc_instruction_state.rs::oc_instructions_hashes_objects_independently_of_key_order`
- P renders sources in composition order — `tests/oc_instruction_state.rs::oc_instructions_renders_sources_in_composition_order`
- P rejects duplicate source keys — `tests/oc_instruction_state.rs::oc_instructions_rejects_duplicate_source_keys`
- P rejects empty model-visible renderings while allowing silent updates — `tests/oc_instruction_state.rs::oc_instructions_rejects_empty_renderings_while_allowing_silent_updates`
- P diffs list values by key — `tests/oc_instruction_state.rs::oc_instructions_diffs_list_values_by_key`

**`reference-instructions.test.ts`**

- P lists available references in the instructions — `tests/oc_references.rs::oc_reference_instructions_list_available_references`
- P omits instructions when no references are available — `tests/oc_references.rs::oc_reference_instructions_are_omitted_without_references`
- P omits references without descriptions — `tests/oc_references.rs::oc_reference_instructions_omit_references_without_descriptions`
- P announces added and removed references as deltas — `tests/oc_references.rs::oc_reference_instructions_announce_added_and_removed_references`
- P announces added, changed, and removed references in one delta — `tests/oc_references.rs::oc_reference_instructions_announce_one_delta_for_several_changes`

**`reference.test.ts`**

- P reads the current editor source by name — `tests/oc_references.rs::oc_reference_reads_each_configured_source_by_name` (adapted: config names each source; there is no editor)
- D reads batched references before cache work and update events · lets update listeners replace references and refetch current info — effect-internal: batched state transforms and update listeners
- P registers normalized sources for the owning scope — `tests/oc_references.rs::oc_reference_lists_normalized_sources` (adapted: no owning scope)
- P derives Git paths without exposing cache operations — `tests/oc_references.rs::oc_reference_derives_git_paths_without_cache_work`
- P preserves configured Git descriptions — `tests/oc_references.rs::oc_reference_preserves_configured_git_descriptions`

**`repository-cache.test.ts`**

- P persists the daily throttle across cache recreation and serializes competing refreshes — `tests/oc_references.rs::oc_repository_cache_persists_the_daily_throttle_and_serializes_refreshes`
- P honors legacy attempt records while allowing forced refreshes — `tests/oc_references.rs::oc_repository_cache_honors_older_attempt_records_and_forced_refreshes`
- P throttles failed refresh attempts until the next interval — `tests/oc_references.rs::oc_repository_cache_throttles_failed_refreshes`
- P refreshes existing untracked checkouts and keeps branch freshness independent — `tests/oc_references.rs::oc_repository_cache_refreshes_untracked_checkouts_per_branch`
- P replaces a stale cache directory before cloning — `tests/oc_references.rs::oc_repository_cache_replaces_a_stale_directory_before_cloning`
- P serializes concurrent materialization for the same checkout — `tests/oc_references.rs::oc_repository_cache_serializes_concurrent_clones`
- P replaces an existing checkout whose origin does not match — `tests/oc_references.rs::oc_repository_cache_replaces_a_checkout_with_another_origin`
- P keeps branch checkouts isolated from branchless refreshes — `tests/oc_references.rs::oc_repository_cache_keeps_branch_checkouts_apart`
- P does not mistake an enclosing repository for the cache checkout — `tests/oc_references.rs::oc_repository_cache_does_not_take_an_enclosing_repository_for_the_checkout`
- P returns typed branch validation and clone failures — `tests/oc_references.rs::oc_repository_cache_returns_typed_branch_and_clone_failures`

**`repository.test.ts`**

- P parses github shorthand and builds an explicit-root cache path — `tests/oc_references.rs::oc_repository_parses_github_shorthand_and_builds_a_cache_path`
- P parses host path and scp remote references — `tests/oc_references.rs::oc_repository_parses_host_path_and_scp_references`
- P caches a host with a port under a directory per host part — `tests/oc_references.rs::oc_repository_caches_a_host_with_a_port_per_host_part`
- P keeps local file repositories distinct from remote repositories — `tests/oc_references.rs::oc_repository_keeps_local_file_repositories_apart`
- P rejects unsafe remote references and branches with typed errors — `tests/oc_references.rs::oc_repository_rejects_unsafe_references_and_branches`
- P rejects %s because its host contains a relative path segment ×7 — `tests/oc_references.rs::oc_repository_rejects_hosts_with_relative_path_segments`
- P compares cache identity independent of input spelling — `tests/oc_references.rs::oc_repository_compares_identity_independent_of_spelling`

</details>

<details><summary>Config (157 of 164)</summary>


**`config/config.test.ts`**

- P discovers the global config directory once when the project walk reaches it — `src/config.rs::oc_config_loads_the_global_config_once_when_the_project_walk_reaches_it`
- D loads system configuration at highest priority · resolves the system configuration directory per platform · resolves macOS managed preferences with user-specific profiles first · strips MDM payload metadata from managed preferences — deliberate: config.rs: no system, MDM, well-known or integration config
- P loads explicit file and content overrides in priority order — `src/config.rs::oc_config_loads_explicit_file_and_content_overrides_in_priority_order`
- D layers integration documents below remote and user config and reloads when they change · ranks integration policies below managed policies and above user and project policies · keeps only the policies of documents from integrations managed policy denies — deliberate: config.rs: no system, MDM, well-known or integration config
- P skips project configuration when project discovery is disabled — `src/config.rs::oc_config_skips_project_configuration_when_project_discovery_is_disabled`
- D reloads file substitutions when their source changes · exposes filesystem updates under config roots through changes · does not reload when an unrelated file under a config root changes · reloads canonical events for a symlinked global root, even after removal · reloads ${source} config when its symlink target changes under a watched root ×2 · keeps watching a deleted config file so recreating it reloads · backs Config.Service and Config.Test with one shared test implementation — deliberate: config.rs: no live reload or file watching
- P returns the latest defined scalar from priority-ordered documents — `src/config.rs::oc_config_returns_the_latest_defined_scalar`
- D tolerates unavailable authenticated wellknown config and reloads it later · keeps the last resolved wellknown config across failures and restarts while a credential exists — deliberate: config.rs: no system, MDM, well-known or integration config
- D logs redacted source-aware diagnostics for every config source — other: OpenCode's source-aware config diagnostics logging
- D migrates arbitrary v1 configuration into valid v2 configuration — deliberate: config.rs: no v1 format migration
- D decodes configuration maps in source order — deliberate: config.rs: named maps ordered by key, not source order
- D decodes v1 permissions in source order at every level · migrates the v1 experimental subagent depth · migrates the v1 update policy · normalizes the native auto update policy · migrates v1 provider lists to policies · migrates v1 provider setup options into AISDK settings · renames old provider IDs while migrating v1 configuration · preserves the generated base URL for v1 Azure OpenAI-compatible providers · ignores old provider IDs when the current provider ID is configured · preserves the built-in package for v1 Vertex Anthropic custom models · migrates v1 interleaved fields to compatibility · migrates v1 command configuration with subtask: ${subtask} ×2 · normalizes renamed permission actions when migrating v1 permissions — deliberate: config.rs: no v1 format migration
- P returns an empty configuration when directory files do not exist — `src/config.rs::oc_config_returns_an_empty_configuration_when_no_files_exist`
- D does not recursively watch ecosystem config roots — deliberate: config.rs: no live reload or file watching
- P loads opencode JSON and JSONC files from lowest to highest priority — `src/config.rs::oc_config_loads_json_and_jsonc_files_from_lowest_to_highest_priority`
- P substitutes environment variables and relative file contents — `src/config.rs::oc_config_substitutes_environment_variables_and_relative_file_contents`
- P keeps env values unescaped when the config already parses — `src/config.rs::oc_config_keeps_env_values_unescaped_when_the_config_already_parses`
- P does not load legacy config.json files — `src/config.rs::oc_config_does_not_load_legacy_or_opencode_config_files`
- P accepts $schema metadata without writing it into config files — `src/config.rs::oc_config_accepts_schema_metadata_without_writing_it_into_config_files`
- P loads supported scalar and resource configuration — `src/config.rs::oc_config_loads_supported_scalar_and_resource_configuration`
- D migrates the deprecated reference key into references · migrates v1 configuration when a v1-only key is present — deliberate: config.rs: no v1 format migration
- D ignores an invalid file while loading valid config values — deliberate: config.rs: an invalid file is an error naming it (src/config.rs::oc_config_an_invalid_file_is_an_error_naming_it pins the difference)
- P loads global and ancestor configuration across the project boundary — `src/config.rs::oc_config_loads_global_and_ancestor_configuration_in_order`

**`config/discovery.test.ts`**

- D skips resolve and stat calls for absent ancestor candidates while keeping watch paths · falls back to direct candidate probes when listing an ancestor directory is denied · probes candidates when readDirectory reports a differently cased entry — effect-internal: filesystem probe minimization during discovery

**`config/entry-observer.test.ts`**

- D closes the startup race and reloads later updates — deliberate: config.rs: no live reload

**`config/image.test.ts`**

- D merges image limits and reloads changed config · refetches config after subscribing to updates — deliberate: config.rs: no live reload (image limits merge is covered by read/media tests)

**`config/model.test.ts`**

- P normalizes short and explicit model selections — `src/config.rs::oc_config_normalizes_short_model_selections`

**`config/normalization.test.ts`**

- D accepts tool.use without losing statement order · drops unsupported policy actions without losing supported policies · rejects every non-object root with one root diagnostic · keeps unrelated native fields when a legacy field is present · canonicalizes transformed native values through decode then encode · preserves arbitrary JSON-round-tripped native configuration · merges named maps by entry and gives valid native entries precedence · does not report canonical-equal duplicates as conflicts · uses agent then mode then native agent precedence · renames legacy provider IDs in the top-level model · keeps an inline variant in legacy model references · migrates the legacy small model to the title agent · merges the legacy small model with the title agent · omits an invalid legacy small model without exposing its value · recovers malformed named entries and retains a valid legacy collision · accepts partial model capabilities without reporting a diagnostic · uses a valid retired provider alias when the canonical legacy entry is malformed · preserves permission source order and appends native rules · preserves legacy %s permission source order ×2 · redacts permission resource keys from invalid diagnostics · recovers list items for skills, instructions, and permissions · omits malformed collection roots instead of synthesizing empty values · omits all-invalid formatter and LSP maps while preserving explicit empty maps · combines legacy and native MCP servers and merges timeout leaves · preserves codemode: false for legacy $type MCP servers ×2 · leaves codemode unset for legacy $type MCP servers when omitted ×2 · expands numeric MCP timeouts to every phase · uses raw MCP discriminators for reserved server names · normalizes MCP timeout fields in schema order with per-leaf recovery · merges bounded compaction leaves and omits unsupported leaves · distinguishes empty, mixed, and wholly malformed enabled provider lists · merges migrated, experimental, and native policies in order · normalizes the image_generate setting · normalizes every registered flag as a boolean · accepts deprecated codemode_vm and ignores retired codemodeVm without diagnostics · passes experimental.all_flags through as a boolean · never emits experimental policies · converts legacy prefixed integration.use statements from config and MDM files · reports unsupported legacy settings without including their values · diagnoses unsupported legacy model selections without dropping their entries · migrates the legacy thinking block-binding opt-out into model compatibility · migrates the legacy setCacheKey provider option into provider compatibility · invalid legacy provider overlays skip only that provider · preserves explicit false, zero, empty list, and empty map presence — deliberate: config.rs: no v1 format migration or normalization layer

**`config/reload.test.ts`**

- D loads skills when ${input.root} appears after startup ×4 · retains readiness signalled synchronously during initial config startup · loads the first config written while a new directory watch is starting · loads references when ${input.file} is first created and keeps watching it ×4 · preserves reference precedence and insertion order across documents · reloads config-backed domains without reloading external plugins · publishes catalog updates on boot and real changes but skips unchanged reloads — deliberate: config.rs: no live reload

**`config/shell.test.ts`**

- D applies the preferred shell and reloads changed config — deliberate: config.rs: no live reload

**`config/snapshot.test.ts`**

- D applies availability and reloads changed config — deliberate: config.rs: no live reload

**`config/tool-output.test.ts`**

- D applies limits and reloads changed config — deliberate: config.rs: no live reload

**`config/watch.test.ts`**

- D groups missing candidates and keeps parent watches when roots appear · adds exact watches for explicit files only when not already covered — deliberate: config.rs: no file watching

**`config/websearch.test.ts`**

- D reloads changed default selection — deliberate: config.rs: no live reload

**`flag.test.ts`**

- D applies %s build defaults ×6 · todo is opt-in only: on only when named in OPENCODE_FLAGS or experimental.todo · full_codemode defaults to on in local and dev builds, off in releases, and follows overrides and wildcards · config overrides the default and OPENCODE_FLAGS overrides config · a flag given settings is on · reads the old schedules key for automations, below the new key · reads experimental from the highest-priority document that sets it, as before · OPENCODE_FLAGS=all turns every flag on, and a -key in the same list still wins · experimental.all_flags turns every flag on below OPENCODE_FLAGS and below a per-flag setting · experimental.all_flags: false is not a kill switch, and all_flags in OPENCODE_FLAGS does nothing · a global all_flags survives a project experimental block, and a project all_flags: false stops it · reads the Config, channel and OPENCODE_FLAGS in scope and follows config updates — deliberate: CodeSmith has no OPENCODE_FLAGS experimental-flag system; features are config settings

**`variant.test.ts`**

- P spells Messages variants for each provider — `tests/oc_variant.rs::oc_variant_spells_messages_variants_for_each_provider`
- P recognizes Claude version spellings and future models — `tests/oc_variant.rs::oc_variant_recognizes_claude_version_spellings_and_future_models`
- P spells Cloudflare AI Gateway variants for their upstream routes — `tests/oc_variant.rs::oc_variant_spells_cloudflare_ai_gateway_variants_for_their_upstream_routes`
- P spells Vercel AI Gateway variants for their selected routes — `tests/oc_variant.rs::oc_variant_spells_vercel_ai_gateway_variants_for_their_selected_routes`
- P spells xAI Responses variants with catalog effort levels — `tests/oc_variant.rs::oc_variant_spells_xai_responses_variants_with_catalog_effort_levels`
- P spells Chat Completions variants for direct providers — `tests/oc_variant.rs::oc_variant_spells_chat_completions_variants_for_direct_providers`
- P spells Bedrock Converse Claude budgets as a thinking setting — `tests/oc_variant.rs::oc_variant_spells_bedrock_converse_claude_budgets_as_a_thinking_setting`
- P spells Bedrock Converse effort for Grok and Nova — `tests/oc_variant.rs::oc_variant_spells_bedrock_converse_effort_for_grok_and_nova`
- P caps Alibaba thinking budget variants at 64k — `tests/oc_variant.rs::oc_variant_caps_alibaba_thinking_budget_variants_at_64k`
- P spells Workers AI thinking controls through the chat template — `tests/oc_variant.rs::oc_variant_spells_workers_ai_thinking_controls_through_the_chat_template`
- P spells Cohere native thinking and compatibility effort variants — `tests/oc_variant.rs::oc_variant_spells_cohere_native_thinking_and_compatibility_effort_variants`
- P spells Chat Completions variants for hosting providers — `tests/oc_variant.rs::oc_variant_spells_chat_completions_variants_for_hosting_providers`
- P spells GitLab Duo variants for gitlab-ai-provider routes — `tests/oc_variant.rs::oc_variant_spells_gitlab_duo_variants_for_gitlab_ai_provider_routes` (GitLab Duo's catalog package keeps its `aisdk:` name)
- P spells reasoning variants for every bundled native package — `tests/oc_variant.rs::oc_variant_spells_reasoning_variants_for_every_bundled_native_package`
- P spells Mantle variants for the API each model is routed through — `tests/oc_variant.rs::oc_variant_spells_mantle_variants_for_the_api_each_model_is_routed_through`
- P spells Google Interactions variants as thinking levels — `tests/oc_variant.rs::oc_variant_spells_google_interactions_variants_as_thinking_levels`
- P reads models.dev v2 reasoning as variant support — `tests/oc_variant.rs::oc_variant_reads_models_dev_v2_reasoning_as_variant_support`

</details>

<details><summary>MCP (62 of 212)</summary>


**`mcp-import-boundary.test.ts`**

- D loads the MCP SDK only when connecting or authorizing — effect-internal: lazy SDK import

**`mcp-instructions.test.ts`**

- P renders additions, changes, and removal — `tests/oc_instruction_state.rs::oc_mcp_instructions_render_additions_changes_and_removal` (adapted: from the session's (server, instructions, Code Mode) list)

**`mcp-socket-client.test.ts`**

- D buffers frames that arrive before start · sends one JSON frame per message and delivers inbound frames · reports invalid frames and stays open · fires onclose once when the socket ends · rejects send after close · close() closes the socket with 1000 and fires onclose once · fires onclose when the socket ends before start, then rejects start · close() before start drops buffered frames and rejects start · close() fires onclose and rejects when the socket close throws · keeps FIFO order for frames delivered while flushing the buffer · send resolves to undefined · rejects a second start · rejects send before start · stops flushing buffered frames once closed · carries an MCP session between the SDK client and server — other: OpenCode's WebSocket MCP transport (CodeSmith speaks stdio and Streamable HTTP/SSE)

**`mcp.test.ts`**

- D spawns local MCP servers through the location environment · reports a local MCP server as failed when the location has no execution plane — client-server: MCP connections pooled across Locations
- D manages live MCP servers entirely through scoped transforms · restores runtime MCP config when a transform is disposed · isolates nested configured MCP mutations and reconciles them · publishes MCP changes while live but not while the location shuts down · batches MCP transforms without connecting intermediate configurations · reconciles only changed MCP server config · keeps MCP config snapshots stable during an in-flight replacement · discards in-flight and queued MCP notifications after its layer closes · is not undone by the queued startup · serializes concurrent MCP lifecycle operations · finishes an MCP connect whose caller is interrupted during the handshake · finishes an MCP disconnect whose caller is interrupted while the session ends · preserves plugin transforms through MCP catalog updates · coalesces queued MCP tool notifications after initial registration — effect-internal: scoped MCP config transforms and lifecycle serialization
- D integration policy denies MCP startup (${modern ? "modern" : "legacy"}) ×2 · revokes and restores a connected MCP through ${source} policy ×2 — cloud: integration/organization policy
- D shares one connection and one tool list between Locations · keeps connections apart when the configs differ · keys connections on everything that changes them, and on no secret or name · keeps a server that negotiates legacy private to each Location · connects on its own when a server refuses the pooled handshake · keeps pooling a server that ${what} when first tried ×3 · decides a legacy server is private right after the handshake, before listing its tools · fails a pooled server that can't be spawned after one attempt · connects each Location on its own without pool · closes the shared connection when its last Location lets go · connects with the credential its key was made from, even when a sign-in lands mid-start · registers a server's prompt commands again after reconnecting${shared ? " to a connection another Location holds" : ""} ×2 · routes an elicitation to the Location whose call caused it · asks every holder about an elicitation no request claims, and takes the first answer · fans tool list changes out to every Location · answers no roots on a shared connection · shares a local server across directories only when its cwd is fixed · shares one local process, fails every holder when it dies, and closes it after the last · keeps nothing of a Location that let go of a shared connection — client-server: MCP connections pooled across Locations
- D refuses to subscribe with the flag off, and ends at a server without events · offers no events with the mcp_events flag off, or from a server without them — deliberate: MCP events are not behind a flag

**`tool-mcp-events.test.ts`**

- D is absent while the mcp_events flag is off — deliberate: MCP events are not behind a flag

</details>

<details><summary>Code Mode (45 of 67)</summary>


**`codemode.test.ts`**

- D owns registrations, execute, and catalog materialization — effect-internal: registration ownership
- D renders Effect numbers in built-in tool signatures as number — effect-internal: Effect number rendering

**`codemode/catalog.test.ts`**

- P retains namespace inventory without retaining tools outside the inline budget — `tests/oc_instruction_state.rs::oc_codemode_catalog_keeps_the_inventory_without_listing_past_the_budget`
- P retains every namespace when no full tool listing fits — `tests/oc_instruction_state.rs::oc_codemode_catalog_keeps_every_namespace_when_nothing_fits`
- P always retains pinned tools beyond the inline budget — `tests/oc_instruction_state.rs::oc_codemode_catalog_always_lists_pinned_tools`
- P spends the budget remaining after pinned tools on unpinned tools — `tests/oc_instruction_state.rs::oc_codemode_catalog_spends_what_pinned_tools_leave`
- P retains only the rendered portion of inline descriptions — `tests/oc_instruction_state.rs::oc_codemode_catalog_keeps_only_the_first_description_line`
- P limits inline descriptions to 500 characters — `tests/oc_instruction_state.rs::oc_codemode_catalog_limits_descriptions_to_500_characters`
- P always retains namespace descriptions and charges them before tool listings — `tests/oc_instruction_state.rs::oc_codemode_catalog_charges_namespace_descriptions_first`
- P selects tools in path order, whatever their cost — `tests/oc_instruction_state.rs::oc_codemode_catalog_selects_tools_in_path_order`
- D renders only the signatures it selects, and the next one it tries — other: codemode_catalog.rs: signatures are strings built from the schema up front, not rendered lazily
- P inlines complete catalogs without search guidance — `tests/oc_instruction_state.rs::oc_codemode_catalog_inlines_a_complete_catalog_without_search`
- P adds search guidance when the catalog exceeds the budget — `tests/oc_instruction_state.rs::oc_codemode_catalog_adds_search_guidance_past_the_budget` (the budgeted form only; sessions list the whole catalog, since CodeSmith's Code Mode has no `search`)
- P budgets signatures round-robin so every namespace remains visible — `tests/oc_instruction_state.rs::oc_codemode_catalog_budgets_round_robin_across_namespaces`
- P charges inline JSDoc in signatures against the catalog token budget — `tests/oc_instruction_state.rs::oc_codemode_catalog_charges_jsdoc_against_the_budget`
- P renders only the no-tools notice for an empty catalog — `tests/oc_instruction_state.rs::oc_codemode_catalog_renders_only_the_notice_when_empty`
- P renders additions, changes, and removals as a compact semantic delta — `tests/oc_instruction_state.rs::oc_codemode_catalog_renders_a_compact_semantic_delta`
- P names removed tools with exact callable expressions including bracket notation — `tests/oc_instruction_state.rs::oc_codemode_catalog_names_removed_tools_with_callable_expressions`
- P restates a namespace whose description changed — `tests/oc_instruction_state.rs::oc_codemode_catalog_restates_a_namespace_whose_description_changed`
- P says nothing when only which tools are shown changes — `tests/oc_instruction_state.rs::oc_codemode_catalog_says_nothing_when_only_the_shown_tools_change`
- P shows a new tool's signature even when the catalog would not — `tests/oc_instruction_state.rs::oc_codemode_catalog_shows_a_new_signature_the_catalog_would_not`
- P counts new tools past the update budget — `tests/oc_instruction_state.rs::oc_codemode_catalog_counts_new_tools_past_the_update_budget`
- P shows a changed signature the model was shown even when the tool is no longer shown — `tests/oc_instruction_state.rs::oc_codemode_catalog_shows_a_changed_signature_no_longer_shown`
- P shows every new tool, without pointing to search, while the catalog lists every tool — `tests/oc_instruction_state.rs::oc_codemode_catalog_shows_every_new_tool_while_complete`
- P introduces search once, when the catalog stops listing every tool — `tests/oc_instruction_state.rs::oc_codemode_catalog_introduces_search_once` (the budgeted form only (see 164))
- P falls back to full replacement when the delta is larger than the catalog — `tests/oc_instruction_state.rs::oc_codemode_catalog_falls_back_to_full_replacement`

**`codemode/instructions.test.ts`**

- P instructs the model not to call execute while the catalog is empty — `tests/oc_instruction_state.rs::oc_codemode_instructions_say_not_to_call_execute_while_empty` (sessions with no Code Mode tools leave the catalog out instead)
- P renders the initial catalog, semantic deltas, and removal — `tests/oc_instruction_state.rs::oc_codemode_instructions_render_the_catalog_deltas_and_removal`
- P stores a canonical sorted snapshot so registration order does not churn history — `tests/oc_instruction_state.rs::oc_codemode_instructions_store_a_canonical_sorted_snapshot`

**`tool-execute.test.ts`**

- D with full_codemode, execute names Bun's file APIs instead of ruling out filesystem access · without the experimental flag there is no path · path is the host's path module, resolving against the session's directory · the other platform's flavour resolves from its own root, not the server's working directory · path is importable by its Node module names · Bun and the bun module exist only with the experimental flag · Bun.file reads a file, a slice of it, and fails on a missing one with a Node-shaped error · Bun.write writes text, bytes and copies, and a writer replaces then appends · Bun.Glob scans for files, with dot files, folders and absolute paths on request · Bun asks the permissions the file tools ask · reads the first chunk before EOF: %s ×3 · whole-body methods share consumption and expose Bun's empty body · discards a redirect body before reading the destination — platform: Bun/Node globals and fetch body internals (CodeSmith's Code Mode runs in Boa without them)
- D executions share the prepared runtime of an inventory while keeping per-call state — effect-internal: shared prepared runtime

</details>

<details><summary>Automations (45 of 107)</summary>


**`automation-event.test.ts`**

- D retains the location only while its quiet subscription is open · pause cancels a prompt still preparing without waiting for its hook or admitting it on resume — client-server: Location retention and prompt-preparation hooks

**`automation.test.ts`**

- D rejects tool-created code and child actions if the session switched agents during the model request — storage-model: agent switches during the model request / inbox IDs
- D adopts the child a crashed run already started · drops a run's result when another process recorded a run meanwhile · does not duplicate a prompt admitted before a crash — deliberate: automation.rs: a lease keeps processes apart; no crash adoption or inbox replay
- D rejects creation when experimental.automations is off · rechecks hourly while its location has the flag off, then catches up once it is on — deliberate: automations are not behind an experimental flag
- D wakes a Session on start when its automation prompt was never claimed · does not call a tool while another server holds the lease, then takes over when it expires · picks up an automation created by another process sharing the DB while idle · keeps distinct child handoffs pending and rewakes the recorded child on restart — deliberate: automation.rs: a lease keeps processes apart; no crash adoption or inbox replay
- D does not use an inbox ID to override changed authority when retrying an admitted child prompt — storage-model: agent switches during the model request / inbox IDs
- D reconciles a child handoff already delivered before settlement — deliberate: automation.rs: a lease keeps processes apart; no crash adoption or inbox replay
- D waits for plugins when authorizing, but counts activation against the run timeout — effect-internal: plugin activation

**`job.test.ts`**

- D tracks process-local work through explicit observation · publishes jobs before starting immediately settling work · preserves authorization and complete failure details without stacks · reuses running work when started again with the same ID · ignores an obsolete callback after a cancellation waiter starts a same-ID replacement · returns finished from a blocking wait when completion wins · returns backgrounded from a blocking wait when background wins · backgrounds only jobs actively blocking a session · retains background ownership and terminal output until notification acknowledgment · persists backgroundAll ownership before releasing a blocked subagent · retains terminal errors for recovery until notification acknowledgment · durably backgrounds recoverable work that has already failed · recovers a background marker after its process-local registry closes · preserves running background ownership when its work is interrupted · interrupts live work without promising settlement after the owning process-local scope closes · interrupts observers without settling a background job, or running its cancel hook, on teardown · runs a job's cancel hook when it is cancelled, alone or with its session · cancels running session jobs and removes unacknowledged SQLite job rows on cancelSession · does not restore a consumed background row when its running work settles · classifies a backgrounded subagent report and marks it settling · starts a fresh generation while the previous subagent report is settling · matches an external cancel to its busy period through the bound generation · writes subagent outbox rows with pure SQL upserts outside the live registry · persists a background marker before forking background work · distinguishes caller cancellation from teardown · matches a creation-tick cancellation without a bound generation exactly once · excludes settling and consumed work from running background snapshots · migrates legacy kv job.background/* markers into the job table — deliberate: session.rs: background work is the shell tool's Jobs; no durable job table, outbox or background subagents

**`tool-automation.test.ts`**

- D are absent unless experimental.automations is enabled · rechecks the flag on each call, since a request keeps its tools after config reloads — deliberate: automations are not behind an experimental flag

</details>

<details><summary>Formatters (17 of 26)</summary>


**`formatter.test.ts`**

- P matches ${entry.file} against ${JSON.stringify(entry.extension)}: ${entry.matches} ×10 — `tests/oc_tools.rs::oc_format_matches_files_against_configured_extensions`
- P does not run formatters marked as disabled in config — `tests/oc_tools.rs::oc_format_skips_disabled_formatters`
- P file() returns false when no formatter runs — `tests/oc_tools.rs::oc_format_returns_none_when_no_formatter_runs`
- D loads formatter state per directory — deliberate: config.rs: no live reload (formatter state is per session)
- P stops after the first matching formatter succeeds — `tests/oc_tools.rs::oc_format_stops_after_the_first_formatter_that_succeeds` (adapted: configured formatters run in key order)
- P tries the next matching formatter when the first fails — `tests/oc_tools.rs::oc_format_tries_the_next_formatter_when_the_first_fails`
- D rebuilds formatter state and clears resolved commands · does not cache a command resolved before reload — deliberate: config.rs: no live reload (formatter state is per session)

</details>

<details><summary>Worktrees, VCS and projects (88 of 158)</summary>


**`git.test.ts`**

- D discovers repository metadata without a work tree · clones a remote and reads checkout metadata · fetches, checks out, and resets remote changes · fingerprint changes with repository metadata, including from a linked worktree · refresh handles check-ignore exit ${exitCode} ×3 · lists both sides of a rename as separate file changes · caps batched tree patches, keeps per-file stats past the cap, and matches non-ASCII names · diffs a path selection longer than any platform command line · captures, compares, previews, and restores scoped trees · packs loose objects and combines packs without losing or delegating any object · applies both the store's own and the source's ignore rules and never splits the index · queued captures share one capture that still sees changes made before the call · skips repeat ignore checks for oversized and info/exclude-only untracked files · seeds and rebuilds the store from a source with a split index · sweeps abandoned temporary indexes by their creation time, not their mtime · records a tracked file replaced by an embedded repository as a gitlink · keeps the index when Git cannot even start — other: OpenCode's Git service (clone/fetch, tree capture with a private index); CodeSmith shells out per feature

**`plugin/vcs-init.test.ts`**

- D Promise VCS initializer reaches the registered provider with its location scope — client-server: VCS init plugin RPC

**`plugin/worktree.test.ts`**

- D Promise worktree callbacks receive interruption through their AbortSignal — client-server: worktree plugin RPC (CodeSmith's worktrees are a CLI flag)

**`project.test.ts`**

- D returns complete projects ordered by recent activity · updates and clears project metadata · records activity without editing metadata time and throttles repeats · lists archived projects only when requested · archives projects whose directories are gone and unarchives them on resolve · moves the canonical directory to a surviving clone instead of archiving · skips projects updated or active within the last sweep interval · never archives the global project · creates distinct deterministic projects for exact markerless directories · repository markers override markerless directory projects · does not publish project updates for first or repeated resolutions · publishes preserved project metadata when its canonical directory is renamed · keeps the canonical project directory when opening another clone · does not write the cache while resolving · reuses git discovery until git metadata changes · does not cache a transient git failure · sees the first commit in a reftable repository — client-server: OpenCode's project registry (list, archive, activity)

**`util/git-executable.test.ts`**

- D uses an absolute executable on Windows · uses a non-shim PATH executable on macOS without probing developer directories · resolves the Apple shim on macOS from developer directories · falls back to PATH lookup on macOS when no developer tools exist or candidate is invalid · orders macOS developer directory candidates by DEVELOPER_DIR then xcode-select link · leaves lookup to the process on Linux · falls back to PATH lookup when Git is unresolved — platform: locating git on macOS/Windows

**`vcs.test.ts`**

- D routes initialization through the requested provider before a repository exists · returns empty results outside version control · serves scoped providers and restores the fallback after disposal · automatically selects a provider matching the resolved repository · reads batched providers without refreshing intermediate selections · passes location scope and bounded diff options to providers · validates provider results and bounds oversized patches · preserves provider interruption · stops in-flight and queued VCS reloads when its layer closes · keeps watching HEAD changes after a transform replay failure · serializes filesystem and config refreshes while reading the latest desired provider · keeps branch streams current when listeners change the selected provider · batches untracked files without spawning Git per file or touching the index · lists an untracked folder as one entry and its entries from inside it when asked for folders · stops listing an untracked folder after its scan budget and says more were not checked · caches branch info and publishes HEAD changes · reports provider failures and validates optional base metadata — client-server: OpenCode's VCS provider service, streams and branch watching

**`worktree.test.ts`**

- D tracks canonical and linked worktrees when a project resolves · accepts arbitrary non-empty strategy ids · reports unavailable recorded strategy ids · requires a tracked project worktree · uses canonical configuration with an explicit source clone · creates from another managed worktree · preserves worktrees whose stored strategy is unavailable · does not publish an event when refresh finds no directory changes · refresh finds and prunes an externally managed git worktree · refresh ignores stale git worktree registrations · refresh ignores existing directories that are no longer git checkouts · refresh seeds the canonical checkout when inventory is empty · refresh removes missing ordinary checkouts · defaults to Git and configured directory without depending on Config · selects the last active registration and restores earlier strategies on disposal · does not fall back to Git when a registered strategy fails · rejects a source override belonging to another project · cannot remove a worktree belonging to another project · list is independent of an ambient workspace-qualified location · only refresh scans and prunes; list reads saved inventory · a failed strategy does not block list or other discovery · applies directory config through the adapter and restores defaults after config removal · resolves ${mode} global directory config from a linked checkout's subdirectory ×3 · normalization retains worktree directory and rejects invalid configuration — client-server: OpenCode's worktree inventory, strategies and refresh

</details>

<details><summary>Providers, models and credentials (494 of 796)</summary>


**`aisdk-native.test.ts`**

- D keeps Cloudflare AI Gateway models on its native gateway package · maps Vercel AI Gateway packages to native routes · maps OpenAI-family packages and request options to native providers · maps Anthropic settings and request options to the native provider · maps Cerebras, DeepInfra, Groq, and Together AI settings, headers, and reasoning options to native providers · maps Google Vertex settings to the native provider · maps supported Mistral settings and request overlays to the native provider · maps both models.dev Cohere packages to native routes · maps both models.dev Bedrock packages to native providers · translates settings for every bundled native package named in config · maps Azure deployments and settings to native routes · maps Bedrock provider and request options · maps Bedrock Mantle models to their supported native APIs · maps static Bedrock Mantle credentials without leaking connection options · forwards Bedrock profile and auth mode for the default credential chain · maps the legacy Bedrock endpoint override · maps OpenRouter settings to native destinations · maps every Google thinking setting · maps Google thinking settings independently · maps Google request options without thinking settings · maps Vertex Gemini settings to the native Gemini route · maps Vertex Anthropic settings to native Messages · maps supported xAI settings — platform: AI SDK npm packages (CodeSmith implements the wire protocols natively in codesmith-ai)

**`aisdk.test.ts`**

- D rejects native provider compaction rather than silently dropping replay state · keys language models by package and flattened overlays · uses canonical names and metadata without merging connection cache partitions · projects request settings, headers, and body overlays · uses only the provider timeout signal when the request signal is null · lowers chronological system updates to wrapped user messages · leaves max output tokens unset when the request omits them · maps pro reasoning bodies to AI SDK provider options · maps package-specific AI SDK provider option keys · forces reasoning and projects both Azure AI SDK namespaces · routes AI Gateway model options by upstream prefix · closes the open AI SDK reasoning part when the next one starts · normalizes repeated, reopened, and overlapping AI SDK fragment boundaries · projects replay metadata onto AI SDK prompt parts · normalizes file data across AI SDK prompt parts · does not treat SSE comment heartbeats as model progress · sends AI SDK requests through the embedder's fetch when the provider names none · routes AI SDK requests and responses through HTTP hook middleware · sends AI SDK requests directly when no HTTP hook middleware is attached · fails with a retryable transport error when response headers time out · emits malformed AI SDK tool input without executing it · keeps malformed provider-executed AI SDK input terminal · preserves non-empty AI SDK error messages · preserves ${typeof payload} AI SDK stream error payloads in the runtime body ×2 · reads gateway ${shape} stream errors as invalid requests ×2 · does not copy Error request internals into the provider body · preserves existing AI errors and their retry semantics unchanged · derives status and code when the AI SDK error message is empty · preserves complete HTTP context on AI SDK call errors · classifies retryable AI SDK failures with retry-after details · classifies data-only AI SDK provider codes · classifies data-only AI SDK authentication errors · classifies SDK data alongside the original ${kind} response body ×3 · detects context overflow from data-only AI SDK errors · retries status-less AI SDK transport failures · classifies native fetch failures as request transport errors · keeps the code of a host name that didn't resolve · classifies mid-stream socket drops as read transport errors · classifies the SSE chunk timeout as a read transport error · keeps unrecognized error codes on the unknown provider path · prefers a structured provider message over the code fallback · falls back to the status alone for malformed response bodies — platform: AI SDK bridging (CodeSmith implements the wire protocols natively in codesmith-ai)

**`catalog.test.ts`**

- D keeps nested model values editable after ${path} model resolution ×4 · publishes an updated event after provider changes · preserves provider identity when updating new and existing providers · derives availability from active credentials without changing provider state · reuses the model catalog across credential switches · persists direct edits to models returned by list and get · gives foreign definitions the registering provider's identity · keeps materialized models when another provider becomes available · derives availability from a provider's integration · makes an explicitly enabled provider available without a connection · projects environment connections without a catalog plugin · stores provider package settings · uses model package settings over provider settings · resolves default model package settings from the provider · rewrites AI SDK packages and their options on write · keeps settings scoped while resolving request overlay merges — storage-model: OpenCode's editable provider/model store, its events and credential-derived availability (CodeSmith resolves config over the catalog at each model resolution; see tests/oc_catalog.rs)
- D defaults to big-pickle when only free OpenCode models are available · keeps the newest default when a paid model is available — cloud: OpenCode free models
- D uses a transform-provided default model until that transform is replaced — storage-model: OpenCode's editable provider/model store, its events and credential-derived availability (CodeSmith resolves config over the catalog at each model resolution; see tests/oc_catalog.rs)
- D small model uses the newest release in the first matching family · small model returns undefined without a matching family — deliberate: title.rs: no small model

**`config/provider.test.ts`**

- D inherits the provider compaction setting with model overrides and rejects unsupported routes — other: provider-native compaction settings (CodeSmith always compacts by summary)
- D carries a configured provider cache format to the runtime model — other: CodeSmith detects the cache marker format from the endpoint; there is no cacheFormat setting
- D keeps the provider websocket policy out of model settings — other: model-level transport settings (CodeSmith's transport is a provider setting only)
- P configured provider transport overrides ${builtin.id} defaults ×2 — `tests/oc_catalog.rs::oc_catalog_configured_provider_transport_overrides_builtin_defaults` (the openai and azure rows)
- D configured provider transport overrides ${builtin.id} defaults ×2 — other: xAI is OpenAI Chat in CodeSmith, and only the azure provider defaults to WebSocket
- P lets config change a catalog model's type and keeps it when unset — `tests/oc_catalog.rs::oc_catalog_lets_config_change_a_catalog_models_type_and_keeps_it_when_unset`
- P preserves catalog capabilities unless config overrides them — `tests/oc_catalog.rs::oc_catalog_preserves_catalog_capabilities_unless_config_overrides_them`
- P merges partial config capabilities onto the base model's existing fields — `tests/oc_catalog.rs::oc_catalog_merges_partial_config_capabilities_onto_the_base_models_existing_fields`
- D uses native model defaults for migrated ${scenario.name} ×7 · matches native configuration for migrated ${scenario.name} ×7 · preserves existing catalog metadata when migrated fields are omitted · keeps catalog capabilities that a migrated override omits — deliberate: config.rs: no v1 format migration
- P generates variants after rewriting a configured model package — `tests/oc_catalog.rs::oc_catalog_generates_variants_after_rewriting_a_configured_model_package` (adapted: a configured protocol for the package)
- D preserves plugin-supplied variants on custom models when config omits variants — deliberate: plugin.rs: plugins supplying variants to custom models
- P respells inherited variants when config moves a catalog model to another package — `tests/oc_catalog.rs::oc_catalog_respells_inherited_variants_when_config_moves_a_catalog_model_to_another_package` (adapted: moved to another protocol)
- P keeps a moved catalog model without reasoning variants free of variants — `tests/oc_catalog.rs::oc_catalog_keeps_a_moved_catalog_model_without_reasoning_variants_free_of_variants`
- P builds configured reasoning variants after provider and model package normalization — `tests/oc_catalog.rs::oc_catalog_builds_configured_reasoning_variants_after_provider_and_model_package_normalization` (adapted: one provider per protocol)
- P applies configured release date, temperature support, and reasoning field — `tests/oc_catalog.rs::oc_catalog_applies_configured_release_date_temperature_support_and_reasoning_field` (adapted: no release dates or freeform formats kept)

**`integration-refresh.test.ts`**

- D shares refresh across Locations${cancel ? " after the first caller is interrupted" : ""} ×2 · bounds a stuck integration refresh and allows a later attempt — cloud: OpenCode integrations

**`integration.test.ts`**

- D registers integrations through the editor · reveals the previous registration when an override closes · registers and overrides methods independently · connects with a key and stores the credential · names unlabeled credentials after the integration · runs command authentication and stores the final output line · completes code OAuth once and stores the credential · keeps code attempts open when the code is missing and closes them on cancel · completes auto OAuth in the background · fails the attempt when credential persistence fails · fails and closes OAuth attempts when a pending transform throws during persistence · expires abandoned OAuth attempts · uses provider-defined OAuth attempt expirations · projects credential and env connections · reports the underlying cause message · falls back when the cause carries no message — cloud: OpenCode integrations (key/command/OAuth credential connections); CodeSmith's `auth login` covers keys

**`model-free.test.ts`**

- D ${providerID} catalog with OPENCODE_DISABLE_FREE_MODELS=${flag} ×8 — cloud: OpenCode free models

**`model-plugins.test.ts`**

- D composes source, configuration, and restriction plugins across reload and unload · shares a plugin's definitions across locations while keeping model edits and cached results local · replays cross-provider policies when shared access changes, including reads inside a batch · removes a failed model plugin's access and providers without retaining its partial model edits · hides an account-specific inventory after switching credentials until its source refreshes · derives a custom provider from inactive templates independently of the upstream account's model policies — deliberate: plugin.rs: plugins are compiled in, so no plugin edits the model catalog

**`model-resolver.test.ts`**

- D routes compatible packages to the host's dedicated package when one exists · resolves environment templates before native providers inspect endpoints — platform: AI SDK / native package routing and loaders
- P maps Bedrock Mantle GPT-OSS models to Chat and other models to Responses — `tests/oc_providers.rs::oc_resolver_maps_bedrock_mantle_chat_and_responses_packages` (adapted: by the catalog's native Mantle packages)
- P resolves Bedrock Mantle catalog endpoints from the configured region — `tests/oc_providers.rs::oc_resolver_resolves_bedrock_mantle_catalog_endpoints_from_the_configured_region`
- P fills Mantle catalog URLs from the provider region — `tests/oc_providers.rs::oc_resolver_fills_mantle_catalog_urls_from_the_provider_region`
- P prefers the configured Mantle region over the environment — `tests/oc_providers.rs::oc_resolver_prefers_the_configured_mantle_region_over_the_environment`
- D keeps catalog apiKey credentials out of provider JSON · uses no native API-key auth for explicitly enabled providers without credentials · keeps native provider environment auth strict when no API key is configured · maps catalog Anthropic AI SDK models into native routes · does not project API key metadata into settings or the request body · does not project OAuth account metadata into the request body · applies plugin-projected OpenAI endpoint and headers · does not route native OpenAI-compatible packages to the codex backend · maps legacy OpenAI organization and project settings to headers · keeps non-ChatGPT OAuth credentials on the configured endpoint · loads dynamic native provider packages through the injected package loader · maps OAuth credentials to native provider auth settings without their metadata · routes supported AISDK catalog packages through native provider packages · never loads the AI SDK for packages with native implementations — platform: AI SDK / native package routing and loaders
- P routes Vertex Anthropic catalog models through native Messages — `tests/oc_providers.rs::oc_resolver_routes_vertex_anthropic_catalog_models_through_native_messages`
- D loads supported AISDK catalog packages as native routes · loads arbitrary AISDK packages through the injected AISDK loader · rejects unresolved variables before loading opaque AISDK packages · rejects placeholders introduced by environment expansion before loading providers · rejects unresolved variables before loading native provider packages · rejects AISDK packages without an available loader · reports provider configuration errors from supported packages · distinguishes unexpected constructor failures from configuration errors · reports package load and AISDK initialization failures with their causes · drops an empty API key before loading an AISDK package · applies provider timeouts to resolved image, video, and decision models · reports whether a catalog model declares a provider package — platform: AI SDK / native package routing and loaders

**`model.test.ts`**

- P parses %j at the first slash ×7 — `src/config.rs::oc_config_normalizes_short_model_selections` (model selection parsing)
- P accepts a model selection without a variant — `src/config.rs::oc_config_normalizes_short_model_selections` (model selection parsing)
- P preserves an explicit model variant — `src/config.rs::oc_config_normalizes_short_model_selections` (model selection parsing)

**`models.test.ts`**

- P normalizes permissive interleaved values to compatibility — `tests/oc_catalog.rs::oc_catalog_normalizes_permissive_interleaved_values_to_compatibility` (config's `interleaved`)
- P get() returns normalized snapshots from KV when a cache entry exists — `tests/oc_catalog.rs::oc_catalog_get_returns_normalized_snapshots_from_kv_when_a_cache_entry_exists`
- P takes each model's package from its first API and skips URLs packages build themselves — `tests/oc_catalog.rs::oc_catalog_takes_each_models_package_from_its_first_api_and_skips_urls_packages_build_themselves`
- P keeps non-chat models with their type, maps MIME modalities, and skips malformed models — `tests/oc_catalog.rs::oc_catalog_keeps_non_chat_models_with_their_type_maps_mime_modalities_and_skips_malformed_models`
- P maps per-token text prices, context tiers, and mode multipliers onto cost — `tests/oc_catalog.rs::oc_catalog_maps_per_token_text_prices_context_tiers_and_mode_multipliers_onto_cost`
- P skips APIs whose opencode entrypoint this build doesn't have — `tests/oc_catalog.rs::oc_catalog_skips_apis_whose_opencode_entrypoint_this_build_doesnt_have`
- P skips non-chat models whose only opencode package serves another type — `tests/oc_catalog.rs::oc_catalog_skips_non_chat_models_whose_only_opencode_package_serves_another_type`
- P drops model types opencode doesn't keep — `tests/oc_catalog.rs::oc_catalog_drops_model_types_not_kept`
- P skips API entries and modes it can't read and keeps the model — `tests/oc_catalog.rs::oc_catalog_skips_api_entries_and_modes_it_cant_read_and_keeps_the_model`
- P reads a models.dev v1 catalog by converting it to v2 — `tests/oc_catalog.rs::oc_catalog_reads_a_models_dev_v1_catalog_by_converting_it_to_v2`
- P falls back to /api.json when a custom mirror has no v2 catalog — `tests/oc_catalog.rs::oc_catalog_falls_back_to_api_json_when_a_custom_mirror_has_no_v2_catalog` (adapted: a local server)
- P get() returns empty catalog when KV is empty, fetch disabled, and the bundled snapshot is disabled — `tests/oc_catalog.rs::oc_catalog_get_returns_empty_catalog_when_kv_is_empty_fetch_disabled_and_the_bundled_snapshot_is_disabled`
- P get() falls back to the bundled snapshot when KV is empty and fetch is disabled — `tests/oc_catalog.rs::oc_catalog_get_falls_back_to_the_bundled_snapshot_when_kv_is_empty_and_fetch_is_disabled`
- P get() recovers from a corrupted KV entry by fetching a fresh catalog — `tests/oc_catalog.rs::oc_catalog_get_recovers_from_a_corrupted_kv_entry_by_fetching_a_fresh_catalog`
- P get() still populates the catalog when the KV cache write fails — `tests/oc_catalog.rs::oc_catalog_get_still_populates_the_catalog_when_the_kv_cache_write_fails`
- P uses the default models URL when the configured URL is empty — `tests/oc_catalog.rs::oc_catalog_uses_the_default_models_url_when_the_configured_url_is_empty` (adapted: checks the source rather than sending the request; CodeSmith defaults to models.dev v1)
- P get() is single-flight under concurrent calls — `tests/oc_catalog.rs::oc_catalog_get_is_single_flight_under_concurrent_calls`
- P get() caches across calls (later KV writes are ignored until invalidate) — `tests/oc_catalog.rs::oc_catalog_get_caches_across_calls_later_kv_writes_are_ignored_until_invalidate`
- P refresh(true) fetches via HttpClient and updates the cache — `tests/oc_catalog.rs::oc_catalog_refresh_true_fetches_via_http_and_updates_the_cache` (adapted: a generic codesmith/<version> User-Agent; fetching is opt-in in CodeSmith)
- P refresh(false) skips fetch when the KV entry is fresh — `tests/oc_catalog.rs::oc_catalog_refresh_false_skips_fetch_when_the_kv_entry_is_fresh`
- P refresh(false) fetches when the KV entry is stale — `tests/oc_catalog.rs::oc_catalog_refresh_false_fetches_when_the_kv_entry_is_stale` (adapted: the subscribe channel for the Refreshed event)
- D refresh publishes Refreshed globally across locations — client-server: the Refreshed event across OpenCode Locations; CodeSmith has one catalog per process
- P refresh(false) stays quiet when the fetched body matches the cached digest — `tests/oc_catalog.rs::oc_catalog_refresh_false_stays_quiet_when_the_fetched_body_matches_the_cached_digest`
- P refresh(false) revalidates with the stored ETag and treats 304 as unchanged — `tests/oc_catalog.rs::oc_catalog_refresh_false_revalidates_with_the_stored_etag_and_treats_304_as_unchanged` (adapted: no tracing spans)
- P refresh(false) republishes once for legacy cache entries without a digest — `tests/oc_catalog.rs::oc_catalog_refresh_false_republishes_once_for_legacy_cache_entries_without_a_digest`
- P refresh swallows HTTP errors and leaves cache intact — `tests/oc_catalog.rs::oc_catalog_refresh_swallows_http_errors_and_leaves_cache_intact`

**`network-guard.test.ts`**

- D refuses requests to hosts other than loopback · the default http client node requests through the harness fetch · an explicit HttpClient replacement is what the node sees · loopback requests pass through to the real fetch — platform: Bun network guard for tests

**`npm-config.test.ts`**

- D reads registry from project .npmrc · reads scoped registries from project .npmrc · flattens boolean and list options · normalizes configured registry without trailing slash · leaves configured registry without trailing slash unchanged — platform: npm configuration

**`npm.test.ts`**

- D keeps normal scoped package specs unchanged · handles git https specs · accepts registry packages and rejects unsupported install targets · accepts registry and npm-compatible Git specs · canonicalizes registry keys and hashes Git specs · locates cached scoped package specs without reifying · reifies when package cache directory exists without the package installed · installs and locates %s Git packages with dependencies ×2 · installs a Git package from an npm ::path: subdirectory at a %s ×2 · checks and updates mutable Git packages without changing pinned installs · rejects only the newest installed version, keeping its files for what still runs it · reports Git's own error when a Git package cannot be fetched · records Git revisions when the cache directory is reached through a symlink · locates a cached package without installing it · installs and updates without requesting registry audits · checks a mutable registry target without mutation and explicitly updates it · never reports a pinned registry target as outdated — platform: npm package installs

**`plugin/models-dev.test.ts`**

- D shares definitions between Locations while provider and model edits own their copies — client-server: definitions shared between OpenCode Locations
- D replaces its snapshot reference on refresh without touching the sibling Location — client-server: snapshot references per OpenCode Location
- P adopts a refresh that completes between its initial read and its subscription — `tests/oc_catalog.rs::oc_catalog_adopts_a_refresh_that_completes_between_its_initial_read_and_its_subscription` (adapted: each model resolution reads the current catalog)
- D keeps the retained definition unchanged across model replay — effect-internal: model replay in OpenCode's editable model store
- P keeps the shared snapshot pristine while provider and model transforms mutate owned records — `tests/oc_catalog.rs::oc_catalog_keeps_the_shared_snapshot_pristine_while_provider_and_model_transforms_mutate_owned_records` (adapted: config overrides instead of transforms)
- P projects normalized definitions for available providers — `tests/oc_catalog.rs::oc_catalog_projects_normalized_definitions_for_available_providers`
- P omits deprecated and non-text model definitions — `tests/oc_catalog.rs::oc_catalog_omits_deprecated_and_non_text_model_definitions`
- P lists non-chat models only by type, so sessions never see them — `tests/oc_catalog.rs::oc_catalog_lists_non_chat_models_only_by_type_so_sessions_never_see_them`
- P registers key methods for providers with environment variables — `tests/oc_catalog.rs::oc_catalog_registers_key_methods_for_providers_with_environment_variables` (adapted: the provider's key variables)
- P preserves provider and model URL templates in the catalog — `tests/oc_catalog.rs::oc_catalog_preserves_provider_and_model_url_templates_in_the_catalog` (CodeSmith fills them from the environment at resolution)
- P copies model request bodies without reinterpreting literal __proto__ keys — `tests/oc_catalog.rs::oc_catalog_copies_model_request_bodies_without_reinterpreting_literal_proto_keys`
- P omits legacy provider aliases — `tests/oc_catalog.rs::oc_catalog_omits_legacy_provider_aliases`
- P registers every listed environment variable for provider plugins to narrow — `tests/oc_catalog.rs::oc_catalog_registers_every_listed_environment_variable_for_provider_plugins_to_narrow`
- P converts reasoning options into settings variants — `tests/oc_catalog.rs::oc_catalog_converts_reasoning_options_into_settings_variants`

**`plugin/provider-amazon-bedrock.test.ts`**

- P moves endpoint setting to baseURL — `tests/oc_providers.rs::oc_bedrock_moves_endpoint_setting_to_base_url`
- P gives the package the profile of a stored AWS profile login — `tests/oc_providers.rs::oc_bedrock_gives_the_package_the_profile_of_a_stored_aws_profile_login` (adapted: `codesmith auth login amazon-bedrock --profile` saves it)
- P keeps an explicit baseURL over endpoint — `tests/oc_providers.rs::oc_bedrock_keeps_an_explicit_base_url_over_endpoint`
- P only treats the bearer token env var as a key credential — `tests/oc_providers.rs::oc_bedrock_only_treats_the_bearer_token_env_var_as_a_key_credential`
- P leaves activation on auto without ambient AWS configuration — `tests/oc_providers.rs::oc_bedrock_leaves_activation_on_auto_without_ambient_aws_configuration` (adapted: activation is whether `codesmith models` lists the provider)
- P enables the provider when ${name} is set ×7 — `tests/oc_providers.rs::oc_bedrock_enables_the_provider_when_an_ambient_variable_is_set`
- P enables the provider when a profile is configured — `tests/oc_providers.rs::oc_bedrock_enables_the_provider_when_a_profile_is_configured`
- D does not override a disabled provider — deliberate: CodeSmith has no provider activation to override; a provider is used when a model names it
- P fills region from AWS_REGION then AWS_DEFAULT_REGION without overriding config — `tests/oc_providers.rs::oc_bedrock_fills_region_from_aws_region_then_aws_default_region_without_overriding_config`
- P falls back to AWS_DEFAULT_REGION then us-east-1 — `tests/oc_providers.rs::oc_bedrock_falls_back_to_aws_default_region_then_us_east_1`
- P applies to Mantle and native Bedrock packages — `tests/oc_providers.rs::oc_bedrock_applies_to_mantle_and_native_bedrock_packages`

**`plugin/provider-cerebras.test.ts`**

- P applies the legacy integration header — `tests/oc_providers.rs::oc_cerebras_applies_the_legacy_integration_header` (adapted: the header names CodeSmith)
- P ignores non-Cerebras providers — `tests/oc_providers.rs::oc_cerebras_ignores_non_cerebras_providers`
- P applies the integration header to custom native Cerebras providers — `tests/oc_providers.rs::oc_cerebras_applies_the_integration_header_to_custom_native_cerebras_providers`

**`plugin/provider-cloudflare-ai-gateway.test.ts`**

- P registers account and gateway forms when the environment does not provide them — `tests/oc_providers.rs::oc_cloudflare_gateway_registers_account_and_gateway_forms_when_the_environment_does_not_provide_them` (adapted: `codesmith auth login` asks the fields after the key)
- P uses environment account and gateway values without requesting form fields — `tests/oc_providers.rs::oc_cloudflare_gateway_uses_environment_account_and_gateway_values_without_requesting_form_fields`
- P requests only the gateway ID when the environment provides the account ID — `tests/oc_providers.rs::oc_cloudflare_gateway_requests_only_the_gateway_id_when_the_environment_provides_the_account_id`
- P gives the provider its environment account and gateway IDs — `tests/oc_providers.rs::oc_cloudflare_gateway_gives_the_provider_its_environment_account_and_gateway_ids` (adapted: asserts the routes the ids give)
- P prefers configured IDs over environment IDs — `tests/oc_providers.rs::oc_cloudflare_gateway_prefers_configured_ids_over_environment_ids`
- P gives the provider the account and gateway IDs of a stored form login — `tests/oc_providers.rs::oc_cloudflare_gateway_gives_the_provider_the_account_and_gateway_ids_of_a_stored_form_login`
- P applies the account and gateway IDs of a form login made after startup — `tests/oc_providers.rs::oc_cloudflare_gateway_applies_the_account_and_gateway_ids_of_a_form_login_made_after_startup` (adapted: routes read the saved login per resolution)
- P leaves a configured endpoint URL untouched — `tests/oc_providers.rs::oc_cloudflare_gateway_leaves_a_configured_endpoint_url_untouched`

**`plugin/provider-cloudflare-workers-ai.test.ts`**

- P registers an account form when the environment does not provide one — `tests/oc_providers.rs::oc_cloudflare_workers_registers_an_account_form_when_the_environment_does_not_provide_one`
- P routes the OpenAI-compatible package to the native Workers AI package — `tests/oc_providers.rs::oc_cloudflare_workers_routes_the_openai_compatible_package_to_the_native_workers_ai_package` (adapted: either package routes to the account's endpoint)
- P gives the native Workers AI package its account ID — `tests/oc_providers.rs::oc_cloudflare_workers_gives_the_native_workers_ai_package_its_account_id`
- P gives the native Workers AI package the account ID of a stored form login — `tests/oc_providers.rs::oc_cloudflare_workers_gives_the_native_workers_ai_package_the_account_id_of_a_stored_form_login`
- P follows the account ID when another form login becomes active — `tests/oc_providers.rs::oc_cloudflare_workers_follows_the_account_id_when_another_form_login_becomes_active` (adapted: one saved login per provider; the latest login wins)
- P preserves a configured endpoint URL instead of deriving one from account ID — `tests/oc_providers.rs::oc_cloudflare_workers_preserves_a_configured_endpoint_url_instead_of_deriving_one_from_account_id`
- P uses configured account ID over env account ID — `tests/oc_providers.rs::oc_cloudflare_workers_uses_configured_account_id_over_env_account_id`

**`plugin/provider-databricks.test.ts`**

- P fills the base URL from a bare host — `tests/oc_providers.rs::oc_databricks_fills_the_base_url_from_a_bare_host_or_one_with_a_scheme`
- P fills the base URL from a host that includes the scheme — `tests/oc_providers.rs::oc_databricks_fills_the_base_url_from_a_bare_host_or_one_with_a_scheme`

**`plugin/provider-github-copilot.test.ts`**

- P prefers the account-specific Copilot API endpoint — `tests/oc_providers.rs::oc_copilot_prefers_the_account_specific_copilot_api_endpoint`
- P rejects accounts without Copilot chat access — `tests/oc_providers.rs::oc_copilot_rejects_accounts_without_copilot_chat_access`
- P only blocks on an explicit entitlement denial — `tests/oc_providers.rs::oc_copilot_only_blocks_on_an_explicit_entitlement_denial`
- P refreshes the account's API endpoint instead of keeping the one stored at login — `tests/oc_providers.rs::oc_copilot_refreshes_the_account_api_endpoint_instead_of_keeping_the_one_stored_at_login` (adapted: looked up once per run)
- P keeps the stored endpoint when the account lookup fails — `tests/oc_providers.rs::oc_copilot_keeps_the_stored_endpoint_when_the_account_lookup_fails`
- P serves an Enterprise login from its own host until the first model list arrives — `tests/oc_providers.rs::oc_copilot_serves_an_enterprise_login_from_its_own_host_until_the_first_model_list_arrives`
- P serves a login from the endpoint saved with it until the first model list arrives — `tests/oc_providers.rs::oc_copilot_serves_a_login_from_the_endpoint_saved_with_it_until_the_first_model_list_arrives`
- P registers GitHub Copilot device OAuth — `tests/oc_providers.rs::oc_copilot_registers_github_copilot_device_oauth` (adapted: the OAuth client id is deliberately not OpenCode's; the user supplies their own app (--client-id, settings.clientId, CODESMITH_COPILOT_CLIENT_ID))
- P removes the generic key method and reads COPILOT_GITHUB_TOKEN instead of GITHUB_TOKEN — `tests/oc_providers.rs::oc_copilot_removes_the_generic_key_method_and_reads_copilot_github_token_instead_of_github_token`
- P adds Copilot authentication and request metadata headers to OpenAI requests — `tests/oc_providers.rs::oc_copilot_adds_copilot_authentication_and_request_metadata_headers_to_openai_requests`
- P adds Copilot authentication to native Anthropic requests — `tests/oc_providers.rs::oc_copilot_adds_copilot_authentication_to_native_anthropic_requests`
- P classifies main-loop steps as agent interactions — `tests/oc_providers.rs::oc_copilot_classifies_steps_by_kind`
- P classifies child-session steps as subagent interactions — `tests/oc_providers.rs::oc_copilot_classifies_steps_by_kind`
- P classifies title generation as a background interaction — `tests/oc_providers.rs::oc_copilot_classifies_steps_by_kind`
- P classifies compaction requests by kind rather than agent — `tests/oc_providers.rs::oc_copilot_classifies_steps_by_kind`
- P does not classify by agent name — `tests/oc_providers.rs::oc_copilot_classifies_steps_by_kind` (the kind comes from the step, never the agent name)
- P ignores other providers' model requests — `tests/oc_providers.rs::oc_copilot_ignores_other_providers_model_requests`
- P flags images returned by tools in Responses requests — `tests/oc_providers.rs::oc_copilot_flags_images_returned_by_tools_in_responses_requests`
- P treats tool images moved into a Chat Completions user message as agent-initiated — `tests/oc_providers.rs::oc_copilot_flags_images_returned_by_tools_in_responses_requests` (the Chat half)
- P keeps a declared agent initiator when the body looks user-initiated — `tests/oc_providers.rs::oc_copilot_classifies_steps_by_kind` (a declared agent step stays agent-initiated)
- P classifies session generation requests as agent interactions — `tests/oc_providers.rs::oc_copilot_classifies_steps_by_kind` (agent steps)
- P routes models.dev fallback models to native Copilot APIs — `tests/oc_providers.rs::oc_copilot_routes_models_dev_fallback_models_to_native_copilot_apis`
- P disables gpt-5-chat-latest before Copilot language selection — `tests/oc_providers.rs::oc_copilot_disables_gpt_5_chat_latest_before_copilot_language_selection` (adapted: hidden from `codesmith models`)
- P does not disable gpt-5-chat-latest for non-Copilot providers — `tests/oc_providers.rs::oc_copilot_disables_gpt_5_chat_latest_before_copilot_language_selection` (only Copilot's list hides it)

**`plugin/provider-google-vertex.test.ts`**

- P ignores OpenAI-compatible providers that are not Google Vertex — `tests/oc_providers.rs::oc_vertex_ignores_openai_compatible_providers_that_are_not_google_vertex`
- P resolves project and location from env using legacy precedence — `tests/oc_providers.rs::oc_vertex_resolves_project_and_location_from_env_using_legacy_precedence`
- P enables the provider when a project resolves and leaves it automatic otherwise — `tests/oc_providers.rs::oc_vertex_enables_the_provider_when_a_project_resolves_and_leaves_it_automatic_otherwise`
- P enables the provider when a project resolves from env — `tests/oc_providers.rs::oc_vertex_enables_the_provider_when_a_project_resolves_from_env`
- P resolves the advertised GOOGLE_VERTEX_PROJECT env for provider updates and SDKs — `tests/oc_providers.rs::oc_vertex_resolves_the_advertised_google_vertex_project_env`
- P keeps configured project and location over env and uses global endpoint — `tests/oc_providers.rs::oc_vertex_keeps_configured_project_and_location_over_env_and_uses_global_endpoint`
- P expands Vertex endpoint templates to the multi-region host for ${location} ×2 — `tests/oc_providers.rs::oc_vertex_expands_endpoint_templates_to_the_multi_region_host`
- P expands endpoint templates on Vertex chat models — `tests/oc_providers.rs::oc_vertex_expands_endpoint_templates_on_vertex_chat_models`
- P defaults location to global when only project is configured — `tests/oc_providers.rs::oc_vertex_defaults_location_to_global_when_only_project_is_configured`

**`plugin/provider-kilo.test.ts`**

- D is registered so legacy referer headers can be applied — deliberate: plugin.rs: provider plugins are compiled in
- P applies legacy referer headers only to Kilo endpoints — `tests/oc_providers.rs::oc_kilo_applies_legacy_referer_headers_only_to_kilo_endpoints` (adapted: the headers name CodeSmith)
- P uses the exact legacy Kilo header casing and set — `tests/oc_providers.rs::oc_kilo_applies_legacy_referer_headers_only_to_kilo_endpoints` (adapted: the headers name CodeSmith)
- P uses endpoint package matching instead of a provider ID guard — `tests/oc_providers.rs::oc_kilo_uses_endpoint_matching_instead_of_a_provider_id_guard`

**`plugin/provider-llmgateway.test.ts`**

- D is registered so legacy referer headers can be applied — deliberate: plugin.rs: provider plugins are compiled in
- P applies legacy referer headers only to enabled llmgateway — `tests/oc_providers.rs::oc_llmgateway_applies_legacy_referer_headers` (adapted: the headers name CodeSmith)
- D does not apply legacy headers to a disabled llmgateway provider — deliberate: CodeSmith has no provider activation; a provider is used when a model names it

**`plugin/provider-nvidia.test.ts`**

- D is registered so legacy referer headers can be applied — deliberate: plugin.rs: provider plugins are compiled in
- P applies NVIDIA tracking headers only to nvidia — `tests/oc_providers.rs::oc_nvidia_applies_tracking_headers_and_preserves_an_explicit_billing_origin` (adapted: the headers name CodeSmith)
- P adds billing origin for custom NVIDIA endpoints — `tests/oc_providers.rs::oc_nvidia_applies_tracking_headers_and_preserves_an_explicit_billing_origin` (adapted: the headers name CodeSmith)
- P preserves an explicit NVIDIA billing origin header — `tests/oc_providers.rs::oc_nvidia_applies_tracking_headers_and_preserves_an_explicit_billing_origin` (adapted: the headers name CodeSmith)

**`plugin/provider-openai.test.ts`**

- P selects WebSocket only from explicit policy — `tests/websocket_session.rs` (WebSocket only from explicit policy)

**`plugin/provider-opencode-artifacts.test.ts`**

- D adds no Console provider while the Console config doesn't advertise artifacts · makes Console the default provider while advertised, and removes it when the header goes · publishes with the session and returns Console's URL; an update keeps the slug and uploads only new files · reads the files back, downloading the ones Console doesn't inline from the listed revision · says Console has no such artifact when it was deleted there · gives two artifacts with one title their own Console artifacts · without event watches, publishes without the session and promises no wake — cloud: OpenCode hosted artifacts

**`plugin/provider-opencode-watches.test.ts`**

- D stays dormant while the Console config doesn't advertise event watches · stays dormant without the console_watches flag, even when the Console advertises event watches · registers the console_watch tools while watches are on · wakes a local session with a steer, framing a teammate's reply as theirs, and acknowledges only local events · reads the member's own reply as theirs, and a status change as its status alone, as information · reads a channel's new top-level message as maybe unrelated information, even the member's own · keeps Console's labels on one line, and never reads a bot's message as the member's own · wakes a session with an MCP server's event as its title, link and fenced data, as the member's own only when Console says so · acknowledges a batch replayed after a lost acknowledgement without waking again · shares one socket between Locations of the same workspace · reconnects at once when Console ends the socket's lease, and replays what it missed · reconnects after a backoff when Console closes the socket with ${code} ×2 · admits the sixth wake in 10 minutes without resuming, saying so from the fifth, until a person prompts · keeps resuming wakes minutes apart until 20 since the person's last prompt · keeps resuming an unattended watch past 20 since the prompt, while other watches still wait · holds an unattended watch's sixth update in 10 minutes, CI included · resumes a held session with a new update after 30 quiet minutes, but not with the held one replayed · resumes for six CI updates in 10 minutes, which don't fill the window, so a Slack reply after them does too · holds the 21st CI update since the person's last prompt · counts a batch of CI and a review toward the 10-minute window · resumes a held session with a later wake once 10 minutes pass · resumes a held session with the member's own Slack reply, but not with a bot's · reads a wake count stored before the 10-minute window as no recent wakes · long polls with Console's cursor when the WebSocket upgrade fails · polls again with each answer's cursor while answers come back full · acknowledges at most the ack limit of events per frame · cancels a deleted session's watches and acknowledges its later events · watches for the calling session and stops only its own watches · watches one event type of a workspace's MCP server for the calling session, and names it back · watches for as many minutes as the agent asks · watches an event type that takes no arguments with empty ones · refuses a watch given both a target and an event type, and watches nothing · lists the event types the member can watch, one line each, with a server's long description cut short · watches the pull request `gh pr create` opened and the branch `git push` updated · tells the session once why Console refused a shell command's watch, and skips other failures · starts and stops with the header on the Console's periodic config check · skips failed, ambiguous and non-GitHub commands — cloud: OpenCode hosted watches

**`plugin/provider-opencode.test.ts`**

- D registers account and service account methods · uses a canonical custom server throughout device authorization · rejects malformed device verification URLs · uses the device token organization during authorization: ${orgID} ×4 · persists rotated credentials for ${scenario.orgID}${scenario.unavailable ? " with discovery unavailable" : ""} ×6 · rejects non-HTTP OpenCode servers · rejects non-string OpenCode servers · loads native V2 providers and models from the connected OpenCode server · builds Console reasoning variants after provider and model package normalization · refreshes hosted search with Console config, retains it on failure, and skips unchanged snapshots · refreshes past future-only policies while retaining config on malformed statements · reports Console SSO sign-in, keeps the last config, and clears once Console accepts again · explains rejected Console inference from at most one config check per 30 seconds · ignores 403 responses that are not a Console SSO error · releases unread Console config responses when the fetch completes · sends credentials saved with the retired console.opencode.ai host to the default Console · reports a rejected Console refresh token as signed out without removing the credential · activates when the Console accepts the config request but never answers · reports a Console session rejected before its token expires as signed out · loads and executes hosted web search from the connected OpenCode server · sends hosted search to the default Console when the credential names none · follows a hosted search redirect without sending credentials to another origin · follows a 302 hosted search redirect as a GET · stops following a hosted search redirect loop at the hop limit · downloads a skill through a redirect without sending credentials to another origin · keeps Console instructions and skills through an edit or a Console error, and clears them on 404 · applies Console policies while its instructions and skills fail, and keeps a list over a malformed one · requests Console instructions and skills together · keeps credentials on a same-origin skill redirect · closes a rejected hosted search response without waiting for its body · preserves native Console OpenAI variant bodies in inference requests · uses a public key and disables paid models without credentials · keeps free models without credentials · treats output-only cost as free without credentials · uses OPENCODE_API_KEY as credentials · uses configured provider env vars as credentials · uses configured apiKey as credentials · ignores non-opencode providers and models · merges the Console response as config, with its policy named after the organization · passes only providers, remote MCP servers, policies, and the web search selection, without credentials · keeps a newer Console config loadable despite values this version does not know · layers Console providers below user config, bound to the Console connection · applies Console providers before provider plugins tune their canonical provider · offers Console web search only when the Console selects it, and user config still wins · replaces the Console config on an account switch and clears it on sign-out · disabled free models with ${key ? "an API key" : "a Console login after startup"} ×2 — cloud: OpenCode Zen / hosted provider

**`plugin/provider-openrouter.test.ts`**

- D is registered so legacy OpenRouter behavior can be applied — deliberate: plugin.rs: provider plugins are compiled in
- P applies legacy referer headers only to openrouter — `tests/oc_providers.rs::oc_openrouter_applies_referer_headers` (adapted: the headers name CodeSmith)

**`plugin/provider-poe.test.ts`**

- P registers Poe browser OAuth alongside generic key and environment methods without fetching — `tests/oc_provider_logins.rs::oc_poe_registers_poe_browser_oauth_alongside_generic_key_and_environment_methods_without_fetching` (adapted: `codesmith auth login poe --oauth` beside the key; the OAuth client id is the user's own)
- P exchanges a PKCE code for a native Poe credential (expiry: ${expiry}) ×3 — `tests/oc_provider_logins.rs::oc_poe_exchanges_a_pkce_code_for_a_native_poe_credential` (adapted: a user-supplied client id)
- P completes a login from a pasted redirect URL and keeps waiting after a wrong paste — `tests/oc_provider_logins.rs::oc_poe_completes_a_login_from_a_pasted_redirect_url_and_keeps_waiting_after_a_wrong_paste`
- P isolates overlapping login attempts and closes cancelled listeners — `tests/oc_provider_logins.rs::oc_poe_isolates_overlapping_login_attempts_and_closes_cancelled_listeners`
- P rejects invalid or denied callbacks (${JSON.stringify(invalid.params)}) ×6 — `tests/oc_provider_logins.rs::oc_poe_rejects_invalid_or_denied_callbacks`
- P preserves the active connection after a failed token exchange (${response.status}: ${response.body}) ×6 — `tests/oc_provider_logins.rs::oc_poe_preserves_the_active_connection_after_a_failed_token_exchange` (adapted: a failed login leaves the saved key)
- P waits for the token exchange and consumes the callback once (cancel: ${cancel}) ×2 — `tests/oc_provider_logins.rs::oc_poe_waits_for_the_token_exchange_and_consumes_the_callback_once`
- P keeps near-expiry keys usable and requires a new login after expiry — `tests/oc_provider_logins.rs::oc_poe_keeps_near_expiry_keys_usable_and_requires_a_new_login_after_expiry`

**`plugin/provider-snowflake-cortex.test.ts`**

- P browser OAuth supplies the native endpoint/token and refreshes a rejected token once — `tests/oc_provider_logins.rs::oc_snowflake_browser_oauth_supplies_the_native_endpoint_token_and_refreshes_a_rejected_token_once` (adapted: a 401 asks the login's authorizer to refresh, then the request is sent once more (codesmith-ai endpoint test); OpenCode's "Conversation complete" 400 rewrite is not ported)
- P manual PAT uses its account and native compatibility rather than an SDK request rewrite — `tests/oc_provider_logins.rs::oc_snowflake_manual_pat_uses_its_account_and_native_compatibility_rather_than_an_sdk_request_rewrite` (adapted: asserts the resolved model's compatibility and cacheFormat)
- P rejects a mismatched callback state before token exchange — `tests/oc_provider_logins.rs::oc_snowflake_rejects_a_mismatched_callback_state_before_token_exchange`
- P uses environment tokens rather than the account identifier and bypasses expired stored OAuth — `tests/oc_provider_logins.rs::oc_snowflake_uses_environment_tokens_rather_than_the_account_identifier_and_bypasses_expired_stored_oauth`
- P requires a token separately from the account and supports environment PATs — `tests/oc_provider_logins.rs::oc_snowflake_requires_a_token_separately_from_the_account_and_supports_environment_pats`
- P refreshes expired OAuth during native model resolution and persists rotated credentials — `tests/oc_provider_logins.rs::oc_snowflake_refreshes_expired_oauth_during_native_model_resolution_and_persists_rotated_credentials`

**`plugin/provider-zenmux.test.ts`**

- D is registered so legacy referer headers can be applied — deliberate: plugin.rs: provider plugins are compiled in
- P applies the exact legacy Zenmux headers — `tests/oc_providers.rs::oc_zenmux_lets_configured_legacy_headers_override_defaults` (adapted: the headers name CodeSmith)
- P merges legacy Zenmux headers with existing headers — `tests/oc_providers.rs::oc_zenmux_lets_configured_legacy_headers_override_defaults` (adapted: the headers name CodeSmith)
- P lets configured Zenmux legacy headers override defaults — `tests/oc_providers.rs::oc_zenmux_lets_configured_legacy_headers_override_defaults` (adapted: the headers name CodeSmith)
- D guards legacy Zenmux headers to the exact zenmux provider id — deliberate: CodeSmith matches ZenMux by its endpoint, as it does Kilo, LLM Gateway and NVIDIA, not by provider id

**`provider.test.ts`**

- D bundles every native provider entrypoint that exports model() · loads every @opencode/ai entrypoint the bundled models.dev snapshot names · keeps loading the hyphenated provider paths existing configs reference · passes flat settings to native packages without Core settings · inherits shared and loose settings without provider-only policies — platform: bundled AI SDK / native package entrypoints

**`usage-limits.test.ts`**

- D reads the member's Go allowance and budgets from the Console they signed in to · has no Go allowance or budgets to show where Console refuses them · has nothing to show for a location that isn't signed in to OpenCode Console · reports a Console that can't answer as unavailable — cloud: OpenCode Console usage limits

</details>

<details><summary>Plugins (67 of 90)</summary>


**`plugin-failure.test.ts`**

- D removes a failed plugin's hooks and RPC handlers without affecting healthy plugins · does not join queued failed-plugin cleanup during ${boundary} · does not restore a disabled generation with a pending failure when its replacement fails setup · preserves ${status} failures when an earlier plugin changes · continues failure reporting and cleanup after a plugin update observer fails · settles readiness before shutdown joins a disabled plugin's finalizers — deliberate: plugin.rs: plugins are compiled in, not loaded at runtime

**`plugin.test.ts`**

- D {scenario.name} ×4 · updates inventory metadata without restarting an unchanged generation · loads a local plugin with its configured options · serves a local plugin's GUI bundle, and reloads it alone when only the bundle changes · unloading a plugin removes its commands and runs cleanup · reports a failed plugin without blocking a healthy plugin · disables a plugin whose transform fails after setup without publishing its partial edits · keeps the suffix after a failed plugin alive across identical activations · attributes replay failure to the broken plugin rather than a later plugin reading the registry · disables plugins after runtime reload failures without retrying an unchanged generation · disables plugins after replay failures discovered during setup without restoring the old generation · does not let asynchronous plugin cleanup block recovered registry readiness · retains Promise plugin groups for later registrations and ignores a disabled group's attempts · normalizes Promise plugin API inputs through JSON · reloading a plugin replaces its command implementation · host event subscriptions deliver requested rpc types and ignore unknown types · unknown event type warnings name the subscribing plugin — deliberate: plugin.rs: native plugins compiled into the binary (no loading, reloading, GUI bundles or Promise groups)

**`plugin/host-model.test.ts`**

- D lists other model types for plugins only when they ask for them — deliberate: plugin.rs: compiled-in plugins

**`plugin/identity.test.ts`**

- D formats the model identity part · inserts the structured model block after the agent prompt — deliberate: plugin.rs: compiled-in plugins

**`plugin/internal-flag.test.ts`**

- D turns flagged built-in tools on for a dev-channel server with no config while keeping todo off — deliberate: plugin.rs: compiled-in plugins

**`plugin/module.test.ts`**

- D watches creation of an external helper with missing parent directories · interrupts pending watcher setup when the loader scope closes during module evaluation · loads plugins and their transitive dependencies against the host's Effect and @opencode/plugin instances · redirects plugin dependencies with a nested Effect 3 installation to the host, allows effect/package.json, and fails loudly on unprovided subpaths · discovers exported specifiers from the resolved tree even when dist/ exists, and validates barrel loader routing, async loader dedup, and Windows foreign filters · blocks a local plugin before module initialization and honors replacement policies · loads a GUI-only package under its package name without running it, and leaves a TUI-only one to the TUI · blocks versioned package ${target} before npm resolution or installation ×2 — deliberate: plugin.rs: compiled-in plugins (no module loading)

**`plugin/promise-tool-cancel.test.ts`**

- D reports the error of a tool that read its signal and gave up when the user stopped it · records the result of a tool that read its signal and resolved with what it had when stopped · interrupts a tool that never read its signal at once · interrupts a tool that read its signal but ignored the stop after the safety net · aborts the signal and records the generic result on a server shutdown · never lets a promise tool called from CodeMode take the execute call's stop — platform: Promise (JS) plugin tools

**`plugin/promise-tool.test.ts`**

- D Promise tool executors receive interruption through their AbortSignal · Promise tool throws become tool failures · Promise tools that rethrow a decline attach it to the tool failure · Promise tools infer codemode override schemas and pass info and codemode to hooks — platform: Promise (JS) plugin tools

**`plugin/supervisor-reload.test.ts`**

- D reloads helper-defined RPC methods ${scenario.name} ×3 · retains a ${mode} plugin change during initial activation ×2 · keeps the running generation when an updated local plugin fails to import · serializes the periodic refresh behind an in-flight reload · applies organization integration changes to running external plugins — deliberate: plugin.rs: compiled-in plugins (no plugin supervisor process)

**`plugin/supervisor.test.ts`**

- D reports a duplicate plugin ID as a failure without dropping the generation · activates the initial generation without waiting for the reload debounce · refreshes every 24 hours without adding an immediate reload · updates only packages written as name@latest when a newer version is out · reloads for a trigger published while the initial generation is activating · keeps policy enforcement and the Console connection through plugin removals: ${targets} ×2 · coalesces a burst of reload triggers after the initial generation into one activation · reloads all booted locations when sdk.register is called inside a location context · runs SDK plugin aisdk.sdk hooks before the dynamic npm fallback — deliberate: plugin.rs: compiled-in plugins (no plugin supervisor process)

</details>

<details><summary>Storage, state and bus (189 of 189)</summary>


**`bus-unpersisted.test.ts`**

- D does not encode payloads it does not store — storage-model: OpenCode's event bus

**`bus.test.ts`**

- D subscribes to multiple event definitions with a discriminated payload union · publishes events with the current location · omits ambient and explicit locations for global events · omits location when no location is available · publishes definition version · selects the latest durable definition independent of declaration order · publishes to typed and wildcard subscriptions · runs projectors inline · commits local operational state inside a new durable event transaction · rolls back the durable event and projector when the local commit fails · projects durable events without retaining their payloads · rejects local commit hooks on live-only events · runs projectors before publishing to streams · runs listeners inline after projectors · isolates observer defects after durable events commit · notifies global listeners only after a durable event is committed · preserves observer interruption · keeps live-only listener defects fail-fast · inserts durable event rows on publish · increments durable event seq per aggregate · publishes a durable batch atomically in provided order · rolls back every batch event when a projector fails · does not interleave a concurrent publish with batch notifications · replays durable aggregate events after a sequence and tails new events · catches durable aggregate events published during replay handoff · retains a durable wake committed while historical replay is paused · coalesces durable aggregate wakes while draining every committed event · omits live-only events from durable aggregate streams · uses custom sync aggregate field · replays durable events through projectors · replays compaction events stored before v2.1 with recent, reading them without it · replay inserts external event rows · replay rejects an envelope aggregate that differs from its payload without mutating the payload aggregate · replay defects on sequence mismatch · replay decodes synchronized transformed values before projection · dispatches durable projectors by exact event version · replay defects on unknown event type · claim fences replay owners · strict owner fences exact replay · exact replay claims an unowned aggregate · replay with owner claims an unowned sequence · replay claims an existing unowned sequence before fencing a different owner · strict replay rejects an owner conflict instead of silently skipping it · publishes accepted replay with its durable sequence and suppresses stale replay · rejects divergent stale replay without publishing it · rejects an event ID reused at another aggregate position · replay from a different owner leaves claimed sequence unchanged · claim updates the event sequence owner · remove clears durable event sequence · log without follow replays events and completes with a synced marker · log synced marker omits seq for an empty log and keeps the cursor otherwise · log with follow emits the synced marker at the replay-to-live boundary · log replays across configured read pages · log with follow emits events committed during replay after the synced marker — storage-model: OpenCode's event bus

**`database-drizzle.test.ts`**

- D selects rows through Effect-yieldable query builders · maps query failures with query, redacted params, and cause · suppresses statement spans · commits successful transactions · rolls back failed transactions · rolls back explicit transaction rollback · preserves failed transaction begin errors · supports returning and rejects empty update sets · supports function-valued update joins with runtime table columns · supports SQL-valued update joins — storage-model: drizzle ORM

**`database-migration.test.ts`**

- D defaults missing workspace names while preserving legacy workspace data · imports unnamed legacy Drizzle journal entries by their actual migration timestamps · rejects unknown legacy Drizzle journal timestamps instead of guessing completed migrations · serializes concurrent embedded initialization for one database path · bootstraps distinct databases without waiting on each other's lock · declared schema has no ungenerated migrations · bootstraps the current schema and records the migration registry · adds nullable attention state to existing sessions · drops the unused session_pending table and its indexes · rejects a non-empty database without a session table · bootstraps alongside underscore-prefixed embedder tables · applies generic migrations once and records their order · preserves previous V2 state through the current migration lineage · rejects previous V2 databases with V1-only session history · copies project directories into worktrees without removing the old table · moves plain live schedules to automations and drops the schedule table · imports legacy JSON credentials without changing the source file or existing credentials · imports legacy credentials when bootstrapping a fresh database · leaves legacy credentials out of in-memory databases · bootstraps a fresh database when the legacy credential file is malformed · skips legacy credential import when the source file is absent · moves Azure CLI connections saved as OAuth credentials to external credentials · rolls back a failed migration without recording it · suspends foreign keys outside migrations that rebuild referenced tables · imports an existing Drizzle migration journal once — storage-model: OpenCode's schema migrations

**`database.test.ts`**

- D creates the database and sidecars readable only by the owner · tightens an existing database and sidecars — storage-model: OpenCode's database layer

**`kv.test.ts`**

- D stores, replaces, and removes JSON values · scans prefixes in deterministic pages · defaults, normalizes, and caps scan limits — storage-model: OpenCode's KV table

**`shared-cache.test.ts`**

- D counts network errors, timeouts and 408, 429 or 5xx as transient, and nothing else · joins a running load, refreshes after the caller's interval, and forces without overlapping · retries transient failures, then keeps the last list or the failure until the next interval · never keeps interrupts or defects, and an interrupted caller leaves the load to the others · drops entries unread for 24 hours, never one still in use or loading — effect-internal: shared cache with retries

**`sqlite-bundle.test.ts`**

- D workerd conditions select the Durable Object driver and never bun:sqlite · %p conditions select the Node client, not bun:sqlite ×2 — platform: bundled SQLite driver

**`sqlite-interrupt.test.ts`**

- D bun client releases the connection when a queued transaction is interrupted · bun client releases the connection when a queued reserve is interrupted · workerd client releases the connection when a queued reserve is interrupted · workerd client releases the connection when a queued transaction is interrupted — storage-model: SQLite interruption in Effect

**`sqlite-node.test.ts`**

- D boots migrations, commits drizzle transactions and types constraint and storage errors · types constraint errors and opens read-only — platform: node SQLite driver

**`sqlite-workerd.test.ts`**

- D executes statements with bindings and maps rows to records · normalizes ArrayBuffer blob values to Uint8Array · withTransaction commits on success and rolls back on failure · nested withTransaction fails with SqlError · boots the full database layer with migrations over injected storage — platform: workerd SQLite driver

**`state-group.test.ts`**

- D detaches every registration of a failed group and refreshes every affected domain · restarts an outer candidate when a nested read disables one of its contributors · disables multiple failing groups once each before publishing a complete fold · does not disable a group for a notification failure after successful replay — storage-model: event-sourced state groups

**`state-replay.test.ts`**

- D matches a full fold across reads, registrations, removals and batched reloads · rebuilds all active callbacks once per change, reads for free otherwise, and never touches retained values — storage-model: event replay

**`state.test.ts`**

- D commits a transform atomically when its updater is interrupted · makes current state visible before notifying · runs transforms during every reload · disposes a transform once and rebuilds remaining state · batches notifications across domains · discards reloads and disposals after shutdown while still running cleanup · keeps teardown suppression separate from an enclosing live batch · notifies once per reload without waiting · publishes the initial projection and skips unchanged reloads · leaves a retained value untouched when later registrations rebuild · recreates the editor with every rebuild · rebuilds lazily on read and notifies even without a final read · replays every callback after each change outside a batch · rebuilds noncommutative edits after removing position ${removed} ×3 · keeps equal callback registrations independently disposable · does not evaluate a pending callback removed before the first read · invalidates on reload and reads new inputs · resamples a changing initial value even when there are no transforms · keeps the previous value when a rebuild throws and retries on the next read · recovers by disposing a failing callback without keeping its partial edits · cancels remaining observers when interrupted during the ${phase}, without rolling back registrations ×2 · shares nested live batches and does not retain an escaped batch · lets observers read other pending domains and register more edits · allows an observer to await a nested reload · attempts every domain notification and preserves both batch and observer failures · surfaces a failed reload notification to its caller and recovers on the next reload — storage-model: event-sourced state

**`storage-failure.test.ts`**

- D a transaction SQLite rolled back itself fails with the storage failure, not the rollback · a full database is a storage failure for a statement and for a query · an extended result code is a storage failure by its primary code · a constraint, a bad statement and other defects are not storage failures — storage-model: storage failure handling

**`v1-migration.test.ts`**

- D maps ordinary user text, agents, ignored fields, order, and timestamps · maps embedded files and deterministic placeholders without external IO · maps attachment-only source variants and uses resource URIs as unavailable labels · splits mixed synthetic content deterministically and preserves adjacency · preserves assistant content, model, usage, finish, snapshots, and marker filtering · normalizes every tool state · appends a legacy tool notice naming only the renamed or removed tools that were called · omits the legacy tool notice when no legacy tools were called or they precede a compaction · normalizes assistant errors and finish reasons · preserves every supported finish reason and omits an absent finish · filters subtasks, collapses compactions, and keeps contiguous sequences · serializes the retained compaction tail and preserves existing session selections · skips malformed rows, reports exact identifiers, and still backfills session aggregates · retains empty ordinary messages and omits failed compactions · orders equal-time rows by ID and keeps SQL and payload timestamps consistent · omits incomplete compactions and subtask assistants while retaining their aggregate usage · reports required and completed status and completes an empty database idempotently · yields while clearing stale events in batches · imports previous V2 sessions and messages containing apostrophes · imports previous V2 databases missing newer nullable columns · derives required status from the durable cursor · migrates $directory in $worktree ×8 · reassigns V1 sessions with missing projects and directory %s ×2 · attributes V1 global sessions to the V2 project of their directory · replaces projections containing apostrophes and checkpoints completion · rolls back one session atomically and resumes from the committed cursor · processes root, child, archived, empty, malformed-only, subtask-only, and incomplete-compaction sessions · serializes concurrent callers and migrates each session once — storage-model: migrating OpenCode v1 storage

</details>

<details><summary>Runtime, locations and filesystem (235 of 242)</summary>


**`app.test.ts`**

- D formats app metadata as a user agent — effect-internal: app layer

**`artifact.test.ts`**

- D stays disabled and hides artifact tools when experimental.artifacts is unset · registers custom providers, routes overrides, and runs the 4 CRUD tools — cloud: hosted artifacts

**`boot-work.test.ts`**

- D counts catalog lookups, Azure model edits and git spawns over a cold boot and one turn — effect-internal: boot work

**`effect-app-node-platform.test.ts`**

- D uses protocol-specific and standard proxy variables · respects no-proxy hosts, domains, ports, and wildcards · rejects redirects without forwarding authorization — effect-internal: Effect platform layer

**`effect-simplification-types.test.ts`**

- D Effect.as preserves the exact undefined success type — effect-internal: Effect type tests

**`effect/cross-spawn-spawner.test.ts`**

- D captures stdout · captures multiple lines · returns exit code · returns non-zero exit code · uses cwd when spawning commands · fails for invalid cwd · passes environment variables with extendEnv · passes multiple environment variables · captures both streams across backpressure · captures stderr output · captures both stdout and stderr · captures stdout via .all when no stderr · captures stderr via .all when no stdout · retains ${combined ? "combined" : "separate"} output after process completion ×2 · allows providing standard input to a command · finishes capture after ${mode} while a grandchild holds stdio ×2 · stops tracking a released child while a grandchild holds stdio · kills a running process · uses the configured kill signal when scope exits · forceKillAfter escalates for stubborn processes · isRunning reflects process state · fails for invalid command · rejects NUL characters without exposing Redacted values · reports a duplex write failure to a later reader without an uncaught error · pipes stdout of one command to stdin of another · three-stage pipeline · pipes stderr with { from: 'stderr' } · pipes combined output with { from: 'all' } · uses shell routing on Windows · runs cmd scripts with spaces on Windows without shell · keeps every line of a command run through an app execution alias — effect-internal: process spawner

**`effect/keyed-mutex.test.ts`**

- D serializes effects with the same key · allows different keys to proceed independently · removes an interrupted waiter without dropping the holder lock — effect-internal: keyed mutex

**`effect/layer-node/layer-node-types.test.ts`**

- D layer node type contracts compile — effect-internal: layer graph types

**`effect/layer-node/layer-node.test.ts`**

- D builds an untagged graph · exposes roots but hides transitive dependencies · replaces exact declarations, not sibling names or native layer identities · requires reachable unbound nodes to be replaced · replaces every use of a declaration with a stored closed-layer replacement · uses the last replacement and ignores unreachable unbound defaults and cycles · resolves target chains independently of replacement order and treats self-replacement as identity · rejects reachable replacement and dependency cycles · does not acquire replaced dependencies or unused replacement targets · mapLayer preserves dependency wiring and replacement traversal · memoizes shared wiring instead of expanding a diamond into a tree · preserves declared memo-service outputs rather than filtering them as build metadata · rejects one implementation wired to different effective dependencies in either memo domain · starts dependencies in parallel and nested group roots in order · LayerMap isolates builds and retains resources ${topLevel ? "with" : "without"} a top-level global owner ×2 — effect-internal: layer graph

**`effect/layer-node/node-build.test.ts`**

- D does not build a location service map when the graph does not require it · detects cycles through a replaced location service map · supplies the lazy map when only a replacement introduces the dependency · caller replacements override the lazy default without building any locations · shares top-level project even when the location service map is built first · returns a composed application layer — effect-internal: layer graph builds

**`effect/observability.test.ts`**

- D parses and decodes OTEL resource attributes · drops OTEL resource attributes when any entry is invalid · keeps built-in attributes when env values conflict · falls back to local logging when OTLP initialization fails · file logger appends concurrent runs with a run on every line · file logger flattens nested objects — effect-internal: OpenTelemetry wiring

**`environment.test.ts`**

- D follows symlinks without changing stat semantics · fails spawn with a typed location error · memory environment · local environment · GNU exec environment — client-server: Location environments (memory/local/GNU exec)

**`event-logger.test.ts`**

- D logs explicitly listed updated events — effect-internal: event logger

**`filesystem/file-watch.test.ts`**

- P shares one watch per file and resubscribes after unwatch — `tests/oc_watch.rs::oc_file_watch_shares_one_watch_per_file_and_resubscribes_after_unwatch`
- P expires a watch after 24 hours unless it is watched again — `tests/oc_watch.rs::oc_file_watch_expires_a_watch_after_24_hours_unless_watched_again`
- P evicts the watch closest to expiring when a location is full — `tests/oc_watch.rs::oc_file_watch_evicts_the_watch_closest_to_expiring_when_a_location_is_full`
- P publishes filesystem.changed on the watch's location — `tests/oc_watch.rs::oc_file_watch_publishes_filesystem_changed_on_the_watchs_location`
- P starts a new watch when the previous one ended — `tests/oc_watch.rs::oc_file_watch_starts_a_new_watch_when_the_previous_one_ended`
- P starts a new watch when the previous one failed — `tests/oc_watch.rs::oc_file_watch_starts_a_new_watch_when_the_previous_one_failed` (adapted: the failure is a panicking native acquisition)
- P fails watch requests — `tests/oc_watch.rs::oc_file_watch_disabled_fails_watch_requests`
- P FileWatch native — `tests/oc_watch.rs::oc_file_watch_native_reports_in_place_edits_and_atomic_saves`

**`filesystem/filesystem.test.ts`**

- D returns true for directories · returns false for files · returns false for non-existent paths · returns true for files · returns false for directories · returns file contents when file exists · returns undefined for missing file (NotFound) · round-trips JSON data · fails invalid JSON through the error channel · creates nested directories · is idempotent · creates parent directories if missing · writes directly when parent exists · writes Uint8Array content · finds target in start directory · finds target in parent directories · returns empty array when not found · finds multiple targets walking up · stops at the first match when requested · finds files matching pattern · supports absolute paths · matches patterns · finds files walking up directories · exists works · remove works · mimeType returns correct types · contains checks path containment · overlaps detects overlapping paths — effect-internal: OpenCode's FSUtil service

**`filesystem/location-watcher-policy.test.ts`**

- P reads batched registrations and disposals before notifying observers — `tests/oc_watch.rs::oc_policy_reads_batched_registrations_and_disposals_before_notifying_observers`
- P passes the latest policy to later observers after a reentrant registration — `tests/oc_watch.rs::oc_policy_passes_the_latest_policy_to_later_observers_after_a_reentrant_registration`

**`filesystem/watcher.test.ts`**

- P ends the stream without logging an error when the watched directory is missing — `tests/oc_watch.rs::oc_watcher_ends_the_stream_without_logging_an_error_when_the_directory_is_missing`
- P still logs an error when watching fails for a reason other than ENOENT — `tests/oc_watch.rs::oc_watcher_still_logs_an_error_when_watching_fails_for_another_reason`
- P signals readiness after acquisition and buffers updates published by the ready callback — `tests/oc_watch.rs::oc_watcher_signals_readiness_after_acquisition_and_buffers_updates_published_by_the_ready_callback`
- P does not signal readiness for an unavailable native watch — `tests/oc_watch.rs::oc_watcher_does_not_signal_readiness_for_an_unavailable_native_watch`
- P watches only named immediate entries, including missing directories — `tests/oc_watch.rs::oc_watcher_watches_only_named_immediate_entries_including_missing_directories`
- P interrupting a consumer interrupts a pending acquisition — `tests/oc_watch.rs::oc_watcher_interrupting_a_consumer_interrupts_a_pending_acquisition` (adapted: interrupting is aborting the consumer's task)
- P shares equivalent entry sets and releases exactly once after the final consumer — `tests/oc_watch.rs::oc_watcher_shares_equivalent_entry_sets_and_releases_exactly_once_after_the_final_consumer`
- P scope shutdown releases an active subscription exactly once — `tests/oc_watch.rs::oc_watcher_shutdown_releases_an_active_subscription_exactly_once` (adapted: scope shutdown is Watcher::shutdown)
- P watches only exact Git branch metadata — `tests/oc_watch.rs::oc_location_watcher_watches_only_exact_git_branch_metadata`
- P watches only exact Hg branch metadata — `tests/oc_watch.rs::oc_location_watcher_watches_only_exact_hg_branch_metadata`
- P reconciles config without duplicate subscriptions — `tests/oc_watch.rs::oc_location_watcher_reconciles_config_without_duplicate_subscriptions` (adapted: config entries are a transform replayed on reload)
- P uses the policy changed while target discovery was suspended — `tests/oc_watch.rs::oc_location_watcher_uses_the_policy_changed_while_target_discovery_was_suspended` (adapted: discovery is suspended through an injected discover function)
- P does not start before configured policy is ready — `tests/oc_watch.rs::oc_location_watcher_does_not_start_before_configured_policy_is_ready` (adapted: the policy is seeded by the activation future)
- P LocationWatcher — `tests/oc_watch.rs::oc_location_watcher_native_publishes_git_head_events` (with the symlinked .git, .hg/branch, exact-target and missing-directory tests beside it; adapted: waits for the change, as FSEvents may report a recent creation in the same event)

**`instance-vanilla.test.ts`**

- D boots without filesystem config discovery — effect-internal: instance bootstrap

**`location-activity.test.ts`**

- D keeps a location with ${count} executions waiting on a question until they go idle ×2 · keeps a location with a running background job owned by its ${owner} until the job finishes ×3 · honors experimental.location_idle_timeout = ${JSON.stringify(timeout)} ×2 · refreshes inactivity deadline for session events with non-canonical location refs — client-server: Locations

**`location-filesystem.test.ts`**

- D reads text and binary files · lists direct children · skips host canonicalization for workspace locations at boot · classifies directory initialization failure ${input.code} ×3 · lists parents and siblings with paths relative to the current location · canonicalizes local symlinked directories · rejects lexical escapes · allows listing through an external symlink without allowing file reads — client-server: Locations

**`location-layer.test.ts`**

- D retries after repairing ${failure}${invalidate ? " with explicit invalidation" : ""} ×4 · keeps the repaired graph after concurrent ${failure} failures release ×2 · closes every location concurrently on shutdown, including borrowed ones · does not refresh lifetime from inferred Session routing · refreshes lifetime from Session events only · warns when a borrower ${holder === "stays" ? "still holds" : "lets go of"} an invalidated location ×2 · retries a location after its missing directory is recreated · keeps a workspace location admitted when its directory does not exist locally · routes located events only to their location · reuses cached services for constructed and decoded location refs · normalizes equivalent refs to one cached location graph · isolates provider state by location · allows the built-in Plan agent to be disabled · rejects an unavailable selected model during location model resolution · explains replacements for unavailable legacy provider models · preserves the selected catalog identity when the package model id differs · lets plugins serve a request with another model from the same provider — client-server: Locations

**`location-missing-project.test.ts`**

- D does not persist a project row when a local directory is missing · does not unarchive a project when its removed directory is requested — client-server: Locations

**`location-retain.test.ts`**

- D idle sweep keeps a retained location and evicts it once released — client-server: Locations

**`location.test.ts`**

- D resolves the current project and vcs information — client-server: Locations

**`logging.test.ts`**

- D uses a local-specific log file for local installs · keeps non-local installs on the default log file — effect-internal: logger layer

**`preload.test.ts`**

- D disables public npm security audits · isolates global home and XDG roots — effect-internal: module preload

**`project-activate.test.ts`**

- D stays lazy and skips spans and forks on throttled repeats · ignores non-matching bus traffic and throttles durable lifecycle updates — client-server: project registry

**`project-icon.test.ts`**

- D discovers the expected icon with a ${kind} ×3 · resolves in the background, preserves overrides, and only writes changed content · discovers ${fixture.name} without a database ×6 · Composer backgrounds stay inert and malformed, oversized or escaping manifests fall through · returns undefined without recording span failures when the directory is missing — client-server: project icons for the desktop/web app

**`rpc-handler-errors.test.ts`**

- D recovers from ${method} through the typed rpc.internal failure ×5 — client-server: RPC handler errors

**`shared-schema.test.ts`**

- D Core reuses the canonical shared schemas · shared record schemas construct and decode plain objects — effect-internal: shared schema

**`util/effect-flock.test.ts`**

- D acquire and release via scoped Effect · supports an acquisition timeout · withLock data-first · withLock pipeable · writes owner metadata · advances the heartbeat timestamp while the lock is held · breaks stale lock dirs · recovers from stale breaker · detects compromise when lock dir removed · detects token mismatch · fails on unwritable lock roots · enforces mutual exclusion under process contention · recovers after a crashed lock owner — effect-internal: file locks (CodeSmith uses the session store's leases)

**`util/error-summary.test.ts`**

- D keeps the message and classification of each error in the cause chain · omits the stack and empty messages — effect-internal: error cause chains

**`util/flock.test.ts`**

- D enforces mutual exclusion under process contention · times out while waiting when lock is still healthy · recovers after a crashed lock owner · breaks stale lock dirs when heartbeat is missing · recovers when a stale breaker claim was left behind · fails clearly if lock dir is removed while held · writes owner metadata while lock is held · supports acquire with await using · refuses token mismatch release and recovers from stale · fails clearly on unwritable lock roots — effect-internal: file locks (CodeSmith uses the session store's leases)

**`util/fs-util.test.ts`**

- D treats ${input.code} as an unresolvable, unreadable path ×2 · keeps other realPath failures fatal — effect-internal: FSUtil realPath

**`util/which.test.ts`**

- D uses PATHEXT on windows · finds a Microsoft Store app execution alias on windows · uses Windows Path casing fallback — platform: Windows PATHEXT / aliases

**`wellknown.test.ts`**

- D loads embedded and remote configuration · persists sources in one KV value and skips unreachable ones · refreshes changed manifests — cloud: well-known remote config

</details>

