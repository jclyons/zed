use super::*;
use crate::{Container, RETENTION_SECS, Refusal, Side};

#[test]
fn section_names_are_unique_per_group_case_insensitive_and_create_is_idempotent() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    let (id1, created) = h.create_section(&a, "Glider").unwrap();
    assert!(created);
    let (id2, created) = h.create_section(&a, "  gLiDeR ").unwrap();
    assert!(!created);
    assert_eq!(id1, id2);
    // Same name in another group is a different section with a different id.
    let (id3, created) = h.create_section(&b, "Glider").unwrap();
    assert!(created);
    assert_ne!(id1, id3);
    assert_eq!(h.create_section(&a, "  "), Err(Refusal::EmptyName));

    let (other, _) = h.create_section(&a, "Other").unwrap();
    assert_eq!(
        h.rename_section(&a, &other, "GLIDER"),
        Err(Refusal::DuplicateName)
    );
    h.rename_section(&a, &id1, "GLIDER").unwrap(); // own name, new case
    h.rename_section(&a, &other, "Renamed").unwrap();
    assert_eq!(h.section(&a, &other).unwrap().name, "Renamed");
}

#[test]
fn delete_section_makes_items_unsectioned_and_touches_no_thread() {
    let a = g("a");
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&a, "S").unwrap();
    h.set_section(&term("x"), &a, Some(&s)).unwrap();
    h.set_section(&term("y"), &a, Some(&s)).unwrap(); // [y, x]
    h.set_parent(&term("kid"), Some(&term("x"))).unwrap();
    h.set_section(&term("free"), &a, None).unwrap();
    let freed = h.delete_section(&a, &s).unwrap();
    assert_eq!(freed, vec![term("y"), term("x")]);
    assert!(h.section(&a, &s).is_none());
    assert_eq!(
        h.unsectioned_order[&a],
        vec![term("free"), term("y"), term("x")]
    );
    assert_eq!(h.parents[&term("kid")], term("x")); // links untouched
    assert_eq!(h.delete_section(&a, &s), Err(Refusal::UnknownSection));
}

#[test]
fn sections_reorder_within_a_project_and_never_across() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    let (s1, _) = h.create_section(&a, "1").unwrap();
    let (s2, _) = h.create_section(&a, "2").unwrap();
    let (s3, _) = h.create_section(&a, "3").unwrap();
    let (sb, _) = h.create_section(&b, "B").unwrap();
    h.move_section(&a, &s3, &a, &s1, Side::Before).unwrap();
    let names = |h: &Hierarchy| {
        h.sections[&a]
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&h), ["3", "1", "2"]);
    h.move_section(&a, &s3, &a, &s2, Side::After).unwrap();
    assert_eq!(names(&h), ["1", "2", "3"]);
    assert_eq!(
        h.move_section(&a, &s1, &b, &sb, Side::After),
        Err(Refusal::SectionAcrossProjects)
    );
    assert_eq!(
        h.move_section(&a, &s1, &a, &s1, Side::After),
        Err(Refusal::SectionIntoItself)
    );
    assert_eq!(names(&h), ["1", "2", "3"]);
}

#[test]
fn set_section_un_nests_and_enforces_the_group() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    let (sa, _) = h.create_section(&a, "A").unwrap();
    h.set_parent(&term("x"), Some(&term("p"))).unwrap();
    h.set_section(&term("x"), &a, Some(&sa)).unwrap();
    assert!(!h.parents.contains_key(&term("x")));
    assert!(!h.child_order.contains_key(&term("p")));
    assert_eq!(h.sections[&a][0].items, vec![term("x")]);
    // A section of project a cannot take a row of project b.
    assert_eq!(
        h.set_section(&term("y"), &b, Some(&sa)),
        Err(Refusal::SectionNotInGroup)
    );
    assert_eq!(
        h.set_section(&term("y"), &a, Some(&crate::SectionId("nope".into()))),
        Err(Refusal::UnknownSection)
    );
    assert!(!h.sections[&a][0].items.contains(&term("y")));
    // None: unsectioned, at the top.
    h.set_section(&term("x"), &a, None).unwrap();
    assert!(h.sections[&a][0].items.is_empty());
    assert_eq!(h.unsectioned_order[&a], vec![term("x")]);
}

