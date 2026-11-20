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

A tool denied for `*` is hidden from the model. In Code Mode, every call a
program makes is checked the same way as a direct call.

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

Programs run in [Boa](https://boajs.dev), a JavaScript engine written in
Rust. They have no filesystem, network, process or timer access of their
own: the only way out is the listed tools. Limits:

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

Images in tool results reach the model as images when tools are called
directly. In Code Mode, whose results are JSON for the program, images are
replaced by a note.

Set `"codemode": false` on a server, or at the top level of the config, to
offer its tools as direct calls instead. That suits models that write
JavaScript poorly.

## OAuth

Remote servers that require OAuth show as needing authorization in
`codesmith mcp list`. Log in with:

```sh
codesmith mcp auth docs      # opens the browser
codesmith mcp logout docs    # forget the tokens
```

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

Images and PDFs embedded in a resource or tool result are sent to the model as
media, fitted to the same [limits](tools.md#images-and-pdfs) as `read`.

## Prompts

A server's prompts become slash commands named `/<server>:<prompt>`.
Arguments fill the prompt's declared arguments in order. Quote an argument
that contains spaces:

```text
/docs:summarize "getting started" short
```

`codesmith commands` lists them with their argument names.

## Instructions, notifications and events

- A server's `instructions` (from its `initialize` reply) join the system
  prompt in an `<mcp_instructions>` block, but only while the session can
  reach at least one of its tools.
- `tools/list_changed` and `prompts/list_changed` notifications refresh the
  lists before the next model request. `notifications/message` log lines go
  to the server's log file. Over HTTP, notifications and server requests
  inside a response's SSE stream are handled too.
- Servers that offer **MCP Events** (the draft
  `io.modelcontextprotocol/events` extension) can trigger
  [automations](automations.md#mcp-events): `--event server:event` runs once
  for each event the server pushes.
