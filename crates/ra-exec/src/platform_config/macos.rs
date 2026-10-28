// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Forced macOS preference access without product configuration dependencies.
use core_foundation::{
    base::{Boolean, CFType, TCFType},
    string::{CFString, CFStringRef},
};
use std::{ffi::c_void, io};
#[allow(
    unsafe_code,
    reason = "CoreFoundation preferences are exposed through native C calls."
)]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFPreferencesCopyAppValue(key: CFStringRef, application_id: CFStringRef) -> *mut c_void;
    fn CFPreferencesAppValueIsForced(key: CFStringRef, application_id: CFStringRef) -> Boolean;
}

#[allow(
    unsafe_code,
    reason = "CoreFoundation preference calls preserve source ownership and forced-value checks."
)]
/// Refreshes the requested managed preference domain before reading policy.
pub fn synchronize_managed_preferences(application_id: &str) -> io::Result<()> {
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFPreferencesAppSynchronize(application_id: CFStringRef) -> u8;
    }
    let application_id = CFString::new(application_id);
    if unsafe { CFPreferencesAppSynchronize(application_id.as_concrete_TypeRef()) } == 0 {
        return Err(io::Error::other(
            "Failed to synchronize managed preferences",
        ));
    }
    Ok(())
}

/// Reads a string only while the key is forced, checking again after copying it.
pub fn load_managed_preference(application_id: &str, key_name: &str) -> io::Result<Option<String>> {
    let key = CFString::new(key_name);
    let application = CFString::new(application_id);
    load_managed_preference_with(
        application_id,
        key_name,
        || preference_is_forced(&key, &application),
        || copy_preference_value(&key, &application),
    )
}

#[allow(
    unsafe_code,
    reason = "CoreFoundation preference calls preserve source ownership and forced-value checks."
)]
fn preference_is_forced(key: &CFString, application: &CFString) -> bool {
    unsafe {
        CFPreferencesAppValueIsForced(key.as_concrete_TypeRef(), application.as_concrete_TypeRef())
            != 0
    }
}

#[allow(
    unsafe_code,
    reason = "CoreFoundation preference calls preserve source ownership and forced-value checks."
)]
fn copy_preference_value(key: &CFString, application: &CFString) -> Option<CFType> {
    let value_ref = unsafe {
        CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), application.as_concrete_TypeRef())
    };
    if value_ref.is_null() {
        return None;
    }

    // CopyAppValue returns an owned property-list value, not necessarily a string.
    Some(unsafe { CFType::wrap_under_create_rule(value_ref) })
}

fn load_managed_preference_with(
    application_id: &str,
    key_name: &str,
    mut is_forced: impl FnMut() -> bool,
    copy_value: impl FnOnce() -> Option<CFType>,
) -> io::Result<Option<String>> {
    // CopyAppValue also searches user-writable domains. Only forced values may
    // supply administrator configuration or override lower requirements layers.
    if !is_forced() {
        tracing::debug!("No forced managed preference for {application_id} key {key_name}");
        return Ok(None);
    }

    let Some(value) = copy_value() else {
        tracing::debug!("Managed preferences for {application_id} key {key_name} not found");
        return Ok(None);
    };

    // Reject a user-default fallback if the preference stopped being forced
    // during the read. These separate calls do not form an atomic snapshot.
    if !is_forced() {
        tracing::debug!(
            "Managed preference {application_id}:{key_name} is no longer forced after reading"
        );
        return Ok(None);
    }

    let value = value.downcast::<CFString>().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Managed preference {application_id}:{key_name} must be a string"),
        )
    })?;
    Ok(Some(value.to_string()))
}

/// Test-only managed preference reads with injected native values.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub fn test_load_managed_preference_with(
    application_id: &str,
    key_name: &str,
    is_forced: impl FnMut() -> bool,
    copy_value: impl FnOnce() -> Option<CFType>,
) -> io::Result<Option<String>> {
    load_managed_preference_with(application_id, key_name, is_forced, copy_value)
}
