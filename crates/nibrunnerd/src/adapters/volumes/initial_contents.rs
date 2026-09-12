//! What a fresh volume is formatted with: the archive the document names, laid out under a
//! directory the way the volume should hold it, for `mke2fs -d` to copy in as it formats. That
//! keeps the host from ever mounting a tenant's volume, and puts the contents behind the same
//! superblock check that stops a volume being formatted twice.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use guest_contract::paths::{TENANT_GID, TENANT_UID, VOLUME_UPPER_NAME};
use protocol::{DesiredVolume, InitialContents, VolumeId};

use crate::adapters::volumes::VolumeError;
use crate::json_store::make_directory;
use crate::ports::{ArtifactStore, ArtifactStoreExt};

const STAGING_DIR_MODE: u32 = 0o700;
/// What the guest's init gives the directories it makes on the volume, and so what the ones made
/// here get: root's, and readable by the tenant.
const ROOT_DIR_MODE: u32 = 0o755;

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const USTAR_MAGIC_OFFSET: usize = 257;
const USTAR_MAGIC: &[u8; 5] = b"ustar";
/// A zip opens with its first entry's header, or with the end-of-directory record when it has none.
const ZIP_MAGICS: [[u8; 4]; 2] = [*b"PK\x03\x04", *b"PK\x05\x06"];

/// What a zip entry gets when the zip does not say: one made on Windows says nothing.
const ZIP_FILE_MODE: u32 = 0o644;

pub struct ContentsStaging {
    artifacts: Arc<dyn ArtifactStore>,
    directory: PathBuf,
    owner: (u32, u32),
}

impl ContentsStaging {
    pub fn new(artifacts: Arc<dyn ArtifactStore>, directory: PathBuf) -> Self {
        Self {
            artifacts,
            directory,
            owner: (TENANT_UID, TENANT_GID),
        }
    }

    #[cfg(test)]
    pub(crate) fn given_to(mut self, uid: u32, gid: u32) -> Self {
        self.owner = (uid, gid);
        self
    }

    /// The volume's initial contents fetched and laid out, or nothing for a volume that starts
    /// empty.
    pub async fn stage_for(&self, desired: &DesiredVolume) -> Result<Option<StagedRoot>, VolumeError> {
        match &desired.initial_contents {
            Some(contents) => self.stage(&desired.volume_id, contents).await.map(Some),
            None => Ok(None),
        }
    }

    /// The archive unpacked where the volume should hold it: under `upper`, which is where
    /// overlayfs keeps every write the tenant makes to its root, at the path the document names.
    /// That directory and everything under it are the tenant's; the ones above it are made the
    /// way the guest's init would make them.
    pub async fn stage(
        &self,
        volume_id: &VolumeId,
        contents: &InitialContents,
    ) -> Result<StagedRoot, VolumeError> {
        let unusable = |reason: String| VolumeError::ContentsUnusable(reason);
        let bytes = self
            .artifacts
            .read_verified(&contents.object)
            .await
            .map_err(|error| unusable(error.message()))?;
        let format = archive_format(&bytes).ok_or_else(|| {
            unusable(format!(
                "{} is not a tar, a gzipped tar or a zip",
                contents.object.digest
            ))
        })?;

        let root = StagedRoot::fresh(&self.directory, volume_id)?;
        let destination = root
            .path()
            .join(VOLUME_UPPER_NAME)
            .join(contents.destination_path.as_str().trim_start_matches('/'));
        make_directory(&destination, ROOT_DIR_MODE)
            .map_err(|error| unusable(format!("{} could not be made: {error}", destination.display())))?;
        match format {
            ArchiveFormat::Tar => unpack_tar(bytes.as_slice(), &destination),
            ArchiveFormat::GzippedTar => {
                unpack_tar(flate2::read::GzDecoder::new(bytes.as_slice()), &destination)
            }
            ArchiveFormat::Zip => unpack_zip(&bytes, &destination),
        }
        .map_err(unusable)?;
        give_to(&destination, self.owner).map_err(|error| {
            unusable(format!(
                "{} could not be given to uid {}: {error}",
                destination.display(),
                self.owner.0
            ))
        })?;
        Ok(root)
    }
}

