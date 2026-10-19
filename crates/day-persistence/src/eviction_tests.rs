// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate as day_persistence;
use day_reactive::Binding;

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "eviction_fixture")]
struct Fixture {
    #[model(id)]
    id: u64,
    value: bool,
}

#[test]
fn a_large_dirty_transaction_amortizes_eviction_scans() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Fixture]).unwrap();
    c.set_autosave(false);
    for id in 1..=4096 {
        c.insert(Fixture { id, value: false });
    }
    c.save().unwrap();
    c.set_cache_limit(1);
    c.set_cache_limit(64);
    let store = c.cache::<Fixture>();
    let scanned = Rc::new(Cell::new(0));
    {
        let mut tables = c.inner.tables.borrow_mut();
        let hooks = tables.get_mut(&store.store_id()).unwrap();
        let original = hooks.resident_keys.clone();
        let count = scanned.clone();
        hooks.resident_keys = Rc::new(move || {
            let keys = original();
            count.set(count.get() + keys.len());
            keys
        });
    }
    // Every visited row becomes pinned until commit, as in a full unread snapshot.
    for id in 1..=4096u64 {
        c.try_get::<Fixture>(id)
            .unwrap()
            .unwrap()
            .value()
            .write(true);
    }
    assert!(
        scanned.get() < 4096 * 20,
        "quadratic eviction work: {} scanned keys",
        scanned.get()
    );
    assert_eq!(store.with_untracked(|k| k.len()), 4096);
    c.save().unwrap();
    // Commit releases the pins: even a missing-row fault retries eviction immediately.
    assert!(c.try_get::<Fixture>(5000u64).unwrap().is_none());
    assert!(store.with_untracked(|k| k.len()) <= 64);
    for id in [1u64, 2048, 4096] {
        assert!(c.try_get::<Fixture>(id).unwrap().unwrap().value().peek());
    }
}
