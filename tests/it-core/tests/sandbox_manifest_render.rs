//! `ra-core::sandbox::manifest_render`: what a manifest looks like when it is shown to a model.
//!
//! The behavior pinned here is what a model reads and then plans against:
//! - the tree, checked line for line against the reference's own rendering
//! - a mount is drawn where it attaches, which is not always where it was declared
//! - depth bounds how far down the tree goes
//! - a cut listing says that it was cut, because a silent one reads as complete

use ra_core::sandbox::{
    Entry, MAX_MANIFEST_DESCRIPTION_CHARS, Manifest, MountPattern, MountProvider, MountStrategy,
    MountpointOptions, S3Mount, truncate_manifest_description,
};

fn mount() -> Entry {
    Entry::mount(
        ra_core::sandbox::Mount::new(
            MountProvider::S3(S3Mount {
                bucket: "bucket".to_owned(),
                ..S3Mount::default()
            }),
            MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions::default())),
        )
        .expect("supported"),
    )
    .with_description("shared data")
}

fn manifest() -> Manifest {
    Manifest::new()
        .with_root("/workspace")
        .with_entry(
            "repo",
            Entry::dir()
                .with_description("project root")
                .with_child("README.md", Entry::file("hi").with_description("overview")),
        )
        .with_entry("data", mount())
        .with_entry("notes.txt", Entry::file("n"))
}

#[test]
fn the_tree_is_drawn_the_way_the_reference_draws_it() {
    // Checked against the reference's own output for the same manifest, line for line: this goes
    // into a model's instructions, so a difference in it is a difference in what the model reads.
    assert_eq!(
        manifest().describe(Some(2)).expect("renders"),
        "/workspace\n\
         ├── data/          # /workspace/data — shared data\n\
         ├── notes.txt      # /workspace/notes.txt\n\
         └── repo/          # /workspace/repo — project root\n    \
             └── README.md  # /workspace/repo/README.md — overview\n"
    );
}

#[test]
fn depth_bounds_how_far_down_the_tree_goes() {
    // The column the comments start in is measured over what is actually drawn, so a shallower
    // tree is a narrower one.
    assert_eq!(
        manifest().describe(Some(1)).expect("renders"),
        "/workspace\n\
         ├── data/      # /workspace/data — shared data\n\
         ├── notes.txt  # /workspace/notes.txt\n\
         └── repo/      # /workspace/repo — project root\n"
    );
}

#[test]
fn an_unbounded_depth_draws_the_whole_tree() {
    assert_eq!(
        manifest().describe(None).expect("renders"),
        manifest().describe(Some(2)).expect("renders")
    );
}

#[test]
fn a_directory_is_drawn_with_a_trailing_slash_and_a_file_is_not() {
    let rendered = manifest().describe(None).expect("renders");

    assert!(rendered.contains("repo/"));
    assert!(rendered.contains("data/"));
    assert!(rendered.contains("notes.txt  "));
    assert!(!rendered.contains("notes.txt/"));
}

#[test]
fn a_mount_is_drawn_where_it_attaches() {
    // The attach path is the one the model would have to use, and it differs from the declaration
    // whenever `mount_path` is set.
    let manifest = Manifest::new().with_root("/workspace").with_entry(
        "logical",
        Entry::mount(
            ra_core::sandbox::Mount::new(
                MountProvider::S3(S3Mount {
                    bucket: "bucket".to_owned(),
                    ..S3Mount::default()
                }),
                MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions::default())),
            )
            .expect("supported")
            .at("actual"),
        ),
    );

    let rendered = manifest.describe(None).expect("renders");
    assert!(rendered.contains("logical/"), "{rendered}");
    assert!(rendered.contains("# /workspace/actual"), "{rendered}");
}

#[test]
fn an_empty_manifest_is_just_its_root() {
    assert_eq!(
        Manifest::new().describe(None).expect("renders"),
        "/workspace\n"
    );
}

#[test]
fn a_manifest_that_could_not_be_materialized_is_not_described_as_though_it_could() {
    // Every path is validated before anything is drawn. A tree showing an entry that would be
    // refused at materialization is a tree a model would plan against and then fail on.
    let manifest = Manifest::new().with_entry(
        "safe",
        Entry::dir().with_child("../outside.txt", Entry::file("nope")),
    );

    assert!(manifest.describe(None).is_err());
}

// --- truncation ---------------------------------------------------------------------------------

#[test]
fn a_cut_listing_says_that_it_was_cut() {
    // A silently truncated tree is one a model reads as complete.
    let long = "0123456789".repeat(100);
    let truncated = truncate_manifest_description(&long, Some(200));

    assert!(truncated.len() <= 200);
    assert!(truncated.contains("truncated"), "{truncated}");
    assert!(truncated.contains("ls"), "{truncated}");
}

#[test]
fn a_listing_that_fits_is_left_alone() {
    let short = "short";

    assert_eq!(truncate_manifest_description(short, None), short);
    assert_eq!(truncate_manifest_description(short, Some(5000)), short);
}

#[test]
fn truncation_respects_even_a_limit_too_small_for_the_notice() {
    // The notice is longer than some limits. It is cut too rather than allowed to overrun, because
    // the limit is what the surrounding prompt budgeted for.
    let long = "0123456789".repeat(20);

    for max_chars in 0..40 {
        let truncated = truncate_manifest_description(&long, Some(max_chars));
        assert!(
            truncated.len() <= max_chars,
            "{max_chars} produced {} chars",
            truncated.len()
        );
    }
}

#[test]
fn a_description_is_bounded_by_default() {
    let wide = (0..2000).fold(Manifest::new(), |manifest, index| {
        manifest.with_entry(format!("file-{index}"), Entry::file("x"))
    });

    let rendered = wide.describe(None).expect("renders");
    assert!(rendered.len() <= MAX_MANIFEST_DESCRIPTION_CHARS);
    assert!(rendered.contains("truncated"));

    // A caller that has its own budget can say so.
    let unbounded = wide.describe_within(None, None).expect("renders");
    assert!(unbounded.len() > MAX_MANIFEST_DESCRIPTION_CHARS);
    assert!(!unbounded.contains("truncated"));
}
