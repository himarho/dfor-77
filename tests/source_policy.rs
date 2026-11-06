use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Clone, Copy)]
struct FortranPolicy {
    allow_dfor77_include: bool,
}

const STRICT_FORTRAN: FortranPolicy = FortranPolicy {
    allow_dfor77_include: false,
};
const EXTENSION_FORTRAN: FortranPolicy = FortranPolicy {
    allow_dfor77_include: true,
};
const PUBLIC_D7_API: &[&str] = &[
    "D7INI", "D7END", "D7NXT", "D7TYP", "D7ENM", "D7GID", "D7CHN", "D7CAT", "D7MID", "D7USR",
    "D7EMJ", "D7TXT", "D7TAV", "D7BOT", "D7IMC", "D7RLC", "D7RLI", "D7IID", "D7APP", "D7ITK",
    "D7MSF", "D7MSI", "D7SFD", "D7INM", "D7ITP", "D7ADM", "D7CAD", "D7CDL", "D7CTM", "D7CSN",
    "D7CDP", "D7CTP", "D7CSP", "D7SND", "D7SJP", "D7STK", "D7POL", "D7UPL", "D7UPM", "D7RAC",
    "D7GRU", "D7PAV", "D7EPL", "D7TPI", "D7DEL", "D7GMS", "D7LMS", "D7EMS", "D7BMD", "D7PIN",
    "D7TMO", "D7UNT", "D7AMR", "D7RMR", "D7EGM", "D7KIK", "D7BAN", "D7UBN", "D7GMB", "D7LMB",
    "D7SMB", "D7AMB", "D7ECM", "D7CCR", "D7ECH", "D7DCH", "D7CRO", "D7ERO", "D7DRO", "D7EGD",
    "D7GCH", "D7CPM", "D7DPM", "D7GIN", "D7CIN", "D7FOL", "D7SVC", "D7GGD", "D7GCS", "D7MCP",
    "D7ATH", "D7LRO", "D7RPS", "D7GBN", "D7LBN", "D7BBN", "D7AUD", "D7AUM", "D7THR", "D7CWH",
    "D7LWH", "D7GWH", "D7EWH", "D7DWH", "D7WHP", "D7WHM", "D7GCU", "D7GUR", "D7CDM", "D7GWB",
    "D7ICB", "D7ACR", "D7IFU", "D7RST", "D7RBD", "D7GWS", "D7PRS", "D7VST", "D7GOP", "D7GRT",
    "D7ERR", "D7WHY", "D7LOG", "D7JES", "D7PEM", "D7PE2", "D7PBT", "D7PAC", "D7PSM", "D7IMR",
    "D7IDR", "D7IMD", "D7SCM", "D7SCX", "D7CMD", "D7AMK", "D7B64", "D7URI", "D7X64", "D7XQ0",
    "D7XQA", "D7XACB", "D7XAMK", "D7XURI", "D7XF64",
];

