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

#[test]
fn scope_api_checks_then_commits_or_undoes() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    guard(&doc);
    let vv = doc.oplog_vv();
    let scope = doc.begin_checked().unwrap();
    assert!(doc.begin_checked().is_err());
    doc.get_map("m").insert("forbidden", 1).unwrap();
    assert!(!doc.check_checked(&scope, "app").unwrap());
    doc.end_checked(scope, "app", false).unwrap();
    assert_eq!(doc.oplog_vv(), vv);
    assert_eq!(doc.get_pending_txn_len(), 0);

    let scope = doc.begin_checked().unwrap();
    doc.get_map("m").insert("ok", 1).unwrap();
    assert!(doc.check_checked(&scope, "app").unwrap());
    doc.end_checked(scope, "app", true).unwrap();
    assert_eq!(doc.get_pending_txn_len(), 0);
    assert!(doc.get_map("m").get("ok").is_some());
}

/// A commit inside the scope, on its own thread, is refused in every build: an
/// explicit commit leaves the ops pending; import, snapshot export, checkout and
/// undo fail; an update export exports the committed ops only. The scope stays
/// intact and commits checked.
#[test]
fn a_commit_inside_the_scope_is_refused() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    doc.get_map("m").insert("before", 1).unwrap();
    doc.commit();
    let mut undo = UndoManager::new(&doc);
    doc.get_map("m").insert("u", 1).unwrap();
    doc.commit();
    let v1 = doc.state_frontiers();
    guard(&doc);
    let other = LoroDoc::new();
    other.get_map("o").insert("k", 1).unwrap();
    let bytes = other.export(ExportMode::all_updates()).unwrap();

    let scope = doc.begin_checked().unwrap();
    doc.get_map("m").insert("x", 2).unwrap();
    doc.commit();
    assert_eq!(doc.get_pending_txn_len(), 1);
    assert!(doc.import(&bytes).is_err());
    assert!(doc.export(ExportMode::Snapshot).is_err());
    assert!(doc.checkout(&v1).is_err());
    assert!(undo.undo().is_err());
    let peer = LoroDoc::new();
    peer.import(&doc.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    assert!(peer.get_map("m").get("x").is_none());
    assert_eq!(doc.get_pending_txn_len(), 1);

    assert!(doc.check_checked(&scope, "app").unwrap());
    doc.end_checked(scope, "app", true).unwrap();
    assert_eq!(doc.get_pending_txn_len(), 0);
    assert!(doc.get_map("m").get("x").is_some());
    // The import works again once the scope ended.
    doc.import(&bytes).unwrap();
}

/// An implicit commit that cannot return an error panics inside a scope of its
/// own thread, in release builds too; the ops are not committed.
#[test]
fn an_implicit_commit_inside_the_scope_panics() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    let vv = doc.oplog_vv();
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = doc.commit_checked("app", || -> Result<(), LoroError> {
            doc.get_map("m").insert("x", 1)?;
            let _ = doc.fork();
            Ok(())
        });
    }));
    assert!(out.is_err());
    assert_eq!(doc.oplog_vv(), vv);
    assert!(doc.get_map("m").get("x").is_none());
}

/// A scope continued on another thread (an `.await` inside it) fails closed:
/// its check and its end fail there, and its ops are undone.
#[test]
fn a_scope_moved_to_another_thread_fails() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    guard(&doc);
    let vv = doc.oplog_vv();
    let scope = doc.begin_checked().unwrap();
    doc.get_map("m").insert("x", 1).unwrap();
    let moved = doc.clone();
    std::thread::spawn(move || {
        assert!(moved.check_checked(&scope, "app").is_err());
        assert!(moved.end_checked(scope, "app", true).is_err());
    })
    .join()
    .unwrap();
    assert_eq!(doc.oplog_vv(), vv);
    assert!(doc.get_map("m").get("x").is_none());
    assert_eq!(doc.get_pending_txn_len(), 0);
    doc.get_map("m").insert("y", 1).unwrap();
    doc.commit();
    assert_ne!(doc.oplog_vv(), vv);
}

/// Scopes begin and end on one thread while another exports updates: the
/// export decides under one guard whether to commit, so it never waits for a
/// scope, however long the scope runs.
#[test]
fn an_update_export_never_waits_for_a_scope() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    let held = std::time::Duration::from_millis(100);
    let stop = Arc::new(AtomicBool::new(false));
    let exporter = {
        let (doc, stop) = (doc.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut slowest = std::time::Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                let start = Instant::now();
                doc.export(ExportMode::all_updates()).unwrap();
                slowest = slowest.max(start.elapsed());
            }
            slowest
        })
    };
    for i in 0..20 {
        let scope = doc.begin_checked().unwrap();
        doc.get_map("m").insert("k", i).unwrap();
        std::thread::sleep(held);
        doc.end_checked(scope, "app", true).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let slowest = exporter.join().unwrap();
    assert!(slowest < held / 2, "{slowest:?}");
}

