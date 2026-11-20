//! OpenCode's patch tests, ported: `patch.test.ts` (the format, `oc_patch_*`),
//! `tool-model-policy.test.ts` (which models get the tool,
//! `oc_model_policy_*`) and `tool-patch.test.ts` (the tool, `oc_tool_patch_*`).
//! Where CodeSmith differs on purpose (resources named from the project
//! root, symlinks judged by their targets, no structured metadata) the test
//! says so and pins CodeSmith's behavior.

use super::super::patch::{derive, join_bom, parse, Chunk, Hunk, ParseError};
use super::super::testing::{Env, Recorder};
use super::super::{Ctx, Tool, ALL};
use super::*;
use crate::permission::{Asker, Effect, Gate, Reply, Rule};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---- the format (patch.test.ts) ----

fn add(path: &str, contents: &str) -> Hunk {
    Hunk::Add {
        path: path.into(),
        contents: contents.into(),
    }
}

fn delete(path: &str) -> Hunk {
    Hunk::Delete { path: path.into() }
}

fn update(path: &str, move_path: Option<&str>, chunks: Vec<Chunk>) -> Hunk {
    Hunk::Update {
        path: path.into(),
        move_path: move_path.map(str::to_string),
        chunks,
    }
}

fn chunk(old: &[&str], new: &[&str]) -> Chunk {
    Chunk {
        old_lines: old.iter().map(|s| s.to_string()).collect(),
        new_lines: new.iter().map(|s| s.to_string()).collect(),
        change_context: None,
        end_of_file: false,
    }
}

fn with_context(mut c: Chunk, context: &str) -> Chunk {
    c.change_context = Some(context.into());
    c
}

fn at_eof(mut c: Chunk) -> Chunk {
    c.end_of_file = true;
    c
}

fn ok(text: &str) -> Vec<Hunk> {
    parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

fn err(text: &str) -> String {
    parse(text).unwrap_err().to_string()
}

fn derived(chunks: &[Chunk], original: &str) -> String {
    derive("update.txt", chunks, original).unwrap().content
}

#[test]
fn oc_patch_parses_add_update_and_delete_hunks() {
    assert_eq!(
        ok("*** Begin Patch\n*** Add File: add.txt\n+added\n*** Update File: update.txt\n@@ section\n-old\n+new\n*** Delete File: delete.txt\n*** End Patch"),
        vec![
            add("add.txt", "added"),
            update(
                "update.txt",
                None,
                vec![with_context(chunk(&["old"], &["new"]), "section")]
            ),
            delete("delete.txt"),
        ]
    );
}

#[test]
fn oc_patch_parses_an_empty_patch() {
    assert_eq!(ok("*** Begin Patch\n*** End Patch"), vec![]);
}

#[test]
fn oc_patch_ignores_a_codex_environment_preamble() {
    assert_eq!(
        ok("*** Begin Patch\n*** Environment ID: remote\n*** Add File: file.txt\n+content\n*** End Patch"),
        vec![add("file.txt", "content")]
    );
}

#[test]
fn oc_patch_parses_an_update_followed_by_an_add() {
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: update.txt\n@@\n+line\n*** Add File: add.txt\n+content\n*** End Patch"),
        vec![
            update("update.txt", None, vec![chunk(&[], &["line"])]),
            add("add.txt", "content"),
        ]
    );
}

#[test]
fn oc_patch_parses_a_file_move() {
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n-old\n+new\n*** End Patch"),
        vec![update(
            "old.txt",
            Some("new.txt"),
            vec![chunk(&["old"], &["new"])]
        )]
    );
}

#[test]
fn oc_patch_identifies_the_missing_patch_boundary() {
    let first = "The first line of the patch must be '*** Begin Patch'";
    let last = "The last line of the patch must be '*** End Patch'";
    assert_eq!(err("This is not a valid patch"), first);
    assert_eq!(err("*** Begin Patch\n*** Add File: add.txt\n+added"), last);
    assert_eq!(err("extra\n*** Begin Patch\n*** End Patch"), first);
    assert_eq!(
        err("*** Begin Patch\n*** Add File: add.txt\n+added\n*** End Patch\nextra"),
        last
    );
    assert_eq!(
        parse("x").unwrap_err(),
        ParseError::Boundary { first: true }
    );
}

#[test]
fn oc_patch_allows_whitespace_after_the_end_marker() {
    assert_eq!(
        ok("*** Begin Patch\n*** Add File: add.txt\n+added\n*** End Patch\n \t\n"),
        vec![add("add.txt", "added")]
    );
}

#[test]
fn oc_patch_strips_a_heredoc_wrapper() {
    assert_eq!(
        ok("cat <<'EOF'\n*** Begin Patch\n*** Add File: add.txt\n+added\n*** End Patch\nEOF"),
        vec![add("add.txt", "added")]
    );
}

#[test]
fn oc_patch_strips_a_heredoc_wrapper_without_cat() {
    assert_eq!(
        ok("<<EOF\n*** Begin Patch\n*** Add File: add.txt\n+added\n*** End Patch\nEOF"),
        vec![add("add.txt", "added")]
    );
}

const ADD_PATCH: &str = "*** Begin Patch\n*** Add File: add.txt\n+added\n*** End Patch";

#[test]
fn oc_patch_strips_quoted_heredoc_wrappers() {
    assert_eq!(
        ok(&format!("<<'EOF'\n{ADD_PATCH}\nEOF")),
        vec![add("add.txt", "added")]
    );
    assert_eq!(
        ok(&format!("<<\"EOF\"\n{ADD_PATCH}\nEOF")),
        vec![add("add.txt", "added")]
    );
}

#[test]
fn oc_patch_rejects_malformed_heredoc_wrappers() {
    assert_eq!(
        err(&format!("<<\"EOF'\n{ADD_PATCH}\nEOF")),
        "The first line of the patch must be '*** Begin Patch'"
    );
    assert_eq!(
        err("<<EOF\n*** Begin Patch\n*** Add File: add.txt\n+added\nEOF"),
        "The last line of the patch must be '*** End Patch'"
    );
}

#[test]
fn oc_patch_parses_a_whitespace_padded_hunk_header() {
    assert_eq!(
        ok("*** Begin Patch\n  *** Update File: foo.txt\n@@\n-old\n+new\n*** End Patch"),
        vec![update("foo.txt", None, vec![chunk(&["old"], &["new"])])]
    );
}

#[test]
fn oc_patch_parses_leading_and_trailing_whitespace_around_patch_markers() {
    assert_eq!(
        ok(" *** Begin Patch\n*** Update File: file.txt\n@@\n-one\n+two\n*** End Patch "),
        vec![update("file.txt", None, vec![chunk(&["one"], &["two"])])]
    );
}

#[test]
fn oc_patch_parses_whitespace_on_the_inner_sides_of_patch_marker_lines() {
    assert_eq!(
        ok("*** Begin Patch \n*** Update File: file.txt\n@@\n-one\n+two\n *** End Patch"),
        vec![update("file.txt", None, vec![chunk(&["one"], &["two"])])]
    );
}

