# CodeSmith user guide

New here? Start with [Getting started](getting-started.md).

## Using CodeSmith

- [Getting started](getting-started.md): install, choose a model, first
  session.
- [The interactive UI](interactive-ui.md): keys, slash commands, pickers,
  `@` mentions, attachments, dialogs.
- [Command line](cli.md): every subcommand and flag.
- [Sessions](sessions.md): continuing, undo and redo, export and import,
  compaction, statistics, worktrees.
- [Tools](tools.md): what the agent can do, including images and PDFs.

## Configuring it

- [Configuration](configuration.md): files and precedence, every key,
  providers and models (options and variants), instructions, environment
  variables.
- [Permissions](permissions.md): the rule model, shell checks, paths outside
  the project, defaults per agent and saved approvals.
- [Sandbox](sandbox.md): the operating-system sandbox under the agent's shell
  commands, its settings, and what to do when it refuses something.
- [Agents and subagents](agents-and-subagents.md): built-in agents, your own
  agents, delegation.
- [Skills and commands](skills-and-commands.md): reusable instructions and
  prompt templates.
- [Language servers and formatters](language-servers-and-formatters.md).

## Extending and integrating

- [MCP servers](mcp.md): tools, Code Mode, OAuth, resources, prompts, events.
- [Code review](code-review.md): `/calibrate` and `/review`.
- [Automations](automations.md): scheduled and event-driven runs.
- [The local bus](bus.md): sessions on one machine asking each other for
  help.
- [tailbus](tailbus.md): reaching a CodeSmith session from other agents on
  your tailnet, or running CodeSmith as a tailbus bridge's agent;
  `codesmith bridge init` sets either one up.
- [Remote access](remote.md): pairing a phone with a session (in
  development), and its [protocol](remote-protocol.md).
- [Editors (ACP)](editors-acp.md): Zed and other ACP clients.
- [Local server](local-server.md): the HTTP API.
- [Plugins](plugins.md): native Rust plugins.

## When something goes wrong

- [Troubleshooting](troubleshooting.md).

## For maintainers

- [Releasing](../RELEASING.md).
- [Provider parity with OpenCode](../crates/codesmith-ai/PARITY.md).
