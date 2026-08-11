#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use eframe::NativeOptions;
use heightmap::gui::{HeightmapApp, install_theme, logger};

fn main() -> Result<(), eframe::Error> {
    logger::init().unwrap();

    eframe::run_native(
        "Brickadia-World-Tools",
        NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("Brickadia-World-Tools")
                // Always-on-top is opt-in via the header checkbox (app state starts
                // false). Defaulting it on here desynced the checkbox and stuck the
                // window above every other app on multi-monitor / scaled desktops.
                .with_decorations(true)
                .with_drag_and_drop(true)
                .with_inner_size([1280.0, 800.0])
                .with_min_inner_size([800.0, 560.0])
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

    // Merge the Phosphor icon glyphs as a fallback in both families, so a
    // `\u{e...}` codepoint in any label resolves to a line-art icon. Appended
    // (not index 0), so it never shadows the text faces above.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    ctx.set_fonts(fonts);
}
