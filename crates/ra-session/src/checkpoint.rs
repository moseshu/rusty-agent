//! Crash-safe incremental checkpoints.
//!
//! There is no separate checkpoint store: as in Codex, a thread's rollout is its checkpoint. What
//! makes it one is spread over the pieces that write it, and this module only says where they are.
//!
//! - **Every record is written as it is recorded.** The
//!   [`RolloutFileRecorder`](crate::rollout::RolloutFileRecorder) hands each record to its writer
//!   task, which appends it as one line, so a run's start and input, each turn it settles, the
//!   plan it records, the files it changes and the usage it pays for reach the file while the run
//!   goes on. A process killed mid-run leaves every complete line behind; a line cut short is
//!   dropped by the reader and repaired when the file is reopened (see
//!   [`RolloutWriter`](crate::rollout::RolloutWriter)).
//! - **Barriers at the points Codex persists.** The runner persists the thread once a run's input
//!   is recorded and before its first model call, and again once input delivered into a running run
//!   is recorded, with the
//!   [`PersistContext`](ra_core::session::rollout::PersistContext) Codex's session uses there. A
//!   run that ends — completed, paused for approval, cancelled after its interrupted-run marker, or
//!   failed — records how it ended and flushes. Closing a spawned agent, or shutting its tree
//!   down, shuts its rollout down once its run has ended.
//! - **Resuming reads what is there.** [`reconstruct_history`](crate::rollout::reconstruct_history)
//!   keeps the records of a run that never recorded an end, and reports that it did not, so a
//!   killed run's settled turns are the history the next run continues from.
//! - **Fast reopening.** A [`RolloutCheckpoint`](crate::rollout::RolloutCheckpoint) record
//!   summarizes the records before it, so reopening a long rollout need not scan it all.
//!
//! What a run has in flight when it is killed — a model answer still streaming, a turn not yet
//! settled — is not kept, as `openai-agents-python` saves a turn to its session only once the turn
//! is complete.
