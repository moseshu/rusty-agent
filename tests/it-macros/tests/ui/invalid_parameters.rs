use ra_macros::tool;

#[tool]
fn duplicate(#[tool(default = 1, default = 2)] value: u32) {}

#[tool]
fn unsupported(#[tool(rename = "x")] value: u32) {}

#[tool]
fn non_owned(value: std::rc::Rc<String>) {}

fn main() {}
