//! Crafted remote changes that used to fail an assertion or overflow on
//! import are import errors before anything is mutated, and honest edge
//! cases import.

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
    alice
        .export_json_updates_without_peer_compression(&VersionVector::default(), &alice.oplog_vv())
        .unwrap()
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

/// The longest delete that passes the position checks: 2^30 - 2 atoms.
const LONG: i32 = (u32::MAX / 4 - 2) as i32;

/// A change of `peer` at counter 0 that deletes `LONG` list elements, with
/// a dep on the last atom of `dep_peer`'s change.
fn long_delete(peer: u64, dep_peer: Option<u64>) -> JsonChange {
    JsonChange {
        id: ID::new(peer, 0),
        timestamp: 0,
        deps: dep_peer.map(|p| ID::new(p, LONG - 1)).into_iter().collect(),
        lamport: 0,
        msg: None,
        ops: vec![JsonOp {
            content: JsonOpContent::List(ListOp::Delete {
                pos: 0,
                len: LONG,
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

/// Peers 1 to `n`, each change depending on the one before.
fn long_chain(n: u64) -> Vec<JsonChange> {
    (1..=n)
        .map(|p| long_delete(p, (p > 1).then(|| p - 1)))
        .collect()
}

/// Five chained changes of 2^30 - 2 atoms each would get lamports past
/// `u32::MAX`, which overflowed the lamport arithmetic of the DAG; three of
/// them overflowed the visible op count, summed as `i32`. Five are an
/// import error, also when they arrive in separate imports; three import.
#[test]
fn lamports_past_u32_are_an_import_error() {
    for detached in [true, false] {
        assert!(import_fails(schema(long_chain(5)), detached));
    }
    // The deletes target elements that do not exist, so the oplog is
    // checked without computing a state.
    let y = LoroDoc::new();
    y.detach();
    let mut chain = long_chain(5);
    let last = chain.pop().unwrap();
    y.import_json_updates(schema(chain)).unwrap();
    let vv = y.oplog_vv();
    assert!(y.import_json_updates(schema(vec![last])).is_err());
    assert_eq!(y.oplog_vv(), vv);
    assert_eq!(y.len_ops(), 4 * LONG as usize);
}

/// Parked changes count too: the change that would overflow arrives
/// first and waits, then its deps arrive.
#[test]
fn parked_changes_count_toward_the_lamport_limit() {
    let y = LoroDoc::new();
    y.detach();
    let mut chain = long_chain(5);
    let last = chain.pop().unwrap();
    let status = y.import_json_updates(schema(vec![last])).unwrap();
    assert!(status.pending.is_some());
    let fourth = chain.pop().unwrap();
    y.import_json_updates(schema(chain)).unwrap();
    assert!(y.import_json_updates(schema(vec![fourth])).is_err());
    assert_eq!(y.oplog_vv().get(&4), None);
    assert_eq!(y.oplog_vv().get(&5), None);
}
