use super::*;

fn hop(url: &str, status: u16, location: Option<&str>) -> Hop {
    Hop {
        url: url.into(),
        status,
        location: location.map(str::to_string),
    }
}

// ------------------------------------------------------------ the list

#[test]
fn every_link_is_https_parses_and_is_listed_once() {
    let links = all();
    assert!(links.len() >= 15, "{links:?}");
    let mut seen = std::collections::HashSet::new();
    for l in &links {
        assert!(l.url.starts_with("https://"), "{}", l.url);
        assert!(reqwest::Url::parse(&l.url).is_ok(), "{}", l.url);
        assert!(!l.what.is_empty(), "{}", l.url);
        assert!(seen.insert(l.url.clone()), "listed twice: {}", l.url);
    }
    // The pages the untested-provider warning and /login point to.
    for want in [
        codesmith_core::providers::verified::CONTRIBUTING,
        codesmith_core::providers::verified::ISSUE_FORM,
        "https://app.fireworks.ai/settings/users/api-keys",
    ] {
        assert!(links.iter().any(|l| l.url == want), "{want}");
    }
}

/// URLs in the source that aren't pages for people: API endpoints, OAuth
/// issuers and scopes, defaults a user's config overrides. Matched by
/// prefix.
const NOT_PAGES: &[&str] = &[
    "https://api.",
    "https://auth.openai.com",
    "https://chatgpt.com/backend-api",
    "https://models.dev",
    "https://mcp.",
    "https://mcp-eu.",
    "https://docs.mcp.cloudflare.com/mcp",
    "https://www.googleapis.com/",
    "https://oauth2.googleapis.com",
    "https://cognitiveservices.azure.com/",
    "https://ai.azure.com/",
    "https://management.azure.com",
    "https://login.microsoftonline.com",
    "https://github.com/login/",
    "https://poe.com",
    "https://openrouter.ai/api",
    "https://open.bigmodel.cn/",
    "https://bedrock",
    "https://sts.",
    "https://generativelanguage.",
    "https://aiplatform.",
    "https://inference.",
    "https://integrate.",
    "https://router.",
    "https://gateway.",
    "https://opencode.ai/",
    "https://cloud.digitalocean.com/v1/oauth/",
    "https://auth.x.ai/",
    "https://zenmux.ai/api/",
    "https://gitlab.com",
    "https://github.com/",
];

/// Whether `url` is example data: an `.example` or `example.com` host, or a
/// format string's truncated host (no dot, as in `https://{host}`).
fn example(url: &str) -> bool {
    let host = url["https://".len()..]
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or("");
    !host.contains('.')
        || host.ends_with(".example")
        || host.ends_with(".test")
        || host == "example.com"
        || host.ends_with(".example.com")
        || url.starts_with("https://github.com/acme/")
}

/// The Rust files under `crates/` (no `target`, test or bench folders).
fn rust_files() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..")];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                if !["target", "tests", "benches", "fixtures", "examples"].contains(&name.as_str())
                {
                    stack.push(p);
                }
            } else if name.ends_with(".rs") {
                out.push(p);
            }
        }
    }
    out
}

/// The `https://` URLs in one file's code, skipping comments and
/// everything from `#[cfg(test)]` on.
fn urls_in(text: &str) -> Vec<String> {
    let code = text.split("#[cfg(test)]").next().unwrap();
    let mut out = Vec::new();
    for line in code.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut rest = line;
        while let Some(i) = rest.find("https://") {
            let url: String = rest[i..]
                .chars()
                .take_while(|c| !c.is_whitespace() && !"\"'`)<>\\{".contains(*c))
                .collect();
            rest = &rest[i + url.len()..];
            out.push(url.trim_end_matches(['.', ',', ';', ':']).to_string());
        }
    }
    out
}

