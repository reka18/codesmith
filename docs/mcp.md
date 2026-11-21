# MCP servers

CodeSmith is a [Model Context Protocol](https://modelcontextprotocol.io)
client. Tools, resources, prompts and events from MCP servers become
available to the agent. The config shape follows OpenCode's.

- [Configuring servers](#configuring-servers)
- [Permissions](#permissions)
- [Code Mode](#code-mode)
- [OAuth](#oauth)
- [Resources](#resources)
- [Prompts](#prompts)
- [Elicitation](#elicitation)
- [Servers a client attaches](#servers-a-client-attaches)
- [Instructions, notifications and events](#instructions-notifications-and-events)

## Configuring servers

```jsonc
{
  "mcp": {
    "servers": {
      "fs": {
        "type": "local",
        "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."],
        "environment": { "LOG_LEVEL": "warn" },
        "cwd": "tools"
      },
      "docs": {
        "type": "remote",
        "url": "http://127.0.0.1:8080/mcp",
        "headers": { "Authorization": "Bearer {env:DOCS_TOKEN}" }
      },
      "slow": { "type": "local", "command": ["./slow-server"], "timeout": { "startup": 60000 } },
      "old": { "type": "local", "command": ["old-server"], "disabled": true }
    },
    "timeout": { "execution": 600000 }
  }
}
```

- **`local`** servers are programs CodeSmith starts and talks to over stdio.
  Their stderr goes to the data folder's `log/mcp-<name>.log`. Fields:
  `command`, `environment`, `cwd` (relative to the project root, which is
  also the default), `disabled`, `codemode`, `timeout`.
- **`remote`** servers are reached over streamable HTTP. Fields: `url`,
  `headers`, `oauth`, `disabled`, `codemode`, `timeout`.
- **`timeout`**, per server or for all servers under `mcp.timeout`, in
  milliseconds: `startup` (connect and initialize, default 30 s), `catalog`
  (listing tools, resources and prompts, default 30 s) and `execution` (one
  tool call, default 5 minutes).

A single message from a server is limited to 16 MiB, over stdio (one line)
as over HTTP. A local server that sends a longer line is treated as exited:
its pending requests fail with that reason.

A tool call that times out is cancelled on the server
(`notifications/cancelled`). If a local server's process exits, its tools
stop being offered and calls to them say the server exited. CodeSmith
starts it again once, on the next call or before the model's next step;
if that fails too, the server stays unavailable until the session is
restarted. (OpenCode marks such a server failed without restarting it.)

`codesmith mcp list` connects to each server and shows its status and tools.

Because a project's `codesmith.json` can add MCP servers, and local servers
are programs that run on your machine, read a repository's config before you
use CodeSmith in it. See
[project config is trusted](configuration.md#files-and-precedence).

## Permissions

The model sees each tool as `<server>_<tool>`, and calls go through
permissions under that name with resource `*`:

```jsonc
{
  "permissions": [
    { "action": "github_*", "resource": "*", "effect": "ask" },
    { "action": "github_delete_*", "resource": "*", "effect": "deny" }
  ]
}
```

Names keep only `[A-Za-z0-9_-]`, so two tools can end up with the same name
(server `x` with tool `y_z`, and server `x_y` with tool `z`, are both
`x_y_z`). Then neither gets it: each is named with a suffix from a hash of
its server and tool, such as `x_y_z_1a2b3c4d`, stable across restarts, and
its description says which server and tool it is. A rule denying the
shared name still denies both; one allowing it (or asking) applies to
neither, so use the suffixed names for those (`codesmith mcp list` shows
them) or rename a server.

Providers accept tool names of at most 64 characters. A longer name is cut
and given a hash suffix the same way (stable, and distinct from other cut
names), and so is a suffixed name.

A tool denied for `*` is hidden from the model. In Code Mode, every call a
program makes is checked the same way as a direct call.

[Automations](automations.md#permissions-for-unattended-runs) run with every
MCP tool denied, including the resource tools and tools reached through
Code Mode. Allow the ones a run needs under `automation.permissions`, for
example `{"action": "github_create_issue", "resource": "*", "effect": "allow"}`.

## Code Mode

As in OpenCode, MCP tools are offered through Code Mode by default. Instead
of calling them one at a time, the model writes a short JavaScript program in
a single `execute` call, and gets back the result and any `console.log`
output:

```js
const issues = await tools.github.list_issues({ repo: "acme/api", state: "open" });
return issues.filter(i => i.labels.some(l => l.name === "bug")).map(i => i.title);
```

The tools are listed in the system prompt with TypeScript-style signatures.
When a server's tools come, go or change mid-session, the model is told what
changed in a short update at that point in the conversation (new and removed
tools, changed signatures), and the system prompt stays as it was; see
[instructions that change](configuration.md#when-instructions-change).

Programs run in [Boa](https://boajs.dev), a JavaScript engine written in
Rust. They have no filesystem, process or timer access of their own: the
only ways out are the listed tools and the [`fetch`](#fetch) global, which
goes through the same checks as the `webfetch` tool. Limits:

- at most 50 tool calls and three minutes per program, with bounded loops and
  recursion;
- each program runs in a child `codesmith` process that is killed at the
  timeout, with a CPU-time limit, a 512 MiB resident-memory limit, and on
  Linux an address-space limit;
- single buffers, typed arrays and repeated strings are capped, so
  `new ArrayBuffer(2**30)` fails with a `RangeError`.

(A program that embeds `codesmith-core` without calling
`codemode::child_main_if_requested` runs programs on a thread instead, where
only the loop, recursion and size caps apply.)

While a program runs, its `execute` call reports a row for each tool call
it makes, as OpenCode does (the server's `tool_progress`): at most one
update per 100 ms, and a failed program's latest rows before its result.

Images in tool results reach the model as images when tools are called
directly. In Code Mode, whose results are JSON for the program, images are
replaced by a note.

Set `"codemode": false` on a server, or at the top level of the config, to
offer its tools as direct calls instead. That suits models that write
JavaScript poorly. An agent whose rules deny `execute` gets every MCP tool
as a direct call too.

### `fetch`

As in OpenCode, programs have a `fetch` global for HTTP requests, with the
`webfetch` tool's checks:

```js
const res = await fetch("https://api.example.com/status", { headers: { accept: "application/json" } });
return res.ok ? await res.json() : res.status;
```

- The URL, without its fragment, must be `http(s)://` with no user name or
  password in it, and asks the `webfetch` permission (a rule for
  `webfetch` covers `fetch` too). "Always" saves the URL's origin, not every
  URL as OpenCode does.
- Redirects are followed one hop at a time, at most 10; each target must
  be `http(s)` without credentials, and is checked against `webfetch` deny
  rules. A redirect to another origin drops the `Authorization`, `Cookie`
  and `Proxy-Authorization` headers; a 303, or a 301 or 302 after a POST,
  becomes a GET without a body.
- CodeSmith adds no credentials and keeps no cookies. Its own User-Agent is
  `codesmith`.
- A response is read whole, at most 5 MiB, within 30 seconds; `fetch`
  resolves to a `Response` with `status`, `ok`, `headers`, `url`,
  `redirected`, `text()`, `json()`, `bytes()`, `arrayBuffer()` and a
  one-chunk `body` reader. Bodies are strings, bytes, `URLSearchParams` or
  `ArrayBuffer`s.
- Each request counts as one of the program's 50 calls.

### OpenAPI tools

An OpenAPI 3.x spec named in config becomes Code Mode tools, one for each
operation, as OpenCode's `OpenAPI.fromSpec` makes them: `tools.<name>.<operationId>(input)`,
where the input holds the path, query and header parameters and the JSON
body's fields, and the result is the decoded response.

```jsonc
"openapi": {
  "billing": {
    "spec": "specs/billing.json",          // a file (relative to the project), a URL, or the document itself
    "base_url": "https://billing.example.com/v2",
    "auth": { "BearerAuth": { "type": "bearer", "token": "{env:BILLING_TOKEN}" } },
    "headers": { "X-Tenant": "acme" },
    "timeout": 60                          // seconds a request may take
  }
}
```

```js
const invoice = await tools.billing.invoices.get({ invoiceId: "inv_42" });
```

- Specs come only from config, never from the model. They must be JSON
  (YAML is not read), at most 20 MiB, and are loaded once per process (a
  failed load is tried again after a minute and reported once). Swagger
  2.0, cookie parameters and authentication, binary bodies and responses,
  SSE and WebSocket operations are left out, as in OpenCode.
- Each call asks the `openapi` permission for `<METHOD> <url>`, for
  example `GET https://billing.example.com/v2/invoices/inv_42`; "always"
  saves that method on that origin (`GET https://billing.example.com/*`).
  An operation whose URL a rule denies is not listed.
- Credentials (`auth`, keyed by the spec's security scheme names, with
  `type` `bearer`, `basic`, `api_key` or `header`) and `headers` come from
  config, usually as `{env:VAR}`, and need `base_url`: they are sent only to
  its origin. An unset variable leaves the credential out, so the next
  alternative the spec allows is tried. Credentials are applied last, so a
  header parameter cannot replace them.
- Redirects are followed one hop at a time, at most 10, each target
  checked against `openapi` deny rules. Once a redirect leaves the
  origin, credentials and configured headers are dropped for the rest of
  the chain, even if it comes back.
- Responses are capped at 10 MiB. A non-2xx status fails the call with the
  start of the response, with any credential in it replaced by
  `[redacted]`; secrets never appear in debug output or error messages.
- `"enabled": false` leaves an API out. Its tools take no path an MCP tool
  already has.

### Built-in tools in Code Mode: `full_codemode`

`"full_codemode": true` (off by default; OpenCode's `full_codemode` flag)
moves the built-in tools as well, per model, as OpenCode places them:

- `glob`, `grep` and `webfetch` are hidden for every model: programs search
  through `shell` and fetch with the [`fetch`](#fetch) global. A model
  offered no `execute` (no Code Mode tools at all) keeps `webfetch`, since
  it has nowhere to call `fetch` from (OpenCode hides it anyway).
- GPT and Claude models (by provider `anthropic`, or a model id or catalog
  family naming `gpt` or `claude`) call `read`, `write`, `edit`, `patch`,
  `shell` and `websearch` from inside `execute`, as `tools.read({...})` and
  so on; they are always listed in the catalog. Each call goes through the
  same permission checks as a direct one.
- Every other model keeps them as direct tools.

Without `execute` (an agent whose rules deny it), nothing moves and nothing
is hidden.

## OAuth

Remote servers that require OAuth show as needing authorization in
`codesmith mcp list`. Log in with:

```sh
codesmith mcp auth docs      # opens the browser
codesmith mcp logout docs    # forget the tokens
```

Without a name, `codesmith mcp auth` lists the remote servers that take an
OAuth sign-in (marking those signed in, and those whose tokens expired with
no way to refresh them) and asks for one; without a terminal it needs the
name.

This follows the MCP authorization spec: discovery, dynamic client
registration and PKCE, with the callback on `127.0.0.1:19876`. Tokens are
saved in the data folder's `mcp-auth.json` (readable only by you), are only
ever sent to the URL they were issued for, and are refreshed automatically.

To use a pre-registered client, or a different callback port:

```jsonc
"docs": { "type": "remote", "url": "https://mcp.example.com/mcp",
          "oauth": { "clientId": "...", "clientSecret": "{env:DOCS_SECRET}", "scope": "read", "port": 19876 } }
```

`"oauth": false` turns OAuth off for a server.

## Resources

When a connected server has resources, the model gets two tools:

- `list_mcp_resources` lists the resources and URI templates of one server,
  or all of them. It is checked with the server name as the resource.
- `read_mcp_resource` reads one URI. It is checked as `server:uri`.

```jsonc
{ "action": "read_mcp_resource", "resource": "docs:secret://*", "effect": "deny" }
```

Images, PDFs and videos embedded in a resource or tool result are sent to the
model as media, fitted to the same [limits](tools.md#images-and-pdfs) as
`read` (video only to models that take it).

## Prompts

A server's prompts become slash commands named `/<server>:<prompt>`.
Arguments fill the prompt's declared arguments in order. Quote an argument
that contains spaces:

```text
/docs:summarize "getting started" short
```

`codesmith commands` lists them with their argument names.

## Elicitation

A server can ask you for input while one of its tools runs
(`elicitation/create`), as OpenCode lets it. The request is asked in the
question UI of the session whose call it came from: the UI's question
dialog, a `question` event on the [server's](local-server.md) turn stream
(answered with `POST /sessions/{id}/questions/{qid}`), or ACP's question
requests.

- A form request asks each field: free text, a number, Yes or No, a choice
  or several. A field that depends on another's answer is asked once it
  applies. An empty form is accepted without asking.
- A URL request asks you to open the URL and confirm with Done; CodeSmith
  shows the URL and never opens it. Only `https` URLs are shown; any other
  is declined without asking. A server that announces the request complete
  (`notifications/elicitation/complete`) settles it without you.
- A server may have 4 requests waiting on you at once and start 10 a
  minute; more are declined without asking.
- Dismissing the question declines it, and so does the server giving up
  (`notifications/cancelled`). Where nobody can answer (`codesmith run
  --auto`, automations), the request is declined at once.

## Servers a client attaches

Besides the configured servers, a client embedding CodeSmith's core can
attach an MCP server it hosts itself over a connection it opened
(`Mcp::attach`, newline-delimited JSON-RPC), scoped to whoever holds the
attachment: dropping it, or the connection closing, detaches it. As in
OpenCode:

- Its tools are offered as `<name>_<tool>` (in Code Mode by default) and
  go through the same permission rules, plugin hooks and output handling as
  a configured server's. Each call carries the session's ID in
  `_meta["codesmith/sessionId"]`.
- Attaching again under the same name stacks: the newest connection serves
  the name's tools, and when it closes the one below takes over, listing
  its tools again (and keeping those it had if that fails). A connection
  below the top that closes changes nothing visible.
- An attached connection's tools shadow a configured server's tools of the
  same name, and it never shows in `codesmith mcp list` or the MCP dialog.

A configured server can also be added, removed or reconnected while the
session runs (`Mcp::add`, `Mcp::remove`, `Mcp::restart`); the MCP dialog
reconnects a server after its sign-in.

## Instructions, notifications and events

- A server's `instructions` (from its `initialize` reply) join the system
  prompt in an `<mcp_instructions>` block, but only while the session can
  reach at least one of its tools. Instructions that change, appear or go
  away later reach the model as an update.
- `tools/list_changed` and `prompts/list_changed` notifications refresh the
  lists before the next model request. `notifications/message` log lines go
  to the server's log file. Over HTTP, notifications and server requests
  inside a response's SSE stream are handled too.
- Servers that offer **MCP Events** (the draft
  `io.modelcontextprotocol/events` extension) can trigger
  [automations](automations.md#mcp-events): `--event server:event` runs once
  for each event the server pushes.
