//! A change that is parked because its deps are missing takes its Lamport
//! time from its deps once they arrive, never from the time its sender
//! claimed.

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

fn value_of_k(doc: &LoroDoc) -> loro::LoroValue {
    doc.get_map("m").get("k").unwrap().into_value().unwrap()
}

/// A sender claims a raised time for Alice's change and orders it before
/// its dep, so it is parked. The time it gets when applied comes from the
/// deps, and the conflict resolves as in the honest document.
#[test]
fn parked_changes_get_lamports_from_their_deps() {
    let (honest, json) = conflict();
    assert_eq!(value_of_k(&honest), "bob".into());
    let mut a0 = pick(&json, 1, 0);
    a0.lamport = 1000;
    let forged = with_changes(
        &json,
        vec![a0, pick(&json, 10, 0), pick(&json, 2, 0), pick(&json, 1, 1)],
    );
    for detached in [true, false] {
        let y = LoroDoc::new();
        if detached {
            y.detach();
        }
        let status = y.import_json_updates(forged.clone()).unwrap();
        assert!(status.pending.is_none());
        assert_eq!(y.oplog_vv(), honest.oplog_vv());
        y.checkout_to_latest();
        assert_eq!(value_of_k(&y), "bob".into(), "detached {detached}");
        assert_eq!(y.get_change(ID::new(1, 0)).unwrap().lamport, 1);
    }
}

/// A parked change with a claimed time below the honest one also gets
/// the time of its deps.
#[test]
fn parked_changes_with_a_lowered_lamport_are_raised() {
    let (honest, json) = conflict();
    let mut a1 = pick(&json, 1, 1);
    a1.lamport = 0;
    let forged = with_changes(
        &json,
        vec![a1, pick(&json, 10, 0), pick(&json, 2, 0), pick(&json, 1, 0)],
    );
    let y = LoroDoc::new();
    y.import_json_updates(forged).unwrap();
    assert_eq!(y.oplog_vv(), honest.oplog_vv());
    assert_eq!(
        y.get_change(ID::new(1, 1)).unwrap().lamport,
        honest.get_change(ID::new(1, 1)).unwrap().lamport
    );
    assert_eq!(y.get_deep_value(), honest.get_deep_value());
}
