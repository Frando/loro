//! First sync of many small record documents: the receiver's Loro work
//! per document (a new document, import with state, drop).
//!
//! `cargo run --release --example small_docs -- [N] [MODE]`, MODE
//! `full` (default), `detached`, `meta`.
use loro::{ExportMode, LoroDoc, LoroMap};
use std::time::Instant;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn text(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| (b'a' + self.below(26) as u8) as char)
            .collect()
    }
}

/// One record document as the bench `session_tables` (layout `bq`,
/// shape `small`, `w = 1.5`) makes it: a creating session, and half of
/// the time an edit of the title by a second session.
fn record(d: u64) -> Vec<u8> {
    let mut r = Rng(d * 7919 + 1);
    let doc = LoroDoc::new();
    doc.set_peer_id(1000 + r.below(5000)).unwrap();
    let map = doc
        .get_map("records")
        .insert_container(&format!("r{d}"), LoroMap::new())
        .unwrap();
    map.insert("title", r.text(20)).unwrap();
    map.insert("created_at", 1_700_000_000_000i64 + d as i64)
        .unwrap();
    map.insert("prop00xxxxxxxxxxxxxxxxxxxx", r.text(15))
        .unwrap();
    map.insert("prop01xxxxxxxxxxxxxxxxxxxx", r.below(1000) as f64)
        .unwrap();
    map.insert("prop02xxxxxxxxxxxxxxxxxxxx", r.below(2) == 0)
        .unwrap();
    doc.commit();
    if d % 2 == 1 {
        doc.set_peer_id(9000 + r.below(5000)).unwrap();
        map.insert("title", r.text(20)).unwrap();
        doc.commit();
    }
    doc.export(ExportMode::all_updates()).unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args.get(1).map_or(100_000, |v| v.parse().unwrap());
    let mode = args.get(2).map_or("full", |s| s.as_str());
    let reps: usize = args.get(3).map_or(3, |v| v.parse().unwrap());
    let blobs: Vec<Vec<u8>> = (0..n).map(record).collect();
    let bytes: usize = blobs.iter().map(|b| b.len()).sum();
    println!("docs {n}, mean blob {} B", bytes as u64 / n);
    let mut best = f64::MAX;
    for _ in 0..reps {
        let t = Instant::now();
        let mut check = 0usize;
        let mut split = [0.0f64; 4];
        for b in &blobs {
            match mode {
                "meta" => {
                    let m = LoroDoc::decode_import_blob_meta(b, false).unwrap();
                    check += m.partial_end_vv.len();
                }
                "split" => {
                    let t0 = Instant::now();
                    let doc = LoroDoc::new();
                    let t1 = Instant::now();
                    doc.import(b).unwrap();
                    let t2 = Instant::now();
                    check += doc.oplog_vv().len();
                    let t3 = Instant::now();
                    drop(doc);
                    let t4 = Instant::now();
                    for (i, (a, z)) in [(t0, t1), (t1, t2), (t2, t3), (t3, t4)].iter().enumerate() {
                        split[i] += (*z - *a).as_secs_f64();
                    }
                }
                "new" => {
                    let doc = LoroDoc::new();
                    check += doc.peer_id() as usize & 1;
                }
                _ => {
                    let doc = LoroDoc::new();
                    if mode == "detached" {
                        doc.detach();
                    }
                    doc.import(b).unwrap();
                    if mode == "value" {
                        check += format!("{:?}", doc.get_deep_value()).len();
                    }
                    check += doc.oplog_vv().len();
                }
            }
        }
        std::hint::black_box(check);
        if mode == "split" {
            println!(
                "new {:.2} import {:.2} vv {:.2} drop {:.2} us",
                split[0] * 1e6 / n as f64,
                split[1] * 1e6 / n as f64,
                split[2] * 1e6 / n as f64,
                split[3] * 1e6 / n as f64
            );
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!(
        "{mode}: {:.2} us per document ({:.3} s for {n})",
        best * 1e6 / n as f64,
        best
    );
}