#[test]
fn oc_patch_parses_relative_and_absolute_hunk_paths() {
    assert_eq!(
        ok("*** Begin Patch\n*** Add File: relative.txt\n+content\n*** Delete File: /tmp/delete.txt\n*** Update File: /tmp/update.txt\n@@\n-old\n+new\n*** End Patch"),
        vec![
            add("relative.txt", "content"),
            delete("/tmp/delete.txt"),
            update("/tmp/update.txt", None, vec![chunk(&["old"], &["new"])]),
        ]
    );
}

#[test]
fn oc_patch_strips_one_carriage_return_from_crlf_patch_lines() {
    assert_eq!(
        ok("*** Begin Patch\r\n*** Update File: file.txt\r\n@@\r\n-old\r\n+new\r\n*** End Patch\r\n"),
        vec![update("file.txt", None, vec![chunk(&["old"], &["new"])])]
    );
}

#[test]
fn oc_patch_preserves_an_extra_carriage_return_in_crlf_patch_lines() {
    assert_eq!(
        ok("*** Begin Patch\r\n*** Update File: file.txt\r\n@@\r\n-old\r\r\n+new\r\n*** End Patch\r\n"),
        vec![update("file.txt", None, vec![chunk(&["old\r"], &["new"])])]
    );
}

#[test]
fn oc_patch_preserves_the_end_of_file_marker() {
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: file.txt\n@@\n+quux\n*** End of File\n\n*** End Patch"),
        vec![update("file.txt", None, vec![at_eof(chunk(&[], &["quux"]))])]
    );
}

#[test]
fn oc_patch_allows_an_end_of_file_marker_before_an_explicit_chunk() {
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: file.txt\n*** End of File\n@@\n-old\n+new\n*** End Patch"),
        vec![update("file.txt", None, vec![chunk(&["old"], &["new"])])]
    );
}

#[test]
fn oc_patch_allows_an_end_of_file_marker_before_an_implicit_chunk_and_move() {
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: file.txt\n*** End of File\n-old\n+new\n*** End Patch"),
        vec![update("file.txt", None, vec![chunk(&["old"], &["new"])])]
    );
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: old.txt\n*** End of File\n*** Move to: new.txt\n@@\n-old\n+new\n*** End Patch"),
        vec![update(
            "old.txt",
            Some("new.txt"),
            vec![chunk(&["old"], &["new"])]
        )]
    );
}

#[test]
fn oc_patch_derives_fuzzy_line_updates_while_preserving_bom() {
    let u = derive(
        "update.txt",
        &[chunk(&["  old   "], &["new"])],
        "\u{feff}old\n",
    )
    .unwrap();
    assert_eq!((u.content.as_str(), u.bom), ("new\n", true));
    assert_eq!(join_bom(&u.content, u.bom), "\u{feff}new\n");
}

#[test]
fn oc_patch_derives_multiple_update_chunks() {
    assert_eq!(
        derived(
            &[
                chunk(&["line 2"], &["LINE 2"]),
                chunk(&["line 4"], &["LINE 4"])
            ],
            "line 1\nline 2\nline 3\nline 4\n"
        ),
        "line 1\nLINE 2\nline 3\nLINE 4\n"
    );
}

#[test]
fn oc_patch_appends_a_pure_addition_chunk_to_a_nonempty_file() {
    assert_eq!(
        derived(&[chunk(&[], &["added 1", "added 2"])], "line 1\nline 2\n"),
        "line 1\nline 2\nadded 1\nadded 2\n"
    );
}

fn equal_offsets(original: &str) {
    let chunks = [chunk(&[], &["first"]), chunk(&[], &["second", "third"])];
    for _ in 0..2 {
        let u = derive("update.txt", &chunks, original).unwrap();
        assert_eq!(u.content, format!("{original}first\nsecond\nthird\n"));
        assert!(!u.bom);
    }
}

#[test]
fn oc_patch_preserves_equal_offset_insertion_order_and_frozen_chunks_for_empty() {
    equal_offsets("");
}

#[test]
fn oc_patch_preserves_equal_offset_insertion_order_and_frozen_chunks_for_original() {
    equal_offsets("original\n");
}

#[test]
fn oc_patch_applies_a_pure_addition_chunk_after_an_earlier_replacement() {
    assert_eq!(
        derived(
            &[
                chunk(&[], &["after-context", "second-line"]),
                chunk(
                    &["line1", "line2", "line3"],
                    &["line1", "line2-replacement"]
                ),
            ],
            "line1\nline2\nline3\n"
        ),
        "line1\nline2-replacement\nafter-context\nsecond-line\n"
    );
}

#[test]
fn oc_patch_applies_a_deletion_only_update_chunk() {
    assert_eq!(
        derived(
            &[chunk(&["line1", "line2", "line3"], &["line1", "line3"])],
            "line1\nline2\nline3\n"
        ),
        "line1\nline3\n"
    );
}

#[test]
fn oc_patch_updates_empty_files_and_adds_a_trailing_newline() {
    assert_eq!(
        derive("empty.txt", &[chunk(&[], &["First line"])], "")
            .unwrap()
            .content,
        "First line\n"
    );
    assert_eq!(
        derive("no-newline.txt", &[chunk(&["old"], &["new"])], "old")
            .unwrap()
            .content,
        "new\n"
    );
}

#[test]
fn oc_patch_disambiguates_updates_with_change_context() {
    assert_eq!(
        derived(
            &[with_context(chunk(&["x=10"], &["x=11"]), "fn b")],
            "fn a\nx=10\nfn b\nx=10\n"
        ),
        "fn a\nx=10\nfn b\nx=11\n"
    );
}

#[test]
fn oc_patch_matches_leading_trailing_and_unicode_punctuation_differences() {
    assert_eq!(
        derived(&[chunk(&["line"], &["next"])], "  line\n"),
        "next\n"
    );
    assert_eq!(
        derived(&[chunk(&["line"], &["next"])], "line  \n"),
        "next\n"
    );
    assert_eq!(
        derived(
            &[chunk(&["He said \"hello\""], &["He said \"hi\""])],
            "He said \u{201C}hello\u{201D}\n"
        ),
        "He said \"hi\"\n"
    );
}

#[test]
fn oc_patch_matches_unicode_minus_signs_and_spaces() {
    assert_eq!(
        derived(
            &[chunk(&["value - 1"], &["value - 2"])],
            "value \u{2212} 1\n"
        ),
        "value - 2\n"
    );
    for space in [
        '\u{00A0}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}',
        '\u{2008}', '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
    ] {
        assert_eq!(
            derived(
                &[chunk(&["hello world"], &["hello there"])],
                &format!("hello{space}world\n")
            ),
            "hello there\n",
            "{space:?}"
        );
    }
}

#[test]
fn oc_patch_does_not_normalize_ellipses() {
    let e = derive(
        "ellipsis.txt",
        &[chunk(&["wait..."], &["done"])],
        "wait\u{2026}\n",
    )
    .unwrap_err();
    assert!(e.contains("Failed to find expected lines"), "{e}");
}

