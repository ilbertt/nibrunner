use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use protocol::{DesiredLayer, GuestPath, StoredObject};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("image import could not read or write its files: {0}")]
    Io(#[from] std::io::Error),
    #[error("image metadata could not be read or written: {0}")]
    Json(#[from] serde_json::Error),
    #[error("image configuration cannot be used by nibrunner: {0}")]
    Invalid(#[from] protocol::InvalidValue),
    #[error("{tool} failed: {message}")]
    Tool { tool: String, message: String },
    #[error("{0}")]
    Unsupported(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Image {
    id: String,
    architecture: String,
    os: String,
    config: ImageConfig,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ImageConfig {
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    env: Option<Vec<String>>,
    working_dir: String,
    user: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Imported {
    image_id: String,
    image_user: String,
    layers: Vec<DesiredLayer>,
    command: protocol::Command,
}

struct Tools {
    docker: PathBuf,
    mksquashfs: PathBuf,
}

pub(super) fn run(image: &str, output: &Path, program: Option<&GuestPath>) -> Result<(), Error> {
    Tools {
        docker: "docker".into(),
        mksquashfs: "mksquashfs".into(),
    }
    .import(image, output, program)
}

fn execute(command: &mut Command) -> Result<Vec<u8>, Error> {
    let tool = command.get_program().to_string_lossy().into_owned();
    let output = command.output().map_err(|error| Error::Tool {
        tool: tool.clone(),
        message: error.to_string(),
    })?;
    if !output.status.success() {
        return Err(Error::Tool {
            tool,
            message: format!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(output.stdout)
}

impl Tools {
    fn import(&self, image: &str, output: &Path, program: Option<&GuestPath>) -> Result<(), Error> {
        if output.exists() {
            return Err(Error::Unsupported(format!(
                "{} already exists; choose a new output directory",
                output.display()
            )));
        }
        let help = execute(Command::new(&self.mksquashfs).arg("-help-all"))?;
        if !String::from_utf8_lossy(&help).contains("-numeric-owner") {
            return Err(Error::Unsupported(
                "image import needs squashfs-tools 4.7.5 or newer with -numeric-owner".into(),
            ));
        }
        let inspected = execute(Command::new(&self.docker).args(["image", "inspect", "--", image]))?;
        let [image]: [Image; 1] = serde_json::from_slice(&inspected)?;
        if image.os != "linux" || image.architecture != "amd64" {
            return Err(Error::Unsupported(format!(
                "the image is {}/{}, but nibrunner requires linux/amd64",
                image.os, image.architecture
            )));
        }
        if !image.id.strip_prefix("sha256:").is_some_and(is_digest) {
            return Err(Error::Unsupported("Docker returned an invalid image ID".into()));
        }
        let mut directory = OutputDirectory::new(output)?;
        let work = tempfile::tempdir_in(output)?;
        let archive = work.path().join("rootfs.tar");
        let created = execute(Command::new(&self.docker).args([
            "create",
            "--platform",
            "linux/amd64",
            "--pull",
            "never",
            "--entrypoint",
            "/__nibrunner_import__",
            "--",
            &image.id,
            "unused",
        ]))?;
        let id = String::from_utf8_lossy(&created).trim().to_owned();
        if !is_digest(&id) {
            return Err(Error::Unsupported(
                "Docker returned an invalid container ID".into(),
            ));
        }
        let mut container = Container {
            docker: &self.docker,
            id: Some(id),
        };
        execute(
            Command::new(&self.docker)
                .args(["export", "--output"])
                .arg(&archive)
                .arg(container.id.as_deref().expect("the created container has an ID")),
        )?;
        container.remove()?;
        let command = image.config.command(&archive, program)?;
        let packed = work.path().join("rootfs.squashfs");
        // Docker omits the root inode; tar mode otherwise creates it world-writable and owned by the packer.
        execute(
            Command::new(&self.mksquashfs)
                .arg("-")
                .arg(&packed)
                .args([
                    "-tar",
                    "-numeric-owner",
                    "-comp",
                    "zstd",
                    "-noappend",
                    "-no-progress",
                    "-mkfs-time",
                    "0",
                    "-root-uid",
                    "0",
                    "-root-gid",
                    "0",
                    "-root-mode",
                    "0755",
                    "-root-time",
                    "0",
                ])
                .stdin(Stdio::from(File::open(&archive)?)),
        )?;
        let digest = hash(&packed)?;
        let imported = Imported {
            image_id: image.id,
            image_user: image.config.user,
            layers: vec![DesiredLayer::Filesystem {
                object: StoredObject {
                    digest: protocol::Sha256Digest::parse(digest.clone())?,
                    object_key: protocol::ObjectKey::parse(digest.clone())?,
                },
            }],
            command,
        };
        let metadata = work.path().join("import.json");
        let mut file = File::create(&metadata)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut file, &imported)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        directory.published.push(output.join(&digest));
        fs::rename(&packed, output.join(digest))?;
        directory.published.push(output.join("import.json"));
        fs::rename(&metadata, output.join("import.json"))?;
        drop(work);
        directory.complete = true;
        Ok(())
    }
}

fn is_digest(value: &str) -> bool {
    protocol::Sha256Digest::parse(value).is_ok()
}

fn hash(path: &Path) -> Result<String, Error> {
    let mut digest = Sha256::new();
    let mut file = File::open(path)?;
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

struct Container<'a> {
    docker: &'a Path,
    id: Option<String>,
}

impl Container<'_> {
    fn remove(&mut self) -> Result<(), Error> {
        if let Some(id) = &self.id {
            execute(Command::new(self.docker).args(["rm", "--volumes", "--", id]))?;
            self.id = None;
        }
        Ok(())
    }
}

impl Drop for Container<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("temporary import container could not be removed: {error}");
        }
    }
}

struct OutputDirectory<'a> {
    path: &'a Path,
    published: Vec<PathBuf>,
    complete: bool,
}

impl<'a> OutputDirectory<'a> {
    fn new(path: &'a Path) -> Result<Self, Error> {
        fs::DirBuilder::new().mode(0o700).create(path)?;
        Ok(Self {
            path,
            published: Vec::new(),
            complete: false,
        })
    }
}

impl Drop for OutputDirectory<'_> {
    fn drop(&mut self) {
        if !self.complete {
            for path in &self.published {
                let _ = fs::remove_file(path);
            }
            let _ = fs::remove_dir(self.path);
        }
    }
}

impl ImageConfig {
    fn command(
        &self,
        archive: &Path,
        override_program: Option<&GuestPath>,
    ) -> Result<protocol::Command, Error> {
        let argv: Vec<_> = self
            .entrypoint
            .iter()
            .flatten()
            .chain(self.cmd.iter().flatten())
            .cloned()
            .collect();
        let program = argv
            .first()
            .map(String::as_str)
            .or_else(|| override_program.map(GuestPath::as_str))
            .ok_or_else(|| {
                Error::Unsupported(
                    "the image has no ENTRYPOINT or CMD; select an absolute program with --program".into(),
                )
            })?;
        let args = argv.get(1..).unwrap_or_default();
        let environment: BTreeMap<String, String> = self
            .env
            .iter()
            .flatten()
            .map(|assignment| {
                let (name, value) = assignment
                    .split_once('=')
                    .ok_or_else(|| Error::Unsupported("an image ENV entry has no equals sign".into()))?;
                Ok((name.to_owned(), value.to_owned()))
            })
            .collect::<Result<_, Error>>()?;
        let working_directory = GuestPath::parse(if self.working_dir.is_empty() {
            "/"
        } else {
            &self.working_dir
        })?;
        let program = match override_program {
            Some(program) => program.clone(),
            None => resolve_program(
                program,
                &working_directory,
                environment.get("PATH").map(String::as_str),
                archive,
            )?,
        };
        let command = serde_json::json!({
            "program": program,
            "args": args,
            "workingDirectory": working_directory,
            "environment": environment,
        });
        Ok(serde_json::from_value(command)?)
    }
}

fn resolve_program(
    program: &str,
    working_directory: &GuestPath,
    search_path: Option<&str>,
    archive: &Path,
) -> Result<GuestPath, Error> {
    let mut files = BTreeMap::new();
    for entry in tar::Archive::new(File::open(archive)?).entries()? {
        let entry = entry?;
        let path = entry.path()?;
        if path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(Error::Unsupported(
                "the exported filesystem contains an unsafe archive path".into(),
            ));
        }
        let target = entry.link_name()?.map(|target| {
            if entry.header().entry_type().is_hard_link() {
                Path::new("/").join(target)
            } else {
                target.into_owned()
            }
        });
        let path = normalize_archive_path(&path);
        for parent in path.ancestors().skip(1) {
            files.entry(parent.to_path_buf()).or_insert(ImageEntry {
                kind: tar::EntryType::Directory,
                mode: 0o755,
                target: None,
            });
        }
        files.insert(
            path,
            ImageEntry {
                kind: entry.header().entry_type(),
                mode: entry.header().mode()?,
                target,
            },
        );
    }
    let candidates = if program.contains('/') {
        vec![Path::new(working_directory.as_str()).join(program)]
    } else {
        let search_path = search_path.ok_or_else(|| {
            Error::Unsupported(format!(
                "the image has no PATH for {program}; use --program with an absolute guest path"
            ))
        })?;
        search_path
            .split(':')
            .map(|directory| {
                Path::new(working_directory.as_str())
                    .join(directory)
                    .join(program)
            })
            .collect()
    };
    for candidate in candidates {
        if let Some(invoked) = executable_path(&candidate, &files) {
            return Ok(GuestPath::parse(candidate.to_string_lossy().into_owned())
                .or_else(|_| GuestPath::parse(invoked.to_string_lossy().into_owned()))?);
        }
    }
    Err(Error::Unsupported(format!(
        "{program} is not executable in the exported image; use --program with an absolute guest path"
    )))
}

struct ImageEntry {
    kind: tar::EntryType,
    mode: u32,
    target: Option<PathBuf>,
}

type ImageFiles = BTreeMap<PathBuf, ImageEntry>;
const MAX_LINKS: usize = 40;

fn executable_path(path: &Path, files: &ImageFiles) -> Option<PathBuf> {
    let mut pending: VecDeque<_> = path.components().collect();
    let mut resolved = PathBuf::from("/");
    let mut invoked = None;
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        match component {
            Component::RootDir => resolved = PathBuf::from("/"),
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                let next = resolved.join(name);
                let entry = files.get(&next)?;
                if pending.is_empty() && invoked.is_none() {
                    invoked = Some(next.clone());
                }
                if let Some(target) = &entry.target {
                    links += 1;
                    if links > MAX_LINKS {
                        return None;
                    }
                    for component in target.components().rev() {
                        pending.push_front(component);
                    }
                } else {
                    if !pending.is_empty() && !entry.kind.is_dir() {
                        return None;
                    }
                    resolved = next;
                }
            }
            Component::CurDir => {}
            Component::Prefix(_) => return None,
        }
    }
    files
        .get(&resolved)
        .filter(|entry| entry.kind.is_file() && entry.mode & 0o111 != 0)?;
    invoked
}

