//! Crafted remote changes that used to fail an assertion or overflow on
//! import are import errors before anything is mutated, and honest edge
//! cases import.

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
