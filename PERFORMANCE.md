# Performance

How CodeSmith behaves on large repositories and long sessions, and how to
measure it.

## Running the benchmarks

```sh
# Tools, snapshots, the session store and stats. Builds a 20,000-file tree
# (CODESMITH_PERF_FILES sets the count), or searches CODESMITH_PERF_TREE.
cargo bench -p codesmith-core --bench perf [name-filter]

# File watching only: start-up, idle CPU and a burst of writes.
# CODESMITH_PERF_IDLE sets the idle seconds (default 10).
cargo bench -p codesmith-core --bench perf watch

# Per-call costs only: shell scan, schema checks, permission rules,
# webfetch's URL checks and HTML conversion. `turn` runs whole turns
# against a scripted model on loopback.
cargo bench -p codesmith-core --bench perf call
cargo bench -p codesmith-core --bench perf turn

# TUI transcript drawing and @ completion (CODESMITH_PERF_TREE for the index).
cargo test --release -p codesmith tui::bench -- --ignored --nocapture
```

`CODESMITH_PERF_RUNS` caps the runs per benchmark. Each line prints the
minimum and median.

## Results

Apple M5 Pro, APFS, release build. Times are medians in milliseconds. The
"before" runs on the 100,000-file tree were single runs because grep took
minutes. The machine was shared with other builds, so differences under
about 30% (the snapshot and store rows) are noise.

Trees used:

- **100k**: 100,000 small files in nested directories, 3 files of 200,000
  lines, plus 12,000 files ignored by `.gitignore`. Committed to git.
- **20k**: the benchmark's own generated tree.
- **OpenCode**: a copy of the OpenCode source checkout (12,000 files).

"rules" adds 12 path rules (denies, asks and allows) to the base rules.

| Benchmark | Tree | Before | After | Nov 20 |
|---|---|---:|---:|---:|
| grep, base rules | 100k | 108,269 | 882 | — |
| grep, with rules | 100k | 239,355 | 984 | — |
| glob `**/*.rs`, base rules | 100k | 10,438 | 106 | — |
| glob `**/*.rs`, with rules | 100k | 22,487 | 116 | — |
| resolve and check 10k paths | 100k | 21,961 | 448 | — |
| 1k files: read + 2 edits each | 100k | 5,198 | 292 | — |
| grep, base rules | 20k | 21,358 | 213 | 201–209 |
| grep, with rules | 20k | 43,505 | 241 | 240–243 |
| glob, base rules | 20k | 2,016 | 33 | 29–31 |
| glob, with rules | 20k | 4,371 | 35 | 31–32 |
| grep, with rules and saved approvals | 20k | — | 363 | 240–270 |
| 1k files: read + 2 edits each | 20k | — | — | 318–319 |
| resolve and check 10k paths | 20k | 20,581 | 705 | 666–717 |
| grep, base rules | OpenCode | 12,816 | 128 | — |
| grep, with rules | OpenCode | 33,289 | 147 | — |
| glob, base rules | OpenCode | 63 | 14 | — |
| glob, with rules | OpenCode | 165 | 14 | — |
| snapshot, first capture | 100k | 4,494 | 2,624 | — |
| snapshot, unchanged tree | 100k | 467 | 296 | — |
| snapshot, one file edited | 100k | 485 | 398 | — |
| snapshot, unchanged tree | OpenCode | 100 | 64 | — |
| snapshot, first capture | 20k | — | — | 547 |
| snapshot, unchanged tree | 20k | — | — | 77 |
| snapshot, one file edited | 20k | — | — | 82 |
| store: load 5k records | — | 4.0 | 3.3 | 3.5–4.2 |
| store: history of 5k records | — | 4.7 | 3.5 | 3.9–4.5 |
| store: append one record | — | 0.13 | 0.12 | 0.04–0.05 |
| compaction estimate, 2.7k messages | — | 0.10 | 0.10 | 0.10 |
| stats, 500 sessions x 100 records | — | 43.3 | 31.0 | 31.3–31.5 |
| TUI: first frame, 2k entries | — | 42.0 | 21.6 | 1.5–1.6 |
| TUI: frame, 2k entries | — | 23.7 | 0.08 | 0.07 |
| TUI: frame scrolled 500 lines | — | 15.9 | 0.16 | 0.14 |
| TUI: streaming frame, 2k entries | — | 14.1 | 0.09 | 0.08 |
| TUI: frame after a resize | — | 14.9 | 0.12 | 0.12 |
| `@` index build (20k cap) | 100k | 57 | 57–100 | — |
| `@` query | 100k | 0.1–2.1 | 0.2–2.9 | — |

