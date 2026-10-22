//! `LoroDoc::import_with_budget`: honest bytes within a budget import as
//! with `import`, and bytes past the budget are refused before they change
//! the document.

use loro::{
    CommitOptions, ExportMode, ImportBudget, LoroDoc, LoroError, ToJson, VersionVector,
    IMPORT_BUDGET_EXCEEDED,
};

/// Three peers editing text (with deletes), a map, a list, a movable list,
/// a tree (with moves) and a counter, merged into one document.
fn rich_doc() -> LoroDoc {
    let docs: Vec<LoroDoc> = (1..=3)
        .map(|peer| {
            let doc = LoroDoc::new();
            doc.set_peer_id(peer).unwrap();
            doc
        })
        .collect();
    for (i, doc) in docs.iter().enumerate() {
        let text = doc.get_text("text");
        text.insert(0, &format!("hello from peer {i}, ")).unwrap();
        text.delete(0, 3).unwrap();
        doc.get_map("map")
            .insert(&format!("k{i}"), i as i64)
            .unwrap();
        doc.get_list("list").insert(0, i as i64).unwrap();
        let movable = doc.get_movable_list("movable");
        movable.push("a").unwrap();
        movable.push("b").unwrap();
        movable.mov(0, 1).unwrap();
        let tree = doc.get_tree("tree");
        let root = tree.create(None).unwrap();
        let child = tree.create(root).unwrap();
        let other = tree.create(None).unwrap();
        tree.mov(child, other).unwrap();
        doc.get_counter("counter")
            .increment(i as f64 + 1.0)
            .unwrap();
        doc.commit_with(CommitOptions::new().commit_msg("edit"));
        doc.get_text("text").insert(0, "more ").unwrap();
        doc.commit();
    }
    let merged = LoroDoc::new();
    for doc in &docs {
        merged
            .import(&doc.export(ExportMode::all_updates()).unwrap())
            .unwrap();
    }
    merged
}

fn plenty() -> ImportBudget {
    ImportBudget {
        changes: 1 << 20,
        ops: 1 << 20,
        decompressed_bytes: 1 << 26,
    }
}

/// The number of ops (atoms) of `doc`, as the budget counts them.
fn atoms(doc: &LoroDoc) -> usize {
    doc.oplog_vv().values().map(|c| *c as usize).sum()
}

fn assert_budget_error(result: Result<loro::ImportStatus, LoroError>) {
    match result {
        Err(LoroError::DecodeError(msg)) => assert!(
            msg.starts_with(IMPORT_BUDGET_EXCEEDED),
            "unexpected decode error: {msg}"
        ),
        other => panic!("expected a budget error, got {other:?}"),
    }
}

fn assert_same(a: &LoroDoc, b: &LoroDoc) {
    assert_eq!(a.oplog_vv(), b.oplog_vv());
    assert_eq!(a.get_deep_value().to_json(), b.get_deep_value().to_json());
}

#[test]
fn honest_bytes_within_the_budget_import_like_plain_import() {
    let source = rich_doc();
    let exports = [
        source.export(ExportMode::all_updates()).unwrap(),
        source.export(ExportMode::Snapshot).unwrap(),
    ];
    for bytes in exports {
        for detached in [false, true] {
            let plain = LoroDoc::new();
            let budgeted = LoroDoc::new();
            if detached {
                plain.detach();
                budgeted.detach();
            }
            plain.import(&bytes).unwrap();
            let status = budgeted.import_with_budget(&bytes, plenty()).unwrap();
            assert!(status.pending.is_none());
            plain.checkout_to_latest();
            budgeted.checkout_to_latest();
            assert_same(&plain, &budgeted);
            assert_same(&source, &budgeted);
        }
    }
}

#[test]
fn incremental_updates_and_snapshots_into_a_non_empty_doc_import_within_the_budget() {
    let source = rich_doc();
    let receiver = LoroDoc::new();
    receiver.set_peer_id(9).unwrap();
    receiver
        .import_with_budget(&source.export(ExportMode::Snapshot).unwrap(), plenty())
        .unwrap();
    let held = receiver.oplog_vv();
    source.get_text("text").insert(0, "later ").unwrap();
    source.get_tree("tree").create(None).unwrap();
    source.commit();
    receiver
        .import_with_budget(
            &source.export(ExportMode::updates(&held)).unwrap(),
            plenty(),
        )
        .unwrap();
    assert_same(&source, &receiver);

    // A full snapshot into a document that already has history goes
    // through the update path.
    let other = LoroDoc::new();
    other.set_peer_id(10).unwrap();
    other.get_text("text").insert(0, "x").unwrap();
    other.commit();
    other
        .import_with_budget(&source.export(ExportMode::Snapshot).unwrap(), plenty())
        .unwrap();
    assert_eq!(
        other.oplog_vv().get(&1),
        source.oplog_vv().get(&1),
        "the snapshot's history is imported"
    );
}

