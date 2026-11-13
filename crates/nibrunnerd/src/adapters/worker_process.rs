use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) fn install_program(directory: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    crate::json_store::make_directory(directory, 0o700)?;
    let path = directory.join("nibrunnerd");
    if std::fs::read(&path).is_ok_and(|existing| existing == bytes) {
        return Ok(path);
    }
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(&path).map_err(|error| error.error)?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(path)
}