#[test]
fn set_parent_refuses_cycles_directly_and_at_depth_1000() {
    let mut h = Hierarchy::default();
    assert_eq!(
        h.set_parent(&term("a"), Some(&term("a"))),
        Err(Refusal::Cycle)
    );
    h.set_parent(&term("b"), Some(&term("a"))).unwrap();
    assert_eq!(
        h.set_parent(&term("a"), Some(&term("b"))),
        Err(Refusal::Cycle)
    );

    for i in 1..=1000 {
        h.set_parent(&term(&format!("d{i}")), Some(&term(&format!("d{}", i - 1))))
            .unwrap();
    }
    // Dropping d0 onto its 1000-deep descendant is refused; no state changed.
    let before = h.clone();
    assert_eq!(
        h.set_parent(&term("d0"), Some(&term("d1000"))),
        Err(Refusal::Cycle)
    );
    assert_eq!(h, before);
    // No depth limit: a 1001st level is fine.
    h.set_parent(&term("deeper"), Some(&term("d1000"))).unwrap();
}

#[test]
fn set_parent_removes_the_row_from_its_section_and_appends_last() {
    let a = g("a");
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&a, "S").unwrap();
    h.set_section(&term("x"), &a, Some(&s)).unwrap();
    h.set_parent(&term("first"), Some(&term("p"))).unwrap();
    h.set_parent(&term("x"), Some(&term("p"))).unwrap();
    assert!(h.sections[&a][0].items.is_empty());
    assert_eq!(h.child_order[&term("p")], vec![term("first"), term("x")]);
    h.set_parent(&term("x"), None).unwrap();
    assert!(!h.parents.contains_key(&term("x")));
    assert_eq!(h.child_order[&term("p")], vec![term("first")]);
}

#[test]
fn move_row_materializes_the_visible_order() {
    let a = g("a");
    let mut h = Hierarchy::default();
    let visible = vec![term("1"), term("2"), term("3")];
    let c = Container::Unsectioned(a.clone());
    h.move_row(&term("3"), &term("1"), Side::Before, &c, &visible)
        .unwrap();
    assert_eq!(
        h.unsectioned_order[&a],
        vec![term("3"), term("1"), term("2")]
    );
    // Now the visible order is the stored one; move 3 after 2.
    let visible = h.unsectioned_order[&a].clone();
    h.move_row(&term("3"), &term("2"), Side::After, &c, &visible)
        .unwrap();
    assert_eq!(
        h.unsectioned_order[&a],
        vec![term("1"), term("2"), term("3")]
    );
    assert_eq!(
        h.move_row(&term("3"), &term("zzz"), Side::After, &c, &visible),
        Err(Refusal::TargetNotFound)
    );
    assert_eq!(
        h.move_row(&term("3"), &term("3"), Side::After, &c, &visible),
        Err(Refusal::SameTarget)
    );
}

#[test]
fn move_row_into_children_and_section_containers() {
    let a = g("a");
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&a, "S").unwrap();
    h.set_section(&term("in-s"), &a, Some(&s)).unwrap();
    // Move an unsectioned row before `in-s`: it joins the section.
    h.move_row(
        &term("x"),
        &term("in-s"),
        Side::Before,
        &Container::Section(a.clone(), s.clone()),
        &[term("in-s")],
    )
    .unwrap();
    assert_eq!(h.sections[&a][0].items, vec![term("x"), term("in-s")]);
    // Move it under a parent next to a sibling: re-parented, and removed from the section.
    h.set_parent(&term("sib"), Some(&term("p"))).unwrap();
    h.move_row(
        &term("x"),
        &term("sib"),
        Side::After,
        &Container::Children(term("p")),
        &[term("sib")],
    )
    .unwrap();
    assert_eq!(h.parents[&term("x")], term("p"));
    assert_eq!(h.child_order[&term("p")], vec![term("sib"), term("x")]);
    assert_eq!(h.sections[&a][0].items, vec![term("in-s")]);
    // Into its own descendant's children list is a cycle.
    assert_eq!(
        h.move_row(
            &term("p"),
            &term("x"),
            Side::Before,
            &Container::Children(term("x")),
            &[term("x")]
        ),
        Err(Refusal::Cycle)
    );
}

#[test]
fn cross_project_move_drops_the_section_and_keeps_children_links() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&a, "S").unwrap();
    h.set_section(&term("t"), &a, Some(&s)).unwrap();
    h.set_parent(&term("kid"), Some(&term("t"))).unwrap();
    h.cross_project_move(&term("t"), &a, &b);
    assert!(h.sections[&a][0].items.is_empty());
    assert_eq!(h.unsectioned_order[&b], vec![term("t")]);
    assert_eq!(h.parents[&term("kid")], term("t"));
    // A moved child keeps its own parent link and is not made a root.
    h.cross_project_move(&term("kid"), &a, &b);
    assert_eq!(h.parents[&term("kid")], term("t"));
    assert_eq!(h.unsectioned_order[&b], vec![term("t")]);
}

