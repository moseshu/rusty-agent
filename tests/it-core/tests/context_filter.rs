//! R10-6b contracts for the model-call input filter chain.

use std::sync::{Arc, Mutex};

use ra_core::{
    error::{Error, Result},
    filter::{ContextFilter, ContextFilterChain, ContextFilterRequest, ModelInputData},
    item::{Message, ModelInputItem},
    state::{RunId, ToolOutputReferenceTracker},
};

/// A filter that replaces every user message body with a fixed replacement.
///
/// Shortening and lengthening are the same operation with a different replacement, which is what
/// lets one fixture cover both signs of a saving.
struct Rewriter {
    name: &'static str,
    replacement: &'static str,
}

impl ContextFilter for Rewriter {
    fn name(&self) -> &str {
        self.name
    }

    fn filter_model_input(
        &self,
        _request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData> {
        let rewritten = data
            .input()
            .iter()
            .map(|item| match item {
                ModelInputItem::Message(_) => {
                    ModelInputItem::Message(Message::user(self.replacement))
                }
                other => other.clone(),
            })
            .collect();
        Ok(data.with_input(rewritten))
    }
}

/// Passes its input through and records what it was handed.
#[derive(Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<Vec<ModelInputItem>>>>,
    instructions: Arc<Mutex<Vec<Option<String>>>>,
}

impl ContextFilter for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }

    fn filter_model_input(
        &self,
        _request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData> {
        self.seen.lock().unwrap().push(data.input().to_vec());
        self.instructions
            .lock()
            .unwrap()
            .push(data.instructions().map(str::to_owned));
        Ok(data)
    }
}

/// Returns data whose instructions differ from the ones it was handed.
struct InstructionRewriter;

impl ContextFilter for InstructionRewriter {
    fn name(&self) -> &str {
        "instruction_rewriter"
    }

    fn filter_model_input(
        &self,
        _request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData> {
        Ok(ModelInputData::new(
            data.input().to_vec(),
            Some("a different prefix".to_owned()),
        ))
    }
}

/// Fails rather than projecting.
struct Failing;

impl ContextFilter for Failing {
    fn name(&self) -> &str {
        "failing"
    }

    fn filter_model_input(
        &self,
        _request: &ContextFilterRequest<'_>,
        _data: ModelInputData,
    ) -> Result<ModelInputData> {
        Err(Error::caller("this result cannot be read"))
    }
}

fn tracker() -> ToolOutputReferenceTracker {
    ToolOutputReferenceTracker::new(RunId::new("run-filter"))
}

fn user(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}

fn data(texts: &[&str]) -> ModelInputData {
    ModelInputData::new(
        texts.iter().map(|text| user(text)).collect(),
        Some("the stable prefix".to_owned()),
    )
}

#[test]
fn an_empty_chain_hands_the_request_back_untouched_and_reports_nothing() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let chain = ContextFilterChain::new();

    let filtered = chain
        .apply(&request, data(&["keep me"]))
        .expect("an empty chain cannot fail");

    assert!(chain.is_empty());
    assert_eq!(chain.len(), 0);
    assert_eq!(filtered.reports(), &[]);
    assert_eq!(filtered.data().input(), [user("keep me")]);
    assert_eq!(filtered.data().instructions(), Some("the stable prefix"));
}

/// A filter that ran and left the request alone is not the same fact as a filter that was never
/// installed, and only the report tells them apart.
#[test]
fn a_filter_that_changes_nothing_still_leaves_a_report() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let mut chain = ContextFilterChain::new();
    chain.push(Arc::new(Recorder::default()));

    let filtered = chain
        .apply(&request, data(&["keep me"]))
        .expect("a pass-through filter succeeds");

    let report = &filtered.reports()[0];
    assert_eq!(report.filter(), "recorder");
    assert!(!report.changed());
    assert_eq!(report.chars_saved(), 0);
    assert_eq!(report.token_estimate_delta(), 0);
}

