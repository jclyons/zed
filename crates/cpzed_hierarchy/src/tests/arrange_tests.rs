use std::collections::BTreeSet;
use std::time::Instant;

use super::*;
use crate::{ArrangeInput, Rollup, Slot, arrange_rows};

#[test]
fn empty_hierarchy_is_todays_newest_first_order() {
    let group = g("a");
    let rows = [
        info(&term("old"), &group, 1),
        info(&thr("new"), &group, 30),
        info(&term("mid"), &group, 20),
    ];
    let slots = arrange(&group, &rows, &[], &Hierarchy::default());
    assert_eq!(shape(&slots), ["new:0", "mid:0", "old:0"]);
}

#[test]
fn query_mode_is_flat_newest_first_even_with_hierarchy() {
    let group = g("a");
    let mut h = Hierarchy::default();
    h.set_parent(&term("child"), Some(&term("parent"))).unwrap();
    h.create_section(&group, "S").unwrap();
    let rows = [
        info(&term("parent"), &group, 1),
        info(&term("child"), &group, 5),
    ];
    let slots = arrange_rows(&ArrangeInput {
        group: &group,
        rows: &rows,
        foreign: &[],
        elsewhere: &BTreeSet::new(),
        hierarchy: &h,
        query_mode: true,
    });
    assert_eq!(shape(&slots), ["child:0", "parent:0"]);
}

#[test]
fn placement_unsectioned_first_then_sections_in_order_children_under_parent() {
    let group = g("a");
    let mut h = Hierarchy::default();
    let (alpha, _) = h.create_section(&group, "Alpha").unwrap();
    let (beta, _) = h.create_section(&group, "Beta").unwrap();
    h.set_section(&term("b1"), &group, Some(&beta)).unwrap();
    h.set_section(&term("a1"), &group, Some(&alpha)).unwrap();
    h.set_section(&term("a2"), &group, Some(&alpha)).unwrap(); // top of Alpha
    h.set_parent(&term("kid"), Some(&term("a1"))).unwrap();
    let rows = [
        info(&term("free-old"), &group, 1),
        info(&term("free-new"), &group, 9),
        info(&term("a1"), &group, 2),
        info(&term("a2"), &group, 3),
        info(&term("b1"), &group, 4),
        info(&term("kid"), &group, 5),
    ];
    let slots = arrange(&group, &rows, &[], &h);
    assert_eq!(
        shape(&slots),
        [
            "free-new:0",
            "free-old:0",
            "[Alpha]",
            "a2:0",
            "a1:0",
            "kid:1",
            "[Beta]",
            "b1:0"
        ]
    );
}

#[test]
fn children_ordering_unordered_first_then_child_order() {
    let group = g("a");
    let mut h = Hierarchy::default();
    for c in ["c1", "c2"] {
        h.set_parent(&term(c), Some(&term("p"))).unwrap();
    }
    // Stored order c1, c2; c3 is new (unordered) and goes first.
    let rows = [
        info(&term("p"), &group, 1),
        info(&term("c1"), &group, 100),
        info(&term("c2"), &group, 50),
        info(&term("c3"), &group, 2),
    ];
    h.parents.insert(term("c3"), term("p"));
    let slots = arrange(&group, &rows, &[], &h);
    assert_eq!(shape(&slots), ["p:0", "c3:1", "c1:1", "c2:1"]);
}

#[test]
fn cross_project_child_shows_under_parent_with_foreign_flag() {
    let a = g("a");
    let b = g("b");
    let mut h = Hierarchy::default();
    h.set_parent(&term("far"), Some(&term("boss"))).unwrap();
    let rows = [info(&term("boss"), &a, 1)];
    let foreign = [info(&term("far"), &b, 5)];
    let slots = arrange(&a, &rows, &foreign, &h);
    assert_eq!(shape(&slots), ["boss:0", "far:1"]);
    match &slots[1] {
        Slot::Row {
            foreign_project, ..
        } => assert!(*foreign_project),
        _ => panic!("expected a row"),
    }
    match &slots[0] {
        Slot::Row {
            foreign_project, ..
        } => assert!(!*foreign_project),
        _ => panic!("expected a row"),
    }
}

