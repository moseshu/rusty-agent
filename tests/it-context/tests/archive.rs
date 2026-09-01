//! Addressable reads of complete history represented by compaction summaries.

use ra_context::{
    archive::read_archived_history,
    compaction::{CompactionLimits, CompactionPolicy, project_compacted_model_input},
};
use ra_core::{
    item::{ArchiveRef, Compaction, ItemId, Message, ModelInputItem, RunItem, RunItemKind},
    session::{Session, SessionId},
};
use ra_session::InMemorySession;
use serde_json::json;

const FIRST_TEXT: &str = "Read config.toml before editing it.";
const SECOND_TEXT: &str = "The exact option is `max_connections = 128`.";

fn message(id: &str, text: &str) -> RunItem {
    RunItem::new(ItemId::new(id), RunItemKind::Message(Message::user(text)))
}

fn compaction_item(id: &str, compaction: Compaction) -> RunItem {
    RunItem::new(ItemId::new(id), RunItemKind::Compaction(compaction))
}

/// A summary covering `message-1` and `message-2`, addressed at `compaction-1` of `session`.
fn archived_summary(session: &SessionId) -> (ArchiveRef, Compaction) {
    let archive_ref = ArchiveRef::new(session, &ItemId::new("compaction-1"))
        .expect("archive reference should be valid");
    let compaction = Compaction::new(
        "The configuration was inspected.",
        vec![ItemId::new("message-1"), ItemId::new("message-2")],
    )
    .with_archive_ref(archive_ref.clone(), None);
    (archive_ref, compaction)
}

#[tokio::test]
async fn an_archive_reference_reads_the_complete_original_records_without_changing_history() {
    let session = InMemorySession::new("session-alpha");
    let (archive_ref, compaction) = archived_summary(session.session_id());
    let first = message("message-1", FIRST_TEXT);
    let second = message("message-2", SECOND_TEXT);
    let summary = compaction_item("compaction-1", compaction);
    let recent = message("message-3", "Now update the documented default.");
    session
        .add_items(vec![
            first.clone(),
            second.clone(),
            summary.clone(),
            recent.clone(),
        ])
        .await
        .expect("history should be stored");

    let archived = read_archived_history(&session, &archive_ref)
        .await
        .expect("archive should be readable")
        .expect("compaction owns this archive reference");

    assert_eq!(archived.archive_ref(), &archive_ref);
    assert_eq!(archived.items(), [first.clone(), second.clone()]);
    assert_eq!(
        session
            .get_items(None)
            .await
            .expect("history remains readable"),
        [first, second, summary, recent]
    );
}

/// History copied into another session keeps the address it was written with. The session named
/// inside a reference is provenance, so grafting a sub-agent transcript into its root — or forking
/// a conversation — must not silently turn every archive into a dead address.
#[tokio::test]
async fn an_archive_reference_still_resolves_after_its_history_moves_to_another_session() {
    let origin = SessionId::new("session-alpha");
    let (archive_ref, compaction) = archived_summary(&origin);
    let first = message("message-1", FIRST_TEXT);
    let second = message("message-2", SECOND_TEXT);

    let host = InMemorySession::new("session-root");
    assert_ne!(host.session_id(), &origin);
    host.add_items(vec![
        first.clone(),
        second.clone(),
        compaction_item("compaction-1", compaction),
    ])
    .await
    .expect("the grafted history should be stored");

    let archived = read_archived_history(&host, &archive_ref)
        .await
        .expect("archive should be readable")
        .expect("the record carrying this reference lives in the session being read");
    assert_eq!(archived.items(), [first, second]);
    assert_eq!(archive_ref.session_id(), Some(&origin));
}

