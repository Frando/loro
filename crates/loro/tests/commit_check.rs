//! `LoroDoc::commit_checked` and `set_commit_check`: a refused local commit
//! becomes no change, for plain edits and for undo.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use loro::op_view::OpContentView;
use loro::{ExportMode, LoroDoc, LoroError, LoroValue, UndoManager};

/// A check that refuses any map set of key "forbidden", and counts what it saw.
fn guard(doc: &LoroDoc) -> Arc<AtomicBool> {
    let saw = Arc::new(AtomicBool::new(false));
    let seen = saw.clone();
    doc.set_commit_check(Some(Arc::new(move |c| {
        seen.store(true, Ordering::Relaxed);
        let mut ok = true;
        assert!(c.visit_ops(&mut |op| {
            if let OpContentView::MapSet { key, .. } = &op.content {
                ok &= *key != "forbidden";
            }
        }));
        ok
    })));
    saw
}

#[test]
fn refused_edit_becomes_no_change() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    doc.get_text("t").insert(0, "hi").unwrap();
    doc.commit();
    let saw = guard(&doc);
    let before = doc.export(ExportMode::Snapshot).unwrap();
    let vv = doc.oplog_vv();
    let value = doc.get_deep_value();

    let out = doc
        .commit_checked("app", || -> Result<(), LoroError> {
            doc.get_text("t").insert(2, " there")?;
            doc.get_map("new_root").insert("forbidden", 1)?;
            Ok(())
        })
        .unwrap();
    assert!(out.is_none());
    assert!(saw.load(Ordering::Relaxed));
    assert_eq!(doc.oplog_vv(), vv);
    assert_eq!(doc.get_deep_value(), value);
    assert_eq!(doc.export(ExportMode::Snapshot).unwrap(), before);

    // The next edit reuses the counters; a peer gets a consistent history.
    let out = doc
        .commit_checked("app", || -> Result<(), LoroError> {
            doc.get_map("m").insert("ok", 1)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(out, Some(()));
    let peer = LoroDoc::new();
    peer.import(&doc.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    assert_eq!(peer.get_deep_value(), doc.get_deep_value());
    assert_eq!(
        doc.get_map("m").get("ok").unwrap().into_value().unwrap(),
        LoroValue::I64(1)
    );
}

#[test]
fn failed_closure_rolls_back_too() {
    let doc = LoroDoc::new();
    let vv = doc.oplog_vv();
    let out: Result<Option<()>, LoroError> = doc.commit_checked("app", || {
        doc.get_list("l").push(1)?;
        Err(LoroError::CommitRefused)
    });
    assert!(out.is_err());
    assert_eq!(doc.oplog_vv(), vv);
    assert!(doc.get_list("l").is_empty());
}

#[test]
fn refused_undo_keeps_its_item() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    let mut undo = UndoManager::new(&doc);
    let map = doc.get_map("m");
    map.insert("forbidden", 1).unwrap();
    doc.commit();
    // Undoing it deletes the key, which is a map op on "forbidden" too.
    guard(&doc);
    let vv = doc.oplog_vv();
    let err = undo.undo().unwrap_err();
    assert_eq!(err, LoroError::CommitRefused);
    assert_eq!(doc.oplog_vv(), vv);
    assert_eq!(
        map.get("forbidden").unwrap().into_value().unwrap(),
        1.into()
    );
    assert!(undo.can_undo());
    // Allowed again, the same item undoes.
    doc.set_commit_check(None);
    assert!(undo.undo().unwrap());
    assert!(map.get("forbidden").is_none());
}

/// Cost per commit, printed; run with `--nocapture`.
#[test]
fn cost_per_commit() {
    const N: usize = 2000;
    let run = |checked: bool, check: bool| {
        let doc = LoroDoc::new();
        doc.set_peer_id(1).unwrap();
        // A page of some size: the cost must not grow with it.
        let text = doc.get_text("t");
        text.insert(0, &"x".repeat(100_000)).unwrap();
        for i in 0..2000 {
            doc.get_map("m").insert(&i.to_string(), i).unwrap();
        }
        doc.commit();
        if check {
            guard(&doc);
        }
        let t = Instant::now();
        for i in 0..N {
            if checked {
                doc.commit_checked("app", || text.insert(i, "a").map(|_| ()))
                    .unwrap();
            } else {
                text.insert(i, "a").unwrap();
                doc.commit();
            }
        }
        t.elapsed().as_secs_f64() * 1e6 / N as f64
    };
    let plain = run(false, false);
    let scoped = run(true, false);
    let checked = run(true, true);
    let doc = LoroDoc::new();
    let text = doc.get_text("t");
    let t = Instant::now();
    for i in 0..200 {
        let _ = doc.commit_checked("app", || -> Result<(), LoroError> {
            text.insert(0, "a")?;
            doc.get_map("m").insert("forbidden", i)?;
            Ok(())
        });
    }
    let _ = guard(&doc);
    let refused = {
        let t = Instant::now();
        for i in 0..200 {
            let _ = doc.commit_checked("app", || -> Result<(), LoroError> {
                text.insert(0, "a")?;
                doc.get_map("m").insert("forbidden", i)?;
                Ok(())
            });
        }
        t.elapsed().as_secs_f64() * 1e6 / 200.0
    };
    let _ = t;
    println!(
        "us per commit: plain {plain:.1}, scoped {scoped:.1}, checked {checked:.1}, refused {refused:.1}"
    );
}