For reference, `rg -c needle_marker` takes about 690 ms on the 100k tree.

The "Nov 20" column is a re-run on the 20k tree after the tool schema
checks, URL checks, shell scanner additions, per-session permission
rules, saved approvals, the media blob store and the new TUI panels
landed: two full runs, shown as the range of their medians, with the
machine at a load of 6 to 28 from other builds. The 100k and OpenCode
trees were not re-run. "After" for the saved-approvals grep is the
first measurement, before the fix below; the read+edit row was
measured with the machine quieter (it rose to 400–900 ms in runs at a
load above 20, with the same binary). The snapshot rows are the quieter
of the two runs; the other run, under load, took 3.2 s, 190 ms and
176 ms. The TUI's first frame dropped from 21.6 to 1.5 ms between the two
measurements; that came with the TUI work in between and was not traced
to one change.

### Per call

What each tool call, request or page pays, in microseconds (medians of
10 runs of about 50 ms each; `cargo bench -p codesmith-core --bench perf
call`, or a name). Two runs each gave the same numbers within 5%.

| Benchmark | µs |
|---|---:|
| shell scan, a typical command (`git status && cargo test … \| tail`) | 2.8 |
| shell scan, a 20-stage pipeline (879 bytes: precommands, printf, arithmetic) | 53 |
| input schema check, `edit`, compiled schema cached | 0.3 |
| input schema check, `edit`, compiled each time | 1.4 |
| permission check, 2 commands, 40 rules and 4 policies | 2.8 |
| the same with the project's saved approvals (one database read) | 8.6 |
| webfetch URL checks (normalize, deny forms, ask) | 5.3 |
| webfetch HTML to markdown, 9 KB page with 40 code blocks | 127–135 (was 1,232–1,258) |

`turn` runs whole turns against the scripted model the integration tests
use, on loopback with the store on: a prompt, a `read` call, then the
answer, 20 turns in one session. A turn takes 2.4–3.1 ms, two model
requests included.

## File watching

What a watch of a whole tree costs (the language-server forwarder's watch;
the branch and `/fs/watch` watches are single files). "Start" is from
subscribing until the native watch is armed; "idle" is the process's CPU
time over 10 s with nothing changing; "burst" is 1,000 new files until the
1,000th update arrives. Release builds; macOS on the M5 Pro (FSEvents, one
recursive watch), Linux in a Docker container on the same machine (inotify,
one watch per directory not ignored).

| Tree | Backend | Directories watched | Start | Idle CPU (10 s) | Burst |
|---|---|---:|---:|---:|---:|
| 100k | FSEvents | 1 | 2.4 ms | 0.08 ms | 66 ms |
| 20k | FSEvents | 1 | 2.6 ms | 0.2 ms | 85 ms |
| this repository | FSEvents | 1 | 2.6 ms | 0.08 ms | 43 ms |
| 100k | inotify | 10,521 | 376 ms | 0.22 ms | 24 ms |
| 20k | inotify | 2,521 | 77 ms | 0.31 ms | 23 ms |
| this repository | inotify | 83 | 8 ms | 0.28 ms | 22 ms |

Re-run on Nov 20 (20k, FSEvents, two runs): start 3.4–5.6 ms, idle
0.06–0.2 ms of CPU over 10 s, burst 56–73 ms. The same as before within
the noise of a loaded machine.