fn normalize_archive_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        if let Component::Normal(name) = component {
            normalized.push(name);
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn archive(root: &Path) -> PathBuf {
        let path = root.join("rootfs.tar");
        let mut archive = tar::Builder::new(File::create(&path).unwrap());
        for path in [
            "usr/bin/python3",
            "bin/sh",
            "app/server",
            "opt/bin/server",
            "opt/app/data",
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append_data(&mut header, path, std::io::empty()).unwrap();
        }
        for (path, target) in [
            ("bin/python", "../usr/bin/python3"),
            ("usr/local/bin", "/bin"),
            ("bin/loop", "loop"),
            ("app/current", "/opt/app"),
            ("app/linked-server", "current/../bin/server"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            archive.append_link(&mut header, path, target).unwrap();
        }
        archive.finish().unwrap();
        path
    }

    #[test]
    fn entrypoint_and_cmd_keep_argument_boundaries_and_environment_equals_signs() {
        let root = tempfile::tempdir().unwrap();
        let archive = archive(root.path());
        let config: ImageConfig = serde_json::from_value(json!({
            "Entrypoint": ["/bin/sh", "-c"], "Cmd": ["printf '%s' \"$TOKEN\""],
            "Env": ["TOKEN=old", "TOKEN=a=b", "PATH=/bin"], "WorkingDir": "", "User": ""
        }))
        .unwrap();
        let command = serde_json::to_value(config.command(&archive, None).unwrap()).unwrap();
        assert_eq!(command["program"], "/bin/sh");
        assert_eq!(command["args"], json!(["-c", "printf '%s' \"$TOKEN\""]));
        assert_eq!(command["environment"]["TOKEN"], "a=b");
        assert_eq!(command["workingDirectory"], "/");
        assert!(ImageConfig::default().command(&archive, None).is_err());
        assert!(ImageConfig::default()
            .command(&archive, Some(&GuestPath::parse("/app/server").unwrap()))
            .is_ok());
    }

    #[test]
    fn image_path_resolution_follows_links_without_reading_host_files() {
        let root = tempfile::tempdir().unwrap();
        let archive = archive(root.path());
        let cwd = GuestPath::parse("/app").unwrap();
        assert_eq!(
            resolve_program("python", &cwd, Some("/missing:/usr/local/bin"), &archive)
                .unwrap()
                .as_str(),
            "/usr/local/bin/python"
        );
        assert_eq!(
            resolve_program("./server", &cwd, None, &archive)
                .unwrap()
                .as_str(),
            "/app/server"
        );
        assert!(resolve_program("loop", &cwd, Some("/bin"), &archive).is_err());
        assert!(resolve_program("cargo", &cwd, Some("/bin"), &archive).is_err());
        assert!(resolve_program("python", &cwd, None, &archive).is_err());
    }

    #[test]
    fn parent_components_are_applied_after_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let archive = archive(root.path());
        let cwd = GuestPath::parse("/app/current").unwrap();
        assert_eq!(
            resolve_program("../bin/server", &cwd, None, &archive)
                .unwrap()
                .as_str(),
            "/opt/bin/server"
        );
        assert_eq!(
            resolve_program("server", &cwd, Some("../bin"), &archive)
                .unwrap()
                .as_str(),
            "/opt/bin/server"
        );
        assert_eq!(
            resolve_program("/app/linked-server", &cwd, None, &archive)
                .unwrap()
                .as_str(),
            "/app/linked-server"
        );
        assert!(resolve_program("/app/server/../server", &cwd, None, &archive).is_err());
        assert!(resolve_program("/missing/../app/server", &cwd, None, &archive).is_err());
    }

    #[test]
    fn a_program_override_keeps_the_images_arguments_and_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let archive = archive(root.path());
        let config: ImageConfig = serde_json::from_value(json!({
            "Cmd": ["server", "--listen", "0.0.0.0"], "WorkingDir": "/app"
        }))
        .unwrap();
        let command = serde_json::to_value(
            config
                .command(&archive, Some(&GuestPath::parse("/app/server").unwrap()))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(command["program"], "/app/server");
        assert_eq!(command["args"], json!(["--listen", "0.0.0.0"]));
        assert_eq!(command["workingDirectory"], "/app");
    }

    fn fake_tools(root: &Path) -> Tools {
        archive(root);
        fs::write(root.join("image.json"), json!([{
            "Id": format!("sha256:{}", "a".repeat(64)),
            "Architecture": "amd64", "Os": "linux",
            "Config": {"Entrypoint": ["python"], "Cmd": ["-m", "http.server"], "Env": ["PATH=/bin", "TOKEN=private"], "WorkingDir": "/app", "User": "65534:65534"}
        }]).to_string()).unwrap();
        let docker = root.join("docker");
        fs::write(
            &docker,
            r#"#!/bin/sh
set -eu
root=$(dirname "$0")
printf '%s\n' "$@" >> "$root/commands"
case "$1" in
    image) cat "$root/image.json" ;;
    create) printf '%064d\n' 0 ;;
    export)
        if test -e "$root/fail-export"; then exit 1; fi
        cp "$root/rootfs.tar" "$3" ;;
    rm) touch "$root/removed" ;;
    *) exit 2 ;;