/// The numbers are measured across the filter's own step rather than declared by it, so they are
/// the difference between what it was handed and what it returned.
#[test]
fn a_saving_is_measured_from_the_input_that_was_replaced() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let mut chain = ContextFilterChain::new();
    chain.push(Arc::new(Rewriter {
        name: "shrink",
        replacement: "ok",
    }));

    let filtered = chain
        .apply(&request, data(&["sixteen chars ok", "sixteen chars ok"]))
        .expect("shrinking succeeds");

    // A message renders to 17 content characters of envelope — its own words are content, its
    // field names are not — so each item measures 33 before and 19 after.
    let report = &filtered.reports()[0];
    assert!(report.changed());
    assert_eq!(report.chars_saved(), 2 * (33 - 19));
    // Rounded per item on the shared four-characters-per-token basis, as every context estimate in
    // this framework is: 9 tokens become 5, twice.
    assert_eq!(report.token_estimate_delta(), 2 * (9 - 5));
}

/// A redaction can replace a short secret with a longer marker. The report has to say the request
/// grew rather than wrap around into a very large saving.
#[test]
fn a_filter_that_grows_the_request_reports_a_negative_saving() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let mut chain = ContextFilterChain::new();
    chain.push(Arc::new(Rewriter {
        name: "grow",
        replacement: "[redacted for containing a credential]",
    }));

    let filtered = chain
        .apply(&request, data(&["hunter2"]))
        .expect("growing succeeds");

    let report = &filtered.reports()[0];
    assert!(report.changed());
    assert_eq!(report.chars_saved(), 7 - 38);
    assert!(report.token_estimate_delta() < 0);
}

/// Order is install order, each filter sees what the previous one produced, and each report covers
/// only its own step — a chain-wide total could not attribute a saving to the filter that made it.
#[test]
fn filters_run_in_install_order_and_each_report_covers_only_its_own_step() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let recorder = Arc::new(Recorder::default());
    let seen = Arc::clone(&recorder.seen);
    let mut chain = ContextFilterChain::new();
    chain.push(Arc::new(Rewriter {
        name: "shrink",
        replacement: "ok",
    }));
    chain.push(Arc::clone(&recorder) as Arc<dyn ContextFilter>);
    chain.push(Arc::new(Rewriter {
        name: "grow",
        replacement: "a considerably longer replacement",
    }));

    let filtered = chain
        .apply(&request, data(&["sixteen chars ok"]))
        .expect("the chain succeeds");

    let names: Vec<&str> = filtered
        .reports()
        .iter()
        .map(|report| report.filter())
        .collect();
    assert_eq!(names, vec!["shrink", "recorder", "grow"]);

    assert!(filtered.reports()[0].chars_saved() > 0);
    // The recorder sat between two filters that each changed the request, and still reports zero:
    // its own step changed nothing.
    assert!(!filtered.reports()[1].changed());
    assert_eq!(filtered.reports()[1].chars_saved(), 0);
    // The last filter is measured from the shrunk request, not from the original one: 19 content
    // characters in, 50 out.
    assert_eq!(filtered.reports()[2].chars_saved(), 19 - 50);

    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![vec![user("ok")]],
        "a filter observes the previous filter's output, not the request the chain started with"
    );
    assert_eq!(
        filtered.data().input(),
        [user("a considerably longer replacement")]
    );
}

