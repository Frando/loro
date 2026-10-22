//! `LoroDoc::visit_ops_in_id_span` reads the same changes and ops as
//! `LoroDoc::export_json_in_id_span`.
//!
//! The visitor gives changes and ops whole; the tests slice them to the
//! span the way the JSON export slices them, build JSON changes from the
//! views, and compare their serialized bytes with the export's.

use loro::json::{JsonChange, JsonOp, JsonOpContent, ListOp, MapOp, MovableListOp, TextOp, TreeOp};
use loro::op_view::{ChangeView, OpContentView, OpView, UnknownValue};
use loro::{
    CommitOptions, ContainerID, ContainerType, Counter, ExpandType, ExportMode, FractionalIndex,
    IdSpan, LoroDoc, LoroList, LoroMap, LoroText, StyleConfig, StyleConfigMap, ID,
};

/// A change and its ops as the visitor gave them, owned.
struct Visited {
    id: ID,
    deps: Vec<ID>,
    message: Option<String>,
    lamport: u32,
    timestamp: i64,
    ops: Vec<VisitedOp>,
}

/// An op as the visitor gave it, owned.
struct VisitedOp {
    counter: Counter,
    len: usize,
    container: ContainerID,
    content: Content,
}

/// An owned copy of [`OpContentView`].
enum Content {
    TextInsert(u32, String),
    ListInsert(u32, Vec<loro::LoroValue>),
    Delete(isize, isize, ID),
    Mark(u32, u32, String, loro::LoroValue, u8),
    MarkEnd,
    Move(u32, u32, loro::IdLp),
    Set(loro::IdLp, loro::LoroValue),
    MapSet(String, Option<loro::LoroValue>),
    TreeCreate(loro::TreeID, Option<loro::TreeID>, Vec<u8>),
    TreeMove(loro::TreeID, Option<loro::TreeID>, Vec<u8>),
    TreeDelete(loro::TreeID),
    Counter(f64),
}

fn own(op: &OpView<'_>) -> Content {
    match &op.content {
        OpContentView::TextInsert { pos, text } => Content::TextInsert(*pos, text.to_string()),
        OpContentView::ListInsert { pos, values } => Content::ListInsert(*pos, values.to_vec()),
        OpContentView::Delete {
            pos,
            signed_len,
            start_id,
        } => Content::Delete(*pos, *signed_len, *start_id),
        OpContentView::Mark {
            start,
            end,
            key,
            value,
            info,
        } => Content::Mark(*start, *end, key.to_string(), (*value).clone(), *info),
        OpContentView::MarkEnd => Content::MarkEnd,
        OpContentView::Move { from, to, elem } => Content::Move(*from, *to, *elem),
        OpContentView::Set { elem, value } => Content::Set(*elem, (*value).clone()),
        OpContentView::MapSet { key, value } => {
            Content::MapSet(key.to_string(), value.cloned())
        }
        OpContentView::TreeCreate {
            target,
            parent,
            position,
        } => Content::TreeCreate(*target, *parent, position.to_vec()),
        OpContentView::TreeMove {
            target,
            parent,
            position,
        } => Content::TreeMove(*target, *parent, position.to_vec()),
        OpContentView::TreeDelete { target } => Content::TreeDelete(*target),
        OpContentView::Counter(x) => Content::Counter(*x),
        OpContentView::Unknown { value, .. } => match value {
            UnknownValue::Value(_) | UnknownValue::Future { .. } | UnknownValue::Internal => {
                panic!("no unknown containers in these documents")
            }
        },
    }
}

/// The visitor's changes and ops for `span`, and its return value.
fn visit(doc: &LoroDoc, span: IdSpan) -> (Vec<Visited>, Counter) {
    let mut out: Vec<Visited> = Vec::new();
    let end = doc.visit_ops_in_id_span(span, &mut |c: &ChangeView<'_>, op: &OpView<'_>| {
        if out.last().is_none_or(|v| v.id != c.id) {
            out.push(Visited {
                id: c.id,
                deps: c.deps.iter().collect(),
                message: c.message.map(str::to_string),
                lamport: c.lamport,
                timestamp: c.timestamp,
                ops: Vec::new(),
            });
        }
        out.last_mut().unwrap().ops.push(VisitedOp {
            counter: op.counter,
            len: op.len,
            container: op.container.clone(),
            content: own(op),
        });
    });
    (out, end)
}

