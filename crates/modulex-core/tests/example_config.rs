//! The shipped example config must always parse and declare a sane grant.

use modulex_core::{steps::builtin_registry, Config};

const EXAMPLE: &str = include_str!("../../../modulex.toml.example");

#[test]
fn example_config_parses() {
    let config = Config::from_toml(EXAMPLE).expect("modulex.toml.example must parse");
    let morning = &config.routines["morning"];
    assert_eq!(morning.description, "Good morning dashboard");
    assert!(morning.steps.len() >= 6);
}

#[test]
fn example_declares_only_expected_programs() {
    let config = Config::from_toml(EXAMPLE).unwrap();
    let declared = config.declared_programs(&builtin_registry());
    for expected in ["git", "gh", "glab", "pa", "pass", "python3"] {
        assert!(declared.contains(expected), "declared: {declared:?}");
    }
    // The script step's command is declared with ~ expanded.
    assert!(
        declared.iter().any(|p| p.ends_with("bin/weather.sh")),
        "declared: {declared:?}"
    );
    // Pure steps declare nothing extra; no stray entries.
    assert_eq!(declared.len(), 7, "declared: {declared:?}");
}

#[test]
fn example_contains_no_credential_values() {
    // The example must hold references only. Heuristic tripwires:
    for needle in ["ghp_", "glpat-", "Bearer ", "PRIVATE KEY"] {
        assert!(
            !EXAMPLE.contains(needle),
            "example config must not contain credential material ({needle})"
        );
    }
}