#[test]
fn child_is_omitted_in_its_own_group_when_parent_is_shown_elsewhere() {
    let a = g("a");
    let b = g("b");
    let mut h = Hierarchy::default();
    h.set_parent(&term("far"), Some(&term("boss"))).unwrap();
    let rows = [info(&term("far"), &b, 5), info(&term("other"), &b, 1)];
    let elsewhere: BTreeSet<RowKey> = [term("boss")].into();
    let slots = arrange_rows(&ArrangeInput {
        group: &b,
        rows: &rows,
        foreign: &[],
        elsewhere: &elsewhere,
        hierarchy: &h,
        query_mode: false,
    });
    assert_eq!(shape(&slots), ["other:0"]);
    let _ = a;
}

#[test]
fn child_is_never_listed_in_a_section() {
    let group = g("a");
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&group, "S").unwrap();
    // Corrupt: a parented row also listed in a section. arrange shows it once, under its parent.
    h.parents.insert(term("kid"), term("p"));
    h.sections.get_mut(&group).unwrap()[0].items = vec![term("kid"), term("p")];
    let _ = s;
    let rows = [info(&term("p"), &group, 1), info(&term("kid"), &group, 2)];
    let slots = arrange(&group, &rows, &[], &h);
    assert_eq!(shape(&slots), ["[S]", "p:0", "kid:1"]);
}

#[test]
fn d4_missing_parent_lifts_children_one_level_and_they_return() {
    let group = g("a");
    let mut h = Hierarchy::default();
    h.set_parent(&term("mid"), Some(&term("top"))).unwrap();
    h.set_parent(&term("leaf"), Some(&term("mid"))).unwrap();
    h.set_parent(&term("leaf2"), Some(&term("leaf"))).unwrap();
    let all = [
        info(&term("top"), &group, 10),
        info(&term("mid"), &group, 9),
        info(&term("leaf"), &group, 8),
        info(&term("leaf2"), &group, 7),
    ];
    assert_eq!(
        shape(&arrange(&group, &all, &[], &h)),
        ["top:0", "mid:1", "leaf:2", "leaf2:3"]
    );
    // `mid` closes: leaf (and its subtree) is lifted to `mid`'s parent.
    let without_mid: Vec<_> = all
        .iter()
        .filter(|r| r.key != term("mid"))
        .cloned()
        .collect();
    assert_eq!(
        shape(&arrange(&group, &without_mid, &[], &h)),
        ["top:0", "leaf:1", "leaf2:2"]
    );
    // `top` closes too: climb to the nearest present ancestor, else the top level.
    let only_leaves: Vec<_> = all[2..].to_vec();
    assert_eq!(
        shape(&arrange(&group, &only_leaves, &[], &h)),
        ["leaf:0", "leaf2:1"]
    );
    // Re-adding restores the nesting.
    assert_eq!(
        shape(&arrange(&group, &all, &[], &h)),
        ["top:0", "mid:1", "leaf:2", "leaf2:3"]
    );
}

#[test]
fn collapsed_subtrees_and_sections_are_omitted_but_rolled_up() {
    let group = g("a");
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&group, "S").unwrap();
    h.set_section(&term("p"), &group, Some(&s)).unwrap();
    h.set_parent(&term("c1"), Some(&term("p"))).unwrap();
    h.set_parent(&term("c2"), Some(&term("c1"))).unwrap();
    h.set_row_collapsed(&term("p"), true);
    let mut rows = vec![
        info(&term("p"), &group, 1),
        info(&term("c1"), &group, 2),
        info(&term("c2"), &group, 3),
    ];
    rows[1].running = true;
    rows[2].waiting = true;
    rows[2].notified = true;
    let slots = arrange(&group, &rows, &[], &h);
    assert_eq!(shape(&slots), ["[S]", "p:0"]);
    match &slots[1] {
        Slot::Row {
            has_children,
            collapsed,
            rollup,
            ..
        } => {
            assert!(*has_children && *collapsed);
            assert_eq!(
                *rollup,
                Rollup {
                    running: 1,
                    waiting: 1,
                    notified: true,
                    count: 2
                }
            );
        }
        _ => panic!(),
    }
    // Collapse the section too: only the header remains, with everything rolled up.
    h.set_section_collapsed(&group, &s, true).unwrap();
    let slots = arrange(&group, &rows, &[], &h);
    assert_eq!(shape(&slots), ["[S]"]);
    match &slots[0] {
        Slot::SectionHeader {
            rollup, collapsed, ..
        } => {
            assert!(*collapsed);
            assert_eq!(rollup.count, 3);
            assert_eq!(rollup.running, 1);
            assert!(rollup.notified);
        }
        _ => panic!(),
    }
}

