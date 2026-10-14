use ra_macros::tool;

#[tool(name = "one", name = "two")]
fn duplicate() {}

#[tool(strict = "yes")]
fn invalid_bool() {}

#[tool(unknown = true)]
fn unsupported() {}

#[tool(timeout = 1)]
fn synchronous_timeout() {}

#[tool(timeout = 0)]
async fn zero_timeout() {}

#[tool(timeout_behavior = "retry")]
async fn invalid_timeout_behavior() {}

#[tool(allowed_callers = [])]
fn no_callers() {}

#[tool(allowed_callers = ["direct", "direct"])]
fn duplicate_callers() {}

#[tool(options = ToolOptions::new())]
fn replacement_options() {}

#[tool(output_json_schema = "schema")]
fn unavailable() {}

fn main() {}
