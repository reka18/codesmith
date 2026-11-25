# Contributing to CodeSmith

Thanks for helping. Issues and pull requests are welcome. Every change is
reviewed by the maintainer before it merges.

## Getting set up

```sh
git clone https://github.com/reka18/codesmith.git
cd codesmith
cargo build
```

`scripts/ci.sh` is the CI: it checks formatting, runs Clippy with
warnings as errors, runs every test and shellchecks the scripts. GitHub
Actions runs it on every pull request and every push to `main`, on Linux
(x86_64 and arm64) and macOS, and a pull request merges only when all
three pass. Run it yourself before opening one. For the Linux checks from
a Mac with Docker, use `scripts/ci-linux.sh`.

## Pull requests

- Keep one change per pull request, with a description of what it changes
  and why.
- Follow the code around you: module and function doc comments that say
  what a thing is for, plain error messages that say what to do next, and
  no secrets in output or logs.
- Add tests with the change: the regression it fixes, and the failure,
  edge and security cases around it. Tests sit next to the code
  (`#[cfg(test)]` modules or a `*_tests.rs` file) and end to end in
  `crates/codesmith/tests/e2e/`, which runs the real binary against a
  scripted model server, so no test needs a real account or network.
- Update the docs in `docs/` and add a line under the top section of
  `CHANGELOG.md` for anything a user notices.

## Providers

Only Anthropic, Claude Code (`claude-agent`) and Fireworks have been tested
with CodeSmith. Every other provider is marked `untested`, and CodeSmith
warns when one is used.

**If an untested provider works for you,** please open a
[Provider verified](https://github.com/reka18/codesmith/issues/new?template=provider-verified.md)
issue saying what you tried. Once it's confirmed, the provider moves to
`VERIFIED` in `crates/codesmith-core/src/providers/verified.rs` and the
warning goes away.

**If it doesn't work,** a pull request that fixes it is welcome. Where
things live:

| What | Where |
|---|---|
| Built-in providers: id, name, protocol, base URL, key variables | `BUILTIN` in `crates/codesmith-core/src/config.rs` |
| Protocols (Anthropic Messages, OpenAI Chat and Responses, Gemini, Bedrock) | `crates/codesmith-ai/src/` |
| Provider-specific sign-in and requests (Azure, Bedrock, Vertex, Copilot, …) | `crates/codesmith-core/src/providers/<provider>.rs` |
| Model lists read from a provider's API | `crates/codesmith-core/src/providers/hosted.rs` (tests in `hosted_tests.rs`) |
| The `/login` entries and key pages | `crates/codesmith-core/src/providers/signin.rs` |
| Provider docs | the providers section of `docs/configuration.md` |

A fix should come with tests that replay the provider's real responses
(saved from the provider's API, with keys and account details removed) so
it keeps working without an account. A pull request that makes a provider
work can also move it to `VERIFIED`, when it says how it was tested
against the real service.

## Security issues

Please don't open a public issue for a security problem. Use GitHub's
[private vulnerability reporting](https://github.com/reka18/codesmith/security/advisories/new)
instead.

## License

CodeSmith is MIT licensed. By contributing, you agree that your
contributions are licensed under the same terms.
