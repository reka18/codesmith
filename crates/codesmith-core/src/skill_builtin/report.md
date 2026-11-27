# Report a CodeSmith Issue

Use this skill when the user wants to report a CodeSmith issue or bug. Turn
the user's problem into a useful GitHub issue with standard diagnostics plus
the context needed to reproduce and resolve it. (Adapted from OpenCode's
`report` skill, MIT.)

## Workflow

1. Collect the standard diagnostics below.
2. Ask only for missing details needed to reproduce or understand impact.
3. Draft the issue in the standard format below.
4. Publish it with GitHub CLI after the user confirms the title and body.

Do not publish an issue without the user's confirmation. If GitHub CLI is
not installed or not signed in, explain the blocker and give the user the
exact title and body.

## Standard Diagnostics

- CodeSmith version: run `codesmith --version`.
- Operating system: run `uname -a`.
- Terminal: `$TERM`, `$TERM_PROGRAM`, `$COLORTERM`, and any terminal app the
  user names.
- Shell: `$SHELL`.
- Configuration: `codesmith debug config` (redact keys and private URLs),
  and which config files exist: `~/.config/codesmith/codesmith.json[c]` and
  the per-repo files in `~/.config/codesmith/projects/` that match the
  project (`codesmith debug config` lists them).
- Model: the provider and model in use.

If a diagnostic command fails, write `Unavailable` with the reason instead
of guessing.

## User-Specific Context

- What the user was trying to do, what happened, and what they expected.
- Reproduction steps, minimal and numbered.
- Relevant logs (see `codesmith debug paths` for the log folder), errors,
  terminal output or config snippets.
- Whether it happens every time, sometimes, or once.
- Recent changes: an update, a config change, another terminal.
- Workarounds tried.

Never paste secrets. Redact tokens, API keys, private URLs, usernames and
confidential project data unless the user says it is safe.

## Issue Format

```markdown
## Summary

<!-- One or two sentences: the bug and its impact. -->

## Environment

- CodeSmith version: <!-- value or Unavailable: reason -->
- OS: <!-- value or Unavailable: reason -->
- Terminal: <!-- value or Unavailable: reason -->
- Shell: <!-- value or Unavailable: reason -->
- Model: <!-- provider/model -->

## Reproduction

1. <!-- step -->

## Expected Behavior

## Actual Behavior

## Additional Context
```

Keep the title short and searchable, as `<area>: <specific failure>`, for
example `tui: model picker loses the filter after Escape`.

## Publishing With GitHub CLI

Write the body to a temporary file first, so newlines and code fences
survive, then:

```sh
gh issue create --repo reka18/codesmith --title "<title>" --body-file <file>
```

Add a label only when it is obvious. Report the created issue's URL and any
diagnostics that were unavailable.
