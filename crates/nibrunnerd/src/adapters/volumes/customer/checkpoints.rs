use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use protocol::{CheckpointId, VolumeId};
use serde::{Deserialize, Serialize};

use super::profile::{failed, io_error, sync_dir};
use crate::adapters::volumes::VolumeError;

pub(in crate::adapters::volumes) type Inventory = BTreeMap<String, String>;

// ZeroFS 2.3.1 lists the immutable checkpoint UUID alongside the reusable operator-facing name.
pub(in crate::adapters::volumes) fn inventory(listing: &str) -> Result<Inventory, VolumeError> {
    if listing.trim() == "No checkpoints found." {
        return Ok(BTreeMap::new());
    }
    let mut entries = Inventory::new();
    let mut header = false;
    for line in listing.lines().map(str::trim).filter(|s| !s.is_empty()) {
        if line.chars().all(|c| ('\u{2500}'..='\u{257f}').contains(&c)) {
            continue;
        }
        let cells = line
            .trim_matches('│')
            .split(['│', '┆'])
            .map(str::trim)
            .collect::<Vec<_>>();
        match cells.as_slice() {
            ["Name", "ID", "Created At"] if !header => header = true,
            [name, id, _] if header && !name.is_empty() => {
                let id =
                    uuid::Uuid::parse_str(id).map_err(|_| failed("unrecognized shared checkpoint ID"))?;
                if entries.insert((*name).to_owned(), id.to_string()).is_some() {
                    return Err(failed("ambiguous shared checkpoint inventory"));
                }
            }
            _ => return Err(failed("unrecognized shared checkpoint listing")),
        }
    }
    if !header || entries.is_empty() {
        return Err(failed("incomplete shared checkpoint listing"));
    }
    Ok(entries)
}

#[derive(Deserialize, Serialize)]
struct Owner {
    volume: VolumeId,
    generation: String,
}

pub(super) fn record(
    root: &Path,
    checkpoint: &CheckpointId,
    volume: &VolumeId,
    generation: String,
) -> Result<(), VolumeError> {
    crate::json_store::make_directory(root, 0o700).map_err(io_error)?;
    let mut staged = tempfile::NamedTempFile::new_in(root).map_err(io_error)?;
    let rendered = serde_json::to_vec(&Owner {
        volume: volume.clone(),
        generation,
    })
    .map_err(|_| failed("cannot encode checkpoint owner"))?;
    staged
        .write_all(&rendered)
        .and_then(|()| staged.as_file().sync_all())
        .map_err(io_error)?;
    staged
        .persist(root.join(checkpoint.as_str()))
        .map_err(|e| io_error(e.error))?;
    sync_dir(root)
}

pub(super) fn belongs_to(
    root: &Path,
    name: &CheckpointId,
    volume: &VolumeId,
    generation: &str,
) -> Result<bool, VolumeError> {
    let file = match File::open(root.join(name.as_str())) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_error(e)),
    };
    let owner: Owner =
        serde_json::from_reader(file).map_err(|_| failed("checkpoint ownership is unreadable"))?;
    Ok(owner.volume == *volume && owner.generation == generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_recorded_generation_of_a_checkpoint_belongs_to_the_source_volume() {
        let root = tempfile::tempdir().unwrap();
        let volume = crate::test_support::volume_id();
        let id = crate::test_support::checkpoint_id();
        let generation = uuid::Uuid::new_v4().to_string();
        let listing =
            format!("│ Name ┆ ID ┆ Created At │\n├─────┼────┼─────┤\n│ {id} ┆ {generation} ┆ date │");
        assert_eq!(inventory(&listing).unwrap()[id.as_str()], generation);
        assert!(inventory("truncated output").is_err());
        assert!(inventory("No checkpoints found.").unwrap().is_empty());
        record(root.path(), &id, &volume, generation.clone()).unwrap();
        assert!(belongs_to(root.path(), &id, &volume, &generation).unwrap());
        assert!(!belongs_to(root.path(), &id, &volume, "replacement").unwrap());
        assert!(!belongs_to(root.path(), &id, &VolumeId::parse("other").unwrap(), &generation).unwrap());
    }
}
