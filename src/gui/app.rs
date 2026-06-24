use std::{
    borrow::Cow,
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self},
    time::Duration,
};

use super::logger;
use super::map_tab::{self, MapTabState};
use super::sculpt::{self, SculptState};
use crate::{opt::*, util::*};
use brdb::assets::bricks::{
    PB_DEFAULT_BRICK, PB_DEFAULT_MICRO_BRICK, PB_DEFAULT_SMOOTH_TILE, PB_DEFAULT_STUDDED,
    PB_DEFAULT_TILE,
};
use eframe::App;
use egui::{
    Button, CentralPanel, Color32, Context, Id, ImageSource, ProgressBar, ScrollArea,
    TopBottomPanel, Ui, vec2,
};
use log::{error, info};
use poll_promise::Promise;
use std::path::Path;

#[derive(PartialEq, Clone)]
enum BrickMode {
    Default,
    Tile,
    SmoothTile,
    Stud,
    Micro,
}

#[derive(PartialEq, Clone)]
enum OptimizationMode {
    None,
    Quad,
    Greedy,
}

#[derive(PartialEq, Clone, Copy)]
enum AppTab {
    Convert,
    Map,
    Sculpt,
}

type Progress = (&'static str, f32);

/// Everything the conversion worker thread needs, captured by value so the
/// thread owns its inputs outright.
struct ConvertJob {
    out_file: String,
    to_clipboard: bool,
    options: GenOptions,
    heightmap_files: Vec<PathBuf>,
    colormap_file: Option<PathBuf>,
}

/// Body of the conversion worker thread: decode inputs, generate bricks,
/// write the save, optionally copy the path to the clipboard. Failures are
/// logged here so the GUI promise only has to display them.
fn convert_worker(
    job: ConvertJob,
    progress: &impl Fn(&'static str, f32),
    is_stopped: &impl Fn() -> bool,
) -> Result<(), String> {
    info!("Reading image files...");
    let (heightmap, colormap) =
        maps_from_files(&job.options, job.heightmap_files, job.colormap_file)
            .inspect_err(|err| error!("{err}"))?;

    if is_stopped() {
        return Err(CANCELLED_MSG.to_string());
    }
    progress("Generating", 0.10);

    let bricks = gen_opt_heightmap(&*heightmap, &*colormap, job.options, None, None, |p| {
        progress("Generating", 0.1 + 0.85 * p);
        !is_stopped()
    })
    .inspect_err(|err| error!("{err}"))?;
    if is_stopped() {
        return Err(CANCELLED_MSG.to_string());
    }

    info!("Writing Save to {}", job.out_file);
    progress("Writing", 0.95);
    write_save(&job.out_file, bricks).inspect_err(|err| error!("{err}"))?;

    if job.to_clipboard {
        copy_path_to_clipboard(&job.out_file).inspect_err(|err| error!("{err}"))?;
    }

    if is_stopped() {
        return Err(CANCELLED_MSG.to_string());
    }
    progress("Finished", 1.0);
    info!("Done!");
    Ok(())
}

fn copy_path_to_clipboard(out_file: &str) -> Result<(), String> {
    // If the path is not absolute, make it absolute relative to the current exe location
    let mut full_path = Path::new(out_file)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(out_file))
        .to_string_lossy()
        .to_string();

    // lowercase the first letter
    if let Some(s) = full_path.get_mut(0..1) {
        s.make_ascii_lowercase()
    }

    let mut cb =
        arboard::Clipboard::new().map_err(|e| format!("failed to open clipboard: {e}"))?;
    cb.set_text(full_path.clone())
        .map_err(|e| format!("failed to set clipboard: {e}"))?;
    info!("Wrote path {full_path} to clipboard");
    Ok(())
}

pub struct HeightmapApp {
    // options for the generator
    heightmaps: Vec<PathBuf>,
    colormap: Option<PathBuf>,
    out_file: String,
    out_clipboard: bool,
    vertical_scale: u32,
    horizontal_size: u16,
    optimization: OptimizationMode,
    opt_cull: bool,
    opt_nocollide: bool,
    opt_lrgb: bool,
    opt_hdmap: bool,
    opt_snap: bool,
    opt_glow: bool,
    mode: BrickMode,
    always_on_top: bool,
    progress: Progress,
    progress_channel: (Sender<Progress>, Receiver<Progress>),
    promise: Option<Promise<Result<(), String>>>,
    gen_interrupt: Option<Sender<()>>,
    current_tab: AppTab,
    map_state: MapTabState,
    sculpt_state: SculptState,
}

impl Default for HeightmapApp {
    fn default() -> Self {
        Self {
            // default generator options
            heightmaps: vec![],
            colormap: None,
            out_file: "out.brz".to_string(),
            out_clipboard: true,
            vertical_scale: 1,
            horizontal_size: 1,
            optimization: OptimizationMode::Quad,
            opt_cull: false,
            opt_nocollide: false,
            opt_lrgb: false,
            opt_snap: false,
            opt_glow: false,
            opt_hdmap: false,
            mode: BrickMode::Micro,
            always_on_top: false,
            promise: None,
            progress: ("Pending", 0.),
            progress_channel: mpsc::channel(),
            gen_interrupt: None,
            current_tab: AppTab::Convert,
            map_state: MapTabState::new(),
            sculpt_state: SculptState::new(),
        }
    }
}

impl HeightmapApp {
    /// True when any selected heightmap/colormap exceeds 1024px in either
    /// dimension. Called from the egui draw path every frame, so it must only
    /// read image headers — `image::open` would decode whole files on the UI
    /// thread and stall frames for hundreds of ms on large PNGs.
    fn has_large_image(&self) -> bool {
        let check_image = |path: &PathBuf| -> bool {
            image::ImageReader::open(path)
                .and_then(|r| r.into_dimensions().map_err(std::io::Error::other))
                .is_ok_and(|(w, h)| w > 1024 || h > 1024)
        };

        self.heightmaps.iter().any(check_image) || self.colormap.as_ref().is_some_and(check_image)
    }

    fn options(&self) -> GenOptions {
        GenOptions {
            size: if self.mode == BrickMode::Micro {
                self.horizontal_size
            } else {
                self.horizontal_size * 5
            },
            scale: self.vertical_scale,
            cull: self.opt_cull,
            asset: match self.mode {
                BrickMode::Default => PB_DEFAULT_BRICK,
                BrickMode::Tile => PB_DEFAULT_TILE,
                BrickMode::SmoothTile => PB_DEFAULT_SMOOTH_TILE,
                BrickMode::Stud => PB_DEFAULT_STUDDED,
                BrickMode::Micro => PB_DEFAULT_MICRO_BRICK,
            },
            micro: self.mode == BrickMode::Micro,
            stud: self.mode == BrickMode::Stud,
            snap: self.opt_snap,
            img: self.heightmaps.is_empty() && self.colormap.is_some(),
            glow: self.opt_glow,
            hdmap: self.opt_hdmap,
            lrgb: self.opt_lrgb,
            nocollide: self.opt_nocollide,
            quadtree: self.optimization == OptimizationMode::Quad,
            greedy: self.optimization == OptimizationMode::Greedy,
            // Convert-tab heights are un-normalized and vertical_scale reaches
            // 100 with no brick cap, so keep the legacy flat surface (no base
            // fill) to avoid an unbounded brick stack.
            fill_to_base: false,
            // Convert tab never skips the floor — output stays byte-identical.
            skip_floor: false,
            // Convert tab never omits columns (skip_floor is off); inert.
            omit_below_h: 0,
        }
    }

    fn run_converter(&mut self) {
        let job = ConvertJob {
            out_file: self.out_file.clone(),
            to_clipboard: self.out_clipboard,
            options: self.options(),
            heightmap_files: self.heightmaps.clone(),
            colormap_file: self.colormap.clone(),
        };

        let progress_tx = self.progress_channel.0.clone();
        // Graceful discard: the receiver is dropped when the window closes
        // mid-generation; .unwrap() here would panic the worker thread.
        let progress = move |status, p| {
            let _ = progress_tx.send((status, p));
        };

        // handle interrupts
        let (tx, rx) = mpsc::channel::<()>();
        self.gen_interrupt = Some(tx);
        let is_stopped = move || rx.try_recv().is_ok();

        self.promise.get_or_insert_with(|| {
            info!("Preparing converter...");
            let (sender, promise) = Promise::new();

            progress("Reading", 0.);

            thread::spawn(move || {
                let result = convert_worker(job, &progress, &is_stopped);
                let finished = result.is_ok();
                sender.send(result);
                if finished {
                    thread::sleep(Duration::from_millis(500));
                    progress("", 2.0);
                }
            });
            promise
        });
    }

    fn draw_header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading("Brickadia-World-Tools");
            ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .checkbox(&mut self.always_on_top, "Always on top")
                    .changed()
                {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                            if self.always_on_top {
                                egui::WindowLevel::AlwaysOnTop
                            } else {
                                egui::WindowLevel::Normal
                            },
                        ));
                }
            });
        });
        ui.hyperlink("https://github.com/brickadia-community/heightmap2brz");
        ui.label(
            "Converts heightmap images (PNG/JPG) to Brickadia save files, also works as img2brick",
        );
        egui::warn_if_debug_build(ui);
    }

    fn draw_settings(&mut self, ui: &mut Ui) {
        ui.heading("Settings");
        ui.label("Configure how the generator outputs the saves as bricks");
        self.draw_settings_grid(ui);

        ui.add_space(8.0);
        ui.separator();
        self.draw_heightmap_picker(ui);
        ui.separator();
        self.draw_colormap_picker(ui);
    }

    fn draw_settings_grid(&mut self, ui: &mut Ui) {
        egui::Grid::new("settings_grid")
            .striped(true)
            .spacing([40.0, 4.0])
            .show(ui, |ui| {
                self.draw_output_rows(ui);
                self.draw_optimization_row(ui);
                self.draw_options_row(ui);
                self.draw_brick_mode_row(ui);
            });
    }

    fn draw_output_rows(&mut self, ui: &mut Ui) {
        ui.label("Save Destination")
            .on_hover_text("The save will be created relative to the location of the exe.");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.out_clipboard, "Copy to clipboard")
                .on_hover_text("Copy the save file path to clipboard after generation");

            ui.add(egui::TextEdit::singleline(&mut self.out_file).hint_text("File Name"));
        });
        ui.end_row();
        let out_file_lowercase = self.out_file.to_lowercase();
        let is_brz = out_file_lowercase.ends_with(".brz");
        if !is_brz && !out_file_lowercase.ends_with(".brdb") {
            ui.label("Warning:");
            ui.colored_label(Color32::RED, "Output file must end with .brz or .brdb");
            ui.end_row();
        }

        ui.label("Horizontal Scale")
            .on_hover_text("The size of each pixel in studs (or microbricks)");
        ui.add(egui::Slider::new(&mut self.horizontal_size, 1..=100).text("studs"));
        ui.end_row();
        ui.label("Vertical Size")
            .on_hover_text("The height of each shade of grey from the heightmap");
        ui.add(egui::Slider::new(&mut self.vertical_scale, 1..=100).text("units"));
        ui.end_row();
    }

    fn draw_optimization_row(&mut self, ui: &mut Ui) {
        ui.label("Optimization")
            .on_hover_text("Algorithm used to reduce brick count");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.optimization, OptimizationMode::None, "None")
                    .on_hover_text("No optimization (~one brick per pixel)");
                ui.radio_value(&mut self.optimization, OptimizationMode::Quad, "Quadtree")
                .on_hover_text("Use quadtree based optimization. Looks prettier. May use more bricks. Uses a lot of memory for larger maps");
                ui.radio_value(&mut self.optimization, OptimizationMode::Greedy, "Greedy")
                    .on_hover_text("Use greedy mesh for each height level. Uses fewer bricks but slower for images with many colors/heights");
            });
            if self.optimization == OptimizationMode::Greedy && !self.heightmaps.is_empty() {
                ui.colored_label(
                    Color32::from_rgb(255, 200, 100),
                    "Note: Greedy meshing does not properly calculate brick heights based on neighbor heights"
                );
            }
            if self.optimization == OptimizationMode::Greedy && self.has_large_image() {
                ui.colored_label(
                    Color32::from_rgb(255, 100, 100),
                    "Warning: Large images (>1024px) may use excessive memory with greedy optimization"
                );
            }});
        ui.end_row();
    }

    fn draw_options_row(&mut self, ui: &mut Ui) {
        ui.label("Options")
            .on_hover_text("A list of options for modifying how the generator works");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.opt_snap, "Snap")
                .on_hover_text("Snap bricks to the brick grid");
            ui.checkbox(&mut self.opt_cull, "Cull").on_hover_text(
                "Automatically remove bottom level bricks and fully transparent bricks\n\
                    In image mode, only transparent bricks are removed",
            );
            ui.checkbox(&mut self.opt_nocollide, "No Collide")
                .on_hover_text("Disable brick collision");
            ui.checkbox(&mut self.opt_lrgb, "LRGB")
                .on_hover_text("Use linear rgb input color instead of sRGB");
            ui.checkbox(&mut self.opt_glow, "Glow")
                .on_hover_text("Glow bricks at lowest intensity");
            ui.checkbox(&mut self.opt_hdmap, "HD Map")
                .on_hover_text("Using a high detail rgb color encoded heightmap");
        });
        ui.end_row();
    }

    fn draw_brick_mode_row(&mut self, ui: &mut Ui) {
        ui.label("Brick Type")
            .on_hover_text("Change which brick type is used for the save file");
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.mode, BrickMode::Default, "Default")
                .on_hover_text("Use default bricks");
            ui.radio_value(&mut self.mode, BrickMode::Tile, "Tile")
                .on_hover_text("Use tile bricks");
            ui.radio_value(&mut self.mode, BrickMode::SmoothTile, "Smooth")
                .on_hover_text("Use smooth tile bricks");
            ui.radio_value(&mut self.mode, BrickMode::Stud, "Stud")
                .on_hover_text("Use studded bricks");
            ui.radio_value(&mut self.mode, BrickMode::Micro, "Micro")
                .on_hover_text("Use micro bricks");
        });
        ui.end_row();
    }

    fn draw_heightmap_picker(&mut self, ui: &mut Ui) {
        ui.heading("Heightmap Images");
        ui.label("Select image files to use for save generation.");

        // handle heightmap multiple file selection
        if ui.button("Select heightmaps").clicked() {
            let result = native_dialog::DialogBuilder::file()
                .add_filter("Image Files", ["png", "jpg", "jpeg"])
                .open_multiple_file()
                .show();

            match result {
                Ok(files) => {
                    self.heightmaps = files;
                    info!("Selected heightmap files: {:?}", &self.heightmaps);
                }
                Err(e) => {
                    error!("Error selecting heightmap files: {e}");
                }
            }
        }

        egui::Grid::new("heightmap_grid")
            .striped(true)
            .spacing([8.0, 4.0])
            .min_col_width(4.0)
            .show(ui, |ui| {
                let mut to_remove = HashSet::new();
                for img in self.heightmaps.clone() {
                    if self.image_row(ui, &img) {
                        to_remove.insert(img.clone());
                    }
                    ui.end_row();
                }
                self.heightmaps.retain(|i| !to_remove.contains(i));
            });
    }

    fn draw_colormap_picker(&mut self, ui: &mut Ui) {
        ui.heading("Colormap Image");
        ui.label("Select image file to use for heightmap coloring. Select only a colormap for img2brick mode.");

        // handle colormap single file selection
        if ui
            .add(Button::new("Select colormap").fill(Color32::from_rgb(60, 60, 120)))
            .clicked()
        {
            let result = native_dialog::DialogBuilder::file()
                .add_filter("Image Files", ["png", "jpg", "jpeg"])
                .open_single_file()
                .show();

            match result {
                Ok(file_path) => {
                    info!("Selected colormap file: {:?}", file_path);
                    self.colormap = file_path;
                }
                Err(e) => {
                    error!("Error selecting colormap file: {e}");
                }
            }
        }

        if let Some(path) = self.colormap.clone() {
            egui::Grid::new("colormap_grid")
                .striped(true)
                .spacing([8.0, 4.0])
                .min_col_width(4.0)
                .show(ui, |ui| {
                    if self.image_row(ui, &path) {
                        self.colormap = None;
                    }
                });
        }
    }

    /// One remove-button + thumbnail + filename row inside a picker grid.
    /// Returns true when the remove button was clicked.
    fn image_row(&mut self, ui: &mut Ui, path: &Path) -> bool {
        let remove = ui.add(Button::new("✖")).clicked();
        self.thumb(ui, path);
        // lossy: non-UTF-8 filenames are legal on Linux; a panic
        // here would take down the GUI over a display label.
        ui.label(path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        ));
        remove
    }

    fn draw_progress(&mut self, ctx: &Context, ui: &mut Ui) -> bool {
        while let Ok(p) = self.progress_channel.1.try_recv() {
            self.progress = p;
        }
        let (progress_text, progress) = self.progress;

        let mut clear_promise = progress > 1.0;
        let mut rendered = false;

        if let Some(p) = &self.promise {
            match p.ready() {
                Some(Ok(())) => {
                    ui.add(
                        ProgressBar::new(ctx.animate_value_with_time(
                            Id::new("progress"),
                            1.0,
                            0.1,
                        ))
                        .text("Finished"),
                    );
                }
                Some(Err(e)) => {
                    ui.horizontal(|ui| {
                        if ui.button("ok").clicked() {
                            clear_promise = true;
                        }
                        ui.colored_label(Color32::RED, format!("Error: {e}"));
                    });
                }
                None => {
                    ui.horizontal(|ui| {
                        let stop_btn = ui.button("Stop");
                        ui.add(
                            ProgressBar::new(ctx.animate_value_with_time(
                                Id::new("progress"),
                                progress,
                                0.1,
                            ))
                            .text(progress_text)
                            .animate(true),
                        );
                        if let (true, Some(tx)) = (stop_btn.clicked(), &self.gen_interrupt) {
                            info!("Sending interrupt...");
                            if let Err(e) = tx.send(()) {
                                error!("error sending interrupt {e}");
                            }
                        }
                    });
                }
            }
            rendered = true;
        }

        if clear_promise {
            self.promise = None
        }

        rendered
    }

    fn draw_submit(&mut self, ui: &mut Ui) {
        // display different text based on the selected image files
        let heightmap_ok = !self.heightmaps.is_empty();
        let colormap_ok = self.colormap.is_some();

        if self.promise.is_some() {
            return;
        }

        if heightmap_ok || colormap_ok {
            if ui
                .add(
                    Button::new(match (heightmap_ok, colormap_ok) {
                        (true, true) => "Generate save",
                        (true, false) => "Generate colorless save",
                        (false, true) => "Generate image2brick save",
                        (false, false) => unreachable!(),
                    })
                    .fill(Color32::from_rgb(50, 90, 50)),
                )
                .clicked()
            {
                self.run_converter();
            }
        } else {
            ui.label("Select some image files to continue...");
        }
    }

    fn thumb(&mut self, ui: &mut Ui, image: &Path) {
        ui.add(
            egui::Image::new(ImageSource::Uri(Cow::from(format!(
                "file://{}",
                image.display().to_string().replace("\\", "/")
            ))))
            .fit_to_exact_size(vec2(32.0, 32.0))
            .maintain_aspect_ratio(false),
        );
    }
}

