use super::*;
use crate::{
    Container, DragItem, DropAction, DropCtx, DropTarget, Refusal, SectionId, Side, Zone, plan_drop,
};

fn plan(
    item: &DragItem,
    target: &DropTarget,
    zone: Zone,
    h: &Hierarchy,
) -> Result<DropAction, Refusal> {
    let visible = |c: &Container| -> Vec<RowKey> {
        match c {
            Container::Children(p) => h.child_order.get(p).cloned().unwrap_or_default(),
            Container::Section(gr, id) => h
                .section(gr, id)
                .map(|s| s.items.clone())
                .unwrap_or_default(),
            Container::Unsectioned(gr) => h.unsectioned_order.get(gr).cloned().unwrap_or_default(),
        }
    };
    let exists = |k: &RowKey| !matches!(k, RowKey::Terminal(s) | RowKey::Thread(s) if s == "ghost");
    plan_drop(
        item,
        target,
        zone,
        h,
        &DropCtx {
            visible: &visible,
            row_exists: &exists,
        },
    )
}

fn row(key: &RowKey, group: &GroupKey) -> DragItem {
    DragItem::Row {
        key: key.clone(),
        group: group.clone(),
    }
}
fn on_row(key: &RowKey, group: &GroupKey) -> DropTarget {
    DropTarget::Row {
        key: key.clone(),
        group: group.clone(),
    }
}