esac
"#,
        )
        .unwrap();
        let mksquashfs = root.join("mksquashfs");
        fs::write(
            &mksquashfs,
            r#"#!/bin/sh
set -eu
root=$(dirname "$0")
if test "$1" = -help-all; then printf '%s\n' -numeric-owner; exit 0; fi
if test -e "$root/fail-pack"; then exit 1; fi
cat >/dev/null
printf 'hsqs fake image' > "$2"
"#,
        )
        .unwrap();
        for path in [&docker, &mksquashfs] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Tools { docker, mksquashfs }
    }

    #[test]
    fn a_successful_import_is_pinned_private_and_does_not_run_the_image() {
        let root = tempfile::tempdir().unwrap();
        let tools = fake_tools(root.path());
        let output = root.path().join("imported");
        tools.import("demo:build", &output, None).unwrap();
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("import.json")).unwrap()).unwrap();
        let layer = &metadata["layers"][0];
        let digest = layer["digest"].as_str().unwrap();
        assert_eq!(digest, layer["objectKey"]);
        assert_eq!(digest, hash(&output.join(digest)).unwrap());
        assert_eq!(metadata["command"]["program"], "/bin/python");
        assert_eq!(metadata["command"]["environment"]["TOKEN"], "private");
        assert_eq!(
            fs::metadata(output.join("import.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::metadata(&output).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read_dir(&output).unwrap().count(), 2);
        assert!(root.path().join("removed").exists());
        let commands = fs::read_to_string(root.path().join("commands")).unwrap();
        assert!(commands.contains(&format!("--\nsha256:{}\nunused\n", "a".repeat(64))));
        assert!(!commands
            .lines()
            .any(|argument| argument == "run" || argument == "start"));
    }

    #[test]
    fn failed_export_and_pack_leave_no_container_or_partial_output() {
        for failure in ["fail-export", "fail-pack"] {
            let root = tempfile::tempdir().unwrap();
            let tools = fake_tools(root.path());
            fs::write(root.path().join(failure), "").unwrap();
            let output = root.path().join("imported");
            assert!(tools.import("demo:build", &output, None).is_err());
            assert!(root.path().join("removed").exists());
            assert!(!output.exists());
        }
    }

    #[test]
    fn existing_output_is_never_changed() {
        let root = tempfile::tempdir().unwrap();
        let tools = fake_tools(root.path());
        let output = root.path().join("imported");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep"), "operator data").unwrap();
        assert!(tools.import("demo:build", &output, None).is_err());
        assert_eq!(fs::read_to_string(output.join("keep")).unwrap(), "operator data");
        assert!(!root.path().join("commands").exists());
    }

    #[test]
    fn an_incompatible_image_is_refused_before_creating_a_container_or_output() {
        let root = tempfile::tempdir().unwrap();
        let tools = fake_tools(root.path());
        let mut image: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("image.json")).unwrap()).unwrap();
        image[0]["Architecture"] = json!("arm64");
        fs::write(root.path().join("image.json"), image.to_string()).unwrap();
        let output = root.path().join("imported");
        let refused = tools.import("demo:build", &output, None).unwrap_err();
        assert!(refused.to_string().contains("linux/amd64"));
        assert!(!output.exists());
        assert!(!fs::read_to_string(root.path().join("commands"))
            .unwrap()
            .lines()
            .any(|argument| argument == "create"));
    }
}
