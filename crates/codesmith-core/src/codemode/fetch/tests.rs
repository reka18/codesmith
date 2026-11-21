// ---------------------------------------------------------------------
// Ported from OpenCode's `packages/core/test/tool-execute.test.ts`
// ("execute rejects filesystem fetches", "streaming fetch" and "fetch
// permissions"; MIT). Requests go to a loopback server, never the
// internet.
//
// Intentional differences:
// - "Always" saves the URL's origin (`http://host` and `http://host/*`),
//   as the webfetch tool does, instead of `*` (every URL).
// - A response is read whole, up to the webfetch limit, before `fetch`
//   resolves: `res.body` is a one-chunk reader over those bytes, not a
//   live stream, so OpenCode's tests of reading the first chunk of a body
//   that never ends, and of interrupting a pending request, have no
//   counterpart. The redirect test reads the destination the same way.
// - The response's content type is the server's (Bun adds `;charset=utf-8`
//   to `Response.json`).
// ---------------------------------------------------------------------
use super::*;
use crate::codemode::fixture::{gate, serve, Answer};
use crate::codemode::{execute_with, Outcome};
use crate::permission::{Effect, Reply, Rule};
use serde_json::json;

async fn program(g: &Gate, code: &str) -> Outcome {
    execute_with(
        None,
        code,
        &[],
        50,
        Duration::from_secs(30),
        |tool, input| async move {
            if tool != TOOL {
                return Err(format!("no tool {tool}"));
            }
            run(g, &input).await.map(|v| result_text(&v))
        },
    )
    .await
}