#[test]
fn production_rust_source_matches_publication_policy() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    check_source_tree(&root, &mut violations);
    assert!(
        violations.is_empty(),
        "production source policy violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn rust_internal_modules_stay_hidden_from_public_docs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = fs::read_to_string(root.join("src/lib.rs")).expect("read src/lib.rs");
    let lines = text.lines().collect::<Vec<_>>();
    let mut violations = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.trim_start().starts_with("pub mod ") {
            continue;
        }
        let previous = lines[..index]
            .iter()
            .rev()
            .find(|value| !value.trim().is_empty())
            .map(|value| value.trim());
        if previous != Some("#[doc(hidden)]") {
            violations.push(format!("src/lib.rs:{} exposes {line}", index + 1));
        }
    }
    assert!(
        violations.is_empty(),
        "Rust internal module visibility policy violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn bundled_dfor77_source_stays_ansi_x3_9_1978_safe() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    for relative in bundled_dfor77_paths(root) {
        check_fortran_source(&root.join(&relative), &relative, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "bundled dfor-77 ANSI X3.9-1978 policy violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn dfor77_standard_samples_stay_separated_and_strict() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let paths = dfor77_standard_sample_paths(root);
    assert!(
        !paths.is_empty(),
        "dfor-77 standard sample directory has no FORTRAN samples"
    );
    let mut violations = Vec::new();
    for relative in paths {
        check_fortran_source_with_policy(
            &root.join(&relative),
            &relative,
            &mut violations,
            STRICT_FORTRAN,
        );
    }
    assert!(
        violations.is_empty(),
        "dfor-77 standard sample policy violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn dfor77_extension_include_stays_hand_expandable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    check_fortran_source(
        &root.join("include/dfor77.inc"),
        "include/dfor77.inc",
        &mut violations,
    );
    assert!(
        violations.is_empty(),
        "dfor-77 extension include hand-expansion violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn dfor77_extension_helpers_stay_separated_and_limited() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    check_fortran_source_with_policy(
        &root.join("extension/dfor77_extension.f"),
        "extension/dfor77_extension.f",
        &mut violations,
        EXTENSION_FORTRAN,
    );
    assert!(
        violations.is_empty(),
        "dfor-77 extension helper policy violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn dfor77_extension_samples_stay_separated_and_limited() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let paths = dfor77_extension_sample_paths(root);
    assert!(
        !paths.is_empty(),
        "dfor-77 extension sample list has no FORTRAN samples"
    );
    let mut violations = Vec::new();
    for relative in paths {
        check_fortran_source_with_policy(
            &root.join(&relative),
            &relative,
            &mut violations,
            EXTENSION_FORTRAN,
        );
    }
    assert!(
        violations.is_empty(),
        "dfor-77 extension sample policy violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn fortran_non_public_d7_routines_use_z_namespace() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let wrapper = fs::read_to_string(root.join("fortran/dfor77_wrapper.f")).expect("read wrapper");
    let extension =
        fs::read_to_string(root.join("extension/dfor77_extension.f")).expect("read extension");
    let extension_public = [
        "D7XQ0", "D7XQA", "D7XACB", "D7XAMK", "D7XURI", "D7XF64", "D7X64",
    ];
    let mut violations = Vec::new();
    for name in fortran_routine_names(&wrapper) {
        if !PUBLIC_D7_API.contains(&name.as_str()) && !name.starts_with("D7Z") {
            violations.push(format!("fortran/dfor77_wrapper.f: {name}"));
        }
        if name.starts_with("D7X") {
            violations.push(format!("fortran/dfor77_wrapper.f: {name}"));
        }
    }
    for name in fortran_routine_names(&extension) {
        if !extension_public.contains(&name.as_str()) && !name.starts_with("D7Z") {
            violations.push(format!("extension/dfor77_extension.f: {name}"));
        }
        if name.starts_with("D7X") && !extension_public.contains(&name.as_str()) {
            violations.push(format!("extension/dfor77_extension.f: {name}"));
        }
    }
    assert!(
        violations.is_empty(),
        "FORTRAN D7 namespace violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn public_d7_api_entries_have_fortran_routines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let wrapper = fs::read_to_string(root.join("fortran/dfor77_wrapper.f")).expect("read wrapper");
    let extension =
        fs::read_to_string(root.join("extension/dfor77_extension.f")).expect("read extension");
    let mut routines = BTreeSet::new();
    for name in fortran_routine_names(&wrapper)
        .into_iter()
        .chain(fortran_routine_names(&extension))
    {
        routines.insert(name);
    }
    let mut seen = BTreeSet::new();
    let mut missing = Vec::new();
    let mut duplicates = Vec::new();
    for api in PUBLIC_D7_API {
        if !routines.contains(*api) {
            missing.push(*api);
        }
        if !seen.insert(*api) {
            duplicates.push(*api);
        }
    }
    assert!(
        missing.is_empty() && duplicates.is_empty(),
        "PUBLIC_D7_API drift:\nmissing routines: {}\nduplicate entries: {}",
        missing.join(", "),
        duplicates.join(", ")
    );
}

#[test]
fn package_required_files_match_manifest_include() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let makefile = fs::read_to_string(root.join("Makefile")).expect("read Makefile");
    let mut manifest_paths = manifest_include_paths(&manifest);
    let mut makefile_paths = makefile_variable_paths(&makefile, "PACKAGE_REQUIRED_FILES");
    manifest_paths.sort();
    makefile_paths.sort();
    assert_eq!(
        manifest_paths, makefile_paths,
        "Cargo.toml package include and Makefile PACKAGE_REQUIRED_FILES differ"
    );
    for path in manifest_paths {
        assert!(
            !path.starts_with("tests/")
                && !path.starts_with(".github/")
                && !path.starts_with("examples/"),
            "development-only path is required for package: {path}"
        );
    }
}

fn bundled_dfor77_paths(root: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    collect_fortran_paths(root, Path::new("fortran"), &mut paths);
    paths.sort();
    paths
}

fn dfor77_standard_sample_paths(_root: &Path) -> Vec<String> {
    let mut paths = vec!["examples/sample_moderation/sample_moderation.f".to_string()];
    paths.sort();
    paths
}

fn dfor77_extension_sample_paths(_root: &Path) -> Vec<String> {
    let mut paths = vec!["examples/sample_moderation/sample_moderation_extension.f".to_string()];
    paths.sort();
    paths
}

fn manifest_include_paths(manifest: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "include = [" {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if trimmed == "]" {
            break;
        }
        let value = trimmed
            .trim_end_matches(',')
            .trim_matches('"')
            .trim_start_matches('/');
        if !value.is_empty() {
            paths.push(value.to_string());
        }
    }
    paths
}

fn makefile_variable_paths(makefile: &str, name: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let prefix = format!("{name} =");
    let mut inside = false;
    for line in makefile.lines() {
        if !inside {
            let Some(rest) = line.strip_prefix(&prefix) else {
                continue;
            };
            inside = true;
            push_makefile_value(rest, &mut paths);
            if !line.trim_end().ends_with('\\') {
                break;
            }
            continue;
        }
        push_makefile_value(line, &mut paths);
        if !line.trim_end().ends_with('\\') {
            break;
        }
    }
    paths
}

fn push_makefile_value(line: &str, paths: &mut Vec<String>) {
    let value = line.trim().trim_end_matches('\\').trim();
    if !value.is_empty() {
        paths.push(value.to_string());
    }
}

fn collect_fortran_paths(root: &Path, relative: &Path, paths: &mut Vec<String>) {
    let path = root.join(relative);
    let entries = fs::read_dir(&path).expect("read FORTRAN source directory");
    for entry in entries {
        let entry = entry.expect("read FORTRAN source entry");
        let path = entry.path();
        let name = relative.join(entry.file_name());
        if path.is_dir() {
            collect_fortran_paths(root, &name, paths);
        } else if is_fortran_path(&path) {
            paths.push(name.display().to_string());
        }
    }
}

fn is_fortran_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("f" | "for" | "f77" | "inc")
    )
}

fn fortran_routine_names(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let name = if let Some(rest) = trimmed.strip_prefix("SUBROUTINE ") {
            rest.split('(').next()
        } else if let Some(rest) = trimmed.strip_prefix("INTEGER FUNCTION ") {
            rest.split('(').next()
        } else if let Some(rest) = trimmed.strip_prefix("LOGICAL FUNCTION ") {
            rest.split('(').next()
        } else {
            None
        };
        if let Some(name) = name {
            names.push(name.trim().to_string());
        }
    }
    names
}

