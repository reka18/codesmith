# Third-party notice

`reviewer.txt` and `verifier.txt` adapt the review method and wording of the
**code-review** plugin from Anthropic's `claude-plugins-official` repository
(author: Boris Cherny, Anthropic), licensed under the Apache License 2.0; a
copy is in `LICENSE-APACHE-2.0` next to this file.

Changes made for CodeSmith: the single pull-request command was split into
per-dimension reviewer and per-finding verifier prompts driven by CodeSmith's
own orchestrator; reviewers are scoped to one review dimension (from
`/calibrate`) and return structured JSON; verification returns a verdict and a
0–100 confidence instead of a pass/fail; GitHub-specific steps (eligibility
checks, inline comments, link formats) were removed or moved into code; the
model tiers named in the original were replaced by the configured model.

`survey.txt` and `synthesize.txt` (used by `/calibrate`) are original to
CodeSmith.
