//! The diff viewer's file tree, ported from OpenCode's
//! `diff-viewer-file-tree-utils`: changed paths as a tree of folders and
//! files, folders first and then by name, with single-folder chains shown
//! as one row (`packages/opencode/src`). Patches are shown in the order the
//! tree lists their files.

use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: usize,
    pub name: String,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub depth: usize,
    pub kind: NodeKind,
    /// The file's index in the diff's file list.
    pub file: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    pub roots: Vec<usize>,
    pub nodes: Vec<Node>,
}

/// One row as shown: a folder chain or a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: usize,
    pub depth: usize,
    pub kind: NodeKind,
    pub name: String,
    pub file: Option<usize>,
}

fn add(
    tree: &mut Tree,
    name: &str,
    parent: Option<usize>,
    depth: usize,
    kind: NodeKind,
    file: Option<usize>,
) -> usize {
    let id = tree.nodes.len();
    tree.nodes.push(Node {
        id,
        name: name.to_string(),
        parent,
        children: vec![],
        depth,
        kind,
        file,
    });
    match parent {
        None => tree.roots.push(id),
        Some(p) => tree.nodes[p].children.push(id),
    }
    id
}

/// The tree of `files` (slash-separated paths), folders shared.
pub fn build<S: AsRef<str>>(files: &[S]) -> Tree {
    let mut tree = Tree::default();
    let mut dirs: HashMap<String, usize> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        let segments: Vec<&str> = file.as_ref().split('/').filter(|s| !s.is_empty()).collect();
        let Some((last, folders)) = segments.split_last() else {
            continue;
        };
        let (mut parent, mut path, mut depth) = (None, String::new(), 0);
        for segment in folders {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(segment);
            parent = Some(match dirs.get(&path) {
                Some(&id) => id,
                None => {
                    let id = add(&mut tree, segment, parent, depth, NodeKind::Directory, None);
                    dirs.insert(path.clone(), id);
                    id
                }
            });
            depth += 1;
        }
        add(&mut tree, last, parent, depth, NodeKind::File, Some(index));
    }
    let order = |t: &Tree, a: &usize, b: &usize| compare(t, *a, *b);
    let snapshot = tree.clone();
    tree.roots.sort_by(|a, b| order(&snapshot, a, b));
    for n in &mut tree.nodes {
        n.children.sort_by(|a, b| order(&snapshot, a, b));
    }
    tree
}

/// Folders before files, then by name (byte order, as OpenCode compares),
/// then in the order they were added.
pub fn compare(tree: &Tree, a: usize, b: usize) -> std::cmp::Ordering {
    let (l, r) = (&tree.nodes[a], &tree.nodes[b]);
    if l.kind != r.kind {
        return if l.kind == NodeKind::Directory {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        };
    }
    l.name.cmp(&r.name).then(a.cmp(&b))
}

/// A folder and the single folders below it, shown as one row.
fn chain(tree: &Tree, id: usize) -> Vec<usize> {
    let mut out = vec![id];
    let mut cur = id;
    loop {
        let n = &tree.nodes[cur];
        match n.children.as_slice() {
            [only] if tree.nodes[*only].kind == NodeKind::Directory => {
                out.push(*only);
                cur = *only;
            }
            _ => return out,
        }
    }
}

/// The rows depth first; only the children of `expanded` folders when it
/// is given (all of them otherwise).
pub fn flatten(tree: &Tree, expanded: Option<&BTreeSet<usize>>) -> Vec<Row> {
    fn visit(
        tree: &Tree,
        id: usize,
        depth: usize,
        expanded: Option<&BTreeSet<usize>>,
        rows: &mut Vec<Row>,
    ) {
        let node = &tree.nodes[id];
        if node.kind == NodeKind::File {
            rows.push(Row {
                id,
                depth,
                kind: NodeKind::File,
                name: node.name.clone(),
                file: node.file,
            });
            return;
        }
        let ids = chain(tree, id);
        let name = ids
            .iter()
            .map(|i| tree.nodes[*i].name.as_str())
            .collect::<Vec<_>>()
            .join("/");
        rows.push(Row {
            id,
            depth,
            kind: NodeKind::Directory,
            name,
            file: None,
        });
        if expanded.is_none_or(|e| e.contains(&id)) {
            let last = *ids.last().unwrap();
            for &child in &tree.nodes[last].children {
                visit(tree, child, depth + 1, expanded, rows);
            }
        }
    }
    let mut rows = Vec::new();
    for &root in &tree.roots {
        visit(tree, root, 0, expanded, &mut rows);
    }
    rows
}