#[tokio::test]
async fn a_reference_no_record_carries_is_unavailable_rather_than_an_error() {
    let session = InMemorySession::new("session-alpha");
    let (archive_ref, _) = archived_summary(session.session_id());
    let stale = ArchiveRef::new(session.session_id(), &ItemId::new("compaction-0"))
        .expect("archive reference should be valid");

    // Same summary at the same item ID, but addressed as `compaction-0`: the reference the caller
    // holds is no longer the one the record carries.
    session
        .add_items(vec![
            message("message-1", FIRST_TEXT),
            message("message-2", SECOND_TEXT),
            compaction_item(
                "compaction-1",
                Compaction::new(
                    "The configuration was inspected.",
                    vec![ItemId::new("message-1"), ItemId::new("message-2")],
                )
                .with_archive_ref(stale.clone(), None),
            ),
        ])
        .await
        .expect("history should be stored");

    assert!(
        read_archived_history(&session, &archive_ref)
            .await
            .expect("a stale reference is not an error")
            .is_none()
    );
    assert!(
        read_archived_history(&session, &stale)
            .await
            .expect("a reference attached to the wrong compaction item is unavailable")
            .is_none(),
        "a parseable archive reference must resolve only from the compaction item it names"
    );

    let empty = InMemorySession::new("session-beta");
    assert!(
        read_archived_history(&empty, &archive_ref)
            .await
            .expect("an empty session is not an error")
            .is_none()
    );
}

#[tokio::test]
async fn broken_coverage_is_reported_as_corruption_rather_than_silently_partial() {
    let missing = InMemorySession::new("missing");
    let missing_ref = ArchiveRef::new(missing.session_id(), &ItemId::new("compaction-1"))
        .expect("archive reference should be valid");
    missing
        .add_items(vec![compaction_item(
            "compaction-1",
            Compaction::new(
                "A summary with a missing source.",
                vec![ItemId::new("gone")],
            )
            .with_archive_ref(missing_ref.clone(), None),
        )])
        .await
        .expect("compaction should be stored");
    let error = read_archived_history(&missing, &missing_ref)
        .await
        .expect_err("missing coverage must be reported as corruption");
    assert_eq!(error.code(), "session.corrupted");

    // The same ID named twice: the summary covers fewer records than it claims to.
    let repeated = InMemorySession::new("repeated");
    let repeated_ref = ArchiveRef::new(repeated.session_id(), &ItemId::new("compaction-1"))
        .expect("archive reference should be valid");
    repeated
        .add_items(vec![
            message("message-1", FIRST_TEXT),
            compaction_item(
                "compaction-1",
                Compaction::new(
                    "A summary naming one source twice.",
                    vec![ItemId::new("message-1"), ItemId::new("message-1")],
                )
                .with_archive_ref(repeated_ref.clone(), None),
            ),
        ])
        .await
        .expect("compaction should be stored");
    let error = read_archived_history(&repeated, &repeated_ref)
        .await
        .expect_err("duplicate coverage must be reported as corruption");
    assert_eq!(error.code(), "session.corrupted");

    // Two stored records share one ID, so "the covered record" is ambiguous.
    let ambiguous = InMemorySession::new("ambiguous");
    let ambiguous_ref = ArchiveRef::new(ambiguous.session_id(), &ItemId::new("compaction-1"))
        .expect("archive reference should be valid");
    ambiguous
        .add_items(vec![
            message("message-1", FIRST_TEXT),
            message("message-1", SECOND_TEXT),
            compaction_item(
                "compaction-1",
                Compaction::new(
                    "A summary over an ambiguous source.",
                    vec![ItemId::new("message-1")],
                )
                .with_archive_ref(ambiguous_ref.clone(), None),
            ),
        ])
        .await
        .expect("compaction should be stored");
    let error = read_archived_history(&ambiguous, &ambiguous_ref)
        .await
        .expect_err("an ambiguous source must be reported as corruption");
    assert_eq!(error.code(), "session.corrupted");
}

#[test]
fn an_archive_reference_is_canonically_encoded() {
    let archive_ref = ArchiveRef::new(&SessionId::new("session / one"), &ItemId::new("summary/%"))
        .expect("reference can encode delimiters");
    assert_eq!(
        archive_ref.as_str(),
        "archive:v1/session%20%2F%20one/summary%2F%25"
    );
    assert_eq!(
        archive_ref.session_id(),
        Some(&SessionId::new("session / one"))
    );
    assert_eq!(
        archive_ref.compaction_item_id(),
        Some(&ItemId::new("summary/%"))
    );

    let restored: ArchiveRef =
        serde_json::from_str(&serde_json::to_string(&archive_ref).expect("reference serializes"))
            .expect("reference deserializes");
    assert_eq!(restored, archive_ref);

    assert!(ArchiveRef::new(&SessionId::new(""), &ItemId::new("summary")).is_err());
    assert!(ArchiveRef::new(&SessionId::new(" padded "), &ItemId::new("summary")).is_err());
}

