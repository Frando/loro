#![allow(unexpected_cfgs)]
use bytes::Bytes;
use loro_kv_store::{mem_store::MemKvConfig, MemKvStore};

#[ctor::ctor]
fn init() {
    dev_utils::setup_test_log();
}

#[test]
fn add_and_remove() {
    let key = &[0];
    let value = Bytes::from_static(&[0]);
    let mut store = MemKvStore::new(MemKvConfig::default());
    store.set(key, value.clone());
    assert_eq!(store.get(key), Some(value));
    store.remove(key);
    assert_eq!(store.get(key), None);
}

#[test]
fn add_flush_remove() {
    let key = &[0];
    let value = Bytes::from_static(&[0]);
    let mut store = MemKvStore::new(MemKvConfig::default());
    store.set(key, value.clone());
    store.export_all();
    store.remove(key);
    assert_eq!(store.get(key), None);
}

#[test]
fn add_flush_add_scan() {
    let key1 = &[0];
    let value1 = Bytes::from_static(&[0]);
    let key2 = &[128];
    let value2 = Bytes::from_static(&[252, 169]);
    let mut store = MemKvStore::new(MemKvConfig::new().should_encode_none(true));
    store.set(key1, value1.clone());
    store.export_all();
    ensure_cov::assert_cov("kv_store::block::NormalBlock::encode::compress_fallback");
    store.set(key2, value2.clone());
    {
        let mut iter = store.scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded);
        assert_eq!(
            iter.next(),
            Some((Bytes::from_static(key1), value1.clone()))
        );
        assert_eq!(
            iter.next(),
            Some((Bytes::from_static(key2), value2.clone()))
        );
        assert_eq!(iter.next(), None);

        let mut iter = store
            .scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
            .rev();
        assert_eq!(
            iter.next(),
            Some((Bytes::from_static(key2), value2.clone()))
        );
        assert_eq!(
            iter.next(),
            Some((Bytes::from_static(key1), value1.clone()))
        );
        assert_eq!(iter.next(), None);
    }

    let bytes = store.export_all();
    let mut store = MemKvStore::new(MemKvConfig::new());
    store.import_all(bytes).unwrap();
    let mut iter = store.scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded);
    assert_eq!(
        iter.next(),
        Some((Bytes::from_static(key1), value1.clone()))
    );
    assert_eq!(
        iter.next(),
        Some((Bytes::from_static(key2), value2.clone()))
    );
    assert_eq!(iter.next(), None);
}

#[test]
fn large_value() {
    use rand::Rng;
    let key = &[0];
    let mut rng = rand::thread_rng();
    let large_value: Vec<u8> = (0..100_000).map(|_| rng.gen()).collect();
    let large_value = Bytes::from(large_value);

    let mut store = MemKvStore::new(MemKvConfig::new());
    store.set(key, large_value.clone());

    let bytes = store.export_all();
    ensure_cov::assert_cov("kv_store::block::LargeValueBlock::encode::compress_fallback");
    let mut imported_store = MemKvStore::new(MemKvConfig::new());
    imported_store.import_all(bytes).unwrap();

    let retrieved_value = imported_store.get(key).unwrap();
    assert_eq!(retrieved_value, large_value);

    let mut iter = imported_store.scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded);
    assert_eq!(iter.next(), Some((Bytes::from_static(key), large_value)));
    assert_eq!(iter.next(), None);
}

#[test]
fn export_import_edit_export() {
    let mut store = MemKvStore::new(MemKvConfig::default());
    store.set(b"a", Bytes::from_static(b"1"));
    for i in 0..3000 {
        let s = format!("b{i}");
        store.set(s.as_bytes(), Bytes::from_static(b"2"));
    }

    let bytes = store.export_all();
    let mut new_store = MemKvStore::new(MemKvConfig::default());
    new_store.import_all(bytes).unwrap();
    new_store.set(b"a", Bytes::from_static(b"2"));
    assert_eq!(new_store.get(b"b0"), Some(Bytes::from_static(b"2")));
    assert_eq!(new_store.get(b"b1001"), Some(Bytes::from_static(b"2")));
    assert_eq!(new_store.get(b"b2999"), Some(Bytes::from_static(b"2")));

    let bytes = new_store.export_all();
    let mut new_new_store = MemKvStore::new(MemKvConfig::default());
    new_new_store.import_all(bytes).unwrap();
    assert_eq!(new_new_store.get(b"b0"), Some(Bytes::from_static(b"2")));
    assert_eq!(new_new_store.get(b"b1001"), Some(Bytes::from_static(b"2")));
    assert_eq!(new_new_store.get(b"b2999"), Some(Bytes::from_static(b"2")));
    assert_eq!(new_new_store.get(b"b99"), Some(Bytes::from_static(b"2")));
    assert_eq!(new_new_store.get(b"a"), Some(Bytes::from_static(b"2")));
}

/// A block whose checksum matches but whose body does not decode is an
/// import error. Blocks are read lazily, and a lazy read of such a block
/// used to panic.
#[test]
fn import_rejects_an_undecodable_block_with_a_valid_checksum() {
    use loro_kv_store::compress::CompressionType;
    use std::ops::Bound;

    let mut store = MemKvStore::new(MemKvConfig::default().compression_type(CompressionType::None));
    store.set(b"a", Bytes::from_static(b"1"));
    store.set(b"b", Bytes::from_static(b"2"));
    let mut bytes = store.export_all().to_vec();

    // One block: it starts after the magic number and the schema version and
    // ends at the meta offset, the last u32 of the table.
    let n = bytes.len();
    let meta_offset = u32::from_le_bytes(bytes[n - 4..].try_into().unwrap()) as usize;
    let (start, end) = (5, meta_offset);
    // The block body ends with the number of entries (u16), then the checksum.
    bytes[end - 6..end - 4].copy_from_slice(&0u16.to_le_bytes());
    let checksum = xxhash_rust::xxh32::xxh32(&bytes[start..end - 4], u32::from_le_bytes(*b"LORO"));
    bytes[end - 4..end].copy_from_slice(&checksum.to_le_bytes());

    let mut imported = MemKvStore::new(MemKvConfig::default());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ok = imported.import_all(Bytes::from(bytes)).is_ok();
        let _ = imported.scan(Bound::Unbounded, Bound::Unbounded).count();
        ok
    }));
    assert_eq!(result.ok(), Some(false));
}
