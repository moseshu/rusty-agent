use ra_macros::tool;

#[tool(timeout = 1e30)]
async fn excessive_timeout() {}

#[tool(timeout = 1e-30)]
async fn unrepresentable_timeout() {}

#[tool(timeout = -1)]
async fn negative_timeout() {}

#[tool(timeout = -0.5)]
async fn negative_fractional_timeout() {}

fn main() {}
