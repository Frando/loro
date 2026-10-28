//! Crafted but decodable ops, imported as JSON updates, must not panic,
//! hang or abort when they are applied to the state.

use loro::{ExportMode, LoroDoc};
use serde_json::{json, Value};

/// A document of peer 1 with a text, a list, a movable list, a map with a
/// child text, a tree with two nodes and a counter. Counters of peer 1:
/// text insert 0..5, mark 5, mark end 6, list insert 7..9, movable list
/// insert 9..11, map `k` 11, map `sub` 12, sub text 13, tree nodes 14 and 15
/// (15 under 14), counter 16.
fn base_doc() -> LoroDoc {
    let d = LoroDoc::new();
    d.set_peer_id(1).unwrap();
    let t = d.get_text("t");
    t.insert(0, "hello").unwrap();
    t.mark(0..2, "bold", true).unwrap();
    let l = d.get_list("l");
    l.insert(0, 1).unwrap();
    l.insert(1, 2).unwrap();
    let m = d.get_movable_list("ml");
    m.insert(0, 1).unwrap();
    m.insert(1, 2).unwrap();
    let mp = d.get_map("m");
    mp.insert("k", 1).unwrap();
    let sub = mp.insert_container("sub", loro::LoroText::new()).unwrap();
    sub.insert(0, "x").unwrap();
    let tr = d.get_tree("tr");
    let a = tr.create(None).unwrap();
    tr.create(a).unwrap();
    d.get_counter("c").increment(1.5).unwrap();
    d.commit();
    d
}

fn op_len(content: &Value) -> i64 {
    if let Some(t) = content.get("text") {
        return t.as_str().unwrap().chars().count() as i64;
    }
    if let Some(Value::Array(a)) = content.get("value") {
        if content["type"] == "insert" {
            return a.len() as i64;
        }
    }
    if let Some(l) = content.get("len") {
        return l.as_i64().unwrap().abs();
    }
    1
}

/// JSON updates with the history of `base`, a change of peer 2 with `ops`
/// and a change of peer 3 with `ops3`, both depending on the last op of
/// `base`. In ids, `n@1` is peer 2 and `n@2` is peer 3.
fn crafted(base: &LoroDoc, ops: &[(&str, Value)], ops3: &[(&str, Value)]) -> String {
    let j = base.export_json_updates(&Default::default(), &base.oplog_vv());
    let mut v: Value = serde_json::to_value(&j).unwrap();
    v["peers"] = json!(["1", "2", "3"]);
    let dep = format!("{}@0", base.oplog_vv().get(&1).unwrap() - 1);
    for (index, ops) in [(1, ops), (2, ops3)] {
        if ops.is_empty() {
            continue;
        }
        let mut counter = 0;
        let mut jops = vec![];
        for (container, content) in ops {
            jops.push(json!({"container": container, "content": content, "counter": counter}));
            counter += op_len(content);
        }
        v["changes"].as_array_mut().unwrap().push(json!({
            "id": format!("0@{index}"), "timestamp": 0, "deps": [dep], "lamport": 0,
            "msg": null, "ops": jops,
        }));
    }
    v.to_string()
}

/// Imports `json` attached, and detached followed by a checkout to the
/// latest version, then reads, exports and edits each document. Returns
/// the import results.
fn import_both(json: &str) -> Vec<Result<(), String>> {
    let mut results = vec![];
    for detached in [false, true] {
        let doc = LoroDoc::new();
        if detached {
            doc.detach();
        }
        let r = doc
            .import_json_updates(json)
            .map(|_| ())
            .map_err(|e| e.to_string());
        doc.checkout_to_latest();
        let _ = doc.get_deep_value();
        let snapshot = doc.export(ExportMode::Snapshot).unwrap();
        LoroDoc::new().import(&snapshot).unwrap();
        if !doc.is_detached() {
            doc.get_text("t").insert(0, "z").unwrap();
            doc.commit();
        }
        results.push(r);
    }
    results
}

fn assert_imports(json: &str) {
    for r in import_both(json) {
        r.unwrap();
    }
}

// Container parent cycles across peers.

/// Peer 2 creates map A in peer 3's map B, and peer 3 creates B in A. Each
/// peer's ops are valid alone; together the parent links form a cycle. Walks
/// up the parents used to recurse or loop forever (a stack overflow aborted
/// the process); now the containers have no depth and count as deleted.
#[test]
fn a_container_parent_cycle_across_peers_does_not_overflow() {
    let base = base_doc();
    let json = crafted(
        &base,
        &[
            (
                "cid:0@2:Map",
                json!({"type":"insert","key":"a","value":"\u{1f99c}:cid:0@1:Map"}),
            ),
            (
                "cid:0@1:Map",
                json!({"type":"insert","key":"c","value":"\u{1f99c}:cid:1@1:Text"}),
            ),
            ("cid:1@1:Text", json!({"type":"insert","pos":0,"text":"hi"})),
        ],
        &[(
            "cid:0@1:Map",
            json!({"type":"insert","key":"b","value":"\u{1f99c}:cid:0@2:Map"}),
        )],
    );
    assert_imports(&json);
    let doc = LoroDoc::new();
    doc.import_json_updates(json.as_str()).unwrap();
    assert_eq!(doc.get_deep_value(), base.get_deep_value());
}
