//! Every outside page CodeSmith shows a user or opens for them, and the
//! check `scripts/release.sh` runs on them before a release, so a page a
//! provider moved is caught before it ships (`codesmith debug links
//! --check`).
//!
//! A test reads the source and fails on a page link that isn't listed
//! here, so the list can't fall behind the code.

use std::time::Duration;

/// A page CodeSmith shows or opens, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub what: String,
}

fn link(url: &str, what: impl Into<String>) -> Link {
    Link {
        url: url.to_string(),
        what: what.into(),
    }
}

/// Every outside page CodeSmith shows or opens.
pub fn all() -> Vec<Link> {
    use codesmith_core::providers::{copilot, signin, verified};
    let mut out: Vec<Link> = signin::KEY_PAGES
        .iter()
        .map(|(p, u)| link(u, format!("{p} API key page (/login)")))
        .collect();
    out.extend([
        link(
            codesmith_ai::claude_agent::catalog::SETUP_URL,
            "Claude Code setup (claude-agent not installed)",
        ),
        link(
            copilot::PLANS_URL,
            "GitHub Copilot plans (no Copilot access)",
        ),
        link(
            copilot::NEW_APP_URL,
            "GitHub OAuth app registration (Copilot device login)",
        ),
        link(
            codesmith_core::providers::digitalocean::APPS_URL,
            "DigitalOcean OAuth app registration (DigitalOcean login)",
        ),
        link(
            verified::REPO,
            "the project (web fetch user agent, gateway referer)",
        ),
        link(crate::upgrade::RELEASES, "releases (codesmith upgrade)"),
        link(
            verified::CONTRIBUTING,
            "contributing guide (untested provider warning)",
        ),
        link(
            verified::ISSUE_FORM,
            "provider verified issue form (untested provider warning)",
        ),
    ]);
    out
}

/// What a check found for one link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The page opens, or sends the user to sign in first.
    Ok,
    /// It works, but is worth a look: blocked to scripts, a server error,
    /// or a permanent move to another address (given, to list instead).
    Warn(String),
    /// The page is gone: fails the release.
    Broken(String),
}

/// One request's answer: the status and, for a redirect, where to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub url: String,
    pub status: u16,
    pub location: Option<String>,
}

/// Whether `url` is a sign-in page: a page behind a login sends a script
/// there, and that counts as working.
fn sign_in(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let (host, path) = match lower.split_once("://").map(|(_, r)| r) {
        Some(rest) => rest.split_once('/').unwrap_or((rest, "")),
        None => return false,
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    ["login", "signin", "sign-in", "sign_in", "auth", "sso"]
        .iter()
        .any(|w| path.contains(w))
        || host.starts_with("accounts.")
        || host.starts_with("auth.")
        || host.starts_with("login.")
        || host.contains(".auth.")
}

/// Judge the chain of answers a link got (`hops`, in order), or the error
/// that stopped it.
pub fn classify(hops: &[Hop], error: Option<&str>) -> Verdict {
    if let Some(e) = error {
        return Verdict::Broken(format!("no answer: {e}"));
    }
    let Some(last) = hops.last() else {
        return Verdict::Broken("no answer".into());
    };
    let moved = hops
        .first()
        .filter(|h| matches!(h.status, 301 | 308))
        .and_then(|_| {
            // The address the move ends at, before any sign-in page.
            hops.iter()
                .skip(1)
                .take_while(|h| !sign_in(&h.url))
                .last()
                .map(|h| h.url.clone())
                .or_else(|| hops[0].location.clone().filter(|l| !sign_in(l)))
        });
    match last.status {
        200..=299 => match moved {
            Some(to) if to != hops[0].url => Verdict::Warn(format!("moved permanently to {to}")),
            _ => Verdict::Ok,
        },
        300..=399 => Verdict::Broken(format!("too many redirects (last {})", last.url)),
        401 | 403 | 429 => {
            if sign_in(&last.url) {
                Verdict::Ok
            } else {
                Verdict::Warn(format!(
                    "{}: blocked to scripts; open it in a browser to check",
                    last.status
                ))
            }
        }
        404 | 410 => Verdict::Broken(format!("{}: page not found at {}", last.status, last.url)),
        500..=599 => Verdict::Warn(format!("{}: server error, try again later", last.status)),
        s => Verdict::Broken(format!("{s} at {}", last.url)),
    }
}

/// At most this many redirects are followed.
const MAX_HOPS: usize = 10;

/// Request `url` and each redirect after it, by hand so each step is seen.
async fn fetch(client: &reqwest::Client, url: &str) -> (Vec<Hop>, Option<String>) {
    let mut hops = Vec::new();
    let mut at = url.to_string();
    for _ in 0..=MAX_HOPS {
        let resp = match client.get(&at).send().await {
            Ok(r) => r,
            Err(e) => return (hops, Some(e.without_url().to_string())),
        };
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|l| reqwest::Url::parse(&at).ok()?.join(l).ok())
            .map(|u| u.to_string());
        hops.push(Hop {
            url: at.clone(),
            status,
            location: location.clone(),
        });
        match (resp.status().is_redirection(), location) {
            (true, Some(next)) => at = next,
            _ => return (hops, None),
        }
    }
    (hops, None)
}

/// Whether an answer is worth asking again: none, a server error or a
/// rate limit, which are often gone a moment later.
fn retry(hops: &[Hop], error: Option<&str>) -> bool {
    error.is_some()
        || hops
            .last()
            .is_some_and(|h| h.status >= 500 || h.status == 429)
}

/// Check one link, asking once more after a second when the first answer
/// was none, a server error or a rate limit.
pub async fn check(client: &reqwest::Client, url: &str) -> Verdict {
    let (hops, err) = fetch(client, url).await;
    if retry(&hops, err.as_deref()) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let (hops, err) = fetch(client, url).await;
        return classify(&hops, err.as_deref());
    }
    classify(&hops, err.as_deref())
}

/// The client the check uses: no automatic redirects, a short timeout.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .user_agent(concat!(
            "Mozilla/5.0 (compatible; CodeSmith-LinkCheck/",
            env!("CARGO_PKG_VERSION"),
            "; +https://github.com/reka18/codesmith)"
        ))
        .build()
        .expect("an HTTP client")
}

/// `codesmith debug links`: list the links, or with `check` request each
/// one and fail when any is gone.
pub async fn run(check_them: bool) -> anyhow::Result<()> {
    let links = all();
    if !check_them {
        for l in &links {
            println!("{}\t{}", l.url, l.what);
        }
        return Ok(());
    }
    let client = client();
    let verdicts =
        futures_util::future::join_all(links.iter().map(|l| check(&client, &l.url))).await;
    let mut broken = 0;
    for (l, v) in links.iter().zip(&verdicts) {
        match v {
            Verdict::Ok => println!("ok      {}", l.url),
            Verdict::Warn(why) => println!("warn    {}  ({}): {why}", l.url, l.what),
            Verdict::Broken(why) => {
                broken += 1;
                println!("BROKEN  {}  ({}): {why}", l.url, l.what);
            }
        }
    }
    if broken > 0 {
        anyhow::bail!(
            "{broken} of {} links are broken; fix them before releasing",
            links.len()
        );
    }
    println!("all {} links answer", links.len());
    Ok(())
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
