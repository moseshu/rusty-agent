//! Empty. A listing reads each rollout's head, as Codex's local store does without its state
//! database (see [`crate::lite`]); metadata derived as records are appended, as Codex's
//! `ThreadMetadataSync` derives it, belongs above the store. The module is kept only because it
//! was released.