/// The instructions are the cached prefix. A filter may price them and may not rewrite them, and
/// the chain says so by name rather than silently restoring the originals.
#[test]
fn a_filter_may_read_the_instructions_but_not_replace_them() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let recorder = Arc::new(Recorder::default());
    let instructions = Arc::clone(&recorder.instructions);
    let mut readonly = ContextFilterChain::new();
    readonly.push(Arc::clone(&recorder) as Arc<dyn ContextFilter>);

    readonly
        .apply(&request, data(&["keep me"]))
        .expect("reading the instructions is allowed");
    assert_eq!(
        instructions.lock().unwrap().clone(),
        vec![Some("the stable prefix".to_owned())]
    );

    let mut rewriting = ContextFilterChain::new();
    rewriting.push(Arc::new(InstructionRewriter));
    let error = rewriting
        .apply(&request, data(&["keep me"]))
        .expect_err("rewriting the instructions is refused");

    let message = error.to_string();
    assert!(message.contains("instruction_rewriter"), "{message}");
    assert!(message.contains("cached prefix"), "{message}");
}

/// A projection that failed halfway is not a request. The chain stops and hands the error back
/// rather than sending whichever prefix of the chain happened to succeed.
#[test]
fn a_failing_filter_stops_the_chain_before_the_rest_of_it_runs() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let recorder = Arc::new(Recorder::default());
    let seen = Arc::clone(&recorder.seen);
    let mut chain = ContextFilterChain::new();
    chain.push(Arc::new(Failing));
    chain.push(Arc::clone(&recorder) as Arc<dyn ContextFilter>);

    let error = chain
        .apply(&request, data(&["keep me"]))
        .expect_err("the chain propagates a filter's error");

    assert!(error.to_string().contains("this result cannot be read"));
    assert!(
        seen.lock().unwrap().is_empty(),
        "the filters after a failed one do not run"
    );
}

/// The turn facts are what separates a reference-aware policy from one that has to read narration
/// to guess which results are still live.
#[test]
fn every_filter_receives_the_same_turn_facts() {
    let run_id = RunId::new("run-filter");
    let mut tracker = ToolOutputReferenceTracker::new(run_id.clone());
    tracker
        .record_turn(1, [ra_core::item::CallId::new("call-1")], [])
        .expect("a completed turn records its outputs");
    let request = ContextFilterRequest::new(&run_id, 4, &tracker);

    assert_eq!(request.run_id(), &run_id);
    assert_eq!(request.current_turn(), 4);
    assert_eq!(
        request
            .tool_output_references()
            .last_referenced_turn(&ra_core::item::CallId::new("call-1")),
        Some(1)
    );
}

/// Two filters of one kind is a legitimate configuration — a trimmer with two allowlists, say — so
/// the name is not a key. Position in the chain is what locates a report.
#[test]
fn two_filters_sharing_a_name_are_reported_in_the_order_they_ran() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let mut chain = ContextFilterChain::new();
    chain.push(Arc::new(Rewriter {
        name: "rewriter",
        replacement: "ok",
    }));
    chain.push(Arc::new(Rewriter {
        name: "rewriter",
        replacement: "a considerably longer replacement",
    }));

    let filtered = chain
        .apply(&request, data(&["sixteen chars ok"]))
        .expect("the chain succeeds");

    assert_eq!(chain.len(), 2);
    let reports = filtered.reports();
    assert_eq!(reports[0].filter(), reports[1].filter());
    assert!(reports[0].chars_saved() > 0);
    assert!(reports[1].chars_saved() < 0);
}

/// The report is what R14 replays against, so it has to survive the round trip it will be stored
/// and compared through.
#[test]
fn a_report_round_trips_through_its_wire_form() {
    let run_id = RunId::new("run-filter");
    let tracker = tracker();
    let request = ContextFilterRequest::new(&run_id, 1, &tracker);
    let mut chain = ContextFilterChain::new();
    chain.push(Arc::new(Rewriter {
        name: "shrink",
        replacement: "ok",
    }));

    let filtered = chain
        .apply(&request, data(&["sixteen chars ok"]))
        .expect("the chain succeeds");
    let report = &filtered.reports()[0];

    let encoded = serde_json::to_string(report).expect("a report serializes");
    let decoded: ra_core::filter::ContextFilterReport =
        serde_json::from_str(&encoded).expect("a report deserializes");
    assert_eq!(&decoded, report);
}
