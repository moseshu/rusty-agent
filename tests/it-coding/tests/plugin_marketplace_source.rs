// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Upstream Local/Git marketplace-origin parser fixtures, without downloads.
use ra_coding::plugins::marketplace_add::test_api::*;
use std::path::Path;
use tempfile::TempDir;

#[test]
fn github_shorthand_parses_ref_suffix() {
    assert_eq!(
        parse_marketplace_source("owner/repo@main", /*explicit_ref*/ None).unwrap(),
        MarketplaceSource::Git {
            url: "https://github.com/owner/repo.git".to_string(),
            ref_name: Some("main".to_string()),
        }
    );
}

#[test]
fn git_url_parses_fragment_ref() {
    assert_eq!(
        parse_marketplace_source(
            "https://example.com/team/repo.git#v1",
            /*explicit_ref*/ None
        )
        .unwrap(),
        MarketplaceSource::Git {
            url: "https://example.com/team/repo.git".to_string(),
            ref_name: Some("v1".to_string()),
        }
    );
}

#[test]
fn explicit_ref_overrides_source_ref() {
    assert_eq!(
        parse_marketplace_source("owner/repo@main", Some("release".to_string())).unwrap(),
        MarketplaceSource::Git {
            url: "https://github.com/owner/repo.git".to_string(),
            ref_name: Some("release".to_string()),
        }
    );
}

#[test]
fn github_shorthand_and_git_url_normalize_to_same_source() {
    let shorthand = parse_marketplace_source("owner/repo", /*explicit_ref*/ None).unwrap();
    let git_url = parse_marketplace_source(
        "https://github.com/owner/repo.git",
        /*explicit_ref*/ None,
    )
    .unwrap();

    assert_eq!(shorthand, git_url);
    assert_eq!(
        shorthand,
        MarketplaceSource::Git {
            url: "https://github.com/owner/repo.git".to_string(),
            ref_name: None,
        }
    );
}

#[test]
fn github_url_with_trailing_slash_normalizes_without_extra_path_segment() {
    assert_eq!(
        parse_marketplace_source("https://github.com/owner/repo/", /*explicit_ref*/ None).unwrap(),
        MarketplaceSource::Git {
            url: "https://github.com/owner/repo.git".to_string(),
            ref_name: None,
        }
    );
}

#[test]
fn non_github_https_source_parses_as_git_url() {
    assert_eq!(
        parse_marketplace_source("https://gitlab.com/owner/repo", /*explicit_ref*/ None).unwrap(),
        MarketplaceSource::Git {
            url: "https://gitlab.com/owner/repo".to_string(),
            ref_name: None,
        }
    );
}

#[test]
fn file_url_source_is_rejected() {
    let err =
        parse_marketplace_source("file:///tmp/marketplace.git", /*explicit_ref*/ None).unwrap_err();

    assert!(
        err.to_string()
            .contains("invalid marketplace source format"),
        "unexpected error: {err}"
    );
}

#[test]
fn local_path_source_parses() {
    let source = parse_marketplace_source(".", /*explicit_ref*/ None).unwrap();

    let MarketplaceSource::Local { path } = source else {
        panic!("expected local path source");
    };
    assert!(path.is_absolute());
}

#[test]
fn windows_absolute_paths_look_like_local_paths_on_every_host() {
    assert!(looks_like_local_path(r"C:\Users\alice\marketplace"));
    assert!(looks_like_local_path("C:/Users/alice/marketplace"));
    assert!(looks_like_local_path(r"\\server\share\marketplace"));
    assert!(!looks_like_local_path(r"C:relative\path"));
}

#[test]
fn local_file_source_is_rejected() {
    let tempdir = TempDir::new().unwrap();
    let file = tempdir.path().join("marketplace.json");
    std::fs::write(&file, "{}").unwrap();

    let err = parse_marketplace_source(file.to_str().unwrap(), /*explicit_ref*/ None).unwrap_err();

    assert!(
        err.to_string()
            .contains("local marketplace source must be a directory, not a file"),
        "unexpected error: {err}"
    );
}

#[test]
fn non_git_sources_reject_ref_override() {
    let err = parse_marketplace_source("./marketplace", Some("main".to_string())).unwrap_err();

    assert!(
        err.to_string()
            .contains("--ref is only supported for git marketplace sources"),
        "unexpected error: {err}"
    );
}

#[test]
fn non_git_sources_reject_sparse_checkout() {
    let path = std::env::current_dir().unwrap();
    let err = stage_marketplace_source(
        &MarketplaceSource::Local { path },
        &["plugins/foo".to_string()],
        Path::new("/tmp"),
        |_url, _ref_name, _sparse_paths, _staged_root| Ok(()),
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("--sparse is only supported for git marketplace sources"),
        "unexpected error: {err}"
    );
}

#[test]
fn ssh_url_parses_as_git_url() {
    assert_eq!(
        parse_marketplace_source(
            "ssh://git@github.com/owner/repo.git#main",
            /*explicit_ref*/ None,
        )
        .unwrap(),
        MarketplaceSource::Git {
            url: "ssh://git@github.com/owner/repo.git".to_string(),
            ref_name: Some("main".to_string()),
        }
    );
}
