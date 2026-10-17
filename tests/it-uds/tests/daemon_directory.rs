//! The shared daemon socket directory that every sandbox hides, from Codex's
//! `uds/src/daemon_directory.rs`.
#![cfg(unix)]

use std::{os::unix::fs::PermissionsExt, path::Path};

use ra_uds::{prepare_shared_daemon_socket_directory, shared_daemon_socket_directory};

#[test]
fn shared_socket_directory_is_fixed_private_and_independent_of_the_state_directory() {
    let path = shared_daemon_socket_directory().unwrap();
    assert_eq!(
        path.parent(),
        Some(std::fs::canonicalize("/tmp").unwrap().as_path())
    );
    assert!(
        path.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("ra-daemon-")
    );
    let name = path.file_name().unwrap().to_str().unwrap();
    let prefix = name.rsplit_once('-').unwrap().0;
    let longest = Path::new("/private/tmp")
        .join(format!("{prefix}-{}", u32::MAX))
        .join("f".repeat(64));
    assert!(
        longest.as_os_str().len() <= 103,
        "physical path must fit Darwin sun_path"
    );
    assert_eq!(prepare_shared_daemon_socket_directory().unwrap(), path);
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