/// A root laid out for one format, and gone with this: it is read once, by that format.
#[derive(Debug)]
pub struct StagedRoot {
    path: PathBuf,
}

impl StagedRoot {
    fn fresh(directory: &Path, volume_id: &VolumeId) -> Result<Self, VolumeError> {
        let unusable = |what: &Path, error: std::io::Error| {
            VolumeError::ContentsUnusable(format!("{} could not be made: {error}", what.display()))
        };
        make_directory(directory, STAGING_DIR_MODE).map_err(|error| unusable(directory, error))?;
        let path = directory.join(volume_id.as_str());
        // What a daemon that died mid-format left here is not what this document asks for.
        if let Err(error) = std::fs::remove_dir_all(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(unusable(&path, error));
            }
        }
        make_directory(&path, ROOT_DIR_MODE).map_err(|error| unusable(&path, error))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    Tar,
    GzippedTar,
    Zip,
}

fn archive_format(bytes: &[u8]) -> Option<ArchiveFormat> {
    if bytes.starts_with(&GZIP_MAGIC) {
        return Some(ArchiveFormat::GzippedTar);
    }
    if ZIP_MAGICS.iter().any(|magic| bytes.starts_with(magic)) {
        return Some(ArchiveFormat::Zip);
    }
    bytes
        .get(USTAR_MAGIC_OFFSET..USTAR_MAGIC_OFFSET + USTAR_MAGIC.len())
        .is_some_and(|magic| magic == USTAR_MAGIC)
        .then_some(ArchiveFormat::Tar)
}

// The archive is a tenant's, unpacked by root on the host: an entry that climbs out of the
// directory it is given, by name or through a symlink an earlier entry planted, fails the whole
// archive rather than being passed over. Set-id bits are dropped on the way, and ownership is
// not read from the archive at all — it is given afterwards, to the one uid that runs there.
fn unpack_tar(reader: impl std::io::Read, into: &Path) -> Result<(), String> {
    let unreadable = |error: std::io::Error| format!("the archive could not be read: {error}");
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(unreadable)? {
        let mut entry = entry.map_err(unreadable)?;
        let path = entry.path().map_err(unreadable)?.into_owned();
        let inside = entry
            .unpack_in(into)
            .map_err(|error| format!("{} could not be unpacked: {error}", path.display()))?;
        if !inside {
            return Err(format!(
                "{} reaches outside the directory it is unpacked into",
                path.display()
            ));
        }
    }
    Ok(())
}

// The same rules, for a zip: a name that climbs out is refused, and so is a symlink, which the
// zip crate would otherwise plant for a later entry to be written through — a zip is what a
// laptop's archiver writes, and those write none. Nothing else is then ever followed, so a
// name that stays inside stays inside.
fn unpack_zip(bytes: &[u8], into: &Path) -> Result<(), String> {
    let unreadable = |error: zip::result::ZipError| format!("the archive could not be read: {error}");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(unreadable)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(unreadable)?;
        let name = entry.name().to_string();
        let Some(inside) = entry.enclosed_name() else {
            return Err(format!(
                "{name} reaches outside the directory it is unpacked into"
            ));
        };
        if entry.is_symlink() {
            return Err(format!("{name} is a symlink, which a zip does not carry here"));
        }
        let path = into.join(inside);
        let mode = entry.unix_mode().map(|mode| mode & 0o777);
        let written = if entry.is_dir() {
            make_directory(&path, mode.unwrap_or(ROOT_DIR_MODE))
        } else {
            write_entry(&mut entry, &path, mode.unwrap_or(ZIP_FILE_MODE))
        };
        written.map_err(|error| format!("{name} could not be unpacked: {error}"))?;
    }
    Ok(())
}

