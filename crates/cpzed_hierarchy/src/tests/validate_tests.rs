use super::*;
use crate::{Repair, Section, SectionId};

fn section(id: &str, items: Vec<RowKey>) -> Section {
    Section {
        id: SectionId(id.into()),
        name: id.into(),
        collapsed: false,
        items,
    }
}

#[test]
fn clean_hierarchy_has_nothing_to_repair() {
    let a = g("a");
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&a, "S").unwrap();
    h.set_section(&term("x"), &a, Some(&s)).unwrap();
    h.set_parent(&term("kid"), Some(&term("x"))).unwrap();
    let before = h.clone();
    assert!(h.validate(&|_| Some(a.clone())).is_empty());
    assert_eq!(h, before);
}

#[test]
fn breaks_a_cycle_at_the_first_node_seen_twice() {
    let mut h = Hierarchy::default();
    // a -> b -> c -> a, plus a tail d -> a.
    h.parents.insert(term("a"), term("b"));
    h.parents.insert(term("b"), term("c"));
    h.parents.insert(term("c"), term("a"));
    h.parents.insert(term("d"), term("a"));
    h.child_order.insert(term("b"), vec![term("a")]);
    let repairs = h.validate(&|_| None);
    assert_eq!(repairs, vec![Repair::CycleBroken { node: term("a") }]);
    assert!(!h.parents.contains_key(&term("a")));
    assert_eq!(h.parents[&term("b")], term("c"));
    assert_eq!(h.parents[&term("d")], term("a"));
    assert!(h.child_order.is_empty());
    // Self-link.
    let mut h = Hierarchy::default();
    h.parents.insert(term("s"), term("s"));
    assert_eq!(
        h.validate(&|_| None),
        vec![Repair::CycleBroken { node: term("s") }]
    );
    assert!(h.parents.is_empty());
}

#[test]
fn keeps_the_first_of_two_sections() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    h.sections.insert(
        a.clone(),
        vec![
            section("s1", vec![term("x")]),
            section("s2", vec![term("x"), term("y")]),
        ],
    );
    h.sections
        .insert(b.clone(), vec![section("s3", vec![term("x")])]);
    let repairs = h.validate(&|_| None);
    assert_eq!(repairs.len(), 2);
    assert!(
        repairs
            .iter()
            .all(|r| matches!(r, Repair::DuplicateMembership { row } if row == &term("x")))
    );
    assert_eq!(h.sections[&a][0].items, vec![term("x")]);
    assert_eq!(h.sections[&a][1].items, vec![term("y")]);
    assert!(h.sections[&b][0].items.is_empty());
}

#[test]
fn removes_a_parented_row_from_its_section() {
    let a = g("a");
    let mut h = Hierarchy::default();
    h.sections
        .insert(a.clone(), vec![section("s1", vec![term("x"), term("y")])]);
    h.parents.insert(term("x"), term("y"));
    h.unsectioned_order.insert(a.clone(), vec![term("x")]);
    let repairs = h.validate(&|_| None);
    assert_eq!(
        repairs,
        vec![
            Repair::ParentedInContainer { row: term("x") },
            Repair::ParentedInContainer { row: term("x") }
        ]
    );
    assert_eq!(h.sections[&a][0].items, vec![term("y")]);
    assert!(h.unsectioned_order.is_empty());
}

#[test]
fn drops_membership_when_the_section_is_not_in_the_rows_current_group() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    h.sections.insert(
        a.clone(),
        vec![section(
            "s1",
            vec![term("moved"), term("stay"), term("unknown")],
        )],
    );
    let group_of = |k: &RowKey| match k {
        RowKey::Terminal(s) if s == "moved" => Some(b.clone()),
        RowKey::Terminal(s) if s == "stay" => Some(a.clone()),
        _ => None, // unknown: kept
    };
    let repairs = h.validate(&group_of);
    assert_eq!(
        repairs,
        vec![Repair::SectionInWrongGroup {
            row: term("moved"),
            section: SectionId("s1".into())
        }]
    );
    assert_eq!(h.sections[&a][0].items, vec![term("stay"), term("unknown")]);
}

#[test]
fn repairs_duplicate_section_ids_and_names() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    let mut dup = section("s1", vec![]);
    dup.name = "same".into();
    let mut dup2 = section("s1", vec![]); // same id in another project
    dup2.name = "SAME".into(); // same name as `dup` (case-insensitive) is only a clash within a project
    h.sections.insert(
        a.clone(),
        vec![dup, {
            let mut other = section("s2", vec![]);
            other.name = "Same".into();
            other
        }],
    );
    h.sections.insert(b, vec![dup2]);
    let repairs = h.validate(&|_| None);
    assert!(
        repairs
            .iter()
            .any(|r| matches!(r, Repair::DuplicateSectionId { .. }))
    );
    assert!(repairs.iter().any(
        |r| matches!(r, Repair::DuplicateSectionName { renamed, .. } if renamed == "Same (2)")
    ));
    let ids: Vec<_> = h
        .sections
        .values()
        .flatten()
        .map(|s| s.id.clone())
        .collect();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len());
    let names: Vec<_> = h.sections[&a]
        .iter()
        .map(|s| s.name.to_lowercase())
        .collect();
    assert_eq!(names, ["same", "same (2)"]);
    // Idempotent.
    assert!(h.validate(&|_| None).is_empty());
}
