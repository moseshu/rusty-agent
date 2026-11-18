// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Keyring error classification and redaction, without touching the real keyring.
//!
//! Ported from Codex `codex-rs/keyring-store/src/{error_kind_tests,error_display_tests}.rs`.
//! Codex calls the private classifier directly in one case; these go through the public
//! `io::Error` conversion, which applies the same classifier.

use std::error::Error;
use std::io::ErrorKind;

use keyring::Error as KeyringError;
use keyring::credential::CredentialApi;
use ra_keyring_store::CredentialStoreError;

fn kind(error: KeyringError) -> ErrorKind {
    std::io::Error::from(CredentialStoreError::new(error)).kind()
}

#[test]
fn preserves_typed_causes_without_guessing_from_messages() {
    let cases = [
        (
            KeyringError::NoStorageAccess(Box::new(std::io::Error::other(
                "locked timeout access denied",
            ))),
            ErrorKind::Other,
        ),
        (
            KeyringError::PlatformFailure(Box::new(std::io::Error::new(
                ErrorKind::TimedOut,
                "private account",
            ))),
            ErrorKind::TimedOut,
        ),
        (
            KeyringError::PlatformFailure(Box::new(std::sync::Arc::new(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "private D-Bus socket",
            )))),
            ErrorKind::PermissionDenied,
        ),
        (
            KeyringError::Invalid("private account".into(), "private reason".into()),
            ErrorKind::Other,
        ),
    ];
    for (error, expected) in cases {
        let error = std::io::Error::from(CredentialStoreError::new(error));
        assert_eq!(error.kind(), expected);
        assert!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<CredentialStoreError>()
                .is_some()
        );
    }
}

#[test]
fn classifies_native_backend_errors() {
    #[cfg(target_os = "macos")]
    let native: Box<dyn Error + Send + Sync> =
        Box::new(security_framework::base::Error::from_code(-25291));
    #[cfg(target_os = "linux")]
    let native: Box<dyn Error + Send + Sync> = Box::new(secret_service::Error::Locked);
    #[cfg(target_os = "windows")]
    let native: Box<dyn Error + Send + Sync> = Box::new(keyring::windows::Error(1312));
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    assert_eq!(
        kind(KeyringError::NoStorageAccess(native)),
        if cfg!(target_os = "linux") {
            ErrorKind::WouldBlock
        } else {
            ErrorKind::NotConnected
        },
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unavailable_secret_service_is_not_connected() {
    assert_eq!(
        kind(KeyringError::PlatformFailure(Box::new(
            secret_service::Error::Unavailable,
        ))),
        ErrorKind::NotConnected
    );
}

#[cfg(target_os = "linux")]
#[test]
fn secret_service_dbus_io_error_preserves_its_kind() {
    let error = secret_service::Error::Zbus(
        std::io::Error::new(ErrorKind::PermissionDenied, "private D-Bus socket").into(),
    );
    assert_eq!(
        kind(KeyringError::PlatformFailure(Box::new(error))),
        ErrorKind::PermissionDenied
    );
}

#[cfg(target_os = "macos")]
#[test]
fn read_only_keychain_is_permission_denied() {
    assert_eq!(
        kind(KeyringError::NoStorageAccess(Box::new(
            security_framework::base::Error::from_code(-25292),
        ))),
        ErrorKind::PermissionDenied
    );
}

#[test]
fn payload_bearing_errors_redact_credentials_and_end_the_source_chain() {
    let credential = keyring::mock::MockCredential::default();
    credential.set_password("private-credential-value").unwrap();
    for (error, message) in [
        (
            KeyringError::Ambiguous(vec![Box::new(credential) as Box<keyring::Credential>]),
            "Entry is matched by 1 credentials",
        ),
        (
            KeyringError::BadEncoding(b"private-credential-value".to_vec()),
            "Data is not UTF-8 encoded",
        ),
    ] {
        let error = CredentialStoreError::new(error);
        assert_eq!(
            (
                error.to_string(),
                error.message(),
                format!("{error:?}"),
                error.source().is_none()
            ),
            (
                message.to_string(),
                message.to_string(),
                message.to_string(),
                true
            ),
        );
    }
}

#[test]
fn debug_preserves_native_error_details() {
    let error = CredentialStoreError::new(KeyringError::PlatformFailure(Box::new(
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied by keyring",
        ),
    )));
    assert_eq!(
        format!("{error:?}"),
        "Platform secure storage failure: permission denied by keyring",
    );
}