#[test]
fn middle_nests_and_edges_reorder_in_the_targets_container() {
    let a = g("a");
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&a, "S").unwrap();
    h.set_section(&term("t"), &a, Some(&s)).unwrap();
    h.set_section(&term("u"), &a, Some(&s)).unwrap();
    assert_eq!(
        plan(
            &row(&term("x"), &a),
            &on_row(&term("t"), &a),
            Zone::Into,
            &h
        ),
        Ok(DropAction::Nest {
            child: term("x"),
            parent: term("t")
        })
    );
    assert_eq!(
        plan(
            &row(&term("x"), &a),
            &on_row(&term("t"), &a),
            Zone::After,
            &h
        ),
        Ok(DropAction::Reorder {
            container: Container::Section(a.clone(), s.clone()),
            key: term("x"),
            target: term("t"),
            side: Side::After,
            visible: vec![term("u"), term("t")],
        })
    );
    // Before a child: the child's container is its parent's children.
    h.set_parent(&term("kid"), Some(&term("t"))).unwrap();
    match plan(
        &row(&thr("x"), &a),
        &on_row(&term("kid"), &a),
        Zone::Before,
        &h,
    ) {
        Ok(DropAction::Reorder {
            container, side, ..
        }) => {
            assert_eq!(container, Container::Children(term("t")));
            assert_eq!(side, Side::Before);
        }
        other => panic!("{other:?}"),
    }
    // Before an unsectioned root.
    h.set_section(&term("free"), &a, None).unwrap();
    match plan(
        &row(&term("x"), &a),
        &on_row(&term("free"), &a),
        Zone::Before,
        &h,
    ) {
        Ok(DropAction::Reorder { container, .. }) => {
            assert_eq!(container, Container::Unsectioned(a.clone()))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn cycles_are_refused_for_nest_and_for_reorder_inside_a_descendants_children() {
    let a = g("a");
    let mut h = Hierarchy::default();
    for i in 1..=1000 {
        h.set_parent(&term(&format!("d{i}")), Some(&term(&format!("d{}", i - 1))))
            .unwrap();
    }
    assert_eq!(
        plan(
            &row(&term("d0"), &a),
            &on_row(&term("d1000"), &a),
            Zone::Into,
            &h
        ),
        Err(Refusal::Cycle)
    );
    assert_eq!(
        plan(
            &row(&term("d0"), &a),
            &on_row(&term("d0"), &a),
            Zone::Into,
            &h
        ),
        Err(Refusal::SameTarget)
    );
    // Reorder before d1000: container is Children(d999), a descendant of d0.
    assert_eq!(
        plan(
            &row(&term("d0"), &a),
            &on_row(&term("d1000"), &a),
            Zone::Before,
            &h
        ),
        Err(Refusal::Cycle)
    );
    // Unrelated drop is fine at depth.
    assert!(
        plan(
            &row(&term("free"), &a),
            &on_row(&term("d1000"), &a),
            Zone::Into,
            &h
        )
        .is_ok()
    );
}

#[test]
fn headers_of_the_same_project() {
    let a = g("a");
    let mut h = Hierarchy::default();
    let (s, _) = h.create_section(&a, "S").unwrap();
    assert_eq!(
        plan(
            &row(&term("x"), &a),
            &DropTarget::SectionHeader {
                id: s.clone(),
                group: a.clone()
            },
            Zone::Into,
            &h
        ),
        Ok(DropAction::IntoSection {
            key: term("x"),
            group: a.clone(),
            section: s
        })
    );
    assert_eq!(
        plan(
            &row(&term("x"), &a),
            &DropTarget::ProjectHeader { group: a.clone() },
            Zone::Into,
            &h
        ),
        Ok(DropAction::ToUnsectioned {
            key: term("x"),
            group: a.clone()
        })
    );
    h.set_parent(&term("x"), Some(&term("p"))).unwrap();
    assert_eq!(
        plan(
            &row(&term("x"), &a),
            &DropTarget::ProjectHeader { group: a.clone() },
            Zone::Into,
            &h
        ),
        Ok(DropAction::Unnest {
            key: term("x"),
            group: a.clone()
        })
    );
    assert_eq!(
        plan(
            &row(&term("x"), &a),
            &DropTarget::SectionHeader {
                id: SectionId("gone".into()),
                group: a.clone()
            },
            Zone::Into,
            &h
        ),
        Err(Refusal::UnknownSection)
    );
}

#[test]
fn terminals_move_across_projects_and_agent_threads_are_refused() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    let (sb, _) = h.create_section(&b, "B").unwrap();
    let hdr = DropTarget::ProjectHeader { group: b.clone() };
    let sec = DropTarget::SectionHeader {
        id: sb.clone(),
        group: b.clone(),
    };
    let target_row = on_row(&term("elsewhere"), &b);
    for (target, section) in [(&hdr, None), (&sec, Some(sb)), (&target_row, None)] {
        assert_eq!(
            plan(&row(&term("t"), &a), target, Zone::After, &h),
            Ok(DropAction::MoveTerminalToGroup {
                key: term("t"),
                from: a.clone(),
                to: b.clone(),
                section
            })
        );
        assert_eq!(
            plan(&row(&thr("agent"), &a), target, Zone::After, &h),
            Err(Refusal::AgentThreadAcrossProjects)
        );
    }
    // Unknown section in the other project is refused.
    assert_eq!(
        plan(
            &row(&term("t"), &a),
            &DropTarget::SectionHeader {
                id: SectionId("no".into()),
                group: b.clone()
            },
            Zone::Into,
            &h
        ),
        Err(Refusal::UnknownSection)
    );
}

#[test]
fn section_drags_reorder_within_the_project_only() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    let (s1, _) = h.create_section(&a, "1").unwrap();
    let (s2, _) = h.create_section(&a, "2").unwrap();
    let (sb, _) = h.create_section(&b, "B").unwrap();
    let drag = DragItem::Section {
        id: s1.clone(),
        group: a.clone(),
    };
    let hdr = |id: &SectionId, gr: &GroupKey| DropTarget::SectionHeader {
        id: id.clone(),
        group: gr.clone(),
    };
    assert_eq!(
        plan(&drag, &hdr(&s2, &a), Zone::Before, &h),
        Ok(DropAction::ReorderSection {
            group: a.clone(),
            id: s1.clone(),
            target: s2.clone(),
            side: Side::Before
        })
    );
    assert_eq!(
        plan(&drag, &hdr(&s2, &a), Zone::Into, &h),
        Ok(DropAction::ReorderSection {
            group: a.clone(),
            id: s1.clone(),
            target: s2.clone(),
            side: Side::After
        })
    );
    assert_eq!(
        plan(&drag, &hdr(&s1, &a), Zone::After, &h),
        Err(Refusal::SectionIntoItself)
    );
    assert_eq!(
        plan(&drag, &hdr(&sb, &b), Zone::After, &h),
        Err(Refusal::SectionAcrossProjects)
    );
    assert_eq!(
        plan(
            &drag,
            &DropTarget::ProjectHeader { group: b.clone() },
            Zone::Into,
            &h
        ),
        Err(Refusal::SectionAcrossProjects)
    );
    assert_eq!(
        plan(
            &drag,
            &DropTarget::ProjectHeader { group: a.clone() },
            Zone::Into,
            &h
        ),
        Err(Refusal::InvalidTarget)
    );
    assert_eq!(
        plan(&drag, &on_row(&term("r"), &a), Zone::Into, &h),
        Err(Refusal::InvalidTarget)
    );
}

#[test]
fn into_a_row_of_another_project_nests_for_terminals_and_agents() {
    let (a, b) = (g("a"), g("b"));
    let mut h = Hierarchy::default();
    for key in [term("t"), thr("agent")] {
        assert_eq!(
            plan(&row(&key, &a), &on_row(&term("far"), &b), Zone::Into, &h),
            Ok(DropAction::Nest {
                child: key.clone(),
                parent: term("far")
            })
        );
    }
    // Cycle check still applies across projects.
    h.set_parent(&term("far"), Some(&term("t"))).unwrap();
    assert_eq!(
        plan(
            &row(&term("t"), &a),
            &on_row(&term("far"), &b),
            Zone::Into,
            &h
        ),
        Err(Refusal::Cycle)
    );
}

#[test]
fn a_target_row_that_no_longer_exists_is_refused() {
    let a = g("a");
    let h = Hierarchy::default();
    for zone in [Zone::Into, Zone::Before, Zone::After] {
        assert_eq!(
            plan(&row(&term("x"), &a), &on_row(&term("ghost"), &a), zone, &h),
            Err(Refusal::TargetNotFound)
        );
    }
}
