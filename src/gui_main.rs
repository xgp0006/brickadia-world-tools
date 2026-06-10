#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use eframe::NativeOptions;
use heightmap::gui::{HeightmapApp, install_theme, logger};

fn main() -> Result<(), eframe::Error> {
    logger::init().unwrap();

    eframe::run_native(
        "heightmap2brs",
        NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_always_on_top()
                .with_decorations(true)
                .with_drag_and_drop(true)
                .with_inner_size([960.0, 720.0])
                .with_min_inner_size([720.0, 540.0])
                .with_resizable(true),
            ..Default::default()
        },
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            install_fonts(&cc.egui_ctx);
            install_theme(&cc.egui_ctx);
            Ok(Box::<HeightmapApp>::default())
        }),
    )
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "fraunces_display".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/Fraunces-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "plex_mono".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/IBMPlexMono-Regular.ttf"
        ))),
    );

    if let Some(props) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        props.insert(0, "fraunces_display".to_owned());
    }
    if let Some(mono) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        mono.insert(0, "plex_mono".to_owned());
    }

    ctx.set_fonts(fonts);
}
