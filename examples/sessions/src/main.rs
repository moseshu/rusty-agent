//! Multi-turn history with shipped in-memory and `SQLite` session implementations.

#[path = "../../support.rs"]
mod support;

use std::sync::Arc;

use rusty_agent::{SqliteSession, prelude::*};
use support::{answer, messages, require};

#[tokio::main]
async fn main() -> Result<()> {
    let example = support::setup(vec![
        Box::new(|request| {
            require(
                messages(request) == ["Remember: my project is Atlas."],
                "unexpected first input",
            )?;
            Ok(answer("I will remember Atlas."))
        }),
        Box::new(|request| {
            require(
                messages(request)
                    == [
                        "Remember: my project is Atlas.",
                        "I will remember Atlas.",
                        "What is my project?",
                    ],
                "session must prepend history exactly once",
            )?;
            Ok(answer("Your project is Atlas."))
        }),
    ])
    .await?;
    let agent = AgentSpec::builder()
        .id(AgentId::new("memory"))
        .name("Memory")
        .instructions("Remember the project name and answer briefly.")
        .build()?;
    let sqlite = std::env::args().any(|argument| argument == "--sqlite");
    // A fresh temporary database makes repeated tutorial runs independent.
    let directory = tempfile::tempdir()
        .map_err(|error| Error::caller("cannot create example directory").with_source(error))?;
    let path = directory.path().join("history.db");
    let session_id = SessionId::generate();
    let session: Arc<dyn Session> = if sqlite {
        Arc::new(SqliteSession::open(session_id.clone(), &path)?)
    } else {
        Arc::new(InMemorySession::new(session_id.clone()))
    };
    // tutorial:session:start
    let first = example
        .runner
        .request(agent.clone(), "Remember: my project is Atlas.")
        .with_session(session.clone())
        .run()
        .await?;
    println!("{}", first.final_text());
    // tutorial:session:end
    // Reopen SQLite between turns to prove that history comes from storage.
    let session: Arc<dyn Session> = if sqlite {
        drop(session);
        Arc::new(SqliteSession::open(session_id, &path)?)
    } else {
        session
    };
    let second = example
        .runner
        .request(agent, "What is my project?")
        .with_session(session.clone())
        .run()
        .await?;
    println!("{}", second.final_text());
    if example.check {
        require(
            second.final_text() == "Your project is Atlas.",
            "wrong remembered answer",
        )?;
        require(
            session.get_items(None).await?.len() == 4,
            "session duplicated or lost history",
        )?;
        require(
            first.state().run_id() != second.state().run_id(),
            "new input needs a new run identity",
        )?;
    }
    drop(session);
    example.finish().await
}