fn check_fortran_source(path: &Path, label: &str, violations: &mut Vec<String>) {
    check_fortran_source_with_policy(path, label, violations, STRICT_FORTRAN);
}

fn check_fortran_source_with_policy(
    path: &Path,
    label: &str,
    violations: &mut Vec<String>,
    policy: FortranPolicy,
) {
    let text = fs::read_to_string(path).expect("read FORTRAN source");
    check_fortran_lines_with_policy(text.lines(), label, violations, policy);
}

fn check_fortran_lines_with_policy<'a>(
    lines: impl Iterator<Item = &'a str>,
    label: &str,
    violations: &mut Vec<String>,
    policy: FortranPolicy,
) {
    for (index, line) in lines.enumerate() {
        let line_number = index + 1;
        if !line.is_ascii() {
            violations.push(format!("{label}:{line_number} contains non-ASCII"));
        }
        if line.len() > 72 {
            violations.push(format!(
                "{label}:{line_number} exceeds fixed-form 72 columns"
            ));
        }
        if line.contains('\t') {
            violations.push(format!("{label}:{line_number} contains tab"));
        }
        let raw_upper = line.trim_start().to_ascii_uppercase();
        let allowed_include = policy.allow_dfor77_include
            && matches!(
                raw_upper.as_str(),
                "INCLUDE 'DFOR77.INC'" | "INCLUDE 'INCLUDE/DFOR77.INC'"
            );
        let code = strip_fortran_literals(line).to_ascii_uppercase();
        if matches!(code.as_bytes().first(), Some(b'C' | b'*' | b'!')) {
            violations.push(format!("{label}:{line_number} contains source comment"));
        }
        let trimmed = code.trim_start();
        if trimmed.starts_with("INCLUDE") && !allowed_include {
            violations.push(format!("{label}:{line_number} uses INCLUDE"));
        }
        for banned in [
            "::",
            "_",
            "ENDDO",
            "END DO",
            "DO WHILE",
            "SELECT",
            "MODULE",
            "USE ",
            "IMPLICIT NONE",
        ] {
            if code.contains(banned) && !allowed_include {
                violations.push(format!(
                    "{label}:{line_number} uses non-FORTRAN77 form {banned}"
                ));
            }
        }
        if code.contains("FORMAT") && code.contains("I0") {
            violations.push(format!("{label}:{line_number} uses nonstandard I0 edit"));
        }
        if code.contains("CALL") && code.contains("FLUSH") {
            violations.push(format!("{label}:{line_number} uses nonstandard FLUSH"));
        }
        for token in fortran_identifier_tokens(&code) {
            if token.len() > 6 && !is_fortran_keyword(token) {
                violations.push(format!(
                    "{label}:{line_number} has identifier over 6 chars: {token}"
                ));
            }
        }
    }
}

