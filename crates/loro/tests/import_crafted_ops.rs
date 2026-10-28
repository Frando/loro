//! Crafted but decodable ops, imported as JSON updates, must not panic,
//! hang or abort when they are applied to the state.

use loro::{ExportMode, LoroDoc, TreeID};
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

const T: &str = "cid:root-t:Text";
const TR: &str = "cid:root-tr:Tree";

fn mark(start: u32, end: u32) -> Value {
    json!({"type":"mark","start":start,"end":end,"style_key":"b","style_value":true,"info":132})
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

// Tree ops: invalid ops have no effect, like cyclic moves.

fn tree_nodes(doc: &LoroDoc) -> Vec<TreeID> {
    let mut nodes = doc.get_tree("tr").nodes();
    nodes.sort();
    nodes
}

fn assert_tree_unchanged(ops: &[(&str, Value)], ops3: &[(&str, Value)]) {
    let base = base_doc();
    let json = crafted(&base, ops, ops3);
    assert_imports(&json);
    for detached in [false, true] {
        let doc = LoroDoc::new();
        if detached {
            doc.detach();
        }
        doc.import_json_updates(json.as_str()).unwrap();
        doc.checkout_to_latest();
        assert_eq!(tree_nodes(&doc), tree_nodes(&base), "detached {detached}");
        assert_eq!(doc.get_deep_value(), base.get_deep_value());
    }
}

/// A node created under itself used to loop forever in the tree diff
/// calculator (`get_children_with_id` found the node as its own child).
#[test]
fn a_tree_node_created_under_itself_has_no_effect() {
    assert_tree_unchanged(
        &[(
            TR,
            json!({"type":"create","target":"0@1","parent":"0@1","fractional_index":"80"}),
        )],
        &[],
    );
}

/// Two nodes created under each other, before either exists, used to form
/// a cycle and loop forever.
#[test]
fn tree_nodes_created_under_each_other_have_no_effect() {
    assert_tree_unchanged(
        &[
            (
                TR,
                json!({"type":"create","target":"0@1","parent":"1@1","fractional_index":"80"}),
            ),
            (
                TR,
                json!({"type":"create","target":"1@1","parent":"0@1","fractional_index":"80"}),
            ),
        ],
        &[],
    );
    // The same across peers, so the result must not depend on the order.
    assert_tree_unchanged(
        &[(
            TR,
            json!({"type":"create","target":"0@1","parent":"0@2","fractional_index":"80"}),
        )],
        &[(
            TR,
            json!({"type":"create","target":"0@2","parent":"0@1","fractional_index":"80"}),
        )],
    );
}

/// Moves and deletes of a node that was never created used to create it
/// ("Parent is not registered" for its metadata map, or the
/// `!entry.has_child` assert of `TreeState::mov`).
#[test]
fn tree_moves_and_deletes_of_unknown_nodes_have_no_effect() {
    assert_tree_unchanged(
        &[(
            TR,
            json!({"type":"move","target":"77@0","parent":null,"fractional_index":"80"}),
        )],
        &[],
    );
    assert_tree_unchanged(&[(TR, json!({"type":"delete","target":"77@0"}))], &[]);
    assert_tree_unchanged(
        &[(
            TR,
            json!({"type":"move","target":"0@0","parent":null,"fractional_index":"80"}),
        )],
        &[],
    );
}

/// A node moved or created under a node that was never created.
#[test]
fn tree_ops_under_unknown_parents_have_no_effect() {
    assert_tree_unchanged(
        &[(
            TR,
            json!({"type":"move","target":"14@0","parent":"77@0","fractional_index":"80"}),
        )],
        &[],
    );
    assert_tree_unchanged(
        &[(
            TR,
            json!({"type":"create","target":"0@1","parent":"55@0","fractional_index":"80"}),
        )],
        &[],
    );
}

/// Honest concurrent tree edits still merge: a node created by peer 2 and
/// moved under it by peer 3 in a later change.
#[test]
fn honest_tree_edits_still_apply() {
    let base = base_doc();
    let a = LoroDoc::new();
    a.set_peer_id(2).unwrap();
    a.import(&base.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    let n = a.get_tree("tr").create(None).unwrap();
    a.get_tree("tr").mov(TreeID::new(1, 15), n).unwrap();
    a.commit();
    let b = LoroDoc::new();
    b.set_peer_id(3).unwrap();
    b.import(&base.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    b.get_tree("tr").delete(TreeID::new(1, 14)).unwrap();
    b.commit();
    let x = LoroDoc::new();
    x.import(&a.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    x.import(&b.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    let y = LoroDoc::new();
    y.detach();
    y.import(&b.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    y.import(&a.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    y.checkout_to_latest();
    assert_eq!(x.get_deep_value(), y.get_deep_value());
    assert_eq!(
        x.get_tree("tr").parent(TreeID::new(1, 15)),
        Some(loro::TreeParentId::Node(n))
    );
}

// Style anchors.

/// A change of peer 2 that starts with a `StyleEnd` arrives before the
/// change with the op before it, which is not a `StyleStart` of the same
/// text. The check before the oplog cannot see that op, so the change is
/// parked; when the op arrives, the end has no effect (it failed an
/// `unreachable!` in the diff calculator).
#[test]
fn a_parked_style_end_without_its_style_start_has_no_effect() {
    let base = base_doc();
    let firsts: Vec<(&str, Value)> = vec![
        (T, json!({"type":"insert","pos":0,"text":"x"})),
        ("cid:12@0:Text", mark(0, 1)),
    ];
    for first in firsts {
        let json = crafted(&base, std::slice::from_ref(&first), &[]);
        let mut v: Value = serde_json::from_str(&json).unwrap();
        let mut end_change = v["changes"].as_array().unwrap().last().unwrap().clone();
        end_change["id"] = json!("1@1");
        end_change["deps"] = json!(["0@1"]);
        end_change["lamport"] = json!(0);
        end_change["ops"] = json!([{"container": T, "content": {"type":"mark_end"}, "counter": 1}]);
        let first_change = v["changes"].as_array_mut().unwrap().pop().unwrap();
        let mut parked = v.clone();
        parked["changes"] = json!([end_change]);
        let mut rest = v.clone();
        rest["changes"].as_array_mut().unwrap().push(first_change);
        for detached in [false, true] {
            let doc = LoroDoc::new();
            if detached {
                doc.detach();
            }
            let status = doc.import_json_updates(parked.to_string()).unwrap();
            assert!(status.pending.is_some());
            doc.import_json_updates(rest.to_string()).unwrap();
            doc.checkout_to_latest();
            assert!(!doc.is_detached());
            assert_eq!(doc.oplog_vv().get(&2), Some(&2));
            let text = doc.get_text("t");
            assert_eq!(text.get_richtext_value(), {
                let honest = LoroDoc::new();
                honest.import_json_updates(rest.to_string()).unwrap();
                honest.get_text("t").get_richtext_value()
            });
            let snapshot = doc.export(ExportMode::Snapshot).unwrap();
            LoroDoc::new().import(&snapshot).unwrap();
        }
    }
}