fn allow_all() -> Vec<Rule> {
    vec![Rule::new("webfetch", "*", Effect::Allow)]
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_rejects_filesystem_fetches() {
    let (g, asked) = gate(allow_all(), Reply::Once);
    let out = program(&g, r#"return await fetch("file:///blocked.txt")"#).await;
    assert!(out
        .result
        .unwrap_err()
        .contains("fetch cannot be used for filesystem access."));
    assert!(asked.asked.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_fetch_asks_the_webfetch_permission_for_its_url_as_the_execute_call() {
    let (base, hits) = serve(|_| Answer::new(200, "text/plain", "ok")).await;
    let (g, asked) = gate(vec![], Reply::Once);
    let url = format!("{base}/allowed");
    let out = program(
        &g,
        &format!(r#"return await (await fetch("{url}", {{ method: "POST" }})).text()"#),
    )
    .await;
    assert_eq!(out.result, Ok(json!("ok")));
    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        (hits[0].method.as_str(), hits[0].path()),
        ("POST", "/allowed")
    );
    let asked = asked.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].action, "webfetch");
    assert_eq!(asked[0].resources, vec![url.clone()]);
    // Narrower than OpenCode's `*`: the origin only.
    assert_eq!(asked[0].save, vec![base.clone(), format!("{base}/*")]);
    assert_eq!(asked[0].title, format!("Fetch {url} (POST)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_a_redirect_to_a_url_denied_for_webfetch_is_never_requested() {
    let (base, hits) = serve(|s| {
        if s.path() == "/redirect" {
            Answer::redirect(302, "/blocked")
        } else {
            Answer::new(200, "text/plain", "ok")
        }
    })
    .await;
    let mut rules = allow_all();
    rules.push(Rule::new(
        "webfetch",
        &format!("{base}/blocked"),
        Effect::Deny,
    ));
    let (g, _) = gate(rules, Reply::Once);
    let out = program(
        &g,
        &format!(r#"try {{ await fetch("{base}/redirect") }} catch (e) {{ return e.message }}"#),
    )
    .await;
    assert!(out
        .result
        .unwrap()
        .as_str()
        .unwrap()
        .contains("Permission denied"));
    let hits: Vec<String> = hits
        .lock()
        .unwrap()
        .iter()
        .map(|s| format!("{} {}", s.method, s.path()))
        .collect();
    assert_eq!(hits, vec!["GET /redirect"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_fetch_matches_rules_without_the_fragment_and_rejects_credentials_in_the_url() {
    let (base, hits) = serve(|_| Answer::new(200, "text/plain", "ok")).await;
    let (g, asked) = gate(vec![], Reply::Once);
    let host = base.trim_start_matches("http://");
    let out = program(
        &g,
        &format!(
            r#"await fetch("{base}/allowed#top"); try {{ await fetch("http://user@{host}/") }} catch (e) {{ return e.message }}"#
        ),
    )
    .await;
    assert_eq!(
        out.result,
        Ok(json!("fetch cannot be used with credentials in the URL."))
    );
    let resources: Vec<Vec<String>> = asked
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.resources.clone())
        .collect();
    assert_eq!(resources, vec![vec![format!("{base}/allowed")]]);
    let hits: Vec<String> = hits
        .lock()
        .unwrap()
        .iter()
        .map(|s| format!("{} {}", s.method, s.path()))
        .collect();
    assert_eq!(hits, vec!["GET /allowed"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_a_rejection_with_feedback_reaches_the_program_as_the_error_message() {
    let (base, hits) = serve(|_| Answer::new(200, "text/plain", "ok")).await;
    let (g, _) = gate(
        vec![],
        Reply::Reject(Some("use the docs site instead".into())),
    );
    let url = format!("{base}/corrected");
    let out = program(
        &g,
        &format!(r#"try {{ await fetch("{url}") }} catch (e) {{ return e.message }}"#),
    )
    .await;
    assert_eq!(
        out.result,
        Ok(json!(format!(
            "The user rejected fetching {url}: use the docs site instead"
        )))
    );
    assert!(hits.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_a_url_denied_for_webfetch_is_never_requested_and_rejects_like_a_failed_fetch() {
    let (base, hits) = serve(|_| Answer::new(200, "text/plain", "ok")).await;
    let url = format!("{base}/denied");
    let (g, _) = gate(vec![Rule::new("webfetch", &url, Effect::Deny)], Reply::Once);
    let out = program(
        &g,
        &format!(
            r#"const [denied] = await Promise.allSettled([fetch("{url}")]); return denied.status"#
        ),
    )
    .await;
    assert_eq!(out.result, Ok(json!("rejected")));
    assert!(hits.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_whole_body_methods_share_consumption_and_expose_an_empty_body() {
    let (base, _) = serve(|s| {
        if s.method == "HEAD" {
            Answer::new(200, "", Vec::new())
        } else {
            Answer::new(200, "application/json;charset=utf-8", "42")
        }
    })
    .await;
    let (g, _) = gate(allow_all(), Reply::Once);
    let out = program(
        &g,
        &format!(
            r#"const res = await fetch("{base}/")
      const before = res.bodyUsed
      const number = await res.json()
      const second = await res.text().catch(e => e.name)
      const bytes = await (await fetch(res.url)).bytes()
      const buffer = await (await fetch(res.url)).arrayBuffer()
      const empty = await fetch(res.url, {{ method: "HEAD" }})
      return [res.headers.get("Content-Type"), before, number, res.bodyUsed, second, [...bytes], [...new Uint8Array(buffer)], empty.body === null]"#
        ),
    )
    .await;
    assert_eq!(
        out.result,
        Ok(json!([
            "application/json;charset=utf-8",
            false,
            42,
            true,
            "TypeError",
            [52, 50],
            [52, 50],
            true
        ]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_discards_a_redirect_body_before_reading_the_destination() {
    let (base, _) = serve(|s| {
        if s.path() == "/" {
            let mut a = Answer::redirect(302, "/next");
            a.body = b"redirect body".to_vec();
            a
        } else {
            Answer::new(200, "text/plain", "ok")
        }
    })
    .await;
    let (g, _) = gate(allow_all(), Reply::Once);
    let out = program(
        &g,
        &format!(
            r#"const res = await fetch("{base}/")
      const reader = res.body.getReader()
      const first = await reader.read()
      const end = await reader.read()
      reader.releaseLock()
      return [res.redirected, [...first.value], end.done, res.body.locked, await res.text().catch(e => e.name)]"#
        ),
    )
    .await;
    assert_eq!(
        out.result,
        Ok(json!([true, [111, 107], true, false, "TypeError"]))
    );
}

// ----- CodeSmith's own -------------------------------------------------

/// "Always" covers the origin asked about, and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn always_saves_only_the_origin() {
    let (base, hits) = serve(|_| Answer::new(200, "text/plain", "ok")).await;
    let other = base.replace("127.0.0.1", "localhost");
    let (g, asked) = gate(vec![], Reply::Always);
    let out = program(
        &g,
        &format!(
            r#"await fetch("{base}/a"); await fetch("{base}/b?x=1"); await fetch("{other}/c"); return "done""#
        ),
    )
    .await;
    assert_eq!(out.result, Ok(json!("done")));
    let resources: Vec<String> = asked
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.resources[0].clone())
        .collect();
    assert_eq!(resources, vec![format!("{base}/a"), format!("{other}/c")]);
    assert_eq!(hits.lock().unwrap().len(), 3);
}

/// A redirect to another origin keeps the program's own headers but not
/// its credentials; CodeSmith never adds any.
#[tokio::test(flavor = "multi_thread")]
async fn a_cross_origin_redirect_drops_credentials() {
    let (base, hits) = serve(|s| match s.path() {
        "/start" => Answer::redirect(307, &format!("{}/end", s.headers["x-other"])),
        "/same" => Answer::redirect(302, "/start"),
        _ => Answer::new(200, "text/plain", "ok"),
    })
    .await;
    let other = base.replace("127.0.0.1", "localhost");
    let (g, _) = gate(allow_all(), Reply::Once);
    let out = program(
        &g,
        &format!(
            r#"const r = await fetch("{base}/same", {{ headers: {{ Authorization: "Bearer t0ken", Cookie: "s=1", "X-Other": "{other}", "X-Keep": "yes" }} }})
      return [r.url, r.redirected, await r.text()]"#
        ),
    )
    .await;
    assert_eq!(out.result, Ok(json!([format!("{other}/end"), true, "ok"])));
    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 3);
    // Within the origin they are still sent.
    for h in &hits[..2] {
        assert_eq!(
            h.headers.get("authorization").map(String::as_str),
            Some("Bearer t0ken")
        );
        assert_eq!(h.headers.get("cookie").map(String::as_str), Some("s=1"));
    }
    let last = &hits[2];
    assert!(!last.headers.contains_key("authorization"));
    assert!(!last.headers.contains_key("cookie"));
    assert_eq!(last.headers.get("x-keep").map(String::as_str), Some("yes"));
    assert_eq!(
        last.headers.get("user-agent").map(String::as_str),
        Some("codesmith")
    );
}

/// 303 (and 301/302 after a POST) becomes a GET without the body and its
/// headers; 307 keeps both.
#[tokio::test(flavor = "multi_thread")]
async fn redirects_change_the_method_as_a_browser_does() {
    let (base, hits) = serve(|s| match s.path() {
        "/see-other" => Answer::redirect(303, "/done"),
        "/temporary" => Answer::redirect(307, "/done"),
        _ => Answer::new(200, "text/plain", "ok"),
    })
    .await;
    let (g, _) = gate(allow_all(), Reply::Once);
    let out = program(
        &g,
        &format!(
            r#"await fetch("{base}/see-other", {{ method: "POST", body: "a=1" }})
      await fetch("{base}/temporary", {{ method: "POST", body: "b=2" }})
      return true"#
        ),
    )
    .await;
    assert_eq!(out.result, Ok(json!(true)));
    let hits = hits.lock().unwrap();
    let seen: Vec<(String, String, String, bool)> = hits
        .iter()
        .map(|h| {
            (
                h.method.clone(),
                h.path().to_string(),
                String::from_utf8_lossy(&h.body).to_string(),
                h.headers.contains_key("content-type"),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("POST".into(), "/see-other".into(), "a=1".into(), true),
            ("GET".into(), "/done".into(), "".into(), false),
            ("POST".into(), "/temporary".into(), "b=2".into(), true),
            ("POST".into(), "/done".into(), "b=2".into(), true),
        ]
    );
}

/// Non-HTTP redirect targets, credentials in a redirect, and endless
/// chains fail; each hop is checked against the deny rules.
#[tokio::test(flavor = "multi_thread")]
async fn redirect_targets_are_checked_like_the_first_url() {
    let (base, hits) = serve(|s| match s.path() {
        "/file" => Answer::redirect(302, "file:///etc/passwd"),
        "/user" => Answer::redirect(302, "http://user:pw@127.0.0.1:1/"),
        _ => Answer::redirect(302, "/loop"),
    })
    .await;
    let (g, _) = gate(allow_all(), Reply::Once);
    let out = program(
        &g,
        &format!(
            r#"const m = async (u) => {{ try {{ await fetch(u); return "fetched" }} catch (e) {{ return e.message }} }}
      return [await m("{base}/file"), await m("{base}/user"), await m("{base}/loop")]"#
        ),
    )
    .await;
    assert_eq!(
        out.result,
        Ok(json!([
            "fetch cannot follow a redirect to file: URLs.",
            "fetch cannot follow a redirect with credentials in the URL.",
            format!("Too many redirects (more than {})", webfetch::MAX_REDIRECTS),
        ]))
    );
    // The loop stops after the limit: the first request and ten hops.
    assert_eq!(hits.lock().unwrap().len(), 2 + 1 + webfetch::MAX_REDIRECTS);
}

/// Responses are capped at the webfetch limit, declared or streamed.
#[tokio::test(flavor = "multi_thread")]
async fn responses_are_capped() {
    let (base, _) = serve(|s| {
        if s.path() == "/declared" {
            let mut a = Answer::new(200, "text/plain", "x");
            a.claimed = Some(webfetch::MAX_BYTES as u64 + 1);
            a.length = false;
            a
        } else {
            let mut a = Answer::new(
                200,
                "application/octet-stream",
                vec![0u8; webfetch::MAX_BYTES + 1],
            );
            a.length = false;
            a
        }
    })
    .await;
    let (g, _) = gate(allow_all(), Reply::Once);
    for path in ["declared", "streamed"] {
        let out = program(&g, &format!(r#"await fetch("{base}/{path}")"#)).await;
        assert!(
            out.result
                .as_ref()
                .unwrap_err()
                .contains("Response too large"),
            "{path}: {:?}",
            out.result
        );
    }
}

/// Binary bodies arrive intact; a program can send bytes and form data.
#[tokio::test(flavor = "multi_thread")]
async fn bodies_round_trip_as_bytes() {
    let (base, hits) =
        serve(|s| Answer::new(200, "application/octet-stream", s.body.clone())).await;
    let (g, _) = gate(allow_all(), Reply::Once);
    let out = program(
        &g,
        &format!(
            r#"const raw = await (await fetch("{base}/echo", {{ method: "PUT", body: new Uint8Array([0, 255, 7]) }})).bytes()
      const form = await (await fetch("{base}/form", {{ method: "POST", body: new URLSearchParams({{ q: "a b" }}) }})).text()
      return [[...raw], form]"#
        ),
    )
    .await;
    assert_eq!(out.result, Ok(json!([[0, 255, 7], "q=a+b"])));
    let hits = hits.lock().unwrap();
    assert_eq!(
        hits[1].headers.get("content-type").map(String::as_str),
        Some("application/x-www-form-urlencoded;charset=UTF-8")
    );
}

#[test]
fn bad_input_fails_before_anything_is_asked() {
    for (input, want) in [
        (
            json!({"url": "http://x", "method": "CONNECT"}),
            "unsupported",
        ),
        (
            json!({"url": "http://x", "method": "B A D"}),
            "not a valid HTTP method",
        ),
        (
            json!({"url": "http://x", "headers": [["bad name", "v"]]}),
            "not a valid header name",
        ),
        (
            json!({"url": "http://x", "headers": [["x", "a\r\nb"]]}),
            "is not valid",
        ),
        (json!({"url": "http://x", "body": "aGk="}), "GET/HEAD"),
    ] {
        let e = parse_input(&input).err().unwrap();
        assert!(e.contains(want), "{input}: {e}");
    }
    let (_, r) = parse_input(
        &json!({"url": "http://x", "method": "POST", "headers": [["Host", "evil"], ["X", "1"]]}),
    )
    .unwrap();
    assert_eq!(r.headers, vec![("x".to_string(), "1".to_string())]);
}

/// A deny rule holds however the URL is spelled: the rules see it as it
/// is requested, and its written and percent-decoded forms are checked
/// too. (Security review 7: an uppercase scheme got past the deny.)
#[tokio::test(flavor = "multi_thread")]
async fn deny_rules_match_the_url_as_requested() {
    let (base, hits) = serve(|_| Answer::new(200, "text/plain", "secret")).await;
    let port = base.rsplit(':').next().unwrap().to_string();
    let mut rules = allow_all();
    rules.push(Rule::new(
        "webfetch",
        &format!("{base}/secret*"),
        Effect::Deny,
    ));
    let (g, asked) = gate(rules, Reply::Reject(None));
    for url in [
        base.replace("http://", "HTTP://") + "/secret",
        format!("http://127.0.0.\t1:{port}/secret"),
        format!("{base}/%73ecret"),
        format!("{base}/a/../secret"),
    ] {
        let out = program(
            &g,
            &format!(
                r#"try {{ return await (await fetch({})).text() }} catch (e) {{ return e.message }}"#,
                json!(url)
            ),
        )
        .await;
        let msg = out.result.unwrap();
        assert!(
            msg.as_str().unwrap().starts_with("Permission denied"),
            "{url}: {msg}"
        );
    }
    assert!(hits.lock().unwrap().is_empty());
    assert!(asked.asked.lock().unwrap().is_empty());
}

/// What is asked is the URL as requested.
#[tokio::test(flavor = "multi_thread")]
async fn the_url_is_asked_as_requested() {
    let (base, _) = serve(|_| Answer::new(200, "text/plain", "ok")).await;
    let (g, asked) = gate(vec![], Reply::Once);
    let written = base.replace("http://127.0.0.1", "HTTP://LOCALHOST");
    let out = program(
        &g,
        &format!(r#"return (await fetch("{written}/A/./b#x")).url"#),
    )
    .await;
    let requested = format!("{}/A/b", base.replace("127.0.0.1", "localhost"));
    assert_eq!(out.result, Ok(json!(requested)));
    let resources: Vec<String> = asked
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.resources[0].clone())
        .collect();
    assert_eq!(resources, vec![requested]);
}

/// A `*` in a URL's host is refused before anything is asked, and
/// "always" never saves a rule with a wildcard origin even if asked for
/// one. (Security review 7: approving `https://*github.com/x` allowed
/// `https://attacker.example/github.com/steal`.)
#[tokio::test(flavor = "multi_thread")]
async fn a_wildcard_host_is_refused_and_never_saved() {
    for (raw, other) in [
        (
            "https://*github.com/x",
            "https://attacker.example/github.com/steal",
        ),
        ("https://*/x", "https://anything.example/whatever"),
    ] {
        let (g, asked) = gate(vec![], Reply::Always);
        let out = program(
            &g,
            &format!(r#"try {{ await fetch("{raw}") }} catch (e) {{ return e.message }}"#),
        )
        .await;
        assert_eq!(out.result, Ok(json!("URL host must not contain * or ?")));
        assert!(asked.asked.lock().unwrap().is_empty());
        let url = url::Url::parse(raw).unwrap();
        let req = Request {
            action: ACTION.into(),
            resources: vec![raw.to_string()],
            save: webfetch::save_patterns(&url),
            title: format!("Fetch {raw}"),
        };
        g.assert(&req).unwrap();
        assert!(g.saved().is_empty(), "{:?}", g.saved());
        assert!(!g.allows(ACTION, &[other.to_string()]), "{other}");
    }
}

/// A redirect to another origin asks for its target, like the first URL:
/// a 307 keeps the method and body, so the user decides. Declining stops
/// the chain before the target is requested. (Security review 7: a
/// cross-origin 307 POST went out without asking.)
#[tokio::test(flavor = "multi_thread")]
async fn a_cross_origin_redirect_asks_for_its_target() {
    let (inner, inner_hits) = serve(|_| Answer::new(200, "text/plain", "internal ok")).await;
    let target = format!("{inner}/admin");
    let (outer, _) = serve(move |_| Answer::redirect(307, &target)).await;
    let rules = vec![Rule::new("webfetch", &format!("{outer}*"), Effect::Allow)];
    let code = format!(
        r#"try {{ return await (await fetch("{outer}/r", {{ method: "POST", body: "payload", headers: {{ "x-thing": "1" }} }})).text() }} catch (e) {{ return e.message }}"#
    );
    let (g, asked) = gate(rules.clone(), Reply::Reject(None));
    let out = program(&g, &code).await;
    let msg = out.result.unwrap();
    assert!(msg.as_str().unwrap().contains("declined"), "{msg}");
    assert!(inner_hits.lock().unwrap().is_empty());
    let asked_for: Vec<(Vec<String>, String)> = asked
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|r| (r.resources.clone(), r.title.clone()))
        .collect();
    assert_eq!(asked_for.len(), 1);
    assert_eq!(asked_for[0].0, vec![format!("{inner}/admin")]);
    assert!(asked_for[0].1.contains("POST"), "{}", asked_for[0].1);

    // Approved, the redirect is followed with the method and body.
    let (g, asked) = gate(rules, Reply::Once);
    let out = program(&g, &code).await;
    assert_eq!(out.result, Ok(json!("internal ok")));
    assert_eq!(asked.asked.lock().unwrap().len(), 1);
    let hits = inner_hits.lock().unwrap();
    assert_eq!(
        (
            hits[0].method.as_str(),
            hits[0].path(),
            hits[0].body.as_slice()
        ),
        ("POST", "/admin", &b"payload"[..])
    );
}

/// A redirect within the origin is not asked about.
#[tokio::test(flavor = "multi_thread")]
async fn a_same_origin_redirect_is_not_asked() {
    let (base, hits) = serve(|s| match s.path() {
        "/r" => Answer::redirect(307, "/end"),
        _ => Answer::new(200, "text/plain", "ok"),
    })
    .await;
    let rules = vec![Rule::new("webfetch", &format!("{base}/r"), Effect::Allow)];
    let (g, asked) = gate(rules, Reply::Reject(None));
    let out = program(
        &g,
        &format!(r#"return await (await fetch("{base}/r")).text()"#),
    )
    .await;
    assert_eq!(out.result, Ok(json!("ok")));
    assert!(asked.asked.lock().unwrap().is_empty());
    assert_eq!(hits.lock().unwrap().len(), 2);
}
