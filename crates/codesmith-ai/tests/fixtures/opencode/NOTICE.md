# OpenCode recordings

The JSON cassettes in this directory are real provider HTTP traffic recorded
by the OpenCode project and are vendored unchanged (file names and directory
layout kept) to pin CodeSmith's protocol parsers to OpenCode's behaviour. The
replay harness lives in `crates/codesmith-ai/src/replay_tests.rs`.

- Source: https://github.com/anomalyco/opencode, commit
  `b4d81f04ad350b9a9de7ec97af3ce93ecb5c5e2c`
- Paths: `packages/ai/test/fixtures/recordings/**` and
  `packages/core/test/fixtures/recordings/**` (only `session-runner/`)
- Selection: HTTP cassettes whose every request goes to a Chat Completions,
  Anthropic Messages, Responses or Gemini `streamGenerateContent` endpoint.
  Image generation, speech, transcription, embeddings, video, Bedrock and
  WebSocket cassettes are not included.
- Requests carry no credentials; OpenCode's recorder redacts auth headers,
  and this was re-checked when vendoring.

`../opencode-expectations.json` summarizes, per cassette and model call, what
OpenCode's recorded tests (`packages/ai/test/provider/*.recorded.test.ts` and
helpers) assert about each response. It is derived from those tests.

## License

OpenCode is distributed under the MIT License:

MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