/// A record must stay readable when it carries an address this build cannot take apart — the
/// downgrade read that the compatibility rules exist for. The address survives verbatim and still
/// selects its own record; only its components are unavailable.
#[tokio::test]
async fn an_unrecognized_address_is_carried_verbatim_instead_of_failing_the_record() {
    let stored = json!({
        "schema_version": 1,
        "summary": "A summary a newer build wrote.",
        "compacted_items": ["message-1"],
        // Neither the version nor the lowercase escape is anything this build would mint.
        "archive_ref": "archive:v2/session%2falpha/compaction-1?generation=4",
    });
    let compaction: Compaction = serde_json::from_value(stored.clone())
        .expect("an unknown address must not fail the record");
    let carried = compaction
        .archive_ref()
        .expect("the address is carried, not dropped")
        .clone();
    assert_eq!(
        carried.as_str(),
        "archive:v2/session%2falpha/compaction-1?generation=4"
    );
    assert_eq!(carried.session_id(), None);
    assert_eq!(carried.compaction_item_id(), None);
    assert_eq!(
        serde_json::to_value(&compaction).expect("the record re-serializes"),
        stored,
        "the address must be written back exactly as it was read"
    );

    let session = InMemorySession::new("session-alpha");
    session
        .add_items(vec![
            message("message-1", FIRST_TEXT),
            compaction_item("compaction-1", compaction),
        ])
        .await
        .expect("history should be stored");
    let archived = read_archived_history(&session, &carried)
        .await
        .expect("an unknown address is still an exact key")
        .expect("the record carrying it is in this session");
    assert_eq!(archived.items(), [message("message-1", FIRST_TEXT)]);
}

#[test]
fn the_model_sees_the_summary_and_only_the_notice_the_host_wrote() {
    let session = SessionId::new("session-alpha");
    let (archive_ref, silent) = archived_summary(&session);

    // Recording an address is not, on its own, something the model is told about: a host that
    // wired up no retrieval tool must not have one advertised on its behalf.
    assert_eq!(silent.model_text(), "The configuration was inspected.");
    assert_eq!(silent.archive_notice(), None);

    let notice = format!("Call `history_read` with `{archive_ref}` if you need the exact wording.");
    let announced = silent.with_archive_ref(archive_ref.clone(), Some(notice.clone()));
    assert_eq!(
        announced.model_text(),
        format!("The configuration was inspected.\n\n{notice}")
    );
    assert_eq!(announced.archive_notice(), Some(notice.as_str()));
    // The address is a retrieval instruction, never an expansion: the covered records named at
    // `message-1`/`message-2` contribute nothing to what the model reads.
    assert!(!announced.model_text().contains(SECOND_TEXT));
    assert!(!announced.model_text().contains("message-1"));
}

#[test]
fn a_compacted_projection_carries_an_archive_reference_without_expanding_the_archive() {
    let history = vec![
        message("message-1", SECOND_TEXT),
        message("message-2", "The latest message stays visible."),
    ];
    let policy = CompactionPolicy::new(
        CompactionLimits::new(None, None, Some(4_000)).expect("a total token trigger"),
        ra_context::compaction::anchor::AnchorRetention::new(0, 0, 1)
            .expect("a non-empty retention policy"),
    )
    .expect("the policy converges");
    let archive_ref = ArchiveRef::new(&SessionId::new("session-alpha"), &ItemId::new("summary-1"))
        .expect("reference should be valid");

    let projection = project_compacted_model_input(&history, policy, [], "A bounded summary.")
        .expect("history compacts")
        .with_archive_ref(archive_ref.clone(), Some("Ask for the archive.".to_owned()));

    assert_eq!(projection.items().len(), 2);
    let ModelInputItem::Compaction(compaction) = &projection.items()[0] else {
        panic!("the summary is the first projected item");
    };
    assert_eq!(compaction.archive_ref(), Some(&archive_ref));
    assert_eq!(compaction.compacted_items(), [ItemId::new("message-1")]);
    assert_eq!(
        compaction.model_text(),
        "A bounded summary.\n\nAsk for the archive."
    );
    // The replaced record's own text is gone from model input; only the summary stands for it.
    assert!(!compaction.model_text().contains(SECOND_TEXT));
}
