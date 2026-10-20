// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! A live container behind a mock-toolkit list: autosave at every turn's end must not cost the
//! UI a single patch. Reproduces the Showcase walkthrough shape (type into a bound field, the
//! row label follows) headlessly.

use day_core::AnyPiece;
use day_model::{Keyed, Op, Store};
use day_persistence::{Model, ModelContainer, Sqlite, schema};
use day_pieces::prelude::*;
use day_reactive::flush_sync;
use day_spec::WindowOptions;

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "rows")]
pub struct Task {
    #[model(id)]
    pub id: u32,
    pub name: String,
}

fn boot(root: impl FnOnce() -> AnyPiece + 'static) -> day_mock::MockProbe {
    day_core::uninstall_tree();
    let (mock, probe) = day_mock::MockToolkit::new();
    day_core::launch_with(
        mock,
        WindowOptions::default().title("t").size(400.0, 600.0),
        root,
    );
    probe
}

fn count(probe: &day_mock::MockProbe, needle: &str) -> usize {
    probe.log().iter().filter(|l| l.contains(needle)).count()
}

#[test]
fn keystrokes_into_a_container_store_keep_patching_the_row_label() {
    let container =
        ModelContainer::open(Sqlite::memory(), schema![Task]).expect("open memory container");
    let store: Store<Keyed<Task>> = container.cache::<Task>();
    store.restructure("seed", Op::Insert, 1, |v| {
        for n in 1..=3u32 {
            v.push(Task {
                id: n,
                name: format!("Task {n}"),
            });
        }
    });

    let probe = boot(move || {
        list(store, |slot: ModelSlot<Task>| {
            label(move || slot.name().read())
        })
        .row_height(RowHeight::Uniform(40.0))
        .any()
    });
    let host = probe.find_by_kind("day.list")[0].0;
    probe.list_bind(host, 0, day_mock::MockHandle(9400));
    flush_sync();
    let labels = count(&probe, "update day.label");

    let mut text = String::new();
    for ch in "Renamed task".chars() {
        text.push(ch);
        store.elem(1).name().write(text.clone());
        flush_sync(); // one turn per keystroke; autosave flushes SQL between each
    }

    assert_eq!(
        count(&probe, "update day.label") - labels,
        "Renamed task".chars().count(),
        "every keystroke patched the bound row label with autosave active"
    );
    let last = probe
        .log()
        .iter()
        .rev()
        .find(|l| l.contains("update day.label"))
        .cloned()
        .unwrap_or_default();
    assert!(
        last.contains("Renamed task"),
        "final text reached the cell: {last}"
    );
}

/// `list(query, row)`: a predicate flip arrives at the native host as an animatable row
/// delta, not a reload, and a column the query never mentions produces neither.
#[cfg(feature = "pieces")]
#[test]
fn a_query_backed_list_splices_instead_of_reloading() {
    use day_persistence::Sort;

    let container =
        ModelContainer::open(Sqlite::memory(), schema![Task]).expect("open memory container");
    let store: Store<Keyed<Task>> = container.cache::<Task>();
    store.update("seed", |k| {
        *k = Keyed::new(
            (1..=5u32)
                .map(|n| Task {
                    id: n,
                    name: format!("Task {n}"),
                })
                .collect(),
        );
    });
    let query = container
        .query::<Task>()
        .filter(Task::name().contains("Task"))
        .sort(Sort::asc("id"))
        .live();

    let probe = boot(move || {
        list(query, |slot: ModelSlot<Task>| {
            label(move || slot.name().read())
        })
        .row_height(RowHeight::Uniform(40.0))
        .any()
    });
    flush_sync();
    let reloads = count(&probe, "list reload");
    let splices = count(&probe, "list splice");

    // Rename row 3 so it leaves the set: one splice (Remove), zero reloads.
    store.elem(3).name().write("renamed away".into());
    flush_sync();
    assert_eq!(
        count(&probe, "list splice") - splices,
        1,
        "one splice patch"
    );
    assert_eq!(count(&probe, "list reload") - reloads, 0, "no reload");
    assert!(
        probe
            .log()
            .iter()
            .any(|l| l.contains("list splice [Remove(2)]")),
        "the delta names the row: {:?}",
        probe
            .log()
            .iter()
            .filter(|l| l.contains("splice"))
            .collect::<Vec<_>>()
    );

    // A value edit that stays in the set and in place produces neither a splice nor a reload.
    let (r, s) = (count(&probe, "list reload"), count(&probe, "list splice"));
    store.elem(1).name().write("Task 1 edited".into());
    flush_sync();
    assert_eq!(count(&probe, "list reload") - r, 0);
    assert_eq!(
        count(&probe, "list splice") - s,
        0,
        "the row repaints itself; the set is untouched"
    );
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "ranked")]
pub struct Ranked {
    #[model(id)]
    pub id: u32,
    pub rank: u32,
}

/// A drag over a rank-sorted query: the app writes the new ranks, and the requery is the echo
/// of the move the native rows already made, so the host sees neither a splice nor a reload.
/// An app that answers with some other order gets a reload, never a replay of the query's own
/// deltas onto the moved rows.
#[cfg(feature = "pieces")]
#[test]
fn a_query_backed_list_takes_a_native_move_as_its_echo() {
    use day_persistence::Sort;

    let container =
        ModelContainer::open(Sqlite::memory(), schema![Ranked]).expect("open memory container");
    let store: Store<Keyed<Ranked>> = container.cache::<Ranked>();
    store.update("seed", |k| {
        *k = Keyed::new((0..4u32).map(|n| Ranked { id: n + 1, rank: n }).collect());
    });
    let query = container.query::<Ranked>().sort(Sort::asc("rank")).live();
    let ids = {
        let query = query.clone();
        move || -> Vec<u32> { query.try_collect().unwrap().iter().map(|r| r.id).collect() }
    };
    // Rotate the ranks as the move describes; `refuse` writes the original order back instead.
    let refuse = std::rc::Rc::new(std::cell::Cell::new(false));
    let write_ranks = {
        let (ids, refuse) = (ids.clone(), refuse.clone());
        move |from: usize, to: usize| {
            let mut order = ids();
            if !refuse.get() {
                let id = order.remove(from);
                order.insert(to, id);
            } else {
                order.swap(0, 1);
            }
            for (rank, id) in order.iter().enumerate() {
                store.elem(*id).rank().write(rank as u32);
            }
        }
    };
    let probe = boot(move || {
        list(query, |slot: ModelSlot<Ranked>| {
            label(move || slot.id().read().to_string())
        })
        .row_height(RowHeight::Uniform(40.0))
        .reorderable(Reorder::new(write_ranks))
        .any()
    });
    flush_sync();
    let host = probe.find_by_kind("day.list")[0].0;
    let patches = || count(&probe, "list splice") + count(&probe, "list reload");

    let before = patches();
    assert!(probe.list_move(host, 2, 0));
    flush_sync();
    assert_eq!(ids(), [3, 1, 2, 4]);
    assert_eq!(patches(), before, "the requery is the move's own echo");

    refuse.set(true);
    let reloads = count(&probe, "list reload");
    assert!(probe.list_move(host, 3, 1));
    flush_sync();
    assert_eq!(ids(), [1, 3, 2, 4]);
    assert_eq!(
        count(&probe, "list reload"),
        reloads + 1,
        "another order reloads"
    );
}
