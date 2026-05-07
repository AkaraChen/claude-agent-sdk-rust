use claude_agent_sdk::{CLI_VERSION, VERSION};
use regex::Regex;

const PY_VERSION: &str =
    include_str!("../claude-agent-sdk-python/src/claude_agent_sdk/_version.py");
const PY_CLI_VERSION: &str =
    include_str!("../claude-agent-sdk-python/src/claude_agent_sdk/_cli_version.py");
const PY_CHANGELOG: &str = include_str!("../claude-agent-sdk-python/CHANGELOG.md");

#[test]
fn rust_package_version_matches_sdk_constant_and_python_source() {
    assert_eq!(env!("CARGO_PKG_VERSION"), VERSION);
    assert!(
        PY_VERSION.contains(&format!("__version__ = \"{VERSION}\"")),
        "Rust VERSION must match vendored Python __version__"
    );
}

#[test]
fn bundled_cli_version_matches_python_source_and_latest_changelog_entry() {
    assert!(
        PY_CLI_VERSION.contains(&format!("__cli_version__ = \"{CLI_VERSION}\"")),
        "Rust CLI_VERSION must match vendored Python __cli_version__"
    );

    assert!(PY_CHANGELOG.starts_with("# Changelog"));
    let latest_version_heading = PY_CHANGELOG
        .lines()
        .find(|line| line.starts_with("## "))
        .expect("changelog should contain at least one version heading");
    assert_eq!(latest_version_heading, format!("## {VERSION}"));

    let latest_section = PY_CHANGELOG
        .split("\n## ")
        .nth(1)
        .expect("changelog should contain a latest version section");
    assert!(
        latest_section.contains(&format!(
            "Updated bundled Claude CLI to version {CLI_VERSION}"
        )),
        "latest changelog section should mention the bundled CLI version"
    );
}

#[test]
fn vendored_python_changelog_keeps_expected_shape() {
    assert!(PY_CHANGELOG.starts_with("# Changelog"));

    let version_pattern = Regex::new(r"^## \d+\.\d+\.\d+(?:\s+\(\d{4}-\d{2}-\d{2}\))?$").unwrap();
    let version_capture = Regex::new(r"^## (\d+)\.(\d+)\.(\d+)").unwrap();
    let mut versions = Vec::new();
    let mut current_section_has_bullet = None;

    for line in PY_CHANGELOG.lines() {
        if line.starts_with("## ") {
            if let Some(false) = current_section_has_bullet {
                panic!("previous changelog version section should have at least one bullet point");
            }
            assert!(
                version_pattern.is_match(line),
                "invalid version format: {line}"
            );
            let captures = version_capture
                .captures(line)
                .expect("version heading should contain major.minor.patch");
            versions.push((
                captures[1].parse::<u64>().unwrap(),
                captures[2].parse::<u64>().unwrap(),
                captures[3].parse::<u64>().unwrap(),
            ));
            current_section_has_bullet = Some(false);
        } else if line.starts_with("- ") {
            current_section_has_bullet = Some(true);
            assert_ne!(
                line.trim(),
                "-",
                "changelog should not have empty bullet points"
            );
        }
    }

    assert!(!versions.is_empty(), "changelog should contain versions");
    assert_ne!(
        current_section_has_bullet,
        Some(false),
        "last changelog version section should have at least one bullet point"
    );
    for pair in versions.windows(2) {
        assert!(
            pair[0] > pair[1],
            "versions should be in descending order: {:?} should be > {:?}",
            pair[0],
            pair[1]
        );
    }
}