/// The `https://` URLs in non-test Rust code under `crates/`. Skipped
/// besides what `urls_in` skips: files a `#[path = ...]` pulls in (all
/// test modules here), `*_tests.rs`, `tests.rs` and the OpenCode parity
/// suites (`parity*.rs`, reached only from test modules).
fn source_urls() -> Vec<(String, String)> {
    let files = rust_files();
    let mut included = std::collections::HashSet::new();
    for p in &files {
        for line in std::fs::read_to_string(p).unwrap().lines() {
            if let Some(rest) = line.trim().strip_prefix("#[path = \"") {
                let name = rest.split('"').next().unwrap();
                included.insert(name.rsplit('/').next().unwrap().to_string());
            }
        }
    }
    let mut out = Vec::new();
    for p in files {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if included.contains(&name)
            || name.ends_with("_tests.rs")
            || name == "tests.rs"
            || name.starts_with("parity")
        {
            continue;
        }
        for url in urls_in(&std::fs::read_to_string(&p).unwrap()) {
            out.push((url, p.display().to_string()));
        }
    }
    out
}

#[test]
fn every_page_link_in_the_source_is_listed() {
    let listed: Vec<String> = all().into_iter().map(|l| l.url).collect();
    let found = source_urls();
    assert!(found.len() > 20, "the scan found too little: {found:?}");
    let mut missing = Vec::new();
    for (url, file) in &found {
        let bare = url.as_str() == "https://";
        let listed_here = listed.iter().any(|l| l == url);
        let not_page = NOT_PAGES.iter().any(|p| url.starts_with(p));
        if !bare && !listed_here && !not_page && !example(url) {
            missing.push(format!("{url}  ({file})"));
        }
    }
    assert!(
        missing.is_empty(),
        "page links not in links::all() (add them there, or to NOT_PAGES if they aren't pages):\n{}",
        missing.join("\n")
    );
}

#[test]
fn the_scan_reads_code_and_skips_comments_and_tests() {
    let text = "const A: &str = \"https://a.dev/k\";\n\
                // see https://comment.dev/x\n\
                let b = format!(\"at https://b.dev/p. Then {x}\");\n\
                let c = \"(https://c.dev/q)\";\n\
                #[cfg(test)]\n\
                mod tests { const T: &str = \"https://test.dev/\"; }\n";
    assert_eq!(
        urls_in(text),
        ["https://a.dev/k", "https://b.dev/p", "https://c.dev/q"]
    );
    let found = source_urls();
    assert!(found
        .iter()
        .any(|(u, _)| u == "https://app.fireworks.ai/settings/users/api-keys"));
    assert!(!found.iter().any(|(_, f)| f.ends_with("links_tests.rs")));
    for (url, _) in [
        ("https://x.example/a", ()),
        ("https://example.com/r.git", ()),
        ("https://{host}", ()),
        ("https://sts", ()),
    ] {
        assert!(example(url), "{url}");
    }
    assert!(!example("https://app.fireworks.ai/settings/users/api-keys"));
}

// ------------------------------------------------------------ verdicts

#[test]
fn a_page_that_opens_is_ok() {
    assert_eq!(
        classify(&[hop("https://a.dev/k", 200, None)], None),
        Verdict::Ok
    );
}

#[test]
fn a_page_behind_a_login_is_ok() {
    for wall in [
        "https://a.dev/login?next=/k",
        "https://a.dev/sign-in",
        "https://accounts.google.com/v3/signin/identifier",
        "https://v2.auth.mistral.ai/login?flow=1",
        "https://a.dev/oauth/authorize",
    ] {
        let hops = [
            hop("https://a.dev/k", 302, Some(wall)),
            hop(wall, 200, None),
        ];
        assert_eq!(classify(&hops, None), Verdict::Ok, "{wall}");
        // A login page that answers 401 or 403 is a login wall too.
        let hops = [
            hop("https://a.dev/k", 302, Some(wall)),
            hop(wall, 403, None),
        ];
        assert_eq!(classify(&hops, None), Verdict::Ok, "{wall}");
    }
}

#[test]
fn a_temporary_redirect_to_a_page_is_ok() {
    let hops = [
        hop("https://a.dev/k", 302, Some("https://a.dev/k2")),
        hop("https://a.dev/k2", 200, None),
    ];
    assert_eq!(classify(&hops, None), Verdict::Ok);
}