fn check_source_tree(root: &Path, violations: &mut Vec<String>) {
    for entry in fs::read_dir(root).expect("read source directory") {
        let entry = entry.expect("read source entry");
        let path = entry.path();
        if path.is_dir() {
            check_source_tree(&path, violations);
        } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
            check_source_file(&path, violations);
        }
    }
}

fn check_source_file(path: &Path, violations: &mut Vec<String>) {
    let text = fs::read_to_string(path).expect("read source file");
    let label = path
        .strip_prefix(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or(path)
        .display()
        .to_string();
    let mut pending_test_cfg = false;
    let mut in_test_module = false;
    let mut in_test_item = false;
    let mut test_module_depth = 0_i32;
    let mut test_item_depth = 0_i32;
    let mut after_test_module = false;
    for (index, line) in text.lines().enumerate() {
        let code = strip_literals(line);
        let trimmed = code.trim();
        if in_test_module {
            test_module_depth += brace_delta(&code);
            if test_module_depth <= 0 {
                in_test_module = false;
                after_test_module = true;
            }
            continue;
        }
        if in_test_item {
            test_item_depth += brace_delta(&code);
            if test_item_depth <= 0 {
                in_test_item = false;
            }
            continue;
        }
        if after_test_module {
            if !trimmed.is_empty() {
                violations.push(format!(
                    "{label}:{} places production code after cfg(test) module",
                    index + 1
                ));
            }
            continue;
        }
        if trimmed.contains("#[cfg(test)]") {
            pending_test_cfg = true;
            continue;
        }
        if pending_test_cfg {
            if trimmed.is_empty() || trimmed.starts_with("#[") {
                continue;
            }
            if trimmed.starts_with("mod tests") {
                in_test_module = true;
                test_module_depth = brace_delta(&code);
                if test_module_depth <= 0 {
                    in_test_module = false;
                    after_test_module = true;
                }
            } else {
                in_test_item = true;
                test_item_depth = brace_delta(&code);
                if test_item_depth <= 0 {
                    in_test_item = false;
                }
            }
            pending_test_cfg = false;
            continue;
        }
        if code.contains(".unwrap(") {
            violations.push(format!("{label}:{} uses unwrap", index + 1));
        }
        if code.contains(".expect(") && !code.contains("self.expect(") {
            violations.push(format!("{label}:{} uses expect", index + 1));
        }
        if code.contains("panic!") {
            violations.push(format!("{label}:{} uses panic", index + 1));
        }
        if contains_macro_call(&code, "dbg") {
            violations.push(format!("{label}:{} uses dbg", index + 1));
        }
        if contains_macro_call(&code, "todo") {
            violations.push(format!("{label}:{} uses todo", index + 1));
        }
        if contains_macro_call(&code, "unimplemented") {
            violations.push(format!("{label}:{} uses unimplemented", index + 1));
        }
        if contains_macro_call(&code, "print") {
            violations.push(format!("{label}:{} uses print", index + 1));
        }
        if contains_macro_call(&code, "eprint") {
            violations.push(format!("{label}:{} uses eprint", index + 1));
        }
        if contains_macro_call(&code, "println") && !label.ends_with("src/main.rs") {
            violations.push(format!("{label}:{} uses println outside CLI", index + 1));
        }
        if contains_unsafe_construct(&code) {
            violations.push(format!("{label}:{} uses unsafe", index + 1));
        }
    }
}

fn brace_delta(code: &str) -> i32 {
    code.bytes().fold(0, |depth, byte| match byte {
        b'{' => depth + 1,
        b'}' => depth - 1,
        _ => depth,
    })
}

fn strip_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            'r' if raw_string_hashes(chars.clone()).is_some() => {
                out.push(' ');
                let hashes = raw_string_hashes(chars.clone()).unwrap_or(0);
                for _ in 0..hashes {
                    let _ = chars.next();
                    out.push(' ');
                }
                let _ = chars.next();
                out.push(' ');
                while let Some(value) = chars.next() {
                    out.push(' ');
                    if value == '"' {
                        let mut remaining = chars.clone();
                        if (0..hashes).all(|_| remaining.next() == Some('#')) {
                            for _ in 0..hashes {
                                let _ = chars.next();
                                out.push(' ');
                            }
                            break;
                        }
                    }
                }
            }
            '"' => {
                out.push(' ');
                let mut escaped = false;
                for value in chars.by_ref() {
                    out.push(' ');
                    if escaped {
                        escaped = false;
                    } else if value == '\\' {
                        escaped = true;
                    } else if value == '"' {
                        break;
                    }
                }
            }
            '\'' if char_literal_follows(chars.clone()) => {
                out.push(' ');
                let mut escaped = false;
                for value in chars.by_ref() {
                    out.push(' ');
                    if escaped {
                        escaped = false;
                    } else if value == '\\' {
                        escaped = true;
                    } else if value == '\'' {
                        break;
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn char_literal_follows(mut chars: impl Iterator<Item = char>) -> bool {
    match chars.next() {
        Some('\\') => true,
        Some(_) => chars.next() == Some('\''),
        None => false,
    }
}

#[test]
fn rust_literal_scanning_preserves_lifetimes_labels_and_following_code() {
    for code in [
        "struct Segmented<'a> {",
        "impl Read for Segmented<'_> {",
        "fn value<'a>(value: &'a str) -> &'a str { value }",
        "'retry: loop { break 'retry; }",
        "fn value<'a>() { unsafe { operation() } }",
    ] {
        assert_eq!(strip_literals(code), code);
    }
    for code in ["b'{'", "'}'", "'\\u{007b}'", r#"'\''"#, r#"'\\'"#] {
        assert_eq!(brace_delta(&strip_literals(code)), 0, "{code}");
    }
    assert!(contains_unsafe_construct(&strip_literals(
        "fn value<'a>() { unsafe { operation() } }"
    )));
}

#[test]
fn raw_literal_scanning_keeps_code_after_adjacent_quotes_and_near_delimiters() {
    for code in [
        "for value in [r#\"\"0\"\"#] {",
        "for value in [r##\"text\"#inside\"##] {",
        "for value in [br#\"\"\"#] {",
    ] {
        assert_eq!(brace_delta(&strip_literals(code)), 1, "{code}");
    }
    let code = "let value = r#\"\"0\"\"#; unsafe { operation() }";
    assert!(contains_unsafe_construct(&strip_literals(code)));
    assert!(!contains_unsafe_construct(&strip_literals(
        "let value = r#\"unsafe { operation() }\"#;"
    )));
}

fn raw_string_hashes<I>(mut chars: I) -> Option<usize>
where
    I: Iterator<Item = char>,
{
    let mut hashes = 0_usize;
    loop {
        match chars.next() {
            Some('#') => hashes += 1,
            Some('"') => return Some(hashes),
            _ => return None,
        }
    }
}

fn contains_unsafe_construct(code: &str) -> bool {
    let bytes = code.as_bytes();
    let needle = b"unsafe";
    let mut index = 0;
    while let Some(offset) = code[index..].find("unsafe") {
        let start = index + offset;
        let end = start + needle.len();
        let before = start
            .checked_sub(1)
            .and_then(|pos| bytes.get(pos))
            .is_none_or(|byte| !is_ident_byte(*byte));
        let after = bytes.get(end).is_none_or(|byte| !is_ident_byte(*byte));
        if before && after {
            return true;
        }
        index = end;
    }
    false
}

fn contains_macro_call(code: &str, name: &str) -> bool {
    let bytes = code.as_bytes();
    let pattern = format!("{name}!");
    let mut index = 0;
    while let Some(offset) = code[index..].find(&pattern) {
        let start = index + offset;
        let before = start
            .checked_sub(1)
            .and_then(|pos| bytes.get(pos))
            .is_none_or(|byte| !is_ident_byte(*byte));
        if before {
            return true;
        }
        index = start + pattern.len();
    }
    false
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn strip_fortran_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            out.push(' ');
            while let Some(value) = chars.next() {
                out.push(' ');
                if value == '\'' {
                    if matches!(chars.peek(), Some('\'')) {
                        let _ = chars.next();
                        out.push(' ');
                    } else {
                        break;
                    }
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn fortran_identifier_tokens(code: &str) -> Vec<&str> {
    let bytes = code.as_bytes();
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_alphanumeric() {
            if start.is_none() && byte.is_ascii_alphabetic() {
                start = Some(index);
            }
        } else if let Some(token_start) = start.take() {
            tokens.push(&code[token_start..index]);
        }
    }
    if let Some(token_start) = start {
        tokens.push(&code[token_start..]);
    }
    tokens
}

fn is_fortran_keyword(token: &str) -> bool {
    matches!(
        token,
        "PROGRAM"
            | "INTEGER"
            | "PARAMETER"
            | "CHARACTER"
            | "CALL"
            | "STOP"
            | "GOTO"
            | "IF"
            | "THEN"
            | "ENDIF"
            | "RETURN"
            | "END"
            | "FUNCTION"
            | "SUBROUTINE"
            | "LOGICAL"
            | "COMMON"
            | "EXTERNAL"
            | "INTRINSIC"
            | "INQUIRE"
            | "INCLUDE"
            | "DIMENSION"
            | "EQUIVALENCE"
            | "DATA"
            | "DOUBLE"
            | "PRECISION"
            | "COMPLEX"
            | "DO"
            | "CONTINUE"
            | "FORMAT"
            | "AND"
            | "OR"
            | "EQ"
            | "NE"
            | "LT"
            | "LE"
            | "GT"
            | "GE"
    )
}
