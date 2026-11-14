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
    assert_eq!(store.with_untracked(day_model::Keyed::len), 4096);
    c.save().unwrap();
    // Commit releases the pins: even a missing-row fault retries eviction immediately.
    assert!(c.try_get::<Fixture>(5000u64).unwrap().is_none());
    assert!(store.with_untracked(day_model::Keyed::len) <= 64);
    for id in [1u64, 2048, 4096] {
        assert!(c.try_get::<Fixture>(id).unwrap().unwrap().value().peek());
    }
}

/// An import inserts and saves and never faults, so the pass that trims the cache must run
/// at the flush itself: the rows it just wrote are clean and in the file, and a ten-million
/// row seed should run in the memory of its batch.
#[test]
fn a_bulk_insert_is_trimmed_at_save() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Fixture]).unwrap();
    c.set_autosave(false);
    c.set_cache_limit(256);
    for id in 1..=4096 {
        c.insert(Fixture { id, value: false });
    }
    let store = c.cache::<Fixture>();
    assert_eq!(
        store.with_untracked(day_model::Keyed::len),
        4096,
        "dirty rows are pinned until the flush"
    );
    c.save().unwrap();
    let resident = store.with_untracked(day_model::Keyed::len);
    assert!(
        resident <= 256 + 64,
        "the flush released the pins and trimmed the cache: {resident} resident"
    );
    // Trimmed, not lost: the rows fault back from the file.
    assert!(
        !c.try_get::<Fixture>(4096u64)
            .unwrap()
            .unwrap()
            .value()
            .peek()
    );
    assert_eq!(c.table_count::<Fixture>().unwrap(), 4096);
}
