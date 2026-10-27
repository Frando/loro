//! Crafted remote changes that used to fail an assertion on import (and
//! abort the process when the document was dropped during the unwind) are
//! import errors before anything is mutated.

use loro::{CommitOptions, ExportMode, JsonSchema, LoroDoc, VersionVector, ID};

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