#[test]
fn oc_patch_prefers_a_later_exact_match_over_an_earlier_normalized_match() {
    assert_eq!(
        derived(
            &[chunk(&["He said \"hello\""], &["He said \"goodbye\""])],
            "He said \u{201C}hello\u{201D}\nmiddle\nHe said \"hello\"\n"
        ),
        "He said \u{201C}hello\u{201D}\nmiddle\nHe said \"goodbye\"\n"
    );
}

#[test]
fn oc_patch_matches_eof_anchored_chunks_from_the_end() {
    assert_eq!(
        derived(
            &[at_eof(chunk(
                &["marker", "end"],
                &["marker changed", "end"]
            ))],
            "marker\nmiddle\nmarker\nend\n"
        ),
        "marker\nmiddle\nmarker changed\nend\n"
    );
}

#[test]
fn oc_patch_does_not_fall_back_to_a_non_eof_match() {
    let e = derive(
        "update.txt",
        &[at_eof(chunk(&["marker", "end"], &["changed", "end"]))],
        "marker\nend\nmiddle\n",
    )
    .unwrap_err();
    assert!(e.contains("Failed to find expected lines"), "{e}");
}

#[test]
fn oc_patch_identifies_a_missing_blank_line() {
    assert_eq!(
        derive("update.txt", &[chunk(&[""], &["added"])], "content\n").unwrap_err(),
        "Failed to find an expected blank line in update.txt"
    );
}

#[test]
fn oc_patch_parses_an_update_without_an_explicit_first_chunk_header() {
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: file.txt\n import foo\n+bar\n*** End Patch"),
        vec![update(
            "file.txt",
            None,
            vec![chunk(&["import foo"], &["import foo", "bar"])]
        )]
    );
}

#[test]
fn oc_patch_keeps_indented_update_markers_as_context_lines() {
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: a.txt\n@@\n-old a\n+new a\n *** Update File: b.txt\n@@\n-old b\n+new b\n*** End Patch"),
        vec![update(
            "a.txt",
            None,
            vec![
                chunk(
                    &["old a", "*** Update File: b.txt"],
                    &["new a", "*** Update File: b.txt"]
                ),
                chunk(&["old b"], &["new b"]),
            ]
        )]
    );
}

#[test]
fn oc_patch_keeps_indented_move_and_eof_markers_as_context_lines() {
    let lines = ["before", "*** Move to: moved.txt", "*** End of File"];
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: file.txt\n@@\n before\n *** Move to: moved.txt\n *** End of File\n*** End Patch"),
        vec![update("file.txt", None, vec![chunk(&lines, &lines)])]
    );
}

#[test]
fn oc_patch_preserves_update_context_indentation() {
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: file.txt\n@@   section\n-old\n+new\n*** End Patch"),
        vec![update(
            "file.txt",
            None,
            vec![with_context(chunk(&["old"], &["new"]), "  section")]
        )]
    );
}

#[test]
fn oc_patch_preserves_bare_empty_update_lines_as_context() {
    let lines = ["context before", "", "context after"];
    assert_eq!(
        ok("*** Begin Patch\n*** Update File: file.txt\n@@\n context before\n\n context after\n*** End Patch"),
        vec![update("file.txt", None, vec![chunk(&lines, &lines)])]
    );
}

#[test]
fn oc_patch_rejects_invalid_add_and_delete_lines() {
    assert_eq!(
        err("*** Begin Patch\n*** Add File: file.txt\nbad\n*** End Patch"),
        "Invalid hunk at line 3: Invalid Add File line for 'file.txt': expected a line starting with '+', got 'bad'"
    );
    assert_eq!(
        err("*** Begin Patch\n*** Delete File: file.txt\nbad\n*** End Patch"),
        "Invalid hunk at line 3: Unexpected line after Delete File 'file.txt': 'bad'. Delete hunks do not contain body lines"
    );
    assert!(err(
        "*** Begin Patch\n*** Delete File: file.txt\n*** Frobnicate File: next.txt\n*** End Patch"
    )
    .starts_with(
        "Invalid hunk at line 3: '*** Frobnicate File: next.txt' is not a valid hunk header"
    ));
}

#[test]
fn oc_patch_rejects_an_empty_update_hunk() {
    assert_eq!(
        err("*** Begin Patch\n*** Update File: file.txt\n*** End Patch"),
        "Invalid hunk at line 2: Update file hunk for path 'file.txt' is empty"
    );
    assert_eq!(
        err("*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n*** Delete File: other.txt\n*** End Patch"),
        "Invalid hunk at line 2: Update file hunk for path 'old.txt' is empty"
    );
    assert_eq!(
        err("*** Begin Patch\n*** Update File: file.txt\n*** End of File\n*** End Patch"),
        "Invalid hunk at line 2: Update file hunk for path 'file.txt' is empty"
    );
}

#[test]
fn oc_patch_rejects_an_empty_update_chunk() {
    assert_eq!(
        err("*** Begin Patch\n*** Update File: file.txt\n@@\n*** End Patch"),
        "Invalid hunk at line 4: Update hunk does not contain any lines"
    );
    assert_eq!(
        err("*** Begin Patch\n*** Update File: file.txt\n@@\n*** End of File\n*** End Patch"),
        "Invalid hunk at line 4: Update hunk does not contain any lines"
    );
    assert!(
        err("*** Begin Patch\n*** Update File: file.txt\n@@\n@@\n-old\n+new\n*** End Patch")
            .starts_with("Invalid hunk at line 4: Unexpected line found in update hunk: '@@'")
    );
    assert!(err(
        "*** Begin Patch\n*** Update File: file.txt\n@@\n*** Update File: other.txt\n@@\n-old\n+new\n*** End Patch"
    )
    .starts_with(
        "Invalid hunk at line 4: Unexpected line found in update hunk: '*** Update File: other.txt'"
    ));
    assert!(
        err("*** Begin Patch\n*** Update File: file.txt\n@@\nbad\n*** End Patch")
            .starts_with("Invalid hunk at line 4: Unexpected line found in update hunk: 'bad'")
    );
}

#[test]
fn oc_patch_rejects_an_invalid_update_line() {
    assert_eq!(
        err("*** Begin Patch\n*** Update File: file.txt\n@@\n-old\nbad\n*** End Patch"),
        "Invalid hunk at line 5: Expected update hunk to start with a @@ context marker, got: 'bad'"
    );
    assert!(
        err("*** Begin Patch\n*** Update File: file.txt\n@@foo\n*** End Patch")
            .starts_with("Invalid hunk at line 3: Unexpected line found in update hunk: '@@foo'")
    );
    assert_eq!(
        err("*** Begin Patch\n*** Update File: file.txt\n@@\n-old\n*** Frobnicate File: foo\n*** End Patch"),
        "Invalid hunk at line 5: Expected update hunk to start with a @@ context marker, got: '*** Frobnicate File: foo'"
    );
}

