//! What the agent asks of the host while a turn runs — approvals and dynamic tool calls, answered
//! through the requests of [`crate::frame`] — and what the host changes while it runs: starting,
//! steering and interrupting a turn, and updating a thread's or turn's settings, as Codex's
//! app-server does.