/// The smallest `decompressed_bytes` with which `bytes` import.
fn least_decompressed_bytes(bytes: &[u8], changes: usize, ops: usize) -> usize {
    let fits = |decompressed_bytes| {
        LoroDoc::new()
            .import_with_budget(
                bytes,
                ImportBudget {
                    changes,
                    ops,
                    decompressed_bytes,
                },
            )
            .is_ok()
    };
    let (mut lo, mut hi) = (0, 1 << 26);
    assert!(fits(hi));
    while lo < hi {
        let mid = (lo + hi) / 2;
        if fits(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

#[test]
fn the_exact_budget_passes_and_one_less_fails() {
    let source = rich_doc();
    let changes = source.len_changes();
    let ops = atoms(&source);
    for bytes in [
        source.export(ExportMode::all_updates()).unwrap(),
        source.export(ExportMode::Snapshot).unwrap(),
    ] {
        let decompressed_bytes = least_decompressed_bytes(&bytes, changes, ops);
        assert!(decompressed_bytes > 0, "tree positions count");
        let exact = ImportBudget {
            changes,
            ops,
            decompressed_bytes,
        };
        let doc = LoroDoc::new();
        doc.import_with_budget(&bytes, exact).unwrap();
        assert_same(&source, &doc);

        for less in [
            ImportBudget {
                changes: changes - 1,
                ..exact
            },
            ImportBudget {
                ops: ops - 1,
                ..exact
            },
            ImportBudget {
                decompressed_bytes: decompressed_bytes - 1,
                ..exact
            },
        ] {
            let doc = LoroDoc::new();
            assert_budget_error(doc.import_with_budget(&bytes, less));
            assert!(doc.oplog_vv().is_empty(), "nothing is imported");
        }
    }
}

#[test]
fn a_refused_import_leaves_the_document_usable() {
    let source = rich_doc();
    let doc = LoroDoc::new();
    doc.set_peer_id(7).unwrap();
    doc.get_text("text").insert(0, "mine ").unwrap();
    doc.commit();
    let before_vv = doc.oplog_vv();
    let before_value = doc.get_deep_value().to_json();
    let tight = ImportBudget {
        changes: 1,
        ops: 1,
        decompressed_bytes: 1,
    };
    for bytes in [
        source.export(ExportMode::all_updates()).unwrap(),
        source.export(ExportMode::Snapshot).unwrap(),
    ] {
        assert_budget_error(doc.import_with_budget(&bytes, tight));
        assert_eq!(doc.oplog_vv(), before_vv);
        assert_eq!(doc.get_deep_value().to_json(), before_value);
    }

    let empty = LoroDoc::new();
    assert_budget_error(
        empty.import_with_budget(&source.export(ExportMode::Snapshot).unwrap(), tight),
    );
    assert!(empty.oplog_vv().is_empty());

    // Both documents still take edits and honest imports.
    for doc in [&doc, &empty] {
        doc.get_map("after").insert("ok", true).unwrap();
        doc.commit();
        doc.import(&source.export(ExportMode::all_updates()).unwrap())
            .unwrap();
        for (peer, counter) in source.oplog_vv().iter() {
            assert_eq!(doc.oplog_vv().get(peer), Some(counter));
        }
    }
}

#[test]
fn shallow_snapshots_are_refused_under_a_budget() {
    let source = rich_doc();
    let bytes = source
        .export(ExportMode::shallow_snapshot(&source.oplog_frontiers()))
        .unwrap();
    let doc = LoroDoc::new();
    assert!(doc.import_with_budget(&bytes, plenty()).is_err());
    assert!(doc.oplog_vv().is_empty());
    doc.import(&bytes).unwrap();
}

#[test]
fn blob_meta_respects_the_budget() {
    let source = rich_doc();
    let changes = source.len_changes();
    let ops = atoms(&source);
    for bytes in [
        source.export(ExportMode::all_updates()).unwrap(),
        source.export(ExportMode::Snapshot).unwrap(),
    ] {
        let plain = LoroDoc::decode_import_blob_meta(&bytes, true).unwrap();
        let budgeted =
            LoroDoc::decode_import_blob_meta_with_budget(&bytes, true, plenty()).unwrap();
        assert_eq!(plain.partial_end_vv, budgeted.partial_end_vv);
        assert_eq!(plain.change_num, budgeted.change_num);
        let tight = ImportBudget {
            changes: changes - 1,
            ops,
            decompressed_bytes: 1 << 26,
        };
        match LoroDoc::decode_import_blob_meta_with_budget(&bytes, true, tight) {
            Err(LoroError::DecodeError(msg)) => assert!(msg.starts_with(IMPORT_BUDGET_EXCEEDED)),
            other => panic!("expected a budget error, got {other:?}"),
        }
    }
}

/// The budget counts what a call decodes, not what is new to the document:
/// changes the document already holds count too.
#[test]
fn the_budget_counts_every_decoded_change() {
    let source = rich_doc();
    let doc = LoroDoc::new();
    let all = source.export(ExportMode::all_updates()).unwrap();
    doc.import(&all).unwrap();
    let exact = ImportBudget {
        changes: source.len_changes(),
        ops: atoms(&source),
        decompressed_bytes: 1 << 26,
    };
    doc.import_with_budget(&all, exact).unwrap();
    assert_budget_error(doc.import_with_budget(
        &all,
        ImportBudget {
            changes: 1,
            ..exact
        },
    ));
    let since = source
        .export(ExportMode::updates(&VersionVector::default()))
        .unwrap();
    doc.import_with_budget(&since, exact).unwrap();
    assert_same(&source, &doc);
}