/// The atoms `from..to` (relative to the op's start) of an op, as the JSON
/// export encodes them.
fn json_content(
    container: &ContainerID,
    content: &Content,
    from: usize,
    to: usize,
) -> JsonOpContent {
    let kind = container.container_type();
    let delete = |pos: isize, signed_len: isize, start: ID| {
        let len = signed_len.unsigned_abs();
        let (pos, signed_len, start) = if signed_len > 0 {
            (pos, (to - from) as isize, start.inc(from as i32))
        } else {
            (
                pos - from as isize,
                from as isize - to as isize,
                start.inc((len - to) as i32),
            )
        };
        (pos as i32, signed_len as i32, start)
    };
    match content {
        Content::TextInsert(pos, text) => JsonOpContent::Text(TextOp::Insert {
            pos: pos + from as u32,
            text: text.chars().skip(from).take(to - from).collect(),
        }),
        Content::ListInsert(pos, values) => {
            let value = values[from..to].to_vec();
            let pos = pos + from as u32;
            if kind == ContainerType::MovableList {
                JsonOpContent::MovableList(MovableListOp::Insert { pos, value })
            } else {
                JsonOpContent::List(ListOp::Insert { pos, value })
            }
        }
        Content::Delete(pos, signed_len, start_id) => {
            let (pos, len, start_id) = delete(*pos, *signed_len, *start_id);
            match kind {
                ContainerType::Text => JsonOpContent::Text(TextOp::Delete { pos, len, start_id }),
                ContainerType::MovableList => {
                    JsonOpContent::MovableList(MovableListOp::Delete { pos, len, start_id })
                }
                _ => JsonOpContent::List(ListOp::Delete { pos, len, start_id }),
            }
        }
        Content::Mark(start, end, key, value, info) => JsonOpContent::Text(TextOp::Mark {
            start: *start,
            end: *end,
            style_key: key.clone(),
            style_value: value.clone(),
            info: *info,
        }),
        Content::MarkEnd => JsonOpContent::Text(TextOp::MarkEnd),
        Content::Move(from, to, elem_id) => JsonOpContent::MovableList(MovableListOp::Move {
            from: *from,
            to: *to,
            elem_id: *elem_id,
        }),
        Content::Set(elem_id, value) => JsonOpContent::MovableList(MovableListOp::Set {
            elem_id: *elem_id,
            value: value.clone(),
        }),
        Content::MapSet(key, value) => JsonOpContent::Map(match value {
            Some(value) => MapOp::Insert {
                key: key.clone(),
                value: value.clone(),
            },
            None => MapOp::Delete { key: key.clone() },
        }),
        Content::TreeCreate(target, parent, position) => JsonOpContent::Tree(TreeOp::Create {
            target: *target,
            parent: *parent,
            fractional_index: FractionalIndex::from_bytes(position.clone()),
        }),
        Content::TreeMove(target, parent, position) => JsonOpContent::Tree(TreeOp::Move {
            target: *target,
            parent: *parent,
            fractional_index: FractionalIndex::from_bytes(position.clone()),
        }),
        Content::TreeDelete(target) => JsonOpContent::Tree(TreeOp::Delete { target: *target }),
        Content::Counter(_) => unreachable!("compared separately"),
    }
}

