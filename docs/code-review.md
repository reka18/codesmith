# Code review

Reviews are tuned to the codebase in two steps. First `/calibrate` chooses
what matters in this repository, and then `/review` checks changes against
it with independent reviewers and verifiers. Both run read-only.

## `/calibrate`

`/calibrate` (or `codesmith calibrate`) studies the whole repository and
chooses its review dimensions:

1. CodeSmith gathers facts from git itself: the mix of files, how files
   group into areas, the most-changed files, and which files and commits bug
   fixes have touched.
2. A read-only agent surveys each area, in parallel.
3. A final agent turns the facts and surveys into 4–12 dimensions specific
   to this code: its invariants, cross-module contracts, formats,
   concurrency, security boundaries, and the kinds of bugs its history shows.

Each dimension has the paths it covers, concrete checks, and the evidence
behind it. They are saved to `.codesmith/review.json`. Edit the file as you
like, and commit it so everyone reviews the same way.

## `/review`

```sh
/review                  # uncommitted changes
/review HEAD~1           # a commit
/review main..feature    # a range
/review feature          # a branch
/review #12              # a pull request (needs gh)
codesmith review #12 --comment
```

`/review [target]` (or `codesmith review`) works like this:

1. It runs one reviewer per dimension that covers the changed files, plus two
   standing ones: correctness, and compliance with `AGENTS.md` and
   `CLAUDE.md` (when the changed files have such instructions). They run in
   parallel, read-only, as the `review` agent.
2. Reviewers report only high-signal issues they can show are real.
3. Findings that describe the same problem are merged.
4. Each finding goes to its own verifier, which tries to refute it.
5. Only findings confirmed at 80% confidence or more are reported, most
   severe first.

In a Mercurial project (the nearest marker above the working directory is
`.hg`, not `.git`), `/review` takes no target: it reviews the working copy
(`hg diff --git`, with new and missing files written out as patches), else
the named branch against where it left `default`. Commits, ranges and bases
are refused, as OpenCode's Mercurial support refuses them.

The report joins the session, so "fix 1" works as the next message.
`--comment` posts the report to the pull request. Without a
`.codesmith/review.json`, only the standing reviewers run, so run
`/calibrate` first.

## Settings

```jsonc
{ "review": { "model": "anthropic/claude-sonnet-4-5", "concurrency": 3, "threshold": 80, "max_diff_chars": 60000 } }
```

| Field | Default | Meaning |
|---|---|---|
| `model` | the session's model | Model for reviewers and verifiers. |
| `concurrency` | `3` | Child sessions run at once. |
| `threshold` | `80` | Minimum verifier confidence (0–100) to report a finding. |
| `max_diff_chars` | `60000` | Diff characters given inline to each reviewer. A longer diff is cut, and the reviewer is told the git command that shows the rest. |

## Attribution

The method builds on Anthropic's
[code-review plugin](https://github.com/anthropics/claude-plugins-official/tree/main/plugins/code-review)
(Apache License 2.0). The adapted prompts, the license and a note of the
changes are in
[`crates/codesmith-core/src/prompt/review/`](../crates/codesmith-core/src/prompt/review/).
