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

| Benchmark | Tree | Before | After |
|---|---|---:|---:|
| grep, base rules | 100k | 108,269 | 882 |
| grep, with rules | 100k | 239,355 | 984 |
| glob `**/*.rs`, base rules | 100k | 10,438 | 106 |
| glob `**/*.rs`, with rules | 100k | 22,487 | 116 |
| resolve and check 10k paths | 100k | 21,961 | 448 |
| 1k files: read + 2 edits each | 100k | 5,198 | 292 |
| grep, base rules | 20k | 21,358 | 213 |
| grep, with rules | 20k | 43,505 | 241 |
| glob, base rules | 20k | 2,016 | 33 |
| glob, with rules | 20k | 4,371 | 35 |
| resolve and check 10k paths | 20k | 20,581 | 705 |
| grep, base rules | OpenCode | 12,816 | 128 |
| grep, with rules | OpenCode | 33,289 | 147 |
| glob, base rules | OpenCode | 63 | 14 |
| glob, with rules | OpenCode | 165 | 14 |
| snapshot, first capture | 100k | 4,494 | 2,624 |
| snapshot, unchanged tree | 100k | 467 | 296 |
| snapshot, one file edited | 100k | 485 | 398 |
| snapshot, unchanged tree | OpenCode | 100 | 64 |
| store: load 5k records | — | 4.0 | 3.3 |
| store: history of 5k records | — | 4.7 | 3.5 |
| store: append one record | — | 0.13 | 0.12 |
| compaction estimate, 2.7k messages | — | 0.10 | 0.10 |
| stats, 500 sessions x 100 records | — | 43.3 | 31.0 |
| TUI: first frame, 2k entries | — | 42.0 | 21.6 |
| TUI: frame, 2k entries | — | 23.7 | 0.08 |
| TUI: frame scrolled 500 lines | — | 15.9 | 0.16 |
| TUI: streaming frame, 2k entries | — | 14.1 | 0.09 |
| TUI: frame after a resize | — | 14.9 | 0.12 |
| `@` index build (20k cap) | 100k | 57 | 57–100 |
| `@` query | 100k | 0.1–2.1 | 0.2–2.9 |

For reference, `rg -c needle_marker` takes about 690 ms on the 100k tree.

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
- **Store, history, compaction estimates and `@` completion** are already
  in the low milliseconds for these sizes.
