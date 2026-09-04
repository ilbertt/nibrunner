//! Slots are per app, never per instance, so a redeploy is invisible to routing. They are
//! released only on an explicit volume `absent`: reusing a port sooner would route one tenant
//! into another.

use std::collections::BTreeMap;
use std::path::Path;

use nft_render::{describe_slot, AppSlot, FIRST_SLOT, SLOT_COUNT};
use protocol::AppId;

use crate::json_store::{read_json, write_json, StoreError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("all {limit} host slots are allocated")]
pub struct SlotExhausted {
    pub limit: u32,
}

impl SlotExhausted {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

const SLOT_SPAN: u32 = SLOT_COUNT - FIRST_SLOT;

/// Both directions, because a cursor read off disk may be negative and the scan has to land on a
/// slot this host has whatever the file said.
fn wrapped(offset: i64) -> u32 {
    let span = i64::from(SLOT_SPAN);
    let inside = ((offset % span) + span) % span;
    FIRST_SLOT + inside as u32
}

/// Where the next scan starts. A hint and never an authority: the scan only ever returns a slot
/// nothing holds, so a cursor that is stale, missing or nonsense costs a different free slot
/// rather than a wrong one.
pub fn read_slot_cursor(value: Option<serde_json::Value>) -> i64 {
    value.and_then(|value| value.as_i64()).unwrap_or(i64::from(FIRST_SLOT))
}

/// A record file that cannot be read degrades to an empty allocator rather than throwing, and a
/// slot two apps claim is honoured for the first of them only.
pub fn assignments_from(records: BTreeMap<String, serde_json::Value>) -> BTreeMap<AppId, u32> {
    let mut assignments = BTreeMap::new();
    let mut taken = std::collections::BTreeSet::new();
    for (app_id, slot) in records {
        let Some(slot) = slot.as_u64().and_then(|slot| u32::try_from(slot).ok()) else {
            continue;
        };
        let Ok(app_id) = AppId::parse(app_id) else {
            continue;
        };
        if slot < FIRST_SLOT || slot >= SLOT_COUNT || taken.contains(&slot) {
            continue;
        }
        assignments.insert(app_id, slot);
        taken.insert(slot);
    }
    assignments
}

pub struct SlotAllocator {
    assignments: BTreeMap<AppId, u32>,
    cursor: i64,
}

impl SlotAllocator {
    pub fn empty() -> Self {
        Self { assignments: BTreeMap::new(), cursor: i64::from(FIRST_SLOT) }
    }

    pub fn load(slots_file: &Path, cursor_file: &Path) -> Result<Self, StoreError> {
        let records: Option<BTreeMap<String, serde_json::Value>> = read_json(slots_file)?;
        Ok(Self {
            assignments: assignments_from(records.unwrap_or_default()),
            cursor: read_slot_cursor(read_json(cursor_file)?),
        })
    }

    /// After the slots, and never in place of them: a cursor written without them would point
    /// past allocations the next boot has no record of.
    pub fn persist(&self, slots_file: &Path, cursor_file: &Path) -> Result<(), StoreError> {
        let records: BTreeMap<String, u32> =
            self.assignments.iter().map(|(app_id, slot)| (app_id.to_string(), *slot)).collect();
        write_json(slots_file, &records)?;
        write_json(cursor_file, &self.cursor)
    }

    /// The next free slot at or after the cursor rather than the lowest free one, wrapping once
    /// so the scan still ends. A slot only comes back when its app's volume is torn down, and
    /// handing it straight to the next app makes an address a client may still be dialling
    /// somebody else's — so a freed slot waits for the cursor to come round to it.
    fn next_free(&self) -> Option<u32> {
        let taken: std::collections::BTreeSet<u32> = self.assignments.values().copied().collect();
        (0..SLOT_SPAN)
            .map(|step| wrapped(self.cursor - i64::from(FIRST_SLOT) + i64::from(step)))
            .find(|slot| !taken.contains(slot))
    }

    pub fn allocate(&mut self, app_id: &AppId) -> Result<AppSlot, SlotExhausted> {
        if let Some(slot) = self.assignments.get(app_id) {
            return Ok(describe_slot(*slot, app_id.clone()));
        }
        let free = self.next_free().ok_or(SlotExhausted { limit: SLOT_COUNT })?;
        self.assignments.insert(app_id.clone(), free);
        // Only past a slot this just gave away. An app being handed the one it already holds is
        // every redeploy, and moving the cursor there would leave it wherever the last redeploy
        // happened to be — which is as likely to sit on a freed slot as anywhere else.
        self.cursor = i64::from(free) + 1;
        Ok(describe_slot(free, app_id.clone()))
    }

    pub fn lookup(&self, app_id: &AppId) -> Option<AppSlot> {
        self.assignments.get(app_id).map(|slot| describe_slot(*slot, app_id.clone()))
    }

    pub fn release(&mut self, app_id: &AppId) {
        self.assignments.remove(app_id);
    }