/// The folders to open so `file` shows, or `None` when no row has it.
pub fn file_selection(tree: &Tree, file: usize) -> Option<BTreeSet<usize>> {
    let node = tree
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::File && n.file == Some(file))?;
    let mut out = BTreeSet::new();
    let mut parent = node.parent;
    while let Some(p) = parent {
        out.insert(p);
        parent = tree.nodes[p].parent;
    }
    Some(out)
}

/// The file single-file view shows: the selected one, else the one in
/// view, else the first.
pub fn single_file(
    selected: Option<usize>,
    current: Option<usize>,
    first: Option<usize>,
) -> Option<usize> {
    selected.or(current).or(first)
}

/// The files in the order the tree lists them.
pub fn ordered_files(rows: &[Row]) -> Vec<usize> {
    rows.iter().filter_map(|r| r.file).collect()
}

/// Whether the tree shows: turned on, and something changed.
pub fn show_tree(enabled: bool, files: usize) -> bool {
    enabled && files > 0
}

/// The file `offset` steps from `current` in `order`, held at the ends;
/// the first when `current` is not in it.
pub fn move_file(order: &[usize], current: Option<usize>, offset: i64) -> Option<usize> {
    let first = *order.first()?;
    let Some(i) = current.and_then(|c| order.iter().position(|&f| f == c)) else {
        return Some(first);
    };
    let next = (i as i64 + offset).clamp(0, order.len() as i64 - 1) as usize;
    Some(order[next])
}

/// Every folder, for the tree opened in full.
pub fn all_expanded(tree: &Tree) -> BTreeSet<usize> {
    tree.nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Directory)
        .map(|n| n.id)
        .collect()
}

/// `expanded` with `selected` opened or closed, when it is a folder.
#[cfg(test)]
pub fn toggle(tree: &Tree, expanded: &BTreeSet<usize>, selected: Option<usize>) -> BTreeSet<usize> {
    let mut next = expanded.clone();
    if let Some(s) = selected.filter(|s| {
        tree.nodes
            .get(*s)
            .is_some_and(|n| n.kind == NodeKind::Directory)
    }) {
        if !next.remove(&s) {
            next.insert(s);
        }
    }
    next
}

#[cfg(test)]
mod tests {
    //! `tui/test/feature-plugins/diff-viewer-file-tree-utils`, ported.
    use super::*;