fn write_entry(entry: &mut impl std::io::Read, path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // Only a directory no entry has made yet: one an entry did make keeps the mode it gave.
    if let Some(parent) = path.parent().filter(|parent| !parent.exists()) {
        make_directory(parent, ROOT_DIR_MODE)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    std::io::copy(entry, &mut file)?;
    // The umask had a say in the mode the file was opened with, and has none here.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn give_to(path: &Path, (uid, gid): (u32, u32)) -> std::io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    if path.symlink_metadata()?.file_type().is_dir() {
        for entry in std::fs::read_dir(path)? {
            give_to(&entry?.path(), (uid, gid))?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ports::ArtifactError;
    use crate::test_support::mocks;
    use crate::test_support::{desired_volume, initial_contents, volume_id};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    pub(crate) const SEEDED_FILE: &str = "hello.txt";
    pub(crate) const SEEDED_BYTES: &[u8] = b"hello from the archive\n";

    /// A tar with a directory, a file in it, an executable and a symlink, the way `tar -c` on a
    /// laptop writes one: owned by whoever ran it, which is nobody the guest knows.
    pub(crate) fn archive() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_uid(1000);
        header.set_gid(1000);
        header.set_size(0);
        builder
            .append_data(&mut header, "nested", std::io::empty())
            .unwrap();

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_uid(1000);
        header.set_gid(1000);
        header.set_size(SEEDED_BYTES.len() as u64);
        builder
            .append_data(&mut header, format!("nested/{SEEDED_FILE}"), SEEDED_BYTES)
            .unwrap();

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o4755);
        header.set_size(2);
        builder.append_data(&mut header, "run.sh", &b"#!"[..]).unwrap();

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        builder
            .append_link(&mut header, "latest", format!("nested/{SEEDED_FILE}"))
            .unwrap();
        builder.into_inner().unwrap()
    }

    /// The same tree as [`archive`], less the symlink, as `zip -r` on that laptop writes it.
    pub(crate) fn zipped() -> Vec<u8> {
        use std::io::Write;
        let deflated =
            || zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .add_directory("nested/", deflated().unix_permissions(0o755))
            .unwrap();
        writer
            .start_file(
                format!("nested/{SEEDED_FILE}"),
                deflated().unix_permissions(0o644),
            )
            .unwrap();
        writer.write_all(SEEDED_BYTES).unwrap();
        writer
            .start_file("run.sh", deflated().unix_permissions(0o755))
            .unwrap();
        writer.write_all(b"#!").unwrap();
        writer.finish().unwrap().into_inner()
    }

    pub(crate) fn gzipped(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    /// Whoever runs these tests is not root, and can give a file to nobody but themself: read
    /// off the nearest directory that exists, which is theirs.
    pub(crate) fn me(directory: &Path) -> (u32, u32) {
        let nearest = directory.ancestors().find(|path| path.exists()).unwrap();
        let owner = std::fs::metadata(nearest).unwrap();
        (owner.uid(), owner.gid())
    }

    /// Staging under `directory` that hands the tenant's files to whoever runs the tests.
    pub(crate) fn staging(directory: &Path, bytes: Vec<u8>) -> ContentsStaging {
        let (uid, gid) = me(directory);
        ContentsStaging::new(mocks::artifacts_holding(bytes), directory.to_path_buf()).given_to(uid, gid)
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[tokio::test]
    async fn the_archive_lands_under_upper_at_the_path_the_document_names() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = archive();
        let staged = staging(directory.path(), bytes.clone())
            .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
            .await
            .unwrap();
        assert_eq!(staged.path(), directory.path().join("vol-1"));
        let data = staged.path().join("upper/app/data");
        assert_eq!(
            std::fs::read(data.join("nested").join(SEEDED_FILE)).unwrap(),
            SEEDED_BYTES
        );
        assert_eq!(
            std::fs::read_link(data.join("latest")).unwrap(),
            Path::new("nested/hello.txt")
        );
        assert_eq!(mode_of(&data.join("run.sh")), 0o755, "the set-id bit is dropped");
        assert_eq!(mode_of(&data.join("nested")), 0o755);
        assert_eq!(mode_of(&staged.path().join("upper")), ROOT_DIR_MODE);
        assert_eq!(mode_of(&staged.path().join("upper/app")), ROOT_DIR_MODE);
    }

    #[tokio::test]
    async fn a_zip_lands_the_same_way_with_the_modes_it_carries() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = zipped();
        let staged = staging(directory.path(), bytes.clone())
            .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
            .await
            .unwrap();
        let data = staged.path().join("upper/app/data");
        assert_eq!(
            std::fs::read(data.join("nested").join(SEEDED_FILE)).unwrap(),
            SEEDED_BYTES
        );
        assert_eq!(mode_of(&data.join("nested")), 0o755);
        assert_eq!(mode_of(&data.join("nested").join(SEEDED_FILE)), 0o644);
        assert_eq!(mode_of(&data.join("run.sh")), 0o755);
        assert_eq!(mode_of(&staged.path().join("upper/app")), ROOT_DIR_MODE);
    }

    #[tokio::test]
    async fn everything_under_the_destination_is_the_tenants() {
        let (uid, gid) = {
            let directory = tempfile::tempdir().unwrap();
            me(directory.path())
        };
        for bytes in [archive(), zipped()] {
            let directory = tempfile::tempdir().unwrap();
            let staged = staging(directory.path(), bytes.clone())
                .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
                .await
                .unwrap();
            let data = staged.path().join("upper/app/data");
            for path in [
                data.clone(),
                data.join("nested"),
                data.join("nested").join(SEEDED_FILE),
                data.join("run.sh"),
            ] {
                let owner = std::fs::symlink_metadata(&path).unwrap();
                assert_eq!((owner.uid(), owner.gid()), (uid, gid), "{}", path.display());
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let bytes = archive();
        let staged = staging(directory.path(), bytes.clone())
            .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
            .await
            .unwrap();
        let link = std::fs::symlink_metadata(staged.path().join("upper/app/data/latest")).unwrap();
        assert_eq!(
            (link.uid(), link.gid()),
            (uid, gid),
            "the symlink itself, not what it points at"
        );
    }

    #[tokio::test]
    async fn a_gzipped_archive_is_the_same_archive() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = gzipped(&archive());
        let staged = staging(directory.path(), bytes.clone())
            .stage(&volume_id(), &initial_contents(&bytes, "/"))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(staged.path().join("upper/nested").join(SEEDED_FILE)).unwrap(),
            SEEDED_BYTES
        );
    }

    #[tokio::test]
    async fn the_root_goes_when_the_format_that_read_it_is_done() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = archive();
        let staged = staging(directory.path(), bytes.clone())
            .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
            .await
            .unwrap();
        let path = staged.path().to_path_buf();
        assert!(path.is_dir());
        drop(staged);
        assert!(!path.exists());
        assert_eq!(mode_of(directory.path()) & 0o777, STAGING_DIR_MODE);
    }

    #[tokio::test]
    async fn what_an_earlier_attempt_left_is_cleared_before_the_archive_is_laid_out() {
        let directory = tempfile::tempdir().unwrap();
        let stale = directory.path().join("vol-1/upper/app/data/stale");
        std::fs::create_dir_all(&stale).unwrap();
        let bytes = archive();
        let staged = staging(directory.path(), bytes.clone())
            .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
            .await
            .unwrap();
        assert!(!stale.exists());
        assert!(staged.path().join("upper/app/data/run.sh").exists());
    }

    #[tokio::test]
    async fn a_volume_that_starts_empty_stages_nothing_and_reads_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let refusing = mocks::artifacts_refusing(ArtifactError::Transfer("never asked".into()));
        let staging = ContentsStaging::new(refusing, directory.path().to_path_buf());
        assert!(staging
            .stage_for(&desired_volume(|_| {}))
            .await
            .unwrap()
            .is_none());
        assert!(std::fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn an_entry_that_climbs_out_fails_the_archive_and_leaves_nothing_staged() {
        let directory = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(1);
        // Written into the header by hand: the builder refuses to write such a name itself.
        header.as_old_mut().name[..10].copy_from_slice(b"../escaped");
        header.set_cksum();
        builder.append(&header, &b"x"[..]).unwrap();
        let bytes = builder.into_inner().unwrap();

        let error = staging(directory.path(), bytes.clone())
            .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
            .await
            .unwrap_err();
        assert!(matches!(error, VolumeError::ContentsUnusable(_)), "{error}");
        assert!(error.message().contains("reaches outside"), "{error}");
        assert!(!directory.path().join("escaped").exists());
        assert!(
            !directory.path().join("vol-1").exists(),
            "nothing half-staged is left"
        );
    }

    #[tokio::test]
    async fn a_zip_entry_that_climbs_out_or_is_a_symlink_fails_the_archive() {
        let escaping = {
            use std::io::Write;
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            writer
                .start_file("../escaped", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"x").unwrap();
            writer.finish().unwrap().into_inner()
        };
        let linking = {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            writer
                .add_symlink("latest", "/etc/passwd", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.finish().unwrap().into_inner()
        };
        for (bytes, refused_for) in [(escaping, "reaches outside"), (linking, "is a symlink")] {
            let directory = tempfile::tempdir().unwrap();
            let error = staging(directory.path(), bytes.clone())
                .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
                .await
                .unwrap_err();
            assert!(matches!(error, VolumeError::ContentsUnusable(_)), "{error}");
            assert!(error.message().contains(refused_for), "{error}");
            assert!(!directory.path().join("escaped").exists());
            assert!(
                !directory.path().join("vol-1").exists(),
                "nothing half-staged is left"
            );
        }
    }

    #[tokio::test]
    async fn bytes_that_are_no_archive_are_refused_by_digest() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"\x7fELF and then a program".to_vec();
        let error = staging(directory.path(), bytes.clone())
            .stage(&volume_id(), &initial_contents(&bytes, "/app/data"))
            .await
            .unwrap_err();
        assert!(
            error.message().contains("is not a tar, a gzipped tar or a zip"),
            "{error}"
        );
        assert!(!directory.path().join("vol-1").exists());
    }

    #[tokio::test]
    async fn bytes_that_are_not_what_the_document_claims_never_reach_a_volume() {
        let directory = tempfile::tempdir().unwrap();
        let store_holds = gzipped(&archive());
        let error = staging(directory.path(), store_holds)
            .stage(&volume_id(), &initial_contents(&archive(), "/app/data"))
            .await
            .unwrap_err();
        assert!(matches!(error, VolumeError::ContentsUnusable(_)), "{error}");
        assert!(error.message().contains("hashes to"), "{error}");
        assert!(!directory.path().join("vol-1").exists());
    }

    #[test]
    fn an_archive_is_known_by_its_magic() {
        assert_eq!(archive_format(&archive()), Some(ArchiveFormat::Tar));
        assert_eq!(
            archive_format(&gzipped(b"anything")),
            Some(ArchiveFormat::GzippedTar)
        );
        assert_eq!(archive_format(&zipped()), Some(ArchiveFormat::Zip));
        let empty_zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()))
            .finish()
            .unwrap()
            .into_inner();
        assert_eq!(archive_format(&empty_zip), Some(ArchiveFormat::Zip));
        assert_eq!(archive_format(b""), None);
        assert_eq!(archive_format(&vec![0u8; 10240]), None);
        assert_eq!(archive_format(b"\x7fELF"), None);
    }
}
