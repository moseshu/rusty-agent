//! A recoverable rollout I/O failure must retain thread ownership until shutdown or discard.
//! File-size limits are process-wide, so both recovery cases run serially in their own test binary.
#![cfg(unix)]

use std::{path::PathBuf, sync::Arc};

use ra_core::{
    agent::control::AgentPath,
    item::{AgentId, Message, ModelInputItem},
    session::{
        SessionId,
        rollout::{
            RolloutItem, RolloutRecorder, RolloutRunStarted, RolloutThreadSpawn, RolloutThreadStore,
        },
    },
    state::RunId,
};
use ra_session::{
    ResumeThreadParams, ResumedThread, RolloutFileRecorder, RolloutSessionMeta,
    RolloutThreadDirectory, ThreadStore,
};

struct FileSizeLimit {
    prior: libc::rlimit,
    prior_signal: libc::sighandler_t,
}

impl FileSizeLimit {
    fn at(limit: u64) -> Self {
        let mut prior = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: the limit calls only operate on valid stack values; ignoring the signal installs no handler.
        let prior_signal = unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut prior), 0);
            let limited = libc::rlimit {
                rlim_cur: limit as libc::rlim_t,
                rlim_max: prior.rlim_max,
            };
            let signal = libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limited), 0);
            signal
        };
        Self {
            prior,
            prior_signal,
        }
    }
}

impl Drop for FileSizeLimit {
    fn drop(&mut self) {
        // SAFETY: restore the exact limit and disposition obtained at construction.
        unsafe {
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &self.prior), 0);
            libc::signal(libc::SIGXFSZ, self.prior_signal);
        }
    }
}

fn directory(name: &str) -> RolloutThreadDirectory {
    let path: PathBuf = std::env::temp_dir()
        .join("rusty_agent_tests/thread_resume_write_failures")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    RolloutThreadDirectory::new(path)
}

fn record(recorder: &dyn RolloutRecorder, run: &str, text: &str) {
    recorder.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new(run), AgentId::new("lead"))
            .with_input(vec![ModelInputItem::Message(Message::user(text))]),
    ));
}

async fn check_recovery(discard: bool) {
    let directory = directory(if discard {
        "discard_raw_recorder"
    } else {
        "shutdown_store_recorder"
    });
    let session_id = SessionId::new("worker");
    let path = directory.rollout_path(&session_id).unwrap();
    let owner: Arc<dyn RolloutRecorder> = if discard {
        Arc::new(RolloutFileRecorder::create_with_session_meta(
            &path,
            RolloutSessionMeta::new(session_id.clone()),
        ))
    } else {
        let spawn = RolloutThreadSpawn::new(
            SessionId::new("root"),
            SessionId::new("root"),
            1,
            AgentPath::root().join("worker").unwrap(),
        );
        directory.create_thread(&session_id, &spawn).await.unwrap()
    };
    record(owner.as_ref(), "run-1", "before");
    owner.flush().await.unwrap();
    let limit = FileSizeLimit::at(std::fs::metadata(&path).unwrap().len());
    record(owner.as_ref(), "run-2", "pending");
    let write_failed = owner.flush().await.is_err();
    let shutdown_failed = owner.shutdown().await.is_err();
    drop(limit);
    assert!(
        write_failed && shutdown_failed,
        "I/O failure must leave a retryable recorder"
    );
    let secondary = RolloutThreadDirectory::new(directory.path());
    let params = ResumeThreadParams::new(session_id);
    let Err(error) = secondary.resume_thread(&params).await else {
        panic!("I/O recovery must not relinquish ownership");
    };
    assert!(error.to_string().contains("another writer"), "{error}");
    if discard {
        owner.discard().await.unwrap();
    } else {
        owner.shutdown().await.unwrap();
    }
    let resumed = ResumedThread::resume(&secondary, &params).await.unwrap();
    let expected: Vec<ModelInputItem> = if discard {
        vec![ModelInputItem::Message(Message::user("before"))]
    } else {
        vec![
            ModelInputItem::Message(Message::user("before")),
            ModelInputItem::Message(Message::user("pending")),
        ]
    };
    assert_eq!(resumed.reconstruction().history(), expected);
    record(resumed.recorder().as_ref(), "run-3", "after");
    resumed.recorder().shutdown().await.unwrap();
    let reopened = ResumedThread::resume(&secondary, &params).await.unwrap();
    let mut expected = expected;
    expected.push(ModelInputItem::Message(Message::user("after")));
    assert_eq!(reopened.reconstruction().history(), expected);
    reopened.recorder().shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_writes_keep_thread_ownership_through_shutdown_retry_and_discard() {
    check_recovery(false).await;
    check_recovery(true).await;
}
