//! Read-back diagnostic for a `.brdb` world — the canonical "why won't it load"
//! probe. Prints brick count, spawn points, procedural size + position bounds so
//! a bad export (only-spawn, a needle, or oversized bricks) is obvious.
//!
//! Usage: `cargo run --example inspect_brdb -- <path-to.brdb>`

use brdb::{BrickType, IntoReader};

fn main() {
    let path = std::env::args().nth(1).expect("usage: inspect_brdb <path-to.brdb>");
    let db = brdb::Brdb::open(&path).expect("open brdb").into_reader();
    let gd = db.global_data().expect("global data");

    println!("file: {path}");

    // Chunk distribution FIRST (brick count comes from the chunk metadata, no
    // per-brick decode needed) — a healthy save spreads bricks across many small
    // spatial chunks; dense/monster chunks choke the loader. Computing this from
    // num_bricks means it works even on newer-format saves our brdb 0.4 reader
    // cannot fully decode.
    let chunks = db.brick_chunk_index(1).expect("chunk index");
    let chunk_total: u64 = chunks.iter().map(|c| u64::from(c.num_bricks)).sum();
    println!("total bricks (from chunk index): {chunk_total}");

    // Full per-brick decode for geometry/spawn stats — degrades gracefully on a
    // newer save format this reader version can't parse.
    let mut bricks = Vec::new();
    let mut decode_ok = true;
    'outer: for chunk in &chunks {
        let soa = match db.brick_chunk_soa(1, chunk.index) {
            Ok(s) => s,
            Err(_) => { decode_ok = false; break; }
        };
        for b in soa.iter_bricks(chunk.index, gd.clone()) {
            match b {
                Ok(brick) => bricks.push(brick),
                Err(_) => { decode_ok = false; break 'outer; }
            }
        }
    }
    if !decode_ok {
        println!("(geometry decode unsupported for this save format — chunk stats only)");
    }

    let mut counts: Vec<u32> = chunks.iter().map(|c| c.num_bricks).collect();
    counts.sort_unstable();
    let n = counts.len();
    let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
    let max = counts.last().copied().unwrap_or(0);
    let median = counts.get(n / 2).copied().unwrap_or(0);
    println!(
        "chunks (grid 1): {n}  |  bricks/chunk: max {max}, median {median}, avg {:.0}",
        if n > 0 { total as f64 / n as f64 } else { 0.0 }
    );

    let spawns: Vec<_> = bricks
        .iter()
        .filter(|b| format!("{:?}", b.asset).contains("SpawnPoint"))
        .collect();
    println!("spawn points: {}", spawns.len());
    for s in &spawns {
        println!("  spawn @ ({}, {}, {})", s.position.x, s.position.y, s.position.z);
    }

    // Procedural size + position bounds across the terrain bricks.
    let mut sx = (u16::MAX, 0u16);
    let mut sy = (u16::MAX, 0u16);
    let mut sz = (u16::MAX, 0u16);
    let mut px = (i32::MAX, i32::MIN);
    let mut py = (i32::MAX, i32::MIN);
    let mut pz = (i32::MAX, i32::MIN);
    let mut assets = std::collections::BTreeMap::<String, usize>::new();
    let mut oversized = 0usize; // bricks with any axis > 500 units (the suspected cap)
    let mut terrain = 0usize;

    for b in &bricks {
        if format!("{:?}", b.asset).contains("SpawnPoint") {
            continue;
        }
        terrain += 1;
        if let BrickType::Procedural { size, asset } = &b.asset {
            *assets.entry(format!("{asset:?}")).or_default() += 1;
            sx = (sx.0.min(size.x), sx.1.max(size.x));
            sy = (sy.0.min(size.y), sy.1.max(size.y));
            sz = (sz.0.min(size.z), sz.1.max(size.z));
            if size.x > 500 || size.y > 500 || size.z > 500 {
                oversized += 1;
            }
        }
        px = (px.0.min(b.position.x), px.1.max(b.position.x));
        py = (py.0.min(b.position.y), py.1.max(b.position.y));
        pz = (pz.0.min(b.position.z), pz.1.max(b.position.z));
    }

    println!("terrain bricks: {terrain}");
    println!("size  x: {:?}  y: {:?}  z: {:?}  (units)", sx, sy, sz);
    println!("pos   x: {:?}  y: {:?}  z: {:?}", px, py, pz);
    println!(
        "footprint: {} × {} units, height span: {} units",
        px.1 - px.0,
        py.1 - py.0,
        pz.1 - pz.0
    );
    println!("bricks with an axis > 500 units (suspected procedural cap): {oversized}");

    // Spawn-vs-local-terrain check: how high is the spawn above the terrain
    // directly under it (within ~1000 units of the spawn XY)? A large gap means
    // the player loads floating in the sky — the "seems not to load" symptom.
    if let Some(s) = spawns.first() {
        let mut local_top = i32::MIN;
        for b in &bricks {
            if format!("{:?}", b.asset).contains("SpawnPoint") {
                continue;
            }
            let near = (b.position.x - s.position.x).abs() < 1000
                && (b.position.y - s.position.y).abs() < 1000;
            if near {
                let top = match &b.asset {
                    BrickType::Procedural { size, .. } => b.position.z + i32::from(size.z),
                    _ => b.position.z,
                };
                local_top = local_top.max(top);
            }
        }
        if local_top == i32::MIN {
            println!(
                "spawn-vs-terrain: NO terrain within 1000 units of the spawn XY ({}, {}) — \
                 player loads over a void/at the centroid which has no nearby bricks",
                s.position.x, s.position.y
            );
        } else {
            println!(
                "spawn-vs-terrain: spawn z={}, nearest terrain top z={}, gap={} units ({} studs)",
                s.position.z,
                local_top,
                s.position.z - local_top,
                (s.position.z - local_top) / 5
            );
        }
    }
    println!("asset types:");
    for (a, n) in &assets {
        println!("  {n:>7} × {a}");
    }
}
