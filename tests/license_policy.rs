use dfor77::{parse_json, JsonValue};
use std::process::Command;

const ALLOWED_LICENSES: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "MIT OR Apache-2.0",
    "Apache-2.0 OR MIT",
    "MIT/Apache-2.0",
    "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT",
    "BSD-2-Clause OR Apache-2.0 OR MIT",
    "Unlicense OR MIT",
    "Zlib",
    "(MIT OR Apache-2.0) AND Unicode-3.0",
    "MIT OR Apache-2.0 OR LGPL-2.1-or-later",
];

const ALLOWED_REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

#[test]
fn crate_declares_lgplv3_license() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read Cargo.toml");
    let copying = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/COPYING"))
        .expect("read COPYING");
    let copying_lesser =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/COPYING.LESSER"))
            .expect("read COPYING.LESSER");
    assert!(manifest.contains("license = \"LGPL-3.0-only\""));
    assert!(copying.contains("GNU GENERAL PUBLIC LICENSE"));
    assert!(copying.contains("Version 3, 29 June 2007"));
    assert!(copying_lesser.contains("GNU LESSER GENERAL PUBLIC LICENSE"));
    assert!(copying_lesser.contains("Version 3, 29 June 2007"));
}

#[test]
fn dependency_licenses_match_public_policy() {
    if std::env::var("DFOR77_LICENSE_CHECK").as_deref() != Ok("1") {
        return;
    }
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata = parse_json(std::str::from_utf8(&output.stdout).expect("metadata is UTF-8"))
        .expect("parse cargo metadata");
    let packages = metadata
        .get("packages")
        .and_then(JsonValue::as_array)
        .expect("metadata packages");
    let mut rejected = Vec::new();
    for package in packages {
        if matches!(package.get("source"), Some(JsonValue::Null)) {
            continue;
        }
        let source = package
            .get("source")
            .and_then(JsonValue::as_str)
            .expect("dependency source");
        let name = package
            .get("name")
            .and_then(JsonValue::as_str)
            .expect("package name");
        let version = package
            .get("version")
            .and_then(JsonValue::as_str)
            .expect("package version");
        if source != ALLOWED_REGISTRY {
            rejected.push(format!("{name} {version}: unapproved source {source}"));
        }
        match package.get("license").and_then(JsonValue::as_str) {
            Some(license) if ALLOWED_LICENSES.contains(&license) => {}
            Some(license) => rejected.push(format!("{name} {version}: {license}")),
            None => rejected.push(format!("{name} {version}: missing license metadata")),
        }
    }
    assert!(
        rejected.is_empty(),
        "unapproved dependency licenses:\n{}",
        rejected.join("\n")
    );
}
