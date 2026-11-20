//! Permission patterns: `*` matches any run of characters (including `/`),
//! `?` matches one, and a pattern ending in ` *` also matches the bare command
//! (so `git *` matches `git`). Ported from OpenCode's `Wildcard.match`.

use std::sync::Arc;

pub fn matches(input: &str, pattern: &str, ignore_case: bool) -> bool {
    // `*` alone matches everything; skip the regex.
    if pattern == "*" {
        return true;
    }
    let input = if input.contains('\\') {
        std::borrow::Cow::Owned(input.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(input)
    };
    compiled(pattern, ignore_case).is_some_and(|r| r.is_match(&input))
}

/// The regex for a pattern. Permission checks run once per file a search
/// visits, so compiled patterns are kept (a project has few distinct ones);
/// one map per case setting, so a lookup needs no allocation. They are
/// shared, not cloned: a cloned `Regex` starts with empty match caches.
fn compiled(pattern: &str, ignore_case: bool) -> Option<Arc<regex::Regex>> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    type Cache = [HashMap<String, Option<Arc<regex::Regex>>>; 2];
    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
    const MAX: usize = 1024;
    let i = usize::from(ignore_case);
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = &mut guard.get_or_insert_with(Default::default)[i];
    if let Some(r) = cache.get(pattern) {
        return r.clone();
    }
    // Compiling under the lock is fine: it happens once per pattern.
    let r = build(pattern, ignore_case).map(Arc::new);
    if cache.len() >= MAX {
        cache.clear();
    }
    cache.insert(pattern.to_string(), r.clone());
    r
}

fn build(pattern: &str, ignore_case: bool) -> Option<regex::Regex> {
    let mut re = String::from("^");
    let pat = pattern.replace('\\', "/");
    let (body, optional_tail) = match pat.strip_suffix(" *") {
        Some(b) => (b.to_string(), true),
        None => (pat, false),
    };
    for c in body.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    if optional_tail {
        re.push_str("( .*)?");
    }
    re.push('$');
    regex::RegexBuilder::new(&re)
        .dot_matches_new_line(true)
        .case_insensitive(ignore_case || cfg!(windows))
        .build()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn star_and_question() {
        assert!(matches("src/a/b.rs", "src/*", false));
        assert!(matches("anything", "*", false));
        assert!(matches("a.rs", "?.rs", false));
        assert!(!matches("ab.rs", "?.rs", false));
        assert!(matches("", "*", false));
    }

    #[test]
    fn literal_characters_are_escaped() {
        assert!(matches("a.b", "a.b", false));
        assert!(!matches("axb", "a.b", false));
        assert!(matches("f(x)+[1]", "f(x)+[1]", false));
        assert!(matches("$HOME/^x", "$HOME/^x", false));
    }

    #[test]
    fn trailing_space_star_matches_bare_command() {
        assert!(matches("git", "git *", false));
        assert!(matches("git status", "git *", false));
        assert!(!matches("gitk", "git *", false));
    }

    #[test]
    fn env_file_rules() {
        assert!(matches(".env", "*.env", false));
        assert!(matches("app/.env.local", "*.env.*", false));
        assert!(matches("x.env.example", "*.env.example", false));
        assert!(!matches("src/main.rs", "*.env", false));
    }

    #[test]
    fn case_handling() {
        assert!(!matches("README.md", "readme.md", false));
        assert!(matches("README.md", "readme.md", true));
    }

    #[test]
    fn backslashes_normalize() {
        assert!(matches("src\\a.rs", "src/*", false));
    }

    #[test]
    fn multiline_resources() {
        assert!(matches("echo a\necho b", "echo *", false));
    }

    #[test]
    fn cached_patterns_keep_their_case_setting() {
        // The same pattern compiled with and without case folding is two
        // cache entries; neither answers for the other.
        assert!(matches("README.md", "readme.*", true));
        assert!(!matches("README.md", "readme.*", false));
        assert!(matches("README.md", "readme.*", true));
        assert!(matches("a\\b", "a/*", false));
        assert!(matches("a\nb", "*", false), "a lone star matches anything");
        // Far more patterns than the cache holds still match correctly.
        for i in 0..3000 {
            let p = format!("dir{i}/*");
            assert!(matches(&format!("dir{i}/x"), &p, false));
            assert!(!matches(&format!("dir{i}x/x"), &p, false));
        }
    }
}
