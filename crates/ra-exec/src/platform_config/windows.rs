// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Native Windows configuration namespace discovery.
use std::{io, path::PathBuf};
/// Discovers `ProgramData` using the Windows known-folder API.
#[allow(
    unsafe_code,
    reason = "SHGetKnownFolderPath returns an owned UTF-16 path freed with CoTaskMemFree."
)]
pub fn windows_program_data_dir() -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramData;
    use windows_sys::Win32::UI::Shell::KF_FLAG_DEFAULT;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;

    let mut path_ptr = std::ptr::null_mut::<u16>();
    let known_folder_flags = u32::try_from(KF_FLAG_DEFAULT).map_err(|_| {
        io::Error::other(format!(
            "KF_FLAG_DEFAULT did not fit in u32: {KF_FLAG_DEFAULT}"
        ))
    })?;
    // Known folder IDs reference:
    // https://learn.microsoft.com/en-us/windows/win32/shell/knownfolderid
    // SAFETY: SHGetKnownFolderPath initializes path_ptr with a CoTaskMem-allocated,
    // null-terminated UTF-16 string on success.
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramData,
            known_folder_flags,
            std::ptr::null_mut(),
            &raw mut path_ptr,
        )
    };
    if hr != 0 {
        return Err(io::Error::other(format!(
            "SHGetKnownFolderPath(FOLDERID_ProgramData) failed with HRESULT {hr:#010x}"
        )));
    }
    if path_ptr.is_null() {
        return Err(io::Error::other(
            "SHGetKnownFolderPath(FOLDERID_ProgramData) returned a null pointer",
        ));
    }

    // SAFETY: path_ptr is a valid null-terminated UTF-16 string allocated by
    // SHGetKnownFolderPath and must be freed with CoTaskMemFree.
    let path = unsafe {
        let mut len = 0usize;
        while *path_ptr.add(len) != 0 {
            len += 1;
        }
        let wide = std::slice::from_raw_parts(path_ptr, len);
        let path = PathBuf::from(OsString::from_wide(wide));
        CoTaskMemFree(path_ptr.cast());
        path
    };

    Ok(path)
}
