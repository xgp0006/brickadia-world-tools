//! Write a tiny save with the current brdb version so its on-disk structure can
//! be diffed against a known-loadable Brickadia save (format-version probe).
//! Usage: `cargo run --example write_sample -- /tmp/sample.brdb`

use heightmap::util::{bricks_to_save, write_save_world};

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "/tmp/sample.brdb".to_string());
    let bricks = vec![
        brdb::Brick { position: brdb::Position::new(0, 0, 6), ..Default::default() },
        brdb::Brick { position: brdb::Position::new(10, 0, 6), ..Default::default() },
    ];
    let world = bricks_to_save(bricks);
    write_save_world(&world, &out).expect("write sample");
    println!("wrote {out}");
}
