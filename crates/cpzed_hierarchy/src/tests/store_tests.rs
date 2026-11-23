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
        s.set_row_collapsed(&term("t"), true, cx).unwrap();
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
        assert_eq!(
            s.set_parent(&term("c"), Some(&term("p")), cx),
            Err(Refusal::ReadOnly)
        );
        assert_eq!(s.create_section(&g("a"), "S", cx), Err(Refusal::ReadOnly));
        assert_eq!(
            s.set_row_collapsed(&term("c"), true, cx),
            Err(Refusal::ReadOnly)
        );
        s.touch_seen(&[term("c")], &[g("a")], 5_000, cx);
        assert_eq!(s.prune(1 << 40, cx), 0);
    });
    assert!(store.read_with(cx, |s, _| s.hierarchy().is_empty()));
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
    // A string or huge version is just as "future".
    for odd in [r#"{"version":"3"}"#, r#"{"version":1e20}"#] {
        db.scoped(KVP_SCOPE)
            .write(KVP_KEY.to_string(), odd.to_string())
            .await
            .unwrap();
        let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
        assert!(store.read_with(cx, |s, _| s.is_read_only()), "{odd}");
    }
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
        // The gap since `old` is wall-clock absence, not use: nothing is stale yet.
        assert_eq!(s.prune(1_000_000_000, cx), 0);
        // Thirty-one days of further use later it is.
        let later = 1_000_000_000 + 31 * 86_400;
        s.touch_seen(&[term("fresh")], &[], later, cx);
        assert_eq!(s.prune(later, cx), 2); // `old` and group `a`
    });
}

#[gpui::test]
async fn a_read_error_goes_read_only_and_never_overwrites_the_intact_blob(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_read_error").await;
    let mut good = Hierarchy::default();
    good.set_parent(&term("c"), Some(&term("p"))).unwrap();
    let blob = good.to_json().unwrap();
    db.scoped(KVP_SCOPE)
        .write(KVP_KEY.to_string(), blob.clone())
        .await
        .unwrap();
    // The read fails (as a busy database would); the blob in the database is intact.
    let store = cx.new(|cx| {
        HierarchyStore::from_stored(db.clone(), Err(anyhow::anyhow!("database is locked")), cx)
    });
    assert!(store.read_with(cx, |s, _| s.is_read_only()));
    store.update(cx, |s, cx| {
        assert_eq!(
            s.set_parent(&term("x"), Some(&term("y")), cx),
            Err(Refusal::ReadOnly)
        );
        s.touch_seen(&[term("x")], &[g("a")], 5_000, cx);
    });
    cx.executor().advance_clock(WRITE_DEBOUNCE * 2);
    cx.run_until_parked();
    store.update(cx, |s, cx| s.flush(cx)).await;
    assert_eq!(store.read_with(cx, |s, _| s.write_count()), 0);
    assert_eq!(
        db.scoped(KVP_SCOPE).read(KVP_KEY).unwrap().as_deref(),
        Some(blob.as_str())
    );
}

#[gpui::test]
async fn the_backup_is_durable_before_new_returns(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_backup_order").await;
    let blob = "definitely not json";
    db.scoped(KVP_SCOPE)
        .write(KVP_KEY.to_string(), blob.to_string())
        .await
        .unwrap();
    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    // No run_until_parked, no clock advance: the backup must already be there, so an edit plus an
    // immediate quit-time flush can never land v2 ahead of it.
    assert_eq!(
        db.scoped(KVP_SCOPE)
            .read(&HierarchyStore::backup_key(blob))
            .unwrap()
            .as_deref(),
        Some(blob)
    );
    store.update(cx, |s, cx| {
        s.set_parent(&term("c"), Some(&term("p")), cx).unwrap();
    });
    store.update(cx, |s, cx| s.flush_if_dirty(cx)).await;
    assert_eq!(
        db.scoped(KVP_SCOPE)
            .read(&HierarchyStore::backup_key(blob))
            .unwrap()
            .as_deref(),
        Some(blob)
    );
    let current = db.scoped(KVP_SCOPE).read(KVP_KEY).unwrap().unwrap();
    assert_eq!(Hierarchy::from_json(&current).parents.len(), 1);
}

#[gpui::test]
async fn a_project_not_opened_this_session_survives_a_long_absence(cx: &mut TestAppContext) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_absence").await;
    let (a, b) = (g("a"), g("b"));
    let day = 86_400;
    let t0 = 1_000_000_000;
    // Last session: project A and project B both in use.
    let mut before = Hierarchy::default();
    let (sb, _) = before.create_section(&b, "Personas").unwrap();
    before.set_section(&term("mgr"), &b, Some(&sb)).unwrap();
    before.set_parent(&term("kid"), Some(&term("mgr"))).unwrap();
    before.touch_seen(&[term("mgr"), term("kid")], std::slice::from_ref(&b), t0);
    before.touch_seen(&[term("a1")], std::slice::from_ref(&a), t0);
    db.scoped(KVP_SCOPE)
        .write(KVP_KEY.to_string(), before.to_json().unwrap())
        .await
        .unwrap();

    // 90 days later the user opens only project A.
    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    let later = t0 + 90 * day;
    store.update(cx, |s, cx| {
        s.touch_seen(&[term("a1")], std::slice::from_ref(&a), later, cx);
        assert_eq!(s.prune(later, cx), 0);
    });
    let kept = store.read_with(cx, |s, _| s.hierarchy().clone());
    assert_eq!(kept.sections[&b][0].items, vec![term("mgr")]);
    assert_eq!(kept.parents[&term("kid")], term("mgr"));

    // Genuinely unused for 30 further days of use: now B is stale.
    let much_later = later + 31 * day;
    store.update(cx, |s, cx| {
        s.touch_seen(&[term("a1")], std::slice::from_ref(&a), much_later, cx);
        assert!(s.prune(much_later, cx) > 0);
    });
    let pruned = store.read_with(cx, |s, _| s.hierarchy().clone());
    assert!(!pruned.sections.contains_key(&b));
    assert!(!pruned.parents.contains_key(&term("kid")));
}

#[gpui::test]
async fn a_failed_backup_write_goes_read_only_and_leaves_the_blob_untouched(
    cx: &mut TestAppContext,
) {
    let db = KeyValueStore::open_test_db("cpzed_hierarchy_backup_fails").await;
    let blob = "unreadable, and the backup cannot be written";
    db.scoped(KVP_SCOPE)
        .write(KVP_KEY.to_string(), blob.to_string())
        .await
        .unwrap();
    // A fake failing KVP: writes of any `v2.backup.*` key abort (like a full disk would).
    db.write(|connection| -> anyhow::Result<()> {
        connection.exec(
            "CREATE TRIGGER fail_backup_writes BEFORE INSERT ON scoped_kv_store \
             WHEN NEW.key LIKE 'v2.backup.%' BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )?()
    })
    .await
    .unwrap();

    let store = cx.new(|cx| HierarchyStore::new(db.clone(), cx));
    assert!(store.read_with(cx, |s, _| s.is_read_only()));
    store.update(cx, |s, cx| {
        assert_eq!(
            s.set_parent(&term("c"), Some(&term("p")), cx),
            Err(Refusal::ReadOnly)
        );
    });
    cx.executor().advance_clock(WRITE_DEBOUNCE * 2);
    cx.run_until_parked();
    store.update(cx, |s, cx| s.flush(cx)).await;
    store.update(cx, |s, cx| s.flush_if_dirty(cx)).await;
    assert_eq!(store.read_with(cx, |s, _| s.write_count()), 0);
    // The original blob is byte-identical, and no backup was recorded.
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
