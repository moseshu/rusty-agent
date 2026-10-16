//! Reconciles per-file upstream attribution with the repository NOTICE manifest.

use crate::{gate::Outcome, source};
use std::collections::BTreeMap;

pub(crate) fn run() -> Outcome {
    let root = source::workspace_root();
    let notice = match std::fs::read_to_string(root.join("NOTICE")) {
        Ok(notice) => notice,
        Err(error) => return Outcome::Fail(vec![format!("cannot read NOTICE: {error}")]),
    };
    let mut recorded = BTreeMap::new();
    let mut violations = Vec::new();
    for line in notice.lines().filter(|line| line.starts_with("| `")) {
        let fields: Vec<_> = line.split('|').map(str::trim).collect();
        if fields.len() != 6 {
            violations.push(format!("invalid NOTICE record: {line}"));
            continue;
        }
        let path = fields[1].trim_matches('`').to_owned();
        let header = format!(
            "// Adapted from {} (Apache-2.0) @ {}, modified.",
            fields[2], fields[3]
        );
        if recorded.insert(path.clone(), header).is_some() {
            violations.push(format!("duplicate NOTICE record: {path}"));
        }
    }
    let count = recorded.len();
    for dir in ["crates", "tests", "xtask", "examples"] {
        for file in source::rust_files(&root.join(dir)) {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let path = source::relative(&file);
            let header = text.lines().next().unwrap_or_default();
            if header.starts_with("// Adapted from ") {
                match recorded.remove(&path) {
                    Some(expected) if expected == header => {}
                    Some(_) => violations.push(format!("{path}: attribution differs from NOTICE")),
                    None => violations.push(format!("{path}: missing NOTICE record")),
                }
            }
        }
    }
    for path in recorded.keys() {
        violations.push(format!(
            "{path}: NOTICE record has no matching source header"
        ));
    }
    Outcome::from_violations(
        violations,
        format!("{count} ported source attributions match NOTICE"),
    )
}
