//! Honest tree positions stay under the position arena's expansion
//! bound (`MAX_POSITION_EXPANSION` in `loro-internal`'s `encoding/arena.rs`).

use loro::{ExportMode, LoroDoc, TreeParentId};

#[test]
fn inserts_into_one_gap_import_from_updates_and_snapshot() {
    let doc = LoroDoc::new();
    let tree = doc.get_tree("t");
    tree.enable_fractional_index(0);
    tree.create(TreeParentId::Root).unwrap();
    tree.create(TreeParentId::Root).unwrap();
    for i in 0..20_000 {
        tree.create_at(TreeParentId::Root, 1).unwrap();
        if i % 100 == 0 {
            doc.commit();
        }
    }
    doc.commit();
    for mode in [ExportMode::all_updates(), ExportMode::Snapshot] {
        let bytes = doc.export(mode).unwrap();
        let other = LoroDoc::new();
        other.import(&bytes).unwrap();
        assert_eq!(other.get_deep_value(), doc.get_deep_value());
    }
}