#[test]
fn oc_patch_rejects_invalid_and_pathless_hunk_headers() {
    assert!(
        err("*** Begin Patch\n*** Frobnicate File: foo\n*** End Patch").starts_with(
            "Invalid hunk at line 2: '*** Frobnicate File: foo' is not a valid hunk header"
        )
    );
    assert!(err("*** Begin Patch\n*** Add File:\n*** End Patch")
        .starts_with("Invalid hunk at line 2: '*** Add File:' is not a valid hunk header"));
    for header in ["*** Add File: ", "*** Delete File: ", "*** Update File: "] {
        assert!(
            err(&format!("*** Begin Patch\n{header}\n*** End Patch")).starts_with(&format!(
                "Invalid hunk at line 2: '{}' is not a valid hunk header",
                header.trim()
            )),
            "{header}"
        );
    }
    assert_eq!(
        err("*** Begin Patch\n*** Update File: old.txt\n*** Move to: \n@@\n-old\n+new\n*** End Patch"),
        "Invalid hunk at line 3: Move destination for 'old.txt' must not be empty"
    );
}

// ---- which models get it (tool-model-policy.test.ts) ----

/// The file tools left for a model, out of `read`, `write`, `edit`, `patch`.
fn file_tools(id: &str, model_id: &str) -> Vec<&'static str> {
    let mut tools: Vec<Tool> = ALL.to_vec();
    offer_for_model(&mut tools, id, model_id);
    tools
        .iter()
        .map(|t| t.name())
        .filter(|n| ["read", "write", "edit", "patch"].contains(n))
        .collect()
}

#[test]
fn oc_model_policy_gives_gpt_5_and_later_patch() {
    // OpenCode also moves the file tools into Code Mode here; CodeSmith's
    // Code Mode has no built-ins, so only the patch rule applies.
    for (id, model_id) in [("gpt-5", "gpt-5"), ("dep", "gpt-6")] {
        assert_eq!(file_tools(id, model_id), ["read", "patch"], "{id}");
    }
}

#[test]
fn oc_model_policy_keeps_edit_and_write_instead_of_patch_for_gpt_4_and_gpt_oss_models() {
    for (id, model_id) in [
        ("gpt-4.1", "gpt-4.1"),
        ("openai/gpt-oss-120b", "openai/gpt-oss-120b"),
        ("gpt-dep", "gpt-4o"),
    ] {
        assert_eq!(file_tools(id, model_id), ["read", "write", "edit"], "{id}");
    }
}

#[test]
fn oc_model_policy_needs_no_demotion_when_full_code_mode_is_off_while_still_applying_patch_rules() {
    assert_eq!(file_tools("gpt-5", "gpt-5"), ["read", "patch"]);
    assert_eq!(file_tools("opus", "opus"), ["read", "write", "edit"]);
    assert_eq!(
        file_tools("claude-sonnet-4-5", "claude-sonnet-4-5"),
        ["read", "write", "edit"]
    );
    // The config's name for the model counts as well as the provider's id.
    let mut tools = ALL.to_vec();
    offer_for_spec(
        &mut tools,
        "openrouter/openai/gpt-5.5#high",
        "openai/gpt-5.5",
    );
    assert!(tools.contains(&Tool::Patch) && !tools.contains(&Tool::Edit));
    let mut tools = ALL.to_vec();
    offer_for_spec(&mut tools, "azure/my-gpt-5", "deployment");
    assert!(tools.contains(&Tool::Patch));
}

// ---- the tool (tool-patch.test.ts) ----

/// Disk operations that fail on demand, counting reads made before the
/// `edit` request.
#[derive(Default)]
struct TestFiles {
    fail_write: Option<&'static str>,
    fail_remove: Option<&'static str>,
    recorder: Option<Arc<Recorder>>,
    asked_edit: Option<Arc<EditAsked>>,
    reads_before_edit: AtomicUsize,
}

fn named(path: &Path, name: Option<&str>) -> bool {
    name.is_some_and(|n| path.file_name().is_some_and(|f| f == n))
}

impl Files for TestFiles {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let edit_asked = self
            .recorder
            .as_ref()
            .is_some_and(|r| r.actions().iter().any(|a| a == "edit"))
            || self
                .asked_edit
                .as_ref()
                .is_some_and(|a| a.0.load(Ordering::SeqCst) > 0);
        if !edit_asked {
            self.reads_before_edit.fetch_add(1, Ordering::SeqCst);
        }
        Disk.read(path)
    }
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if named(path, self.fail_write) {
            return Err(io::Error::other("forced write failure"));
        }
        Disk.write(path, data)
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        if named(path, self.fail_remove) {
            return Err(io::Error::other("forced remove failure"));
        }
        Disk.remove(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        Disk.rename(from, to)
    }
}

/// Counts `edit` requests and runs a hook on each, then allows it.
#[derive(Default)]
struct EditAsked(AtomicUsize, Mutex<Option<Hook>>);

/// Runs on the `n`th `edit` request.
type Hook = Box<dyn Fn(usize) + Send + Sync>;

impl Asker for EditAsked {
    fn ask(&self, req: &crate::permission::Request) -> Reply {
        if req.action == "edit" {
            let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(hook) = self.1.lock().unwrap().as_ref() {
                hook(n);
            }
        }
        Reply::Once
    }
}

fn hooked(hook: impl Fn(usize) + Send + Sync + 'static) -> (Env, Arc<EditAsked>) {
    let asker = Arc::new(EditAsked::default());
    *asker.1.lock().unwrap() = Some(Box::new(hook));
    let env = Env {
        dir: tempfile::tempdir().unwrap(),
        gate: Gate::new(allow_read(), asker.clone()),
    };
    (env, asker)
}

fn allow_read() -> Vec<Rule> {
    vec![Rule::new("read", "*", Effect::Allow)]
}

/// An environment whose `edit` and `external_directory` requests are
/// recorded (and allowed), with reads allowed.
fn recording() -> (Env, Arc<Recorder>) {
    Env::recording(allow_read())
}

fn apply(ctx: &Ctx, text: &str) -> Outcome {
    run(ctx, &json!({"patchText": text}))
}

fn apply_with(ctx: &Ctx, files: &TestFiles, text: &str) -> Outcome {
    run_with(ctx, &json!({"patchText": text}), files)
}

fn read(env: &Env, rel: &str) -> String {
    std::fs::read_to_string(env.root().join(rel)).unwrap()
}

fn exists(env: &Env, rel: &str) -> bool {
    std::fs::symlink_metadata(env.root().join(rel)).is_ok()
}

