//! A remote change that starts past its peer's held counter waits in the
//! pending set until the gap fills, even when its deps are held.

use loro::{CommitOptions, ExportMode, JsonChange, JsonSchema, LoroDoc, VersionVector, ID};

/// Base Z0 (peer 10), Alice (peer 1, two changes, the second depends on
/// the first only) and Bob (peer 2), concurrent on `m.k`. Returns the
/// honest document and its JSON.
fn conflict() -> (LoroDoc, JsonSchema) {
    let z = LoroDoc::new();
    z.set_peer_id(10).unwrap();
    z.get_map("m").insert("z", 1).unwrap();
    z.commit();
    let base = z.export(ExportMode::all_updates()).unwrap();
    let alice = LoroDoc::new();
    alice.set_peer_id(1).unwrap();
    alice.import(&base).unwrap();
    alice.get_map("m").insert("k", "alice").unwrap();
    alice.commit_with(CommitOptions::new().commit_msg("a0"));
    alice.get_map("m").insert("j", "alice").unwrap();
    alice.commit_with(CommitOptions::new().commit_msg("a1"));
    let bob = LoroDoc::new();
    bob.set_peer_id(2).unwrap();
    bob.import(&base).unwrap();
    bob.get_map("m").insert("k", "bob").unwrap();
    bob.commit();
    let honest = LoroDoc::new();
    honest
        .import(&alice.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    honest
        .import(&bob.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    let json = honest.export_json_updates_without_peer_compression(
        &VersionVector::default(),
        &honest.oplog_vv(),
    );
    (honest, json)
}

/// The `n`th change of `peer` in `json`.
fn pick(json: &JsonSchema, peer: u64, n: usize) -> JsonChange {
    json.changes
        .iter()
        .filter(|c| c.id.peer == peer)
        .nth(n)
        .unwrap()
        .clone()
}

/// `json` with only `changes`, in that order.
fn with_changes(json: &JsonSchema, changes: Vec<JsonChange>) -> JsonSchema {
    let mut s = json.clone();
    s.changes = changes;
    s
}

/// Bob's change moved to counter 5 while its dep (Z0) is held: parked as
/// pending, no counter assert.
#[test]
fn a_counter_gap_with_held_deps_is_parked() {
    let (_, json) = conflict();
    let mut gap = pick(&json, 2, 0);
    gap.id.counter = 5;
    for op in &mut gap.ops {
        op.counter += 5;
    }
    let forged = with_changes(&json, vec![pick(&json, 10, 0), gap]);
    for detached in [true, false] {
        let y = LoroDoc::new();
        if detached {
            y.detach();
        }
        let status = y.import_json_updates(forged.clone()).unwrap();
        assert!(status.pending.is_some(), "detached {detached}");
        assert_eq!(y.oplog_vv().get(&2), None);
        y.checkout_to_latest();
        assert!(y.get_map("m").get("k").is_none());
    }
}

/// Alice's second change without its self-dep (its only dep is Z0)
/// arrives before her first: it waits for the gap, then applies.
#[test]
fn a_parked_gap_applies_once_the_gap_fills() {
    let (_, json) = conflict();
    let mut a1 = pick(&json, 1, 1);
    a1.deps = vec![ID::new(10, 0)];
    let y = LoroDoc::new();
    y.import_json_updates(with_changes(&json, vec![pick(&json, 10, 0), a1]))
        .unwrap();
    assert_eq!(y.oplog_vv().get(&1), None);
    let status = y
        .import_json_updates(with_changes(&json, vec![pick(&json, 1, 0)]))
        .unwrap();
    assert!(status.pending.is_none());
    assert_eq!(y.oplog_vv().get(&1), Some(&2));
    assert_eq!(
        y.get_map("m").get("j").unwrap().into_value().unwrap(),
        "alice".into()
    );
}