#[test]
fn retention_keeps_empty_sections_and_prunes_stale_rows_links_and_groups() {
    let (a, b) = (g("a"), g("b"));
    let day = 86_400;
    let t0 = 1_000_000_000;
    let mut h = Hierarchy::default();
    let (sa, _) = h.create_section(&a, "Keep").unwrap(); // stays empty
    let (sb, _) = h.create_section(&b, "Stale").unwrap();
    h.set_section(&term("old"), &b, Some(&sb)).unwrap();
    h.set_parent(&term("old-kid"), Some(&term("old"))).unwrap();
    h.set_parent(&term("fresh-kid"), Some(&term("old")))
        .unwrap();
    h.set_row_collapsed(&term("old"), true);
    h.set_section(&term("fresh"), &a, Some(&sa)).unwrap();
    h.touch_seen(
        &[term("old"), term("old-kid")],
        std::slice::from_ref(&b),
        t0,
    );
    h.touch_seen(
        &[term("fresh"), term("fresh-kid")],
        std::slice::from_ref(&a),
        t0 + 31 * day,
    );

    // Right after seeing nothing stale: nothing pruned, empty section persists.
    let now = t0 + 29 * day;
    assert_eq!(h.clone().prune(now), 0);
    let mut empty = Hierarchy::default();
    let (e, _) = empty.create_section(&a, "Empty").unwrap();
    empty.touch_seen(&[], std::slice::from_ref(&a), t0);
    empty.prune(t0 + 29 * day);
    assert!(empty.section(&a, &e).is_some());

    // 31 days after t0: `old`, `old-kid` and group b are stale.
    let now = t0 + 31 * day + 1;
    let pruned = h.prune(now);
    assert_eq!(pruned, 3); // old, old-kid, group b
    assert!(!h.sections.contains_key(&b));
    assert!(!h.parents.contains_key(&term("old-kid")));
    assert!(!h.collapsed_rows.contains(&term("old")));
    assert!(!h.last_seen.contains_key(&term("old")));
    // The fresh child of the stale parent is lifted (no grandparent: becomes a root).
    assert!(!h.parents.contains_key(&term("fresh-kid")));
    // Fresh rows and the fresh group's (otherwise empty-able) section survive.
    assert!(h.section(&a, &sa).is_some());
    assert_eq!(h.sections[&a][0].items, vec![term("fresh")]);
    assert!(h.last_seen.contains_key(&term("fresh")));
    let _ = RETENTION_SECS;
}

#[test]
fn prune_lifts_children_to_the_nearest_surviving_ancestor() {
    let day = 86_400;
    let mut h = Hierarchy::default();
    h.set_parent(&term("mid"), Some(&term("top"))).unwrap();
    h.set_parent(&term("leaf"), Some(&term("mid"))).unwrap();
    h.touch_seen(&[term("top"), term("leaf")], &[], 100 * day);
    h.touch_seen(&[term("mid")], &[], 1);
    h.prune(100 * day);
    assert_eq!(h.parents.get(&term("leaf")), Some(&term("top")));
    assert!(!h.parents.contains_key(&term("mid")));
    assert_eq!(h.child_order[&term("top")], vec![term("leaf")]);
}

#[test]
fn serde_round_trip_with_version_2() {
    let (a, b) = (g("a"), RowKey::Terminal("x".into()));
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&a, "S").unwrap();
    h.set_section(&b, &a, Some(&s)).unwrap();
    h.set_parent(&thr("c"), Some(&term("p"))).unwrap();
    h.set_row_collapsed(&term("p"), true);
    h.touch_seen(std::slice::from_ref(&b), std::slice::from_ref(&a), 42);
    let json = h.to_json().unwrap();
    assert!(json.contains("\"version\":2"));
    assert_eq!(Hierarchy::from_json(&json), h);
}

#[test]
fn v1_blobs_and_garbage_load_as_empty() {
    assert_eq!(
        Hierarchy::from_json(r#"{"version":1,"roots":[],"parents":{}}"#),
        Hierarchy::default()
    );
    assert_eq!(Hierarchy::from_json("not json {"), Hierarchy::default());
    assert_eq!(Hierarchy::from_json(""), Hierarchy::default());
    assert_eq!(
        Hierarchy::from_json(r#"{"version":2,"sections":"bad"}"#),
        Hierarchy::default()
    );
    assert_eq!(
        Hierarchy::from_json(r#"{"version":99}"#),
        Hierarchy::default()
    );
    // Missing optional maps default.
    assert_eq!(
        Hierarchy::from_json(r#"{"version":2}"#),
        Hierarchy::default()
    );
}
