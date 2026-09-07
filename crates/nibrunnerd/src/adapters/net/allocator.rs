use std::collections::BTreeMap;

use nft_render::{describe_slot, AppSlot, FIRST_SLOT, SLOT_COUNT};
use protocol::AppId;

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

fn wrapped(offset: i64) -> u32 {
    let span = i64::from(SLOT_SPAN);
    let inside = ((offset % span) + span) % span;
    FIRST_SLOT + inside as u32
}

pub fn read_slot_cursor(value: Option<serde_json::Value>) -> i64 {
    value
        .and_then(|value| value.as_i64())
        .unwrap_or(i64::from(FIRST_SLOT))
}

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
        if !(FIRST_SLOT..SLOT_COUNT).contains(&slot) || taken.contains(&slot) {
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
    pub fn restore(&mut self, assignments: BTreeMap<AppId, u32>, cursor: i64) {
        self.assignments = assignments;
        self.cursor = cursor;
    }

    pub fn assignments(&self) -> &BTreeMap<AppId, u32> {
        &self.assignments
    }

    pub fn cursor(&self) -> i64 {
        self.cursor
    }

    pub fn empty() -> Self {
        Self {
            assignments: BTreeMap::new(),
            cursor: i64::from(FIRST_SLOT),
        }
    }

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
        self.cursor = i64::from(free) + 1;
        Ok(describe_slot(free, app_id.clone()))
    }

    pub fn lookup(&self, app_id: &AppId) -> Option<AppSlot> {
        self.assignments
            .get(app_id)
            .map(|slot| describe_slot(*slot, app_id.clone()))
    }

    pub fn release(&mut self, app_id: &AppId) {
        self.assignments.remove(app_id);
    }

    pub fn slots(&self) -> Vec<AppSlot> {
        self.assignments
            .iter()
            .map(|(app_id, slot)| describe_slot(*slot, app_id.clone()))
            .collect()
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

    #[test]
    fn a_released_slot_is_not_the_next_one_handed_out() {
        let mut allocator = SlotAllocator::empty();
        let first = allocator.allocate(&app(1)).unwrap();
        allocator.release(&app(1));
        assert_ne!(allocator.allocate(&app(2)).unwrap().slot, first.slot);
    }

    #[test]
    fn being_handed_the_slot_an_app_already_holds_does_not_move_the_cursor() {
        let mut allocator = SlotAllocator::empty();
        let staying = allocator.allocate(&app("staying")).unwrap();
        allocator.allocate(&app("leaving")).unwrap();
        allocator.release(&app("leaving"));
        allocator.allocate(&app("staying")).unwrap();
        assert_ne!(
            allocator.allocate(&app("arriving")).unwrap().slot,
            staying.slot + 1
        );
    }

    #[test]
    fn running_out_of_slots_is_a_typed_failure_not_a_silent_reuse() {
        let mut allocator = SlotAllocator::empty();
        for index in 0..SLOT_COUNT {
            allocator.allocate(&app(index)).unwrap();
        }
        assert_eq!(
            allocator.allocate(&app(1000)).unwrap_err(),
            SlotExhausted { limit: SLOT_COUNT }
        );
    }

    #[test]
    fn a_cursor_read_off_disk_cannot_hand_out_a_slot_somebody_holds() {
        assert_eq!(read_slot_cursor(None), i64::from(FIRST_SLOT));
        assert_eq!(
            read_slot_cursor(Some(serde_json::json!("7"))),
            i64::from(FIRST_SLOT)
        );
        assert_eq!(
            read_slot_cursor(Some(serde_json::json!(1.5))),
            i64::from(FIRST_SLOT)
        );
        assert_eq!(read_slot_cursor(Some(serde_json::json!(3))), 3);
        for cursor in [1000i64, -1000] {
            let mut allocator = SlotAllocator {
                assignments: BTreeMap::new(),
                cursor,
            };
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
        assert_eq!(assignments.get(&app(3)), None);
        assert_eq!(assignments.get(&app(4)), None);
        assert_eq!(assignments.len(), 1);
    }

    #[test]
    fn restoring_replaces_what_was_held_rather_than_merging_with_it() {
        let mut allocator = SlotAllocator::empty();
        let stale = allocator.allocate(&app(1)).unwrap();
        assert_eq!(allocator.lookup(&app(1)).map(|slot| slot.slot), Some(stale.slot));

        allocator.restore(BTreeMap::from([(app(2), 7)]), 8);
        assert_eq!(allocator.lookup(&app(1)), None);
        assert_eq!(allocator.lookup(&app(2)).map(|slot| slot.slot), Some(7));
        assert_eq!(allocator.cursor(), 8);
        assert_eq!(allocator.assignments().len(), 1);
    }

    #[test]
    fn every_slot_the_host_holds_is_listed_so_the_ruleset_can_be_rendered_from_it() {
        let mut allocator = SlotAllocator::empty();
        assert!(allocator.slots().is_empty());
        let first = allocator.allocate(&app(1)).unwrap();
        let second = allocator.allocate(&app(2)).unwrap();
        let slots = allocator.slots();
        assert_eq!(slots.len(), 2);
        let ports: std::collections::BTreeSet<u16> = slots.iter().map(|slot| slot.host_port.get()).collect();
        assert_eq!(
            ports,
            std::collections::BTreeSet::from([first.host_port.get(), second.host_port.get()])
        );
        allocator.release(&app(1));
        assert_eq!(allocator.slots().len(), 1);
    }

    #[test]
    fn the_cursor_follows_the_slot_that_was_just_handed_out() {
        let mut allocator = SlotAllocator::empty();
        let handed = allocator.allocate(&app(1)).unwrap();
        assert_eq!(allocator.cursor(), i64::from(handed.slot) + 1);
        allocator.allocate(&app(1)).unwrap();
        assert_eq!(allocator.cursor(), i64::from(handed.slot) + 1);
    }

    #[test]
    fn a_host_with_no_room_left_says_how_many_slots_it_has_rather_than_only_that_it_is_full() {
        let exhausted = SlotExhausted { limit: SLOT_COUNT };
        assert_eq!(
            exhausted.message(),
            format!("all {SLOT_COUNT} host slots are allocated")
        );
    }

    #[test]
    fn a_slot_read_off_disk_is_the_slot_the_app_keeps_being_given() {
        let restored = assignments_from(BTreeMap::from([("app-1".to_string(), serde_json::json!(9))]));
        let mut allocator = SlotAllocator::empty();
        allocator.restore(restored, 10);
        assert_eq!(allocator.allocate(&app(1)).unwrap().slot, 9);
        assert_eq!(allocator.lookup(&app(1)).unwrap().slot, 9);
        assert!(allocator.lookup(&app(2)).is_none());
    }

    #[test]
    fn a_slot_number_no_host_could_have_handed_out_is_not_restored() {
        for unusable in [serde_json::json!(-1), serde_json::json!(u64::MAX)] {
            let restored = assignments_from(BTreeMap::from([("app-1".to_string(), unusable.clone())]));
            assert!(restored.is_empty(), "{unusable} was restored as a slot");
        }
    }
}
