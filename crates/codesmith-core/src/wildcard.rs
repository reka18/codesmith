//! Permission patterns: `*` matches any run of characters (including `/`),
//! `?` matches one, and a pattern ending in ` *` also matches the bare command
//! (so `git *` matches `git`). Ported from OpenCode's `Wildcard.match`.

pub fn matches(input: &str, pattern: &str, ignore_case: bool) -> bool {
    let input = input.replace('\\', "/");
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
        .map(|r| r.is_match(&input))
        .unwrap_or(false)
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
}
