//! Imports after a snapshot import whose change blocks are loaded lazily.

use loro::{ExportMode, LoroDoc, ID};

/// A peer's history spans several change blocks. After a snapshot import,
/// reading an early change loads only an early block of that peer. A later
/// update of the peer whose only dep is another peer's change (so the DAG
/// does not load the peer's last block first) must still be appended after
/// the peer's last block.
#[test]
fn update_after_reading_an_early_change_of_a_lazy_snapshot() {
    let a = LoroDoc::new();
    a.set_peer_id(1).unwrap();
    a.set_change_merge_interval(0);
    let text = a.get_text("text");
    for i in 0..400 {
        text.insert(0, &format!("change {i} with some text to fill blocks"))
            .unwrap();
        a.commit();
    }
    let snapshot = a.export(ExportMode::Snapshot).unwrap();

    let b = LoroDoc::new();
    b.set_peer_id(2).unwrap();
    b.import(&a.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    b.get_map("m").insert("b", 1).unwrap();
    b.commit();
    let b_update = b.export(ExportMode::updates(&a.oplog_vv())).unwrap();
    a.import(&b_update).unwrap();
    let a_vv = a.oplog_vv();
    a.get_map("m").insert("a", 2).unwrap();
    a.commit();
    let a_update = a.export(ExportMode::updates(&a_vv)).unwrap();

    let snapshot_with_b = {
        let x = LoroDoc::new();
        x.import(&snapshot).unwrap();
        x.import(&b_update).unwrap();
        x.export(ExportMode::Snapshot).unwrap()
    };

    for (snapshot, with_b) in [(&snapshot, false), (&snapshot_with_b, true)] {
        for read_early in [false, true] {
            let c = LoroDoc::new();
            c.import(snapshot).unwrap();
            if read_early {
                assert!(c.get_change(ID::new(1, 0)).is_some());
            }
            if !with_b {
                c.import(&b_update).unwrap();
            }
            c.import(&a_update).unwrap();
            assert_eq!(c.get_deep_value(), a.get_deep_value());
            assert_eq!(c.oplog_vv(), a.oplog_vv());
            let again = LoroDoc::new();
            again
                .import(&c.export(ExportMode::Snapshot).unwrap())
                .unwrap();
            assert_eq!(again.get_deep_value(), a.get_deep_value());
        }
    }
}
