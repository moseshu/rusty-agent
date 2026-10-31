use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn copy_sources(source: &Path, target: &Path, tests: &Path, fragments: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let path = entry.unwrap().path();
        println!("cargo:rerun-if-changed={}", path.display());
        let destination = target.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_sources(
                &path,
                &destination,
                &tests.join(path.file_name().unwrap()),
                &fragments.join(path.file_name().unwrap()),
            );
        } else {
            let mut text = fs::read_to_string(&path).unwrap();
            // Product-only unused-code expectations do not apply when private tests use helpers.
            while let Some(start) = text.find("#[expect(") {
                let end = start + text[start..].find(")]").unwrap() + 2;
                text.replace_range(start..end, "");
            }
            // Test-only fixtures replace one import each; an unmatched import must fail the build
            // rather than silently fall back to the real resolver or the developer's home.
            match path.file_name().unwrap().to_str().unwrap() {
                "runtime.rs" => replace_exactly_once(
                    &mut text,
                    &path,
                    "use tokio::net::lookup_host;",
                    "use crate::dns_fixture::lookup_host;",
                ),
                "certs.rs" => replace_exactly_once(
                    &mut text,
                    &path,
                    "use ra_config::find_ra_home;",
                    "use crate::home_fixture::find_ra_home;",
                ),
                _ => {}
            }
            let test = tests.join(path.file_name().unwrap());
            if test.is_file() {
                text.push_str(&format!(
                    "
#[cfg(test)]
#[path = {:?}]
mod tests;
",
                    test.to_str().unwrap()
                ));
            }
            let fragment = fragments.join(path.file_name().unwrap());
            if fragment.is_file() {
                text.push_str(&format!(
                    "
#[cfg(test)] include!({:?});
",
                    fragment.to_str().unwrap()
                ));
            }
            fs::write(destination, text).unwrap();
        }
    }
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest
        .join("../../crates/ra-network-proxy/src")
        .canonicalize()
        .unwrap();
    let tests = manifest.join("tests/modules").canonicalize().unwrap();
    println!("cargo:rerun-if-changed={}", tests.display());
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("proxy");
    // Start from an empty copy so a renamed or deleted product module cannot linger.
    if output.exists() {
        fs::remove_dir_all(&output).unwrap();
    }
    let fragments = manifest.join("tests/root_modules").canonicalize().unwrap();
    println!("cargo:rerun-if-changed={}", fragments.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("tests/upstream").display()
    );
    copy_sources(&source, &output, &tests, &fragments);
    assert_no_orphan_tests(&tests, &output);
    assert_no_orphan_tests(&fragments, &output);
    let root = fs::read_to_string(output.join("lib.rs")).unwrap();
    fs::write(output.join("test_root.rs"), item_declarations(&root)).unwrap();
}

fn replace_exactly_once(text: &mut String, path: &Path, from: &str, to: &str) {
    assert_eq!(
        text.matches(from).count(),
        1,
        "expected exactly one `{from}` in {}",
        path.display()
    );
    *text = text.replacen(from, to, 1);
}

// Every test file, at any depth, must have a product module; an orphan must fail the build visibly.
fn assert_no_orphan_tests(tests: &Path, output: &Path) {
    for entry in fs::read_dir(tests).unwrap() {
        let path = entry.unwrap().path();
        let product = output.join(path.file_name().unwrap());
        if path.is_dir() {
            assert_no_orphan_tests(&path, &product);
        } else {
            assert!(
                product.is_file(),
                "test file has no product module: {}",
                path.display()
            );
        }
    }
}

// Returns the crate root after its leading comments and inner attributes, so every item keeps its
// own outer attributes, such as a platform `#[cfg]` on the first module.
fn item_declarations(root: &str) -> &str {
    let mut rest = root;
    loop {
        rest = rest.trim_start();
        if rest.starts_with("//") {
            rest = rest.split_once('\n').map_or("", |(_, tail)| tail);
        } else if rest.starts_with("#![") {
            rest = &rest[inner_attribute_len(rest)..];
        } else {
            return rest;
        }
    }
}

// Length of the leading `#![...]`, skipping brackets inside string literals such as `reason`.
fn inner_attribute_len(text: &str) -> usize {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in text.bytes().enumerate() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
    }
    panic!("unterminated inner attribute in product lib.rs");
}
