---
name: Provider verified
about: Confirm that a provider marked "untested" works with CodeSmith
title: "Provider verified: "
labels: provider-verified
---

Thanks for confirming a provider. A maintainer checks the report and marks
the provider tested in `crates/codesmith-core/src/providers/verified.rs`.

**Provider id** (as in `codesmith auth list`):

**CodeSmith version** (`codesmith --version`):

**OS and architecture** (for example macOS 15 arm64, Ubuntu 24.04 x86_64):

**How you signed in** (API key, environment variable, browser login,
config `providers` entry, local server):

**Models you used** (`provider/model`):

**What you tried**, tick what worked:

- [ ] A chat turn with a streamed answer
- [ ] Tool calls (reading and editing files, running a shell command)
- [ ] A long session (several turns; compaction if it came up)
- [ ] `/model` lists the provider's models
- [ ] `/effort`, if the model has levels
- [ ] Images in a prompt, if the model takes them

**Anything that didn't work, or needed a workaround:**

Please leave out API keys, tokens and other secrets.