Idle cost is under 0.01% of a core everywhere: the platform delivers
events and nothing polls. Nothing is watched at startup: the
language-server watch starts when the first server is ready, the server's
branch watch in the background, and inotify's directory walk runs off the
async runtime. On inotify the walk skips ignored directories (the 100k
tree's 2,000-file `build/` and anything `.gitignore`d), and at most half of
`fs.inotify.max_user_watches` directories are watched. The burst gives
more updates than files on macOS, as FSEvents reports a creation and a
modification separately.

## What changed

- **Permission patterns are compiled once.** `wildcard::matches` built a
  regex for every check, and grep and glob check the read rules for every
  file they visit, so nearly all of a search's time went into compiling
  regexes. Compiled patterns are now kept in a small shared cache (shared,
  not cloned: a cloned `Regex` starts with empty match caches), `*` alone
  skips the regex, and `Gate::allows` no longer copies the rule list.
- **Searches don't resolve every file again.** Grep and glob resolved each
  file in full: canonicalizing it, the project root and the scratch
  directory. Their walk does not follow symlinks, so every directory below
  its start is a real directory, and a regular file's real path is the
  start's real path plus the relative path the walk read from disk (in its
  on-disk case). `Ctx::resolve_walked` builds the target from that, and the
  root and scratch directory are resolved once per search (`Ctx::roots`).
  Symlinks and anything not below the start are resolved in full, and the
  rules still see the same names: a test checks that `resolve_walked`
  agrees with `resolve` for symlinked directories, links to secrets,
  case variants and other spellings of the project.
- **Parallel walk and search.** The tree is walked on several threads, and
  grep searches files on several threads in batches, in file order, so
  results and truncation match a one-by-one search.
- **Stats read only what they need.** Records are read for their kind,
  message roles, typed text and tool results; tool output and media are
  skipped instead of copied.
- **The TUI lays out only what is visible.** Each frame laid out the whole
  transcript to show its last screenful, and past 512 answers the markdown
  cache overflowed, so every answer was rendered again each frame. Now only
  the entries that reach the visible rows (plus the scroll offset) are laid
  out.

- **A search reads saved approvals once.** "Always" answers kept per
  project are read from the session database when a request is judged,
  so that an edit or removal applies at once. Grep judges every file it
  visits, so a session's grep ran one SQLite query per file, each under
  the approval book's lock: 363 ms on the 20k tree against 241 ms
  without saved approvals. A search now reads them once
  (`Gate::durable_now`) and judges every file against that; a single
  check still reads them afresh. A seeded randomized test checks that
  the snapshot judges as reading each time does.
- **Fixed regexes are compiled once.** The HTML to markdown converter
  compiled a regex for every `<code>` element with a class and three for
  every conversion, and edit one for every match it replaced. They are
  statics now: webfetch's conversion of a 9 KB page with 40 code blocks
  went from 1.2 ms to 0.13 ms. Tests pin the output of thousands of
  seeded random pages and edits to what the old code gave.

## Left as they are

- **Snapshots** cost one `git add --all` and one `git write-tree` per
  capture, twice a turn. Git stats every file and lists untracked ones;
  about 300 ms at 100,000 files, 65 ms for OpenCode. The untracked cache,
  index v4 and `feature.manyFiles` made no difference here. `core.fsmonitor`
  would, but it leaves a watcher daemon running for each project, so it
  should be a user's choice. Skipping the end capture when a turn changed
  nothing would change what undo restores when the user edits files
  during a turn.
- **Single path resolution** (read, write, edit) still resolves the
  project root and scratch directory on each call, about 50 µs. That keeps
  a renamed or relinked root correct without any invalidation.
- **Per-call checks** (the shell scan, schema checks, permission rules,
  webfetch's URL checks) cost microseconds per call, next to model
  requests of seconds. The shell scan is the largest, at about 60 ns per
  byte of command.
- **Saved approvals for single checks** cost one small database read
  (about 6 µs) per permission check. Caching them would need to notice
  writes from other processes sharing the database.
- **The interrupted-turn copy** of a step's stream (`Streamed` in
  `session.rs`) collects every event a second time, beside the stream's
  own collector. It appends the same deltas, so it costs about what the
  collection already did. It was not measured on its own; a whole turn,
  with it, takes 2.4–3.1 ms on loopback.
- **Store, history, compaction estimates and `@` completion** are already
  in the low milliseconds for these sizes.
