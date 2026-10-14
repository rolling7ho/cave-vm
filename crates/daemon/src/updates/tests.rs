use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

fn entry(name: &str, digit: char) -> String {
    let hex: String = std::iter::repeat_n(digit, 64).collect();
    format!(
        r#"{{"name": "{name}", "disk": {{"url": "d", "sha256": "{hex}", "size": 1}},
            "kernel": {{"url": "k", "sha256": "{hex}", "size": 1}}}}"#
    )
}

fn manifest() -> String {
    format!(
        r#"{{"version": 1, "images": [{}, {}, {}]}}"#,
        entry("debian", 'b'),
        entry("ubuntu", 'c'),
        entry("debian", 'a')
    )
}

/// A detector whose lookups are counted, with its record in a new folder.
fn detector(name: &str, answer: io::Result<String>) -> (Detector, PathBuf, Arc<AtomicUsize>) {
    let dir = std::env::temp_dir().join(format!("cave-updates-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(FILE);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let answer = answer.map_err(|e| e.to_string());
    let fetch: Fetch = Box::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        answer.clone().map_err(io::Error::other)
    });
    (Detector::new(file.clone(), fetch), file, calls)
}

#[test]
fn turned_off_it_sends_nothing() {
    let (d, file, calls) = detector("off", Ok(manifest()));
    d.check_soon(false);
    d.wait();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!file.exists());
}

#[test]
fn a_lookup_records_the_newest_digest_of_each_image() {
    let (d, file, calls) = detector("on", Ok(manifest()));
    d.check_soon(true);
    d.wait();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let newest = newest(&file);
    assert_eq!(newest.len(), 2);
    assert_eq!(newest["debian"], format!("sha256:{}", "b".repeat(64)));
    assert_eq!(newest["ubuntu"], format!("sha256:{}", "c".repeat(64)));
}

#[test]
fn it_looks_up_at_most_once_a_day() {
    let (d, file, calls) = detector("daily", Ok(manifest()));
    d.check_soon(true);
    d.wait();
    d.check_soon(true);
    d.wait();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A new daemon reads when the last lookup was.
    let fresh_calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&fresh_calls);
    let fresh = Detector::new(
        file.clone(),
        Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(manifest())
        }),
    );
    fresh.check_soon(true);
    fresh.wait();
    assert_eq!(fresh_calls.load(Ordering::SeqCst), 0);

    // A day later it looks again.
    let old = now_unix() - INTERVAL - 1;
    fs::write(&file, json!({ "checked": old, "newest": {} }).to_string()).unwrap();
    fresh.check_soon(true);
    fresh.wait();
    assert_eq!(fresh_calls.load(Ordering::SeqCst), 1);
    assert_eq!(newest(&file).len(), 2);
}

#[test]
fn a_failed_lookup_records_nothing_and_waits_a_day() {
    let (d, file, calls) = detector("offline", Err(io::Error::other("no network")));
    d.check_soon(true);
    d.wait();
    d.check_soon(true);
    d.wait();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!file.exists());
    assert!(newest(&file).is_empty());
}
