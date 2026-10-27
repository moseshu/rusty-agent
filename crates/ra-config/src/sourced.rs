// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Managed values paired with the authority used in diagnostics.

use super::{Constrained, RequirementSource};

#[derive(Debug, Clone, PartialEq)]
/// A validated value paired with its optional enforcing authority.
pub struct ConstrainedWithSource<T> {
    /// The validated value and retained validator.
    pub value: Constrained<T>,
    /// The authority imposing the constraint, if any.
    pub source: Option<RequirementSource>,
}

impl<T> ConstrainedWithSource<T> {
    /// Pairs a constraint with its enforcing authority.
    pub fn new(value: Constrained<T>, source: Option<RequirementSource>) -> Self {
        Self { value, source }
    }
}

impl<T> std::ops::Deref for ConstrainedWithSource<T> {
    type Target = Constrained<T>;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> std::ops::DerefMut for ConstrainedWithSource<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

/// Value paired with the requirement source it came from, for better error
/// messages.
#[derive(Debug, Clone, PartialEq)]
pub struct Sourced<T> {
    /// The value contributed by this authority.
    pub value: T,
    /// The original authority contributing this value.
    pub source: RequirementSource,
}

impl<T> Sourced<T> {
    /// Pairs a value with its source.
    pub fn new(value: T, source: RequirementSource) -> Self {
        Self { value, source }
    }
}

impl<T> std::ops::Deref for Sourced<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}