    pub fn slots(&self) -> Vec<AppSlot> {
        self.assignments.iter().map(|(app_id, slot)| describe_slot(*slot, app_id.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: impl std::fmt::Display) -> AppId {
        AppId::parse(format!("app-{name}")).unwrap()
    }

    #[test]
    fn a_redeploy_keeps_the_host_port_and_distinct_apps_never_share_a_slot() {
        let mut allocator = SlotAllocator::empty();
        let first = allocator.allocate(&app(1)).unwrap();
        let again = allocator.allocate(&app(1)).unwrap();
        assert_eq!(first.host_port, again.host_port);
        let ports: std::collections::BTreeSet<u16> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|name| allocator.allocate(&app(name)).unwrap().host_port.get())
            .collect();
        assert_eq!(ports.len(), 3);
    }

    /// Asserted against a full host rather than an empty one: the freed slot is then the only one
    /// left, so this says the pool grew back without saying which order it is drawn from.
    #[test]
    fn a_released_slot_becomes_available_again() {
        let mut allocator = SlotAllocator::empty();
        for index in 0..SLOT_COUNT {
            allocator.allocate(&app(index)).unwrap();
        }
        let freed = allocator.allocate(&app(3)).unwrap();
        allocator.release(&app(3));
        assert!(allocator.lookup(&app(3)).is_none());
        assert_eq!(allocator.allocate(&app(1000)).unwrap().slot, freed.slot);
    }

    /// The address a tenant hands its own users is this slot's, and a slot handed straight back
    /// out is that address pointing at somebody else.
    #[test]
    fn a_released_slot_is_not_the_next_one_handed_out() {
        let mut allocator = SlotAllocator::empty();
        let first = allocator.allocate(&app(1)).unwrap();
        allocator.release(&app(1));
        assert_ne!(allocator.allocate(&app(2)).unwrap().slot, first.slot);
    }

    /// A redeploy asks for a slot the app already holds. Moving the cursor there would park it
    /// wherever the busiest app sits, and a slot freed beside it would go straight back out.
    #[test]
    fn being_handed_the_slot_an_app_already_holds_does_not_move_the_cursor() {
        let mut allocator = SlotAllocator::empty();
        let staying = allocator.allocate(&app("staying")).unwrap();
        allocator.allocate(&app("leaving")).unwrap();
        allocator.release(&app("leaving"));
        allocator.allocate(&app("staying")).unwrap();
        assert_ne!(allocator.allocate(&app("arriving")).unwrap().slot, staying.slot + 1);
    }

    #[test]
    fn running_out_of_slots_is_a_typed_failure_not_a_silent_reuse() {
        let mut allocator = SlotAllocator::empty();
        for index in 0..SLOT_COUNT {
            allocator.allocate(&app(index)).unwrap();
        }
        assert_eq!(allocator.allocate(&app(1000)).unwrap_err(), SlotExhausted { limit: SLOT_COUNT });
    }

    /// The cursor is a hint and the scan is the authority: nothing a file holds can make this
    /// give away a slot another app has.
    #[test]
    fn a_cursor_read_off_disk_cannot_hand_out_a_slot_somebody_holds() {
        assert_eq!(read_slot_cursor(None), i64::from(FIRST_SLOT));
        assert_eq!(read_slot_cursor(Some(serde_json::json!("7"))), i64::from(FIRST_SLOT));
        assert_eq!(read_slot_cursor(Some(serde_json::json!(1.5))), i64::from(FIRST_SLOT));
        assert_eq!(read_slot_cursor(Some(serde_json::json!(3))), 3);
        for cursor in [1000i64, -1000] {
            let mut allocator = SlotAllocator { assignments: BTreeMap::new(), cursor };
            let slot = allocator.allocate(&app(1)).unwrap().slot;
            assert!((FIRST_SLOT..SLOT_COUNT).contains(&slot));
        }
    }

    #[test]
    fn allocation_survives_a_restart_and_a_file_that_lost_its_shape() {
        assert!(assignments_from(BTreeMap::new()).is_empty());
        let records = BTreeMap::from([
            ("app-1".to_string(), serde_json::json!("three")),
            ("app-2".to_string(), serde_json::json!(3)),
            ("has.a.dot".to_string(), serde_json::json!(4)),
            ("app-3".to_string(), serde_json::json!(3)),
            ("app-4".to_string(), serde_json::json!(SLOT_COUNT)),
        ]);
        let assignments = assignments_from(records);
        assert_eq!(assignments.get(&app(2)), Some(&3));
        // A duplicate slot is honoured once, and one past the host limit not at all.
        assert_eq!(assignments.get(&app(3)), None);
        assert_eq!(assignments.get(&app(4)), None);
        assert_eq!(assignments.len(), 1);
    }

    #[test]
    fn what_was_persisted_is_what_comes_back() {
        let directory = tempfile::tempdir().unwrap();
        let slots = directory.path().join("slots.json");
        let cursor = directory.path().join("cursor.json");
        let mut allocator = SlotAllocator::empty();
        let held = allocator.allocate(&app(1)).unwrap();
        allocator.persist(&slots, &cursor).unwrap();
        let reloaded = SlotAllocator::load(&slots, &cursor).unwrap();
        assert_eq!(reloaded.lookup(&app(1)).map(|slot| slot.slot), Some(held.slot));
    }
}