/// The JSON changes for `span` built from the visitor's views, sliced to
/// the span as `export_json_in_id_span` slices.
fn json_from_views(doc: &LoroDoc, mut span: IdSpan) -> Vec<serde_json::Value> {
    let (visited, end) = visit(doc, span);
    span.normalize_();
    let held = doc.oplog_vv().get(&span.peer).copied().unwrap_or(0);
    let (s, e) = (span.counter.start.max(0), span.counter.end.min(held));
    if s < e {
        assert!(
            end >= e || doc.is_shallow(),
            "the visitor reached the span's end"
        );
    } else {
        assert!(visited.is_empty());
    }
    let mut changes = Vec::new();
    for v in visited {
        let first = v.ops.first().unwrap();
        let last = v.ops.last().unwrap();
        let change_start = v.id.counter;
        let lo = change_start.max(s);
        assert!(first.counter <= lo && lo < first.counter + first.len as i32);
        assert!(last.counter < e);
        let mut ops = Vec::new();
        for op in &v.ops {
            let op_lo = op.counter.max(s);
            let op_hi = (op.counter + op.len as i32).min(e);
            assert!(op_lo < op_hi, "only overlapping ops are visited");
            let (from, to) = ((op_lo - op.counter) as usize, (op_hi - op.counter) as usize);
            let content = match &op.content {
                Content::Counter(x) => {
                    let ans = serde_json::json!({
                        "container": op.container.to_string(),
                        "content": {"type": "counter", "value_type": "f64", "value": x, "prop": 0},
                        "counter": op_lo,
                    });
                    ops.push(ans);
                    continue;
                }
                c => json_content(&op.container, c, from, to),
            };
            let json_op = JsonOp {
                content,
                container: op.container.clone(),
                counter: op_lo,
            };
            ops.push(serde_json::to_value(&json_op).unwrap());
        }
        let deps = if lo > change_start {
            vec![ID::new(v.id.peer, lo - 1)]
        } else {
            let mut deps = v.deps.clone();
            deps.sort();
            deps
        };
        let change = JsonChange {
            id: ID::new(v.id.peer, lo),
            timestamp: v.timestamp,
            deps,
            lamport: v.lamport + (lo - change_start) as u32,
            msg: v.message.clone(),
            ops: Vec::new(),
        };
        let mut change = serde_json::to_value(&change).unwrap();
        change["ops"] = serde_json::Value::Array(ops);
        changes.push(change);
    }
    changes
}

/// The export's changes for `span`.
fn json_from_export(doc: &LoroDoc, span: IdSpan) -> Vec<serde_json::Value> {
    doc.export_json_in_id_span(span)
        .iter()
        .map(|c| serde_json::to_value(c).unwrap())
        .collect()
}

/// Asserts that the views and the export agree on `span`, byte for byte
/// after serialization.
fn check(doc: &LoroDoc, span: IdSpan) {
    let views = json_from_views(doc, span);
    let export = json_from_export(doc, span);
    let views = serde_json::to_string(&views).unwrap();
    let export = serde_json::to_string(&export).unwrap();
    assert_eq!(views, export, "span {span:?}");
}

/// Every span of every peer whose ends lie on `step`-spaced counters, plus
/// the empty span, a reversed span and a span past the version vector.
fn check_all(doc: &LoroDoc, step: usize) {
    for (&peer, &end) in doc.oplog_vv().iter() {
        let mut points: Vec<i32> = (0..=end).step_by(step).collect();
        points.push(end);
        for &a in &points {
            for &b in &points {
                if a < b {
                    check(doc, IdSpan::new(peer, a, b));
                }
            }
        }
        check(doc, IdSpan::new(peer, 0, 0));
        check(doc, IdSpan::new(peer, end, end + 10));
        check(doc, IdSpan::new(peer, end + 5, end + 10));
        check(doc, IdSpan::new(peer, -3, end + 10));
    }
    check(doc, IdSpan::new(12345, 0, 10));
}

