//! Buscador de archivos con interfaz gráfica.
//!
//! Estructura del proyecto:
//! - `config`: carga/guarda las preferencias del usuario en config/config.json
//! - `search`: lógica de búsqueda (por nombre o contenido), paralelizada
//! - `util`:   utilidades pequeñas (formateo, filtros de archivos ocultos)
//! - `app`:    interfaz gráfica (egui/eframe) y su estado

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod search;
mod util;

use app::SearchApp;

fn window_icon() -> eframe::egui::IconData {
    let image = image::load_from_memory(include_bytes!("../assets/icon_1024.png"))
        .expect("No se pudo cargar icon_1024.png")
        .into_rgba8();
    let (width, height) = image.dimensions();

    eframe::egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([480.0, 360.0])
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Buscador de archivos",
        options,
        Box::new(|_cc| Ok(Box::new(SearchApp::default()))),
    )
}