impl Drop for HeightmapApp {
    fn drop(&mut self) {
        // On window close, eframe drops the App; signal in-flight workers to
        // stop so a detached build/convert thread doesn't keep running (and,
        // for the Convert tab, doesn't panic on the orphaned progress channel).
        self.map_state.cancel_fetch();
        self.sculpt_state.cancel_convert();
        if let Some(tx) = &self.gen_interrupt {
            let _ = tx.send(());
        }
    }
}

impl App for HeightmapApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Map → Sculpt handoff: when the Map tab finishes fetching a DEM for
        // "Send to Sculpt", it parks a HeightField here. Pick it up, load it into
        // the Sculpt tab, and switch to that tab.
        if let Some(field) = self.map_state.take_sculpt_handoff() {
            self.sculpt_state.set_field(field);
            self.current_tab = AppTab::Sculpt;
        }

        TopBottomPanel::top("app_tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.current_tab, AppTab::Convert, "Convert");
                ui.selectable_value(&mut self.current_tab, AppTab::Map, "Map");
                ui.selectable_value(&mut self.current_tab, AppTab::Sculpt, "Sculpt");
            });
        });
        CentralPanel::default().show(ctx, |ui| match self.current_tab {
            AppTab::Convert => self.draw_convert_tab(ctx, ui),
            AppTab::Map => map_tab::draw(&mut self.map_state, ctx, ui),
            AppTab::Sculpt => sculpt::draw(&mut self.sculpt_state, ctx, ui),
        });
    }
}

impl HeightmapApp {
    fn draw_convert_tab(&mut self, ctx: &Context, ui: &mut Ui) {
        self.draw_header(ui);
        ScrollArea::vertical().show(ui, |ui| {
            ui.separator();
            self.draw_settings(ui);
            ui.separator();
            if !self.draw_progress(ctx, ui) {
                self.draw_submit(ui);
            }
        });
        TopBottomPanel::bottom(Id::new("logs"))
            .min_height(30.0)
            .resizable(true)
            .frame(egui::Frame {
                fill: Color32::BLACK,
                inner_margin: 4.0.into(),
                outer_margin: 0.0.into(),
                ..Default::default()
            })
            .show_inside(ui, |ui| {
                logger::draw(ui);
            });
    }
}
