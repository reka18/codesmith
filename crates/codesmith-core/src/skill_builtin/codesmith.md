# CodeSmith

Use this guide as the starting point for questions about CodeSmith itself.
It is an index: before answering, fetch the page for the topic from the
documentation and use it as the source of truth, following its links when
the question needs more detail. Run `codesmith --help` or `codesmith
<command> --help` for the exact flags of the installed version, and
`codesmith debug config` to see the configuration CodeSmith resolved.

CodeSmith is a Rust port of OpenCode (MIT): most concepts carry over, but
its documentation, not OpenCode's, describes how CodeSmith behaves.

Documentation index: <https://github.com/reka18/codesmith/blob/main/docs/README.md>.

## Topics

- [getting-started](https://github.com/reka18/codesmith/blob/main/docs/getting-started.md): installing, first run, models and keys
- [cli](https://github.com/reka18/codesmith/blob/main/docs/cli.md): every command and flag (`run`, `generate`, `session`, `stats`, `worktree`, `mcp`, `auth`, ...)
- [interactive-ui](https://github.com/reka18/codesmith/blob/main/docs/interactive-ui.md): the terminal UI: keys, slash commands, dialogs, themes
- [configuration](https://github.com/reka18/codesmith/blob/main/docs/configuration.md): `codesmith.json`: providers and models, variants, media, compaction, the full key reference
- [permissions](https://github.com/reka18/codesmith/blob/main/docs/permissions.md): rules, defaults, policies, session rules and saved approvals
- [agents-and-subagents](https://github.com/reka18/codesmith/blob/main/docs/agents-and-subagents.md): built-in and custom agents, subagents
- [skills-and-commands](https://github.com/reka18/codesmith/blob/main/docs/skills-and-commands.md): skills (`SKILL.md`) and custom commands
- [tools](https://github.com/reka18/codesmith/blob/main/docs/tools.md): the built-in tools, images and PDFs, Code Mode
- [mcp](https://github.com/reka18/codesmith/blob/main/docs/mcp.md): MCP servers, OAuth, prompts and resources
- [sessions](https://github.com/reka18/codesmith/blob/main/docs/sessions.md): sessions, forks, archiving, undo, worktrees
- [automations](https://github.com/reka18/codesmith/blob/main/docs/automations.md): scheduled and event-driven unattended runs
- [code-review](https://github.com/reka18/codesmith/blob/main/docs/code-review.md): `/review` and `codesmith review`
- [language-servers-and-formatters](https://github.com/reka18/codesmith/blob/main/docs/language-servers-and-formatters.md): LSP and formatters
- [editors-acp](https://github.com/reka18/codesmith/blob/main/docs/editors-acp.md): editors over the Agent Client Protocol
- [local-server](https://github.com/reka18/codesmith/blob/main/docs/local-server.md): `codesmith serve`: the HTTP API
- [plugins](https://github.com/reka18/codesmith/blob/main/docs/plugins.md): native plugins and their hooks
- [troubleshooting](https://github.com/reka18/codesmith/blob/main/docs/troubleshooting.md): logs, debug commands, common problems

## Answering

- Quote config keys and commands exactly as the page gives them.
- When the page and the installed version disagree, trust `--help` and
  `codesmith debug config`, and say so.
- Say when the documentation does not cover something rather than guessing.
