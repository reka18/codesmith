//! Security review 8, finding 4: a `list` command printing 300 MB with no
//! newline used to be held whole in memory. It has a test binary of its
//! own, so the process's peak memory is this test's alone.

#![cfg(unix)]

use codesmith_core::providers::swap;

/// The process's peak resident size, in bytes.
fn peak() -> u64 {
    // SAFETY: getrusage only writes the struct it is given.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let v = u.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        v
    } else {
        v * 1024
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_300_mb_line_stays_bounded_in_memory() {
    let before = peak();
    let s = swap::SwapConfig {
        list: vec![
            "/bin/sh".into(),
            "-c".into(),
            "head -c 300000000 /dev/zero | tr '\\0' a".into(),
        ],
        use_: vec!["/bin/echo".into(), "{model}".into()],
        timeout: Some(60),
    };
    let e = swap::list(&s).await.unwrap_err().to_string();
    assert!(e.contains("more than 1 MiB"), "{e}");
    let grew = peak().saturating_sub(before);
    // Before the fix this grew by the whole line (300 MB and more).
    assert!(grew < 48 << 20, "peak memory grew by {} MB", grew >> 20);
}
