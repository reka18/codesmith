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
  the project, defaults per agent, and why project config is trusted.
- [Agents and subagents](agents-and-subagents.md): built-in agents, your own
  agents, delegation.
- [Skills and commands](skills-and-commands.md): reusable instructions and
  prompt templates.
- [Language servers and formatters](language-servers-and-formatters.md).

## Extending and integrating

- [MCP servers](mcp.md): tools, Code Mode, OAuth, resources, prompts, events.
- [Code review](code-review.md): `/calibrate` and `/review`.
- [Automations](automations.md): scheduled and event-driven runs.
- [Editors (ACP)](editors-acp.md): Zed and other ACP clients.
- [Local server](local-server.md): the HTTP API.
- [Plugins](plugins.md): native Rust plugins.

## When something goes wrong

- [Troubleshooting](troubleshooting.md).

## For maintainers

- [Releasing](../RELEASING.md).
- [Provider parity with OpenCode](../crates/codesmith-ai/PARITY.md).