#[test]
fn empty_sections_still_render_a_header() {
    let group = g("a");
    let mut h = Hierarchy::default();
    h.create_section(&group, "Empty").unwrap();
    let slots = arrange(&group, &[info(&term("x"), &group, 1)], &[], &h);
    assert_eq!(shape(&slots), ["x:0", "[Empty]"]);
}

#[test]
fn deep_chain_of_ten_thousand_arranges_fast_without_overflow() {
    let group = g("a");
    let mut h = Hierarchy::default();
    let n = 10_000;
    let mut rows = Vec::new();
    for i in 0..n {
        rows.push(info(&term(&format!("n{i}")), &group, i as i64));
        if i > 0 {
            h.parents
                .insert(term(&format!("n{i}")), term(&format!("n{}", i - 1)));
        }
    }
    let start = Instant::now();
    let slots = arrange(&group, &rows, &[], &h);
    let elapsed = start.elapsed();
    assert_eq!(slots.len(), n);
    match slots.last() {
        Some(Slot::Row { depth, .. }) => assert_eq!(*depth, n - 1),
        _ => panic!(),
    }
    assert!(elapsed.as_millis() < 100, "took {elapsed:?}");
}

#[test]
fn corrupt_parent_cycle_never_hides_rows_or_panics() {
    let group = g("a");
    let mut h = Hierarchy::default();
    h.parents.insert(term("a"), term("b"));
    h.parents.insert(term("b"), term("a"));
    h.parents.insert(term("self"), term("self"));
    let rows = [
        info(&term("a"), &group, 2),
        info(&term("b"), &group, 1),
        info(&term("self"), &group, 3),
    ];
    let slots = arrange(&group, &rows, &[], &h);
    assert_eq!(slots.len(), 3);
}

/// Property-style: random small (possibly corrupt) hierarchies. Seeded and deterministic.
#[test]
fn random_corrupt_hierarchies_never_panic_and_list_each_row_at_most_once() {
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let groups = [g("a"), g("b")];
    for _ in 0..300 {
        let count = 1 + rng.below(9) as usize;
        let keys: Vec<RowKey> = (0..count)
            .map(|i| {
                if i % 2 == 0 {
                    term(&format!("r{i}"))
                } else {
                    thr(&format!("r{i}"))
                }
            })
            .collect();
        let mut h = Hierarchy::default();
        for k in &keys {
            if rng.below(2) == 0 {
                let p = &keys[rng.below(count as u64) as usize];
                h.parents.insert(k.clone(), p.clone());
            }
            if rng.below(3) == 0 {
                let list = h
                    .child_order
                    .entry(keys[rng.below(count as u64) as usize].clone())
                    .or_default();
                list.push(k.clone());
                list.push(k.clone());
            }
            if rng.below(4) == 0 {
                h.set_row_collapsed(k, true);
            }
        }
        for gr in &groups {
            let (id, _) = h.create_section(gr, "S").unwrap();
            for k in &keys {
                if rng.below(3) == 0 {
                    h.sections.get_mut(gr).unwrap()[0].items.push(k.clone());
                }
            }
            let _ = id;
            let mut listed = Vec::new();
            for k in &keys {
                if rng.below(2) == 0 {
                    listed.push(k.clone());
                }
            }
            h.unsectioned_order.insert(gr.clone(), listed);
        }
        let mut rows: Vec<RowInfo> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            if rng.below(5) == 0 {
                continue;
            }
            let group = groups[rng.below(2) as usize].clone();
            rows.push(info(k, &group, (i as i64) * (rng.below(3) as i64)));
        }
        let (mine, foreign): (Vec<_>, Vec<_>) =
            rows.iter().cloned().partition(|r| r.group == groups[0]);
        let slots = arrange(&groups[0], &mine, &foreign, &h);
        let mut seen = BTreeSet::new();
        for slot in &slots {
            if let Slot::Row { key, .. } = slot {
                assert!(seen.insert(key.clone()), "row listed twice: {key:?}");
            }
        }
        // Every in-group row is listed, unless it is under a collapsed ancestor (hidden by
        // design); with nothing collapsed none may be missing.
        if h.collapsed_rows.is_empty() {
            let sections_collapsed = h.sections.values().flatten().any(|s| s.collapsed);
            if !sections_collapsed {
                for r in &mine {
                    assert!(seen.contains(&r.key), "row hidden: {:?}", r.key);
                }
            }
        }
    }
}
