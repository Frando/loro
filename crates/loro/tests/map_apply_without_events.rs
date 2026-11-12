//! A map's state is the same whether an import records events or not:
//! without subscribers the map applies its diff without building the
//! event.
use std::sync::Arc;

use loro::{ExportMode, LoroDoc, LoroMap, LoroText, ToJson};

fn edits() -> Vec<u8> {
    let a = LoroDoc::new();
    a.set_peer_id(1).unwrap();
    let b = LoroDoc::new();
    b.set_peer_id(2).unwrap();
    let m = a.get_map("m");
    m.insert("title", "a").unwrap();
    m.insert("n", 1).unwrap();
    let child = m.insert_container("child", LoroMap::new()).unwrap();
    child.insert("x", "y").unwrap();
    let merged = m.ensure_mergeable_map("merged").unwrap();
    merged.insert("k", true).unwrap();
    m.insert("bytes", vec![1u8, 2, 3]).unwrap();
    a.commit();
    b.import(&a.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    // Concurrent writes of the same keys: the larger (lamport, peer) wins.
    a.get_map("m").insert("title", "from a").unwrap();
    a.commit();
    let bm = b.get_map("m");
    bm.insert("title", "from b").unwrap();
    bm.delete("n").unwrap();
    bm.insert_container("text", LoroText::new())
        .unwrap()
        .insert(0, "hello")
        .unwrap();
    bm.ensure_mergeable_map("merged")
        .unwrap()
        .insert("j", 2)
        .unwrap();
    b.commit();
    a.import(&b.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    a.export(ExportMode::all_updates()).unwrap()
}

#[test]
fn map_state_does_not_depend_on_event_recording() {
    let blob = edits();
    let quiet = LoroDoc::new();
    quiet.import(&blob).unwrap();
    let recording = LoroDoc::new();
    let _sub = recording.subscribe_root(Arc::new(|_| {}));
    recording.import(&blob).unwrap();
    assert_eq!(
        quiet.get_deep_value().to_json_value(),
        recording.get_deep_value().to_json_value()
    );
    // Child containers reached through the map work as handlers.
    let child = quiet
        .get_map("m")
        .get("child")
        .unwrap()
        .into_container()
        .unwrap()
        .into_map()
        .unwrap();
    assert_eq!(child.get("x").unwrap().into_value().unwrap(), "y".into());
    let merged = quiet.get_map("m").ensure_mergeable_map("merged").unwrap();
    assert_eq!(merged.len(), 2);
    // Reimporting a snapshot of the quiet document gives the same state.
    let again = LoroDoc::new();
    again
        .import(&quiet.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    assert_eq!(
        quiet.get_deep_value().to_json_value(),
        again.get_deep_value().to_json_value()
    );
}
