use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_REAL_REPO: &str = "/home/vitaly/p/gentoo-zh";

#[test]
#[ignore = "requires a local Gentoo overlay checkout with full git history"]
fn real_overlay_generates_rss() {
    let repo = env::var("GENTOO_OVERLAY_RSS_REAL_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_REAL_REPO));
    if !repo.join(".git").exists() {
        eprintln!(
            "skipping real overlay test: {} is not a git checkout",
            repo.display()
        );
        return;
    }

    let test_dir = temp_test_dir("real-overlay");
    fs::create_dir_all(&test_dir).expect("test dir can be created");
    let output = test_dir.join("feed.rss");
    let binary = env!("CARGO_BIN_EXE_gentoo-overlay-new-packages-to-rss");

    let command_output = Command::new(binary)
        .arg("--repo")
        .arg(&repo)
        .arg("--repo-url")
        .arg("https://github.com/microcai/gentoo-zh")
        .arg("--self-url")
        .arg("https://microcai.github.io/gentoo-zh/gentoo-zh.rss")
        .arg("--output")
        .arg(&output)
        .output()
        .expect("generator can run");

    assert!(
        command_output.status.success(),
        "generator failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&command_output.stdout),
        String::from_utf8_lossy(&command_output.stderr)
    );

    let rss = fs::read_to_string(&output).expect("rss output can be read");
    assert!(rss.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(rss.contains("<rss version=\"2.0\""));
    assert!(rss.contains("<channel>"));
    assert!(rss.contains("<item>"));
    assert!(rss.matches("<item>").count() > 1_000);
    assert!(rss.contains("<atom:link"));
    assert!(rss.contains("Metadata description:"));

    fs::remove_dir_all(&test_dir).expect("test dir can be removed");
}

fn temp_test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after unix epoch")
        .as_nanos();
    env::temp_dir().join(format!(
        "gentoo-overlay-new-packages-to-rss-{name}-{}-{nanos}",
        std::process::id()
    ))
}
