use brdb::assets::bricks::{
    PB_DEFAULT_BRICK, PB_DEFAULT_MICRO_BRICK, PB_DEFAULT_SMOOTH_TILE, PB_DEFAULT_STUDDED,
    PB_DEFAULT_TILE,
};
use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use env_logger::Builder;
use heightmap::{
    opt::gen_opt_heightmap,
    util::{GenOptions, maps_from_files, write_save},
};
use log::{LevelFilter, error, info};
use std::{io::Write, path::PathBuf, process::ExitCode};

// exception: data-builder — one chained Arg declaration per CLI flag
fn cli() -> Command {
    let flag = |name: &'static str, help: &'static str| {
        Arg::new(name)
            .long(name)
            .action(ArgAction::SetTrue)
            .help(help)
    };
    Command::new("heightmap")
        .version(env!("CARGO_PKG_VERSION"))
        .author("github.com/Meshiest")
        .about("Converts heightmap images (PNG/JPG) to Brickadia save files")
        .arg(
            Arg::new("INPUT")
                .required(true)
                .num_args(1..)
                .value_parser(value_parser!(PathBuf))
                .help("Input heightmap image files (PNG/JPG)"),
        )
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .value_parser(value_parser!(String))
                .help("Output file (BRDB, BRZ)"),
        )
        .arg(
            Arg::new("colormap")
                .short('c')
                .long("colormap")
                .value_parser(value_parser!(PathBuf))
                .help("Input colormap image (PNG/JPG)"),
        )
        .arg(
            Arg::new("vertical")
                .short('v')
                .long("vertical")
                .value_parser(value_parser!(u32))
                .default_value("1")
                .help("Vertical scale multiplier (default 1)"),
        )
        .arg(
            Arg::new("size")
                .short('s')
                .long("size")
                .value_parser(value_parser!(u16))
                .default_value("1")
                .help("Brick stud size (default 1)"),
        )
        .arg(flag(
            "cull",
            "Automatically remove bottom level bricks and fully transparent bricks",
        ))
        .arg(flag("tile", "Render bricks as tiles"))
        .arg(flag("smooth", "Render bricks as smooth tiles"))
        .arg(flag("micro", "Render bricks as micro bricks"))
        .arg(flag("stud", "Render bricks as stud cubes"))
        .arg(flag("snap", "Snap bricks to the brick grid"))
        .arg(flag("lrgb", "Use linear rgb input color instead of sRGB"))
        .arg(
            Arg::new("img")
                .short('i')
                .long("img")
                .action(ArgAction::SetTrue)
                .help("Make the heightmap flat and render an image"),
        )
        .arg(flag("glow", "Make the heightmap glow at 0 intensity"))
        .arg(flag("hdmap", "Using a high detail rgb color encoded heightmap"))
        .arg(flag("nocollide", "Disable brick collision"))
        .arg(flag("greedy", "Use greedy optimization"))
}

fn options_from_matches(matches: &ArgMatches) -> GenOptions {
    let micro = matches.get_flag("micro");
    GenOptions {
        // defaulted by clap; the parser already rejected non-u16 values
        size: matches.get_one::<u16>("size").copied().unwrap_or(1) * if micro { 1 } else { 5 },
        scale: matches.get_one::<u32>("vertical").copied().unwrap_or(1),
        cull: matches.get_flag("cull"),
        asset: if micro {
            PB_DEFAULT_MICRO_BRICK
        } else if matches.get_flag("tile") {
            PB_DEFAULT_TILE
        } else if matches.get_flag("smooth") {
            PB_DEFAULT_SMOOTH_TILE
        } else if matches.get_flag("stud") {
            PB_DEFAULT_STUDDED
        } else {
            PB_DEFAULT_BRICK
        },
        micro,
        stud: matches.get_flag("stud"),
        snap: matches.get_flag("snap"),
        img: matches.get_flag("img"),
        glow: matches.get_flag("glow"),
        hdmap: matches.get_flag("hdmap"),
        lrgb: matches.get_flag("lrgb"),
        nocollide: matches.get_flag("nocollide"),
        quadtree: true,
        greedy: matches.get_flag("greedy"),
        // CLI keeps the legacy flat surface; base-fill is a Map-tab feature
        // (its heights are normalized and brick-capped). Unchanged here.
        fill_to_base: false,
        // CLI never skips the floor — its output stays byte-identical.
        skip_floor: false,
    }
}

fn main() -> ExitCode {
    Builder::new()
        .format(|buf, record| writeln!(buf, "{}", record.args()))
        .filter(None, LevelFilter::Info)
        .init();

    let matches = cli().get_matches();

    let heightmap_files: Vec<PathBuf> = matches
        .get_many::<PathBuf>("INPUT")
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let colormap_file = matches.get_one::<PathBuf>("colormap").cloned();
    let out_file = matches
        .get_one::<String>("output")
        .map(String::as_str)
        .unwrap_or("./out.brz")
        .to_string();
    let options = options_from_matches(&matches);

    info!("Reading image files");
    let (heightmap, colormap) = match maps_from_files(&options, heightmap_files, colormap_file) {
        Ok(maps) => maps,
        Err(err) => {
            error!("{err} — check that the input paths exist and are PNG/JPG images");
            return ExitCode::FAILURE;
        }
    };

    let bricks = match gen_opt_heightmap(&*heightmap, &*colormap, options, None, None, |_| true) {
        Ok(bricks) => bricks,
        Err(err) => {
            error!(
                "Generation failed: {err} — try a smaller --size/--vertical scale or a smaller input image"
            );
            return ExitCode::FAILURE;
        }
    };

    info!("Writing Save to {}", out_file);
    if let Err(err) = write_save(&out_file, bricks) {
        error!("{err} — pass --output with a writable .brz or .brdb path");
        return ExitCode::FAILURE;
    }

    info!("Done!");
    ExitCode::SUCCESS
}
