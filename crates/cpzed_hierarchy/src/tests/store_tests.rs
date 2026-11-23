//! Store tests: in-memory KVP only (`KeyValueStore::open_test_db`), no real HOME or data dirs.
//! Run with `cargo test -p cpzed_hierarchy --features test-support`.

use std::cell::Cell;
use std::rc::Rc;

use db::kvp::KeyValueStore;
use gpui::{AppContext as _, TestAppContext};

use super::*;
use crate::{HierarchyStore, KVP_KEY, KVP_SCOPE, Refusal, WRITE_DEBOUNCE};

#[gpui::test]
async fn edits_persist_through_kvp_and_reload_equal(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_persist").await;
    let a = g("a");
    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    let section = store.update(cx, |s, cx| {
        let (id, created) = s.create_section(&a, "Personas", cx).unwrap();
        assert!(created);
        s.set_section(&term("t"), &a, Some(&id), cx).unwrap();
        s.set_parent(&thr("kid"), Some(&term("t")), cx).unwrap();
        s.set_row_collapsed(&term("t"), true, cx);
        id
    });
    cx.executor().advance_clock(WRITE_DEBOUNCE * 2);
    cx.run_until_parked();

    let reloaded = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    let (live, loaded) = (
        store.read_with(cx, |s, _| s.hierarchy().clone()),
        reloaded.read_with(cx, |s, _| s.hierarchy().clone()),
    );
    assert_eq!(live, loaded);
    assert_eq!(loaded.sections[&a][0].id, section);
    assert!(db.scoped(KVP_SCOPE).read(KVP_KEY).unwrap().is_some());
}

#[gpui::test]
async fn two_observers_each_see_one_notification_per_edit(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_observers").await;
    let store = cx.new(|cx| HierarchyStore::new(db, cx));
    let (first, second) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    for counter in [first.clone(), second.clone()] {
        cx.update(|cx| {
            cx.observe(&store, move |_, _| counter.set(counter.get() + 1))
                .detach()
        });
    }
    store.update(cx, |s, cx| {
        s.set_parent(&term("c"), Some(&term("p")), cx).unwrap();
    });
    cx.run_until_parked();
    assert_eq!((first.get(), second.get()), (1, 1));
    // A refused edit changes nothing and notifies nobody.
    let refused = store.update(cx, |s, cx| s.set_parent(&term("p"), Some(&term("c")), cx));
    assert_eq!(refused, Err(Refusal::Cycle));
    cx.run_until_parked();
    assert_eq!((first.get(), second.get()), (1, 1));
}