    fn shape(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                let kind = match r.kind {
                    NodeKind::Directory => "directory",
                    NodeKind::File => "file",
                };
                format!("{}{kind}:{}", "  ".repeat(r.depth), r.name)
            })
            .collect()
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.name.clone()).collect()
    }

    fn dir(t: &Tree, name: &str) -> usize {
        t.nodes
            .iter()
            .find(|n| n.kind == NodeKind::Directory && n.name == name)
            .unwrap()
            .id
    }

    #[test]
    fn oc_builds_a_nested_tree_with_deduplicated_directories_and_file_indexes() {
        let t = build(&[
            "src/config/tui.ts",
            "src/config/keybind.ts",
            "src/session/index.ts",
        ]);
        for name in ["src", "config", "session"] {
            let n = t
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Directory && n.name == name)
                .count();
            assert_eq!(n, 1, "{name}");
        }
        let files: Vec<(String, Option<usize>, usize)> = t
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::File)
            .map(|n| (n.name.clone(), n.file, n.depth))
            .collect();
        assert_eq!(
            files,
            vec![
                ("tui.ts".into(), Some(0), 2),
                ("keybind.ts".into(), Some(1), 2),
                ("index.ts".into(), Some(2), 2),
            ]
        );
    }

    #[test]
    fn oc_sorts_directories_before_files_and_alphabetically_within_each_group() {
        let rows = flatten(
            &build(&[
                "z-file.ts",
                "b/file.ts",
                "a/zeta.ts",
                "b/alpha.ts",
                "a/alpha.ts",
            ]),
            None,
        );
        assert_eq!(
            shape(&rows),
            [
                "directory:a",
                "  file:alpha.ts",
                "  file:zeta.ts",
                "directory:b",
                "  file:alpha.ts",
                "  file:file.ts",
                "file:z-file.ts",
            ]
        );
    }

    #[test]
    fn oc_sorts_root_level_files_without_creating_directories() {
        let t = build(&["zeta.ts", "alpha.ts", "beta.ts"]);
        assert!(t.nodes.iter().all(|n| n.kind == NodeKind::File));
        assert_eq!(
            names(&flatten(&t, None)),
            ["alpha.ts", "beta.ts", "zeta.ts"]
        );
    }

    #[test]
    fn oc_collapses_unary_directory_chains_while_flattening() {
        let rows = flatten(
            &build(&[
                "packages/opencode/src/cli/app.ts",
                "packages/opencode/src/server/server.ts",
            ]),
            None,
        );
        assert_eq!(
            shape(&rows),
            [
                "directory:packages/opencode/src",
                "  directory:cli",
                "    file:app.ts",
                "  directory:server",
                "    file:server.ts",
            ]
        );
    }

    #[test]
    fn oc_does_not_collapse_a_directory_into_a_file_row() {
        let rows = flatten(&build(&["packages/opencode/src/app.ts"]), None);
        assert_eq!(
            shape(&rows),
            ["directory:packages/opencode/src", "  file:app.ts"]
        );
    }

    #[test]
    fn oc_stops_collapsing_at_branches() {
        let rows = flatten(
            &build(&[
                "packages/opencode/src/cli/app.ts",
                "packages/opencode/src/server/server.ts",
                "packages/readme.md",
            ]),
            None,
        );
        assert_eq!(
            shape(&rows),
            [
                "directory:packages",
                "  directory:opencode/src",
                "    directory:cli",
                "      file:app.ts",
                "    directory:server",
                "      file:server.ts",
                "  file:readme.md",
            ]
        );
    }

    #[test]
    fn oc_keeps_same_directory_names_under_different_parents_separate() {
        let rows = flatten(
            &build(&["components/button.ts", "docs/components/usage.md"]),
            None,
        );
        assert_eq!(
            shape(&rows),
            [
                "directory:components",
                "  file:button.ts",
                "directory:docs/components",
                "  file:usage.md",
            ]
        );
    }

    #[test]
    fn oc_flattens_all_expanded_rows_depth_first_with_depths_and_file_references() {
        let rows = flatten(
            &build(&["src/config/tui.ts", "src/config/keybind.ts", "README.md"]),
            None,
        );
        let got: Vec<(String, NodeKind, usize, Option<usize>)> = rows
            .iter()
            .map(|r| (r.name.clone(), r.kind, r.depth, r.file))
            .collect();
        assert_eq!(
            got,
            vec![
                ("src/config".into(), NodeKind::Directory, 0, None),
                ("keybind.ts".into(), NodeKind::File, 1, Some(1)),
                ("tui.ts".into(), NodeKind::File, 1, Some(0)),
                ("README.md".into(), NodeKind::File, 0, Some(2)),
            ]
        );
    }

    #[test]
    fn oc_collapses_expanded_unary_children_under_the_first_visible_directory_id() {
        let t = build(&[
            "packages/opencode/src/cli/app.ts",
            "packages/opencode/src/server/server.ts",
        ]);
        let packages = dir(&t, "packages");
        assert_eq!(
            names(&flatten(&t, Some(&BTreeSet::new()))),
            ["packages/opencode/src"]
        );
        assert_eq!(
            names(&flatten(&t, Some(&BTreeSet::from([packages])))),
            ["packages/opencode/src", "cli", "server"]
        );
    }

    #[test]
    fn oc_flattens_only_expanded_directory_descendants_when_expansion_is_provided() {
        let t = build(&["src/config/tui.ts", "src/session/index.ts", "README.md"]);
        let (src, config) = (dir(&t, "src"), dir(&t, "config"));
        assert_eq!(
            names(&flatten(&t, Some(&BTreeSet::new()))),
            ["src", "README.md"]
        );
        assert_eq!(
            names(&flatten(&t, Some(&BTreeSet::from([src])))),
            ["src", "config", "session", "README.md"]
        );
        assert_eq!(
            names(&flatten(&t, Some(&BTreeSet::from([src, config])))),
            ["src", "config", "tui.ts", "session", "README.md"]
        );
    }

    #[test]
    fn oc_finds_the_parent_directories_to_expand_for_a_patch_file() {
        let t = build(&["src/config/tui.ts", "src/session/index.ts", "README.md"]);
        let mut got: Vec<&str> = file_selection(&t, 1)
            .unwrap()
            .iter()
            .map(|id| t.nodes[*id].name.as_str())
            .collect();
        got.sort();
        assert_eq!(got, ["session", "src"]);
        assert!(file_selection(&t, 99).is_none());
    }

    #[test]
    fn oc_prefers_the_selected_file_when_choosing_the_single_patch_file() {
        assert_eq!(single_file(Some(2), Some(0), Some(3)), Some(2));
        assert_eq!(single_file(None, Some(0), Some(3)), Some(0));
        assert_eq!(single_file(None, None, Some(3)), Some(3));
    }

    #[test]
    fn oc_orders_patches_by_the_flattened_file_tree_order() {
        let rows = flatten(
            &build(&[
                "src/dir-8/juniper-4.ts",
                "src/dir-8/harbor-94.ts",
                "src/dir-8/cedar-16.ts",
            ]),
            None,
        );
        assert_eq!(ordered_files(&rows), [2, 1, 0]);
    }

    #[test]
    fn oc_shows_the_diff_viewer_file_tree_only_when_enabled_and_files_exist() {
        assert!(show_tree(true, 1));
        assert!(!show_tree(true, 0));
        assert!(!show_tree(false, 1));
        assert!(!show_tree(false, 0));
    }

    #[test]
    fn oc_moves_patch_selection_through_the_ordered_patch_file_indexes() {
        let order = [2, 1, 0];
        assert_eq!(move_file(&order, None, 1), Some(2));
        assert_eq!(move_file(&order, None, -1), Some(2));
        assert_eq!(move_file(&order, Some(2), 1), Some(1));
        assert_eq!(move_file(&order, Some(1), -1), Some(2));
        assert_eq!(move_file(&order, Some(0), 1), Some(0));
        assert_eq!(move_file(&order, Some(99), 1), Some(2));
        assert_eq!(move_file(&order, Some(99), -1), Some(2));
        assert_eq!(move_file(&[], None, 1), None);
    }

    #[test]
    fn oc_toggles_only_selected_directory_expansion() {
        let t = build(&["src/config/tui.ts", "README.md"]);
        let src = dir(&t, "src");
        let readme = t.nodes.iter().find(|n| n.name == "README.md").unwrap().id;
        let expanded = all_expanded(&t);
        let collapsed = toggle(&t, &expanded, Some(src));
        assert!(!collapsed.contains(&src));
        assert_eq!(
            names(&flatten(&t, Some(&collapsed))),
            ["src/config", "README.md"]
        );
        let reopened = toggle(&t, &collapsed, Some(src));
        assert!(reopened.contains(&src));
        assert_eq!(toggle(&t, &reopened, Some(readme)), reopened);
        assert_eq!(toggle(&t, &reopened, None), reopened);
    }
}
