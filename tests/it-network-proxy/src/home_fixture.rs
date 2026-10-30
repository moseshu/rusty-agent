//! Keeps managed MITM CA files from policy tests out of the developer's real `RA_HOME`.
use ra_paths::AbsolutePathBuf;
use std::{fs, io};

pub(crate) fn find_ra_home() -> io::Result<AbsolutePathBuf> {
    // Lives under this crate's build output, so it is shared by every test process of one build,
    // like the upstream shared test home, and is removed by `cargo clean`.
    let home = concat!(env!("OUT_DIR"), "/ra-home");
    fs::create_dir_all(home)?;
    AbsolutePathBuf::from_absolute_path(fs::canonicalize(home)?)
}