#[test]
fn a_permanent_move_warns_with_the_new_address() {
    for code in [301, 308] {
        let hops = [
            hop("https://old.dev/k", code, Some("https://new.dev/k")),
            hop("https://new.dev/k", 200, None),
        ];
        assert_eq!(
            classify(&hops, None),
            Verdict::Warn("moved permanently to https://new.dev/k".into())
        );
    }
    // Moved, then behind a login: the address before the login is given.
    let hops = [
        hop("https://old.dev/k", 301, Some("https://new.dev/k")),
        hop("https://new.dev/k", 302, Some("https://new.dev/login")),
        hop("https://new.dev/login", 200, None),
    ];
    assert_eq!(
        classify(&hops, None),
        Verdict::Warn("moved permanently to https://new.dev/k".into())
    );
    // Moved straight to a login page: nothing to list instead.
    let hops = [
        hop("https://a.dev/k", 301, Some("https://a.dev/login")),
        hop("https://a.dev/login", 200, None),
    ];
    assert_eq!(classify(&hops, None), Verdict::Ok);
}

#[test]
fn a_missing_page_is_broken_even_after_redirects() {
    for code in [404, 410] {
        assert!(matches!(
            classify(&[hop("https://a.dev/k", code, None)], None),
            Verdict::Broken(_)
        ));
    }
    // Fireworks' old page: a redirect to another host that is a 404.
    let hops = [
        hop(
            "https://fireworks.ai/account/api-keys",
            308,
            Some("https://app.fireworks.ai/account/api-keys"),
        ),
        hop("https://app.fireworks.ai/account/api-keys", 404, None),
    ];
    let Verdict::Broken(why) = classify(&hops, None) else {
        panic!("not broken")
    };
    assert!(why.contains("app.fireworks.ai/account/api-keys"), "{why}");
}

#[test]
fn blocked_and_failing_servers_warn_but_dont_fail() {
    for code in [401, 403, 429] {
        assert!(matches!(
            classify(&[hop("https://a.dev/k", code, None)], None),
            Verdict::Warn(_)
        ));
    }
    for code in [500, 502, 503] {
        assert!(matches!(
            classify(&[hop("https://a.dev/k", code, None)], None),
            Verdict::Warn(_)
        ));
    }
}

#[test]
fn no_answer_and_odd_answers_are_broken() {
    assert!(matches!(
        classify(&[], Some("dns error")),
        Verdict::Broken(_)
    ));
    assert!(matches!(classify(&[], None), Verdict::Broken(_)));
    for code in [400, 405, 418, 451] {
        assert!(matches!(
            classify(&[hop("https://a.dev/k", code, None)], None),
            Verdict::Broken(_)
        ));
    }
    // Redirects that never end.
    let hops: Vec<Hop> = (0..=MAX_HOPS)
        .map(|_| hop("https://a.dev/loop", 302, Some("https://a.dev/loop")))
        .collect();
    assert!(matches!(classify(&hops, None), Verdict::Broken(_)));
}

#[test]
fn sign_in_pages_are_told_apart_from_pages_about_signing_in() {
    for yes in [
        "https://a.dev/login",
        "https://a.dev/u/signin?x=1",
        "https://auth.a.dev/",
        "https://login.a.dev/x",
        "https://accounts.a.dev/x",
        "https://a.dev/sso/start",
    ] {
        assert!(sign_in(yes), "{yes}");
    }
    for no in [
        "https://a.dev/settings/keys",
        "https://a.dev/keys?next=login",
        "https://a.dev/#login",
        "https://author.dev/keys",
        "not a url",
    ] {
        assert!(!sign_in(no), "{no}");
    }
}

