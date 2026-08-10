//! Tool-set registration.
//!
//! Only the editing entry lives here. Everything else this agent advertises — `exec_command`,
//! `write_stdin`, `read_file`, `grep`, `glob`, `view_image`, `web_search`, `web_fetch`,
//! `ask_user`, `update_plan`, `skill`, and the `agent.*` family — comes from [`ra_tools`],
//! because swapping this product out would not change a line of any of them, only force them to
//! be written again (R2-12). What stays is what a second product would genuinely write
//! differently: the V4A wrapper and the editing discipline around it.

pub(crate) mod apply_patch;