/// Two peers editing every container kind, concurrently and in turn, with
/// commit messages, nested containers, astral characters, forward and
/// backward deletes and marks.
fn every_kind() -> LoroDoc {
    let a = LoroDoc::new();
    a.set_peer_id(1).unwrap();
    let mut styles = StyleConfigMap::new();
    styles.insert(
        "bold".into(),
        StyleConfig {
            expand: ExpandType::After,
        },
    );
    a.config_text_style(styles);
    let text = a.get_text("text");
    text.insert(0, "Hello \u{1F600} world \u{1D11E}!").unwrap();
    text.mark(0..5, "bold", true).unwrap();
    a.commit_with(CommitOptions::new().commit_msg("text \u{1F600}"));
    // A backward delete (backspaces) and a forward delete.
    for pos in (8..12).rev() {
        text.delete(pos, 1).unwrap();
    }
    text.delete(1, 3).unwrap();
    a.commit();
    let map = a.get_map("map");
    map.insert("k", 1).unwrap();
    map.insert("s", "v").unwrap();
    map.insert("f", 1.5).unwrap();
    map.delete("k").unwrap();
    let nested = map.insert_container("list", LoroList::new()).unwrap();
    nested.insert(0, "x").unwrap();
    nested
        .insert_container(1, LoroText::new())
        .unwrap()
        .insert(0, "nested \u{1F600}")
        .unwrap();
    a.commit_with(CommitOptions::new().commit_msg("map"));
    let list = a.get_list("list");
    for i in 0..6 {
        list.insert(i, i as i64).unwrap();
    }
    list.delete(1, 3).unwrap();
    list.insert_container(0, LoroMap::new())
        .unwrap()
        .insert("in", true)
        .unwrap();
    let mlist = a.get_movable_list("mlist");
    for i in 0..5 {
        mlist.insert(i, format!("v{i}")).unwrap();
    }
    mlist.mov(0, 3).unwrap();
    mlist.set(1, "set").unwrap();
    mlist.delete(2, 2).unwrap();
    a.commit();
    let tree = a.get_tree("tree");
    tree.enable_fractional_index(0);
    let root = tree.create(None).unwrap();
    let child = tree.create(root).unwrap();
    let other = tree.create(None).unwrap();
    tree.mov(child, other).unwrap();
    tree.delete(root).unwrap();
    tree.get_meta(other).unwrap().insert("name", "n").unwrap();
    let counter = a.get_counter("counter");
    counter.increment(1.5).unwrap();
    counter.decrement(0.25).unwrap();
    a.commit_with(CommitOptions::new().commit_msg("tree and counter"));

    let b = LoroDoc::new();
    b.set_peer_id(2).unwrap();
    b.import(&a.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    // Concurrent edits on both peers.
    b.get_text("text").insert(2, "\u{1F680}b").unwrap();
    b.get_map("map").insert("s", "b").unwrap();
    b.get_movable_list("mlist").mov(1, 0).unwrap();
    b.commit_with(CommitOptions::new().commit_msg("b"));
    a.get_text("text").insert(0, "a\u{1F680}").unwrap();
    a.get_map("map").insert("s", "a").unwrap();
    a.commit();
    a.import(&b.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    a.get_text("text").insert(0, "merged").unwrap();
    a.get_counter("counter").increment(2.0).unwrap();
    a.commit();
    a
}

#[test]
fn views_equal_the_json_export_on_every_kind() {
    let doc = every_kind();
    check_all(&doc, 1);
}

#[test]
fn views_equal_the_json_export_after_a_snapshot_import() {
    let doc = every_kind();
    let copy = LoroDoc::new();
    copy.import(&doc.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    check_all(&copy, 1);
}

#[test]
fn views_equal_the_json_export_on_a_long_history() {
    let doc = LoroDoc::new();
    doc.set_peer_id(7).unwrap();
    let text = doc.get_text("t");
    for i in 0..300 {
        text.insert(text.len_unicode(), "ab\u{1F600}").unwrap();
        if i % 3 == 0 {
            text.delete(i % 7, 2).unwrap();
        }
        if i % 10 == 0 {
            doc.commit_with(CommitOptions::new().commit_msg(&format!("c{i}")));
        }
    }
    doc.commit();
    // Through the encoded change store too, where blocks are parsed lazily.
    let copy = LoroDoc::new();
    copy.import(&doc.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    check_all(&doc, 37);
    check_all(&copy, 37);
}

#[test]
fn views_equal_the_json_export_on_a_shallow_snapshot() {
    let doc = every_kind();
    let frontiers = doc.oplog_frontiers();
    doc.get_text("text").insert(0, "after").unwrap();
    doc.commit();
    let shallow = LoroDoc::new();
    shallow
        .import(
            &doc.export(ExportMode::shallow_snapshot(&frontiers))
                .unwrap(),
        )
        .unwrap();
    assert!(shallow.is_shallow());
    check_all(&shallow, 1);
}

#[test]
fn uncommitted_ops_are_not_visited() {
    let doc = LoroDoc::new();
    doc.set_peer_id(3).unwrap();
    doc.get_text("t").insert(0, "abc").unwrap();
    doc.commit();
    doc.get_text("t").insert(0, "uncommitted").unwrap();
    let (visited, end) = visit(&doc, IdSpan::new(3, 0, 100));
    assert_eq!(end, 3);
    assert_eq!(visited.len(), 1);
}

#[test]
fn ops_are_given_whole() {
    let doc = LoroDoc::new();
    doc.set_peer_id(4).unwrap();
    doc.get_text("t").insert(0, "abcdef").unwrap();
    doc.commit();
    let (visited, end) = visit(&doc, IdSpan::new(4, 2, 4));
    assert_eq!(end, 6);
    assert_eq!(visited.len(), 1);
    assert_eq!(visited[0].id, ID::new(4, 0));
    let op = &visited[0].ops[0];
    assert_eq!((op.counter, op.len), (0, 6));
    assert!(matches!(&op.content, Content::TextInsert(0, t) if t == "abcdef"));
}
