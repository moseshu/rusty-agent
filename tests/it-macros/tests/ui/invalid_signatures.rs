use core_contract::tool::ToolContext;
use ra_macros::tool;

#[tool]
fn generic<T>(value: T) {}

#[tool]
unsafe fn unsafe_function() {}

#[tool]
fn borrowed(value: &str) {}

#[tool]
fn pattern((left, right): (u32, u32)) {}

#[tool]
fn late_context(value: u32, context: ToolContext<'_>) {}

#[tool]
fn invalid_context(context: &mut ToolContext<'_>) {}

fn main() {}