#[gpui::test]
async fn a_burst_of_edits_is_one_debounced_write(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_debounce").await;
    let a = g("a");
    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    for i in 0..10 {
        store.update(cx, |s, cx| {
            s.create_section(&a, &format!("S{i}"), cx).unwrap();
        });
        cx.executor().advance_clock(WRITE_DEBOUNCE / 10);
    }
    assert_eq!(store.read_with(cx, |s, _| s.write_count()), 0);
    cx.executor().advance_clock(WRITE_DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(store.read_with(cx, |s, _| s.write_count()), 1);
    let stored = db.scoped(KVP_SCOPE).read(KVP_KEY).unwrap().unwrap();
    assert_eq!(Hierarchy::from_json(&stored).sections[&a].len(), 10);
}

#[gpui::test]
async fn v1_and_garbage_blobs_load_empty_and_idempotent_create_is_quiet(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_bad_blob").await;
    for blob in [r#"{"version":1,"roots":[]}"#, "{{{ nope"] {
        db.scoped(KVP_SCOPE)
            .write(KVP_KEY.to_string(), blob.to_string())
            .await
            .unwrap();
        let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
        assert!(store.read_with(cx, |s, _| s.hierarchy().is_empty()));
    }
    let a = g("a");
    let store = cx.new(|cx| HierarchyStore::new(db, cx));
    store.update(cx, |s, cx| s.create_section(&a, "X", cx).unwrap());
    cx.executor().advance_clock(WRITE_DEBOUNCE * 2);
    cx.run_until_parked();
    let writes = store.read_with(cx, |s, _| s.write_count());
    let (_, created) = store.update(cx, |s, cx| s.create_section(&a, "x", cx).unwrap());
    assert!(!created);
    cx.executor().advance_clock(WRITE_DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(store.read_with(cx, |s, _| s.write_count()), writes);
}

#[gpui::test]
async fn touch_seen_is_quiet_and_flush_writes_immediately(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_touch").await;
    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    let notified = Rc::new(Cell::new(0));
    let n = notified.clone();
    cx.update(|cx| cx.observe(&store, move |_, _| n.set(n.get() + 1)).detach());
    store.update(cx, |s, cx| s.touch_seen(&[term("t")], &[g("a")], 1_000, cx));
    cx.run_until_parked();
    assert_eq!(notified.get(), 0);
    let task = store.update(cx, |s, cx| s.flush(cx));
    task.await;
    let stored = db.scoped(KVP_SCOPE).read(KVP_KEY).unwrap().unwrap();
    assert_eq!(Hierarchy::from_json(&stored).last_seen[&term("t")], 1_000);
    // Seeing it again within the hour is a no-op.
    let before = store.read_with(cx, |s, _| s.hierarchy().clone());
    store.update(cx, |s, cx| s.touch_seen(&[term("t")], &[g("a")], 1_100, cx));
    assert_eq!(store.read_with(cx, |s, _| s.hierarchy().clone()), before);
}

#[gpui::test]
async fn unreadable_blob_is_backed_up_before_the_first_write_replaces_it(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_backup").await;
    let blob = r#"{"version":2,"parents":[[{"Mystery":"x"},{"Terminal":"y"}]]}"#;
    db.scoped(KVP_SCOPE)
        .write(KVP_KEY.to_string(), blob.to_string())
        .await
        .unwrap();
    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    assert!(store.read_with(cx, |s, _| s.hierarchy().is_empty() && !s.is_read_only()));
    store.update(cx, |s, cx| {
        s.set_parent(&term("c"), Some(&term("p")), cx).unwrap();
    });
    cx.executor().advance_clock(WRITE_DEBOUNCE * 2);
    cx.run_until_parked();
    let backup = db
        .scoped(KVP_SCOPE)
        .read(&HierarchyStore::backup_key(blob))
        .unwrap();
    assert_eq!(backup.as_deref(), Some(blob));
    let current = db.scoped(KVP_SCOPE).read(KVP_KEY).unwrap().unwrap();
    assert_eq!(Hierarchy::from_json(&current).parents.len(), 1);
}

#[gpui::test]
async fn future_version_blob_is_never_overwritten(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_future").await;
    let blob = r#"{"version":3,"sections":"a shape this build cannot read"}"#;
    db.scoped(KVP_SCOPE)
        .write(KVP_KEY.to_string(), blob.to_string())
        .await
        .unwrap();
    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    assert!(store.read_with(cx, |s, _| s.is_read_only()));
    store.update(cx, |s, cx| {
        s.set_parent(&term("c"), Some(&term("p")), cx).unwrap();
        s.touch_seen(&[term("c")], &[g("a")], 5_000, cx);
    });
    cx.executor().advance_clock(WRITE_DEBOUNCE * 2);
    cx.run_until_parked();
    let flushed = store.update(cx, |s, cx| s.flush(cx));
    flushed.await;
    assert_eq!(store.read_with(cx, |s, _| s.write_count()), 0);
    assert_eq!(
        db.scoped(KVP_SCOPE).read(KVP_KEY).unwrap().as_deref(),
        Some(blob)
    );
    assert_eq!(
        db.scoped(KVP_SCOPE)
            .read(&HierarchyStore::backup_key(blob))
            .unwrap(),
        None
    );
}

#[gpui::test]
async fn flush_if_dirty_writes_pending_edits_without_waiting_for_the_debounce(
    cx: &mut TestAppContext,
) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_quit").await;
    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    // Clean store: nothing to flush.
    store.update(cx, |s, cx| s.flush_if_dirty(cx)).await;
    assert_eq!(store.read_with(cx, |s, _| s.write_count()), 0);
    store.update(cx, |s, cx| {
        s.set_parent(&term("c"), Some(&term("p")), cx).unwrap();
    });
    // No clock advance: the debounce has not fired. This is what the app-quit hook calls.
    store.update(cx, |s, cx| s.flush_if_dirty(cx)).await;
    let stored = db.scoped(KVP_SCOPE).read(KVP_KEY).unwrap().unwrap();
    assert_eq!(Hierarchy::from_json(&stored).parents.len(), 1);
    // And now it is clean again.
    let writes = store.read_with(cx, |s, _| s.write_count());
    store.update(cx, |s, cx| s.flush_if_dirty(cx)).await;
    assert_eq!(store.read_with(cx, |s, _| s.write_count()), writes);
}

#[gpui::test]
async fn prune_is_a_no_op_until_rows_have_been_touched(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_prune").await;
    let store = cx.new(|cx| HierarchyStore::new(db, cx));
    let a = g("a");
    store.update(cx, |s, cx| {
        s.edit(cx, |h| {
            h.touch_seen(&[term("old")], std::slice::from_ref(&a), 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(s.prune(1_000_000_000, cx), 0);
        s.touch_seen(&[term("fresh")], &[], 1_000_000_000, cx);
        assert_eq!(s.prune(1_000_000_000, cx), 2); // `old` and group `a`
    });
}