fn outside() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().canonicalize().unwrap();
    (d, p)
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_registers_and_sequentially_applies_add_update_and_delete_hunks() {
    let (env, rec) = recording();
    env.write("update.txt", "before\n");
    env.write("remove.txt", "remove\n");
    let spec = Tool::Patch.spec("/bin/sh");
    assert_eq!(spec.name, "patch");
    assert_eq!(spec.parameters["required"], json!(["patchText"]));
    assert!(spec.description.contains("*** Begin Patch"));
    assert_eq!(Tool::Patch.action(), "edit");
    let files = TestFiles {
        recorder: Some(rec.clone()),
        ..Default::default()
    };
    let out = apply_with(
        &env.ctx(),
        &files,
        "*** Begin Patch\n*** Add File: nested/new.txt\n+created\n*** Update File: update.txt\n@@\n-before\n+after\n*** Delete File: remove.txt\n*** End Patch",
    )
    .unwrap();
    assert_eq!(
        out.content,
        "Success. Updated the following files:\nA nested/new.txt\nM update.txt\nD remove.txt"
    );
    let asked = rec.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].action, "edit");
    assert_eq!(
        asked[0].resources,
        ["nested/new.txt", "update.txt", "remove.txt"]
    );
    assert_eq!(asked[0].save, ["*"]);
    assert_eq!(
        asked[0].title,
        "Patch nested/new.txt, update.txt, remove.txt"
    );
    assert_eq!(files.reads_before_edit.load(Ordering::SeqCst), 2);
    assert_eq!(read(&env, "nested/new.txt"), "created\n");
    assert_eq!(read(&env, "update.txt"), "after\n");
    assert!(!exists(&env, "remove.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_replaces_a_file_with_a_directory_containing_an_added_file() {
    let env = Env::new();
    env.write("parent", "before\n");
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Delete File: parent\n*** Add File: parent/child.txt\n+after\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read(&env, "parent/child.txt"), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_counts_deleted_lines_with_and_without_a_trailing_newline() {
    // OpenCode counts them in its result metadata; CodeSmith's front ends
    // count them from the preview, which keeps each file's text.
    let env = Env::new();
    env.write("trailing.txt", "remove\n");
    env.write("unterminated.txt", "remove");
    let text = "*** Begin Patch\n*** Delete File: trailing.txt\n*** Delete File: unterminated.txt\n*** End Patch";
    let shown = preview(&env.root(), text);
    assert_eq!(
        shown
            .iter()
            .map(|p| (p.old.as_deref(), p.new.as_deref()))
            .collect::<Vec<_>>(),
        [(Some("remove\n"), None), (Some("remove"), None)]
    );
    let out = apply(&env.ctx(), text).unwrap();
    assert_eq!(
        out.content,
        "Success. Updated the following files:\nD trailing.txt\nD unterminated.txt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_serializes_concurrent_patch_transactions() {
    // The first approval waits, so the second patch would read the file
    // before the first one writes it if they were not serialized.
    let (env, _asked) = hooked(|n| {
        if n == 1 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    env.write("concurrent.txt", "one\ntwo\n");
    let results: Vec<Outcome> = std::thread::scope(|s| {
        let handles: Vec<_> = [("one", "ONE"), ("two", "TWO")]
            .into_iter()
            .map(|(old, new)| {
                let env = &env;
                s.spawn(move || {
                    apply(
                        &env.ctx(),
                        &format!("*** Begin Patch\n*** Update File: concurrent.txt\n@@\n-{old}\n+{new}\n*** End Patch"),
                    )
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(read(&env, "concurrent.txt"), "ONE\nTWO\n");
}

/// A formatter for `.txt` and `.cs` files that runs `perl -pi -e <script>`.
fn perl_formatter(env: &Env, script: &str) -> Arc<crate::format::Formatter> {
    use crate::format::{Entry, Formatter, Setting};
    let entry = Entry {
        command: Some(vec![
            "perl".into(),
            "-pi".into(),
            "-e".into(),
            script.into(),
            "$FILE".into(),
        ]),
        extensions: Some(vec![".txt".into(), ".cs".into()]),
        ..Default::default()
    };
    let setting = Setting::Custom([("test".to_string(), entry)].into_iter().collect());
    Arc::new(Formatter::new(Some(&setting), env.root(), env.root()))
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_returns_file_diffs_for_final_formatted_content() {
    // OpenCode's metadata diffs show the formatted text; CodeSmith tells
    // the model the file changed after it wrote it.
    let env = Env::new();
    let mut ctx = env.ctx();
    ctx.format = Some(perl_formatter(&env, "s/created/FORMATTED/"));
    let out = apply(
        &ctx,
        "*** Begin Patch\n*** Add File: formatted.txt\n+created\n*** End Patch",
    )
    .unwrap();
    assert_eq!(
        out.content,
        "Success. Updated the following files:\nA formatted.txt\n\nformatted.txt was reformatted by test, so it now differs from what you wrote; read it again before editing it."
    );
    assert_eq!(read(&env, "formatted.txt"), "FORMATTED\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_moves_and_updates_a_file() {
    let env = Env::new();
    env.write("old.txt", "before\n");
    let out = apply(
        &env.ctx(),
        "*** Begin Patch\n*** Add File: created.txt\n+created\n*** Update File: old.txt\n*** Move to: moved.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap();
    assert_eq!(
        out.content,
        "Success. Updated the following files:\nA created.txt\nM moved.txt"
    );
    assert!(!exists(&env, "old.txt"));
    assert_eq!(read(&env, "moved.txt"), "after\n");
    assert_eq!(read(&env, "created.txt"), "created\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_moves_a_file_over_an_existing_destination() {
    let env = Env::new();
    env.write("old.txt", "before\n");
    env.write("nested/moved.txt", "existing\n");
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: old.txt\n*** Move to: nested/moved.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap();
    assert!(!exists(&env, "old.txt"));
    assert_eq!(read(&env, "nested/moved.txt"), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_moves_a_file_without_changing_its_contents() {
    let env = Env::new();
    env.write("old.txt", "same\n");
    let out = apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: old.txt\n*** Move to: moved.txt\n@@\n same\n*** End Patch",
    )
    .unwrap();
    assert_eq!(
        out.content,
        "Success. Updated the following files:\nM moved.txt"
    );
    assert!(!exists(&env, "old.txt"));
    assert_eq!(read(&env, "moved.txt"), "same\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_refuses_to_rewrite_a_file_that_is_not_valid_utf8_and_applies_none_of_the_patch(
) {
    let env = Env::new();
    env.write("a.txt", "before\n");
    // "// José" in Latin-1, far from the patched line.
    let mut bytes = b"// Jos\xe9\n".to_vec();
    bytes.extend_from_slice(b"int MAX = 10;\n");
    std::fs::write(env.root().join("Invoice.java"), &bytes).unwrap();
    std::fs::write(env.root().join("Old.java"), &bytes).unwrap();
    let e = apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: a.txt\n@@\n-before\n+after\n*** Update File: Invoice.java\n@@\n-int MAX = 10;\n+int MAX = 25;\n*** End Patch",
    )
    .unwrap_err();
    assert!(
        e.starts_with(&format!(
            "patch verification failed: Failed to read file to update {}: file is not valid UTF-8.",
            env.root().join("Invoice.java").display()
        )),
        "{e}"
    );
    assert_eq!(read(&env, "a.txt"), "before\n");
    assert_eq!(
        std::fs::read(env.root().join("Invoice.java")).unwrap(),
        bytes
    );
    // Deleting does not rewrite the bytes, so it stays allowed.
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Delete File: Old.java\n*** End Patch",
    )
    .unwrap();
    assert!(!exists(&env, "Old.java"));
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_renames_a_file_when_the_move_only_changes_its_case() {
    let env = Env::new();
    env.write("same.txt", "before\n");
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: same.txt\n*** Move to: Same.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap();
    assert_eq!(entries(&env.root()), ["Same.txt"]);
    assert_eq!(read(&env, "Same.txt"), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_moves_into_a_directory_whose_name_differs_only_in_case() {
    let env = Env::new();
    env.write("Foo/a.txt", "before\n");
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: Foo/a.txt\n*** Move to: foo/a.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read(&env, "foo/a.txt"), "after\n");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_moves_a_symlink_without_deleting_its_target() {
    let env = Env::new();
    let target = env.write("target.txt", "before\n");
    std::os::unix::fs::symlink(&target, env.root().join("link.txt")).unwrap();
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: link.txt\n*** Move to: moved.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap();
    assert!(!exists(&env, "link.txt"));
    assert_eq!(read(&env, "target.txt"), "before\n");
    assert_eq!(read(&env, "moved.txt"), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_includes_move_file_info_in_metadata() {
    // OpenCode's metadata names the destination with the source's diff;
    // CodeSmith's preview and recorded changes do the same.
    let env = Env::new();
    env.write("old/name.txt", "old content\n");
    let text = "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-old content\n+new content\n*** End Patch";
    let dest = env.root().join("renamed/dir/name.txt");
    assert_eq!(
        preview(&env.root(), text),
        [Preview {
            path: dest.clone(),
            old: Some("old content\n".into()),
            new: Some("new content\n".into()),
        }]
    );
    assert_eq!(
        touched(&env.root(), text),
        [(dest, env.root().join("old/name.txt"))]
    );
    let out = apply(&env.ctx(), text).unwrap();
    assert!(out.content.ends_with("\nM renamed/dir/name.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_includes_the_move_destination_in_edit_permission_resources() {
    let (env, rec) = recording();
    env.write("old/name.txt", "old content\n");
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-old content\n+new content\n*** End Patch",
    )
    .unwrap();
    assert_eq!(
        rec.requests(),
        [(
            "edit".to_string(),
            vec![
                "old/name.txt".to_string(),
                "renamed/dir/name.txt".to_string()
            ]
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_uses_location_relative_resources_for_move_targets_in_a_nested_location() {
    // OpenCode names resources from the active Location; CodeSmith names
    // them from the project root, wherever the session's directory is.
    let (env, rec) = recording();
    env.write("nested/location/old.txt", "before\n");
    let mut ctx = env.ctx();
    ctx.cwd = env.root().join("nested/location");
    let out = apply(
        &ctx,
        "*** Begin Patch\n*** Update File: old.txt\n*** Move to: moved.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap();
    assert!(out.content.ends_with("\nM nested/location/moved.txt"));
    assert_eq!(
        rec.requests(),
        [(
            "edit".to_string(),
            vec![
                "nested/location/old.txt".to_string(),
                "nested/location/moved.txt".to_string()
            ]
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_inserts_lines_with_an_insert_only_hunk() {
    let env = Env::new();
    env.write("insert-only.txt", "alpha\nomega\n");
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: insert-only.txt\n@@\n alpha\n+beta\n omega\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read(&env, "insert-only.txt"), "alpha\nbeta\nomega\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_rejects_deleting_a_directory() {
    let env = Env::new();
    std::fs::create_dir(env.root().join("dir")).unwrap();
    let e = apply(
        &env.ctx(),
        "*** Begin Patch\n*** Delete File: dir\n*** End Patch",
    )
    .unwrap_err();
    assert_eq!(
        e,
        "patch verification failed: Failed to delete dir: path is a directory"
    );
    assert!(env.root().join("dir").is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_rejects_a_missing_second_chunk_context() {
    let env = Env::new();
    env.write("two-chunks.txt", "a\nb\nc\nd\n");
    assert!(apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: two-chunks.txt\n@@\n-b\n+B\n\n-d\n+D\n*** End Patch",
    )
    .is_err());
    assert_eq!(read(&env, "two-chunks.txt"), "a\nb\nc\nd\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_requires_patch_text() {
    let env = Env::new();
    assert_eq!(apply(&env.ctx(), "").unwrap_err(), "patchText is required");
    assert_eq!(
        run(&env.ctx(), &json!({})).unwrap_err(),
        "patchText is required"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_rejects_invalid_patch_format() {
    let env = Env::new();
    assert_eq!(
        apply(&env.ctx(), "invalid patch").unwrap_err(),
        "patch verification failed: The first line of the patch must be '*** Begin Patch'"
    );
    assert_eq!(
        apply(&env.ctx(), "*** Begin Patch\n*** Add File: foo\n+hello").unwrap_err(),
        "patch verification failed: The last line of the patch must be '*** End Patch'"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_rejects_an_empty_patch() {
    let env = Env::new();
    for text in [
        "*** Begin Patch\n*** End Patch",
        " *** Begin Patch \n *** End Patch ",
        "<<EOF\n*** Begin Patch\n*** End Patch\nEOF",
        "*** Begin Patch\n*** Environment ID: remote\n*** End Patch",
    ] {
        assert_eq!(
            apply(&env.ctx(), text).unwrap_err(),
            "patch rejected: empty patch",
            "{text:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_rejects_an_invalid_hunk_header() {
    let env = Env::new();
    assert_eq!(
        apply(&env.ctx(), "*** Begin Patch\n*** Frobnicate File: foo\n*** End Patch").unwrap_err(),
        "patch verification failed: Invalid hunk at line 2: '*** Frobnicate File: foo' is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_applies_successive_update_operations_to_one_file() {
    let env = Env::new();
    env.write("successive.txt", "a\nb\n");
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: successive.txt\n@@\n-a\n+A\n*** Update File: successive.txt\n@@\n-b\n+B\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read(&env, "successive.txt"), "A\nB\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_preserves_files_across_self_moves_and_multi_hunk_moves_in_one_patch() {
    let env = Env::new();
    env.write("same.txt", "before\n");
    env.write("old.txt", "a\nb\nc\n");
    let text = [
        "*** Begin Patch",
        "*** Update File: same.txt",
        "*** Move to: ./same.txt",
        "@@",
        "-before",
        "+after",
        "*** Update File: old.txt",
        "@@",
        "-a",
        "+A",
        "*** Update File: old.txt",
        "*** Move to: moved.txt",
        "@@",
        "-b",
        "+B",
        "*** Update File: moved.txt",
        "@@",
        "-c",
        "+C",
        "*** End Patch",
    ]
    .join("\n");
    apply(&env.ctx(), &text).unwrap();
    assert_eq!(read(&env, "same.txt"), "after\n");
    assert!(!exists(&env, "old.txt"));
    assert_eq!(read(&env, "moved.txt"), "A\nB\nC\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_does_not_invent_a_first_line_diff_for_bom_files() {
    let env = Env::new();
    env.write("example.cs", "\u{feff}using System;\n\nclass Test {}\n");
    let mut ctx = env.ctx();
    // A formatter that drops the BOM; the tool puts it back.
    ctx.format = Some(perl_formatter(&env, "s/^\\xEF\\xBB\\xBF//"));
    let text = "*** Begin Patch\n*** Update File: example.cs\n@@\n class Test {}\n+class Next {}\n*** End Patch";
    let shown = preview(&env.root(), text);
    let (old, new) = (shown[0].old.clone().unwrap(), shown[0].new.clone().unwrap());
    // Both sides keep the BOM, so a diff only adds the new class.
    assert!(old.starts_with('\u{feff}') && new.starts_with('\u{feff}'));
    assert_eq!(
        new.strip_prefix(old.as_str()),
        Some("class Next {}\n"),
        "{new:?}"
    );
    let out = apply(&ctx, text).unwrap();
    assert_eq!(
        out.content,
        "Success. Updated the following files:\nM example.cs"
    );
    assert_eq!(
        read(&env, "example.cs"),
        "\u{feff}using System;\n\nclass Test {}\nclass Next {}\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_rejects_an_update_with_missing_context() {
    let env = Env::new();
    env.write("unchanged.txt", "line1\nline2\n");
    assert_eq!(
        apply(
            &env.ctx(),
            "*** Begin Patch\n*** Update File: unchanged.txt\n@@\n-missing\n+changed\n*** End Patch"
        )
        .unwrap_err(),
        "patch verification failed: Failed to find expected lines in unchanged.txt:\nmissing"
    );
    assert_eq!(read(&env, "unchanged.txt"), "line1\nline2\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_rejects_an_update_when_the_target_file_is_missing() {
    let env = Env::new();
    assert_eq!(
        apply(
            &env.ctx(),
            "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch"
        )
        .unwrap_err(),
        format!(
            "patch verification failed: Failed to read file to update {}: file does not exist",
            env.root().join("missing.txt").display()
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_identifies_a_directory_used_as_an_update_target() {
    let env = Env::new();
    std::fs::create_dir(env.root().join("nested")).unwrap();
    assert_eq!(
        apply(
            &env.ctx(),
            "*** Begin Patch\n*** Update File: nested\n@@\n-old\n+new\n*** End Patch"
        )
        .unwrap_err(),
        format!(
            "patch verification failed: Failed to read file to update {}: path is a directory",
            env.root().join("nested").display()
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_identifies_a_missing_delete_target() {
    let env = Env::new();
    assert_eq!(
        apply(
            &env.ctx(),
            "*** Begin Patch\n*** Delete File: missing.txt\n*** End Patch"
        )
        .unwrap_err(),
        "patch verification failed: Failed to delete missing.txt: file does not exist"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_reports_the_failing_destination_and_filesystem_error() {
    let env = Env::new();
    env.write("old.txt", "before\n");
    let files = TestFiles {
        fail_write: Some("new.txt"),
        ..Default::default()
    };
    assert_eq!(
        apply_with(
            &env.ctx(),
            &files,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n-before\n+after\n*** End Patch"
        )
        .unwrap_err(),
        "Failed to write new.txt: forced write failure"
    );
    assert_eq!(read(&env, "old.txt"), "before\n");
    assert!(!exists(&env, "new.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_reports_the_successful_prefix_and_filesystem_error() {
    let env = Env::new();
    let files = TestFiles {
        fail_write: Some("second.txt"),
        ..Default::default()
    };
    assert_eq!(
        apply_with(
            &env.ctx(),
            &files,
            "*** Begin Patch\n*** Add File: first.txt\n+first\n*** Add File: second.txt\n+second\n*** End Patch"
        )
        .unwrap_err(),
        "Failed to write second.txt: forced write failure. Completed before failure: first.txt"
    );
    assert_eq!(read(&env, "first.txt"), "first\n");
    assert!(!exists(&env, "second.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_reports_a_destination_written_before_move_removal_fails() {
    let env = Env::new();
    env.write("old.txt", "before\n");
    let files = TestFiles {
        fail_remove: Some("old.txt"),
        ..Default::default()
    };
    assert_eq!(
        apply_with(
            &env.ctx(),
            &files,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n-before\n+after\n*** End Patch"
        )
        .unwrap_err(),
        "Wrote new.txt but failed to remove old.txt: forced remove failure"
    );
    assert_eq!(read(&env, "old.txt"), "before\n");
    assert_eq!(read(&env, "new.txt"), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_approves_an_external_directory_before_reading_and_requests_edit_permission_afterward(
) {
    // OpenCode names the directory (`<dir>/*`) and saves the repository's;
    // CodeSmith names the file and saves its directory, as `edit` does.
    let (env, rec) = recording();
    let (_o, out) = outside();
    let dir = out.join("repository/nested");
    std::fs::create_dir_all(out.join("repository/.git")).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("external.txt");
    std::fs::write(&target, "before\n").unwrap();
    let files = TestFiles {
        recorder: Some(rec.clone()),
        ..Default::default()
    };
    apply_with(
        &env.ctx(),
        &files,
        &format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch",
            target.display()
        ),
    )
    .unwrap();
    assert_eq!(rec.actions(), ["external_directory", "edit"]);
    let asked = rec.asked.lock().unwrap().clone();
    assert_eq!(asked[0].resources, [target.display().to_string()]);
    assert_eq!(asked[0].save, [format!("{}/*", dir.display())]);
    assert_eq!(asked[1].resources, [target.display().to_string()]);
    assert_eq!(files.reads_before_edit.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_does_not_inspect_an_external_file_when_external_permission_is_denied() {
    let (env, rec) = recording();
    rec.deny_action("external_directory");
    let (_o, out) = outside();
    let target = out.join("external.txt");
    std::fs::write(&target, "before\n").unwrap();
    let files = TestFiles {
        recorder: Some(rec.clone()),
        ..Default::default()
    };
    let e = apply_with(
        &env.ctx(),
        &files,
        &format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch",
            target.display()
        ),
    )
    .unwrap_err();
    assert!(e.contains("declined"), "{e}");
    assert_eq!(rec.actions(), ["external_directory"]);
    assert_eq!(files.reads_before_edit.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "before\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_preserves_edit_permission_rejection() {
    let (env, rec) = recording();
    rec.deny_action("edit");
    env.write("target.txt", "before\n");
    let e = apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: target.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap_err();
    assert!(e.contains("declined"), "{e}");
    assert_eq!(rec.actions(), ["edit"]);
    assert_eq!(read(&env, "target.txt"), "before\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_treats_a_sibling_path_inside_the_project_worktree_as_internal() {
    // CodeSmith names it from the project root (`sibling.txt`), not from
    // the session's directory (`../sibling.txt`).
    let (env, rec) = recording();
    std::fs::create_dir(env.root().join("active")).unwrap();
    env.write("sibling.txt", "before\n");
    let mut ctx = env.ctx();
    ctx.cwd = env.root().join("active");
    apply(
        &ctx,
        "*** Begin Patch\n*** Update File: ../sibling.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap();
    assert_eq!(
        rec.requests(),
        [("edit".to_string(), vec!["sibling.txt".to_string()])]
    );
    assert_eq!(read(&env, "sibling.txt"), "after\n");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_follows_an_internal_symlink_to_an_external_file_without_external_permission()
{
    // OpenCode resolves paths lexically, so a link inside the project needs
    // no external approval; CodeSmith judges a symlink by its target, as
    // `edit` and `write` do, so it asks `external_directory` first.
    let (env, rec) = recording();
    let (_o, out) = outside();
    let target = out.join("external.txt");
    std::fs::write(&target, "before\n").unwrap();
    std::os::unix::fs::symlink(&target, env.root().join("link.txt")).unwrap();
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Update File: link.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap();
    assert_eq!(rec.actions(), ["external_directory", "edit"]);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_approves_a_relative_external_target_before_reading_and_requests_edit_permission_afterward(
) {
    let (env, rec) = recording();
    let (_o, out) = outside();
    let target = out.join("external.txt");
    std::fs::write(&target, "before\n").unwrap();
    let up = env.root().components().count() - 1;
    let relative = format!(
        "{}{}",
        "../".repeat(up),
        target.strip_prefix("/").unwrap().display()
    );
    let files = TestFiles {
        recorder: Some(rec.clone()),
        ..Default::default()
    };
    apply_with(
        &env.ctx(),
        &files,
        &format!(
            "*** Begin Patch\n*** Update File: {relative}\n@@\n-before\n+after\n*** End Patch"
        ),
    )
    .unwrap();
    assert_eq!(rec.actions(), ["external_directory", "edit"]);
    assert_eq!(files.reads_before_edit.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_uses_canonical_external_permissions_and_resources_for_a_move_destination() {
    let (env, rec) = recording();
    let (_o, out) = outside();
    env.write("source.txt", "before\n");
    let dest = out.join("moved.txt");
    let result = apply(
        &env.ctx(),
        &format!(
            "*** Begin Patch\n*** Update File: source.txt\n*** Move to: {}\n@@\n-before\n+after\n*** End Patch",
            dest.display()
        ),
    )
    .unwrap();
    assert!(result.content.ends_with(&format!("\nM {}", dest.display())));
    let asked = rec.asked.lock().unwrap().clone();
    assert_eq!(rec.actions(), ["external_directory", "edit"]);
    assert_eq!(asked[0].resources, [dest.display().to_string()]);
    assert_eq!(asked[0].save, [format!("{}/*", out.display())]);
    assert_eq!(
        asked[1].resources,
        ["source.txt".to_string(), dest.display().to_string()]
    );
    assert!(!exists(&env, "source.txt"));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_approves_each_external_file_under_the_same_parent() {
    let (env, rec) = recording();
    let (_o, out) = outside();
    let (first, second) = (out.join("first.txt"), out.join("second.txt"));
    std::fs::write(&first, "before\n").unwrap();
    std::fs::write(&second, "before\n").unwrap();
    apply(
        &env.ctx(),
        &format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch",
            first.display(),
            second.display()
        ),
    )
    .unwrap();
    assert_eq!(
        rec.actions(),
        ["external_directory", "external_directory", "edit"]
    );
    let asked = rec.asked.lock().unwrap().clone();
    assert_eq!(asked[0].save, [format!("{}/*", out.display())]);
    assert_eq!(asked[1].save, asked[0].save);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_rejects_invalid_later_update_before_applying_an_earlier_add() {
    let env = Env::new();
    let e = apply(
        &env.ctx(),
        "*** Begin Patch\n*** Add File: created.txt\n+created\n*** Update File: missing.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .unwrap_err();
    assert!(
        e.starts_with("patch verification failed: Failed to read file to update"),
        "{e}"
    );
    assert!(!exists(&env, "created.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_adds_files_by_overwriting_existing_targets() {
    let env = Env::new();
    env.write("existing.txt", "sentinel\n");
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Add File: existing.txt\n+replacement\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read(&env, "existing.txt"), "replacement\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_overwrites_an_add_target_that_appears_during_permission_approval() {
    let slot: Arc<Mutex<Option<PathBuf>>> = Arc::default();
    let path = slot.clone();
    let (env, _asked) = hooked(move |_| {
        if let Some(p) = path.lock().unwrap().as_ref() {
            std::fs::write(p, "winner\n").unwrap();
        }
    });
    *slot.lock().unwrap() = Some(env.root().join("appeared.txt"));
    apply(
        &env.ctx(),
        "*** Begin Patch\n*** Add File: appeared.txt\n+replacement\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read(&env, "appeared.txt"), "replacement\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_patch_preserves_a_later_commit_defect_after_earlier_sequential_applications() {
    let env = Env::new();
    env.write("first.txt", "first");
    env.write("second.txt", "second");
    let files = TestFiles {
        fail_remove: Some("second.txt"),
        ..Default::default()
    };
    let e = apply_with(
        &env.ctx(),
        &files,
        "*** Begin Patch\n*** Delete File: first.txt\n*** Delete File: second.txt\n*** End Patch",
    )
    .unwrap_err();
    assert_eq!(
        e,
        "Failed to delete second.txt: forced remove failure. Completed before failure: first.txt"
    );
    assert!(!exists(&env, "first.txt"));
    assert!(exists(&env, "second.txt"));
}

// ---- CodeSmith's own ----

#[tokio::test(flavor = "multi_thread")]
async fn asks_before_verifying_a_file_reads_do_not_plainly_allow() {
    // `.env` reads ask, so a patch against it is approved before its lines
    // are checked: a mismatch cannot reveal the file's contents unasked.
    let mut rules = crate::permission::base_rules();
    rules.push(Rule::new("edit", "*", Effect::Ask));
    let (env, rec) = Env::recording(rules);
    env.write(".env", "SECRET=1\n");
    let files = TestFiles {
        recorder: Some(rec.clone()),
        ..Default::default()
    };
    rec.deny_action("edit");
    let e = apply_with(
        &env.ctx(),
        &files,
        "*** Begin Patch\n*** Update File: .env\n@@\n-SECRET=2\n+SECRET=3\n*** End Patch",
    )
    .unwrap_err();
    assert!(e.contains("declined"), "{e}");
    assert_eq!(files.reads_before_edit.load(Ordering::SeqCst), 0);
    assert_eq!(rec.actions(), ["edit"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn previews_each_hunk_in_order() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    env.write("gone.txt", "bye\n");
    let shown = preview(
        &env.root(),
        "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+two\n*** Update File: a.txt\n@@\n-two\n+three\n*** Delete File: gone.txt\n*** Update File: missing.txt\n@@\n-x\n+y\n*** Add File: new.txt\n+hi\n*** End Patch",
    );
    let got: Vec<(String, Option<&str>, Option<&str>)> = shown
        .iter()
        .map(|p| {
            (
                p.path
                    .strip_prefix(env.root())
                    .unwrap()
                    .display()
                    .to_string(),
                p.old.as_deref(),
                p.new.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("a.txt".into(), Some("one\n"), Some("two\n")),
            ("a.txt".into(), Some("two\n"), Some("three\n")),
            ("gone.txt".into(), Some("bye\n"), None),
            ("new.txt".into(), None, Some("hi\n")),
        ]
    );
    assert!(preview(&env.root(), "not a patch").is_empty());
}
