//! Crafted remote changes that used to fail an assertion on import (and
//! abort the process when the document was dropped during the unwind) are
//! import errors before anything is mutated.

use loro::{
    json::{JsonOp, JsonOpContent, ListOp},
    CommitOptions, ContainerID, ContainerType, ExportMode, JsonChange, JsonSchema, LoroDoc,
    VersionVector, ID,
};

/// Alice's three changes (counters 0, 1, 2), one map key each, and a
/// change of Bob that depends on her second (so her node at counter 1 has
/// a successor when it arrives first).
fn alice_three_and_bob() -> JsonSchema {
    let alice = LoroDoc::new();
    alice.set_peer_id(1).unwrap();
    for k in ["a", "b"] {
        alice.get_map("m").insert(k, k).unwrap();
        alice.commit_with(CommitOptions::new().commit_msg(k));
    }
    let bob = LoroDoc::new();
    bob.set_peer_id(2).unwrap();
    bob.import(&alice.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    bob.get_map("m").insert("bob", 1).unwrap();
    bob.commit();
    alice
        .import(&bob.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    alice.get_map("m").insert("c", "c").unwrap();
    alice.commit_with(CommitOptions::new().commit_msg("c"));
    alice.export_json_updates_without_peer_compression(&VersionVector::default(), &alice.oplog_vv())
}

/// Imports `json` into a fresh document, detached or attached, and returns
/// whether the import failed. A failed import leaves the document empty
/// and usable.
fn import_fails(json: JsonSchema, detached: bool) -> bool {
    let y = LoroDoc::new();
    if detached {
        y.detach();
    }
    let failed = y.import_json_updates(json).is_err();
    if failed {
        assert!(y.oplog_vv().is_empty());
    }
    y.checkout_to_latest();
    let _ = y.get_deep_value();
    y.get_map("x").insert("still", "usable").unwrap();
    y.commit();
    let _ = y.export(ExportMode::Snapshot).unwrap();
    failed
}

/// A change whose only dep is an older op of its own peer, `(1, 0)` for
/// the change at counter 2, used to be merged into the peer's last DAG
/// node and fail "lamport is not continuous". It is an import error,
/// whether or not the last node has a successor, detached or attached.
#[test]
fn a_dep_on_an_older_own_op_is_an_import_error() {
    let honest = alice_three_and_bob();
    let c = honest
        .changes
        .iter()
        .position(|c| c.id == ID::new(1, 2))
        .unwrap();
    for with_successor in [false, true] {
        let mut json = honest.clone();
        json.changes[c].deps = vec![ID::new(1, 0)];
        json.changes[c].lamport = 1;
        if !with_successor {
            json.changes.retain(|c| c.id.peer == 1);
        }
        for detached in [true, false] {
            assert!(
                import_fails(json.clone(), detached),
                "successor {with_successor}, detached {detached}"
            );
        }
    }
}

/// The same change next to an honest dep on another peer is refused too.
#[test]
fn a_dep_on_an_older_own_op_next_to_other_deps_is_an_import_error() {
    let honest = alice_three_and_bob();
    let c = honest
        .changes
        .iter()
        .position(|c| c.id == ID::new(1, 2))
        .unwrap();
    let mut json = honest.clone();
    json.changes[c].deps = vec![ID::new(1, 0), ID::new(2, 0)];
    for detached in [true, false] {
        assert!(import_fails(json.clone(), detached));
    }
}

/// Deps on the change's own counter, a later own counter, or a counter
/// past the change are import errors; the honest history imports.
#[test]
fn other_self_deps_are_import_errors() {
    let honest = alice_three_and_bob();
    let c = honest
        .changes
        .iter()
        .position(|c| c.id == ID::new(1, 1))
        .unwrap();
    for dep in [ID::new(1, 1), ID::new(1, 2), ID::new(1, 7)] {
        let mut json = honest.clone();
        json.changes[c].deps = vec![dep];
        for detached in [true, false] {
            assert!(import_fails(json.clone(), detached), "dep {dep}");
        }
    }
    for detached in [true, false] {
        assert!(!import_fails(honest.clone(), detached));
    }
}

/// A change at counter 0 with a dep on its own peer is an import error.
#[test]
fn a_first_change_with_a_self_dep_is_an_import_error() {
    let honest = alice_three_and_bob();
    let mut json = honest.clone();
    json.changes[0].deps = vec![ID::new(1, 0)];
    for detached in [true, false] {
        assert!(import_fails(json.clone(), detached));
    }
}

/// A refused change refuses the whole import, also the honest changes
/// before it in the same update, and the document keeps its version.
#[test]
fn a_refused_change_refuses_the_whole_import() {
    let honest = alice_three_and_bob();
    let y = LoroDoc::new();
    y.import_json_updates(JsonSchema {
        changes: honest.changes[..1].to_vec(),
        ..honest.clone()
    })
    .unwrap();
    let before = y.oplog_vv();
    let value = y.get_deep_value();
    let mut json = honest.clone();
    let c = json
        .changes
        .iter()
        .position(|c| c.id == ID::new(1, 2))
        .unwrap();
    json.changes[c].deps = vec![ID::new(1, 0)];
    assert!(y.import_json_updates(json).is_err());
    assert_eq!(y.oplog_vv(), before);
    assert_eq!(y.get_deep_value(), value);
    y.import_json_updates(honest).unwrap();
    assert_eq!(
        y.get_map("m").get("c").unwrap().into_value().unwrap(),
        "c".into()
    );
}

/// A change of `peer` at counter 0 that deletes `len` list elements, with
/// `deps`.
fn long_delete(peer: u64, len: i32, deps: Vec<ID>) -> JsonChange {
    JsonChange {
        id: ID::new(peer, 0),
        timestamp: 0,
        deps,
        lamport: 0,
        msg: None,
        ops: vec![JsonOp {
            content: JsonOpContent::List(ListOp::Delete {
                pos: 0,
                len,
                start_id: ID::new(peer, 0),
            }),
            container: ContainerID::new_root("l", ContainerType::List),
            counter: 0,
        }],
    }
}

fn schema(changes: Vec<JsonChange>) -> JsonSchema {
    JsonSchema {
        schema_version: 1,
        start_version: Default::default(),
        peers: None,
        changes,
    }
}

/// Three chained changes of 2^31 - 2 atoms each would get lamports past
/// `u32::MAX`, which overflowed the lamport arithmetic of the DAG. They
/// are an import error, also when they arrive in separate imports.
#[test]
fn lamports_past_u32_are_an_import_error() {
    let len = i32::MAX - 1;
    let a = long_delete(1, len, vec![]);
    let b = long_delete(2, len, vec![ID::new(1, len - 1)]);
    let c = long_delete(3, len, vec![ID::new(2, len - 1)]);
    for detached in [true, false] {
        assert!(import_fails(
            schema(vec![a.clone(), b.clone(), c.clone()]),
            detached
        ));
    }
    // The deletes target elements that do not exist, so the oplog is
    // checked without computing a state.
    let y = LoroDoc::new();
    y.detach();
    y.import_json_updates(schema(vec![a, b])).unwrap();
    let vv = y.oplog_vv();
    assert!(y.import_json_updates(schema(vec![c])).is_err());
    assert_eq!(y.oplog_vv(), vv);
    assert_eq!(y.len_ops(), 2 * len as usize);
}

/// Parked changes count too: the change that would overflow arrives
/// first and waits, then its deps arrive.
#[test]
fn parked_changes_count_toward_the_lamport_limit() {
    let len = i32::MAX - 1;
    let y = LoroDoc::new();
    y.detach();
    let status = y
        .import_json_updates(schema(vec![long_delete(3, len, vec![ID::new(2, len - 1)])]))
        .unwrap();
    assert!(status.pending.is_some());
    y.import_json_updates(schema(vec![long_delete(1, len, vec![])]))
        .unwrap();
    assert!(y
        .import_json_updates(schema(vec![long_delete(2, len, vec![ID::new(1, len - 1)])]))
        .is_err());
    assert_eq!(y.oplog_vv().get(&2), None);
    assert_eq!(y.oplog_vv().get(&3), None);
}

/// Changes without ops, with ops that do not continue from the change's
/// counter, or with a negative dep counter broke the counter arithmetic
/// of the DAG and the change store. They are import errors.
#[test]
fn malformed_change_counters_are_import_errors() {
    let honest = alice_three_and_bob();
    let c = honest
        .changes
        .iter()
        .position(|c| c.id == ID::new(1, 1))
        .unwrap();
    let mut cases = Vec::new();
    let mut json = honest.clone();
    json.changes[c].ops.clear();
    cases.push(("no ops", json));
    let mut json = honest.clone();
    json.changes[c].ops[0].counter = 5;
    cases.push(("gap", json));
    let mut json = honest.clone();
    json.changes.retain(|c| c.id.peer == 2);
    json.changes[0].deps = vec![ID::new(1, -3)];
    cases.push(("negative dep", json));
    for (what, json) in cases {
        for detached in [true, false] {
            assert!(import_fails(json.clone(), detached), "{what}");
        }
    }
}

/// Alice's first two changes with timestamps `t0` and `t1`.
fn two_changes_at(t0: i64, t1: i64) -> JsonSchema {
    let mut json = alice_three_and_bob();
    json.changes.retain(|c| c.id.peer == 1 && c.id.counter < 2);
    json.changes[0].timestamp = t0;
    json.changes[1].timestamp = t1;
    json
}

/// Timestamps whose difference may not fit an `i64` overflowed when the
/// change store merged two changes, or when it delta-encoded a block on
/// export. They are import errors.
#[test]
fn extreme_timestamps_are_import_errors() {
    for (t0, t1) in [
        (0, i64::MIN),
        (i64::MAX, i64::MIN),
        (0, 1 << 62),
        (0, -(1 << 62)),
    ] {
        for detached in [true, false] {
            assert!(import_fails(two_changes_at(t0, t1), detached));
        }
    }
}

/// A peer's second change with a timestamp before its first one used to
/// fail a debug assertion when the change store merged the two. It
/// imports, keeps its timestamp, and the document exports and reimports.
#[test]
fn backward_timestamps_import() {
    const M: i64 = (1 << 62) - 1;
    for (t0, t1) in [(10, 5), (M, -M), (-M, M)] {
        let json = two_changes_at(t0, t1);
        for detached in [true, false] {
            let y = LoroDoc::new();
            if detached {
                y.detach();
            }
            y.import_json_updates(json.clone()).unwrap();
            y.checkout_to_latest();
            let z = LoroDoc::new();
            z.import(&y.export(ExportMode::Snapshot).unwrap()).unwrap();
            assert_eq!(z.get_deep_value(), y.get_deep_value());
            let z = LoroDoc::new();
            z.import(&y.export(ExportMode::all_updates()).unwrap())
                .unwrap();
            assert_eq!(z.get_deep_value(), y.get_deep_value());
            assert_eq!(z.get_change(ID::new(1, 1)).unwrap().timestamp, t1);
        }
    }
}

/// Honest input: a change that depends on more concurrent changes than
/// fit a change block (here 1,100) underflowed the room left in the block
/// when the change store split it, on commit and on import.
#[test]
fn a_change_with_more_deps_than_fit_a_block_imports() {
    let base = LoroDoc::new();
    for p in 0..1100u64 {
        let d = LoroDoc::new();
        d.set_peer_id(p + 10).unwrap();
        d.get_map("m").insert(&format!("k{p}"), 1).unwrap();
        d.commit();
        base.import(&d.export(ExportMode::all_updates()).unwrap())
            .unwrap();
    }
    assert_eq!(base.oplog_frontiers().len(), 1100);
    base.set_peer_id(1).unwrap();
    base.get_map("m").insert("merge", 1).unwrap();
    base.get_text("t").insert(0, &"x".repeat(10_000)).unwrap();
    base.commit();
    for mode in [ExportMode::all_updates(), ExportMode::Snapshot] {
        let y = LoroDoc::new();
        y.import(&base.export(mode).unwrap()).unwrap();
        assert_eq!(y.get_deep_value(), base.get_deep_value());
        assert_eq!(y.oplog_vv(), base.oplog_vv());
    }
}