/// Another thread that writes and commits while scopes run: its ops never join
/// a scope, so every scope stays intact, every refusal undoes all its ops, and
/// no refused op is ever committed.
#[test]
fn scope_check_races_a_commit_of_another_thread() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    guard(&doc);
    let stop = Arc::new(AtomicBool::new(false));
    let other = {
        let (doc, stop) = (doc.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                doc.get_map("other").insert("k", i).unwrap();
                doc.commit();
                i += 1;
            }
            i
        })
    };
    for i in 0..5_000 {
        let scope = doc.begin_checked().unwrap();
        let key = if i % 2 == 0 { "ok" } else { "forbidden" };
        doc.get_map("m").insert(key, i).unwrap();
        let pass = doc.check_checked(&scope, "app").unwrap();
        assert_eq!(pass, i % 2 == 0);
        doc.end_checked(scope, "app", pass).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    assert!(other.join().unwrap() > 0);
    doc.commit();
    let peer = LoroDoc::new();
    peer.import(&doc.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    assert!(peer.get_map("m").get("forbidden").is_none());
    assert!(peer.get_map("m").get("ok").is_some());
}

/// A scope held longer than any wait: an update export of another thread
/// exports the committed ops only, a snapshot export and a commit wait for the
/// scope's end, a write fails after 5 s. The refused op never lands.
#[test]
fn a_long_scope_is_never_committed_by_another_thread() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    doc.get_map("m").insert("before", 1).unwrap();
    doc.commit();
    guard(&doc);
    let scope = doc.begin_checked().unwrap();
    doc.get_map("m").insert("forbidden", 1).unwrap();
    let held = std::time::Duration::from_secs(6);
    let start = Instant::now();
    let spawn = |f: fn(&LoroDoc) -> Vec<u8>| {
        let doc = doc.clone();
        std::thread::spawn(move || (f(&doc), start.elapsed()))
    };
    let updates = spawn(|d| d.export(ExportMode::all_updates()).unwrap());
    let snapshot = spawn(|d| d.export(ExportMode::Snapshot).unwrap());
    let commit = spawn(|d| {
        d.commit();
        Vec::new()
    });
    let write = {
        let doc = doc.clone();
        std::thread::spawn(move || (doc.get_map("m").insert("other", 1), start.elapsed()))
    };
    // The update export does not wait, and has no op of the scope.
    let (bytes, took) = updates.join().unwrap();
    assert!(took < held, "{took:?}");
    let peer = LoroDoc::new();
    peer.import(&bytes).unwrap();
    assert!(peer.get_map("m").get("before").is_some());
    assert!(peer.get_map("m").get("forbidden").is_none());
    // The write waits 5 s, then fails, and joins no scope.
    let (out, took) = write.join().unwrap();
    assert!(out.is_err() && took >= std::time::Duration::from_secs(5));
    while start.elapsed() < held {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!doc.check_checked(&scope, "app").unwrap());
    doc.end_checked(scope, "app", false).unwrap();
    // The snapshot export and the commit waited for the end.
    for t in [snapshot, commit] {
        let (_, took) = t.join().unwrap();
        assert!(took >= held, "{took:?}");
    }
    let snap = LoroDoc::new();
    snap.import(&doc.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    assert!(snap.get_map("m").get("forbidden").is_none());
    assert!(doc.get_map("m").get("forbidden").is_none());
    assert!(doc.get_map("m").get("other").is_none());
}

/// A panic in the closure of `commit_checked` undoes its ops; the doc goes on.
#[test]
fn panic_in_commit_checked_undoes_its_ops() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    let vv = doc.oplog_vv();
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = doc.commit_checked("app", || -> Result<(), LoroError> {
            doc.get_map("m").insert("x", 1)?;
            panic!("boom");
        });
    }));
    assert!(out.is_err());
    assert_eq!(doc.oplog_vv(), vv);
    assert_eq!(doc.get_pending_txn_len(), 0);
    let scope = doc.begin_checked().unwrap();
    doc.end_checked(scope, "app", true).unwrap();
}

/// Ops added after the check are not committed with the checked ones.
#[test]
fn ops_after_the_check_refuse_the_commit() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    guard(&doc);
    let vv = doc.oplog_vv();
    let scope = doc.begin_checked().unwrap();
    doc.get_map("m").insert("ok", 1).unwrap();
    assert!(doc.check_checked(&scope, "app").unwrap());
    doc.get_map("m").insert("forbidden", 1).unwrap();
    assert!(doc.end_checked(scope, "app", true).is_err());
    assert_eq!(doc.oplog_vv(), vv);
    assert!(doc.get_map("m").get("forbidden").is_none());
    assert_eq!(doc.get_pending_txn_len(), 0);
}

/// An export of another thread commits implicitly. It waits for a running
/// checked commit, so the check sees every op and a refusal undoes them.
#[test]
fn export_of_another_thread_waits_for_the_scope() {
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    guard(&doc);
    let stop = Arc::new(AtomicBool::new(false));
    let other = {
        let (doc, stop) = (doc.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                doc.export(ExportMode::all_updates()).unwrap();
            }
        })
    };
    for i in 0..1_000 {
        let scope = doc.begin_checked().unwrap();
        let key = if i % 2 == 0 { "ok" } else { "forbidden" };
        doc.get_map("m").insert(key, i).unwrap();
        let pass = doc.check_checked(&scope, "app").unwrap();
        assert_eq!(pass, i % 2 == 0);
        doc.end_checked(scope, "app", pass).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    other.join().unwrap();
    assert!(doc.get_map("m").get("forbidden").is_none());
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