#[test]
fn verdicts_never_panic_on_any_chain() {
    // A bounded property check over random chains of answers.
    let codes = [
        200, 204, 301, 302, 303, 307, 308, 401, 403, 404, 410, 429, 500, 503, 418,
    ];
    let urls = [
        "https://a.dev/k",
        "https://a.dev/login",
        "https://b.dev/x",
        "https://auth.c.dev/",
    ];
    let mut seed: u32 = 0x1234_5678;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as usize
    };
    for _ in 0..3000 {
        let n = next() % 6;
        let hops: Vec<Hop> = (0..n)
            .map(|_| {
                let s = codes[next() % codes.len()];
                let loc = (300..400).contains(&s).then(|| urls[next() % urls.len()]);
                hop(urls[next() % urls.len()], s, loc)
            })
            .collect();
        let v = classify(&hops, None);
        // A chain ending in a 404 is always broken.
        if hops.last().is_some_and(|h| h.status == 404) {
            assert!(matches!(v, Verdict::Broken(_)), "{hops:?} -> {v:?}");
        }
    }
}

// ------------------------------------------------------------ requests

static FLAKY_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A local server with canned answers by path, for `check`.
fn server() -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap_or(0);
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (status, location) = match path.as_str() {
                "/ok" => ("200 OK", None),
                "/gone" => ("404 Not Found", None),
                "/moved" => ("301 Moved Permanently", Some("/ok")),
                "/wall" => ("302 Found", Some("/login?next=/wall")),
                "/blocked" => ("403 Forbidden", None),
                "/loop" => ("302 Found", Some("/loop")),
                "/to-gone" => ("308 Permanent Redirect", Some("/gone")),
                "/flaky" => {
                    // A server error the first time, then the page.
                    let first = !FLAKY_SEEN.swap(true, std::sync::atomic::Ordering::SeqCst);
                    if first {
                        ("503 Service Unavailable", None)
                    } else {
                        ("200 OK", None)
                    }
                }
                "/down" => ("503 Service Unavailable", None),
                p if p.starts_with("/login") => ("200 OK", None),
                _ => ("404 Not Found", None),
            };
            let mut stream = stream;
            let loc = location
                .map(|l| format!("Location: {l}\r\n"))
                .unwrap_or_default();
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\n{loc}Content-Length: 2\r\nConnection: close\r\n\r\nhi"
            );
        }
    });
    base
}

#[tokio::test]
async fn check_follows_each_step_and_judges_the_end() {
    let base = server();
    let c = client();
    let at = |p: &str| format!("{base}{p}");
    assert_eq!(check(&c, &at("/ok")).await, Verdict::Ok);
    assert_eq!(check(&c, &at("/wall")).await, Verdict::Ok);
    assert_eq!(
        check(&c, &at("/moved")).await,
        Verdict::Warn(format!("moved permanently to {}", at("/ok")))
    );
    assert!(matches!(check(&c, &at("/gone")).await, Verdict::Broken(_)));
    assert!(matches!(
        check(&c, &at("/to-gone")).await,
        Verdict::Broken(_)
    ));
    assert!(matches!(check(&c, &at("/blocked")).await, Verdict::Warn(_)));
    assert!(matches!(check(&c, &at("/loop")).await, Verdict::Broken(_)));
    // A server error once is asked again; one that stays only warns.
    assert_eq!(check(&c, &at("/flaky")).await, Verdict::Ok);
    assert!(matches!(check(&c, &at("/down")).await, Verdict::Warn(_)));
}

#[test]
fn only_no_answer_server_errors_and_rate_limits_are_asked_again() {
    assert!(retry(&[], Some("timeout")));
    for s in [429, 500, 503] {
        assert!(retry(&[hop("https://a.dev/", s, None)], None), "{s}");
    }
    for s in [200, 301, 403, 404] {
        assert!(!retry(&[hop("https://a.dev/", s, None)], None), "{s}");
    }
}

#[tokio::test]
async fn a_host_that_doesnt_answer_is_broken() {
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let v = check(&client(), &format!("http://127.0.0.1:{port}/x")).await;
    let Verdict::Broken(why) = v else {
        panic!("{v:?}")
    };
    assert!(why.starts_with("no answer"), "{why}");
}
