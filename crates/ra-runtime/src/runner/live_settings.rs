//! Settings published for subsequent steps of one live run, as Codex's
//! `TurnContext::next_step_settings` is captured once by each inference step.

use super::RunConfig;
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Default)]
struct SettingsState {
    next: Option<RunConfig>,
    captured: Option<RunConfig>,
}

/// Published settings and the frozen snapshot that owns outstanding tool calls.
#[derive(Clone)]
pub(crate) struct LiveRunSettings(Arc<Mutex<SettingsState>>);

impl LiveRunSettings {
    pub(crate) fn new(config: RunConfig) -> Self {
        Self(Arc::new(Mutex::new(SettingsState {
            next: Some(config),
            captured: None,
        })))
    }

    pub(crate) fn capture(&self) -> Option<RunConfig> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next
            .clone()
    }

    pub(crate) fn capture_step(&self) -> Option<RunConfig> {
        let mut settings = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let next = settings.next.clone();
        settings.captured.clone_from(&next);
        next
    }

    pub(crate) fn captured_step(&self) -> Option<RunConfig> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .captured
            .clone()
    }

    pub(crate) fn update(&self, update: impl FnOnce(&mut RunConfig)) -> bool {
        let mut settings = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(config) = settings.next.as_mut() else {
            return false;
        };
        update(config);
        true
    }

    pub(crate) fn close(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = SettingsState::default();
    }
}
