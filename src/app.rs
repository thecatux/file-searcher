//! Interfaz gráfica ligera con egui/eframe.
//!
//! La búsqueda se lanza en un hilo aparte y se comunica con la UI por un
//! `mpsc::channel`, así la ventana nunca se congela mientras se escanean
//! miles de archivos o varios GB de datos.

use crate::config::AppConfig;
use crate::search::{self, SearchEvent, SearchMatch, SearchParams, SearchProgress};
use crate::util::format_duration;
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;

pub struct SearchApp {
    // Configuración persistida + copia editable en el panel de ajustes.
    cfg: AppConfig,
    cfg_draft: AppConfig,
    excluded_dirs_text: String,
    show_settings: bool,
    settings_message: Option<String>,

    // Campos del formulario de búsqueda.
    dir: String,
    pattern: String,
    content: bool,
    case_sensitive: bool,
    basename_only: bool,
    limit_text: String,

    // Estado de la búsqueda en curso / resultados.
    searching: bool,
    status: String,
    results: Vec<SearchMatch>,
    // Límite pedido en la búsqueda en curso: los resultados que lleguen
    // por el canal más allá de este número se descartan en el cliente
    // (por si varios hilos entregan alguno de más justo al cruzar el
    // límite casi a la vez).
    current_limit: usize,
    last_elapsed: Option<std::time::Duration>,
    current_page: usize,

    // Progreso en vivo (carpeta/archivo actual, nº de archivos vistos),
    // actualizado por el hilo de búsqueda y leído aquí en cada frame.
    progress: Arc<SearchProgress>,
    // Flag compartido con el hilo de búsqueda para poder cancelarla desde
    // el botón "Detener". `None` cuando no hay una búsqueda en curso.
    cancel_flag: Option<Arc<AtomicBool>>,

    tx: Sender<SearchEvent>,
    rx: Receiver<SearchEvent>,
}

impl Default for SearchApp {
    fn default() -> Self {
        let cfg = AppConfig::load();
        let (tx, rx) = std::sync::mpsc::channel();
        let cfg_default_limit = cfg.max_results;
        Self {
            dir: cfg.default_dir.clone(),
            pattern: String::new(),
            content: cfg.search_content,
            case_sensitive: cfg.case_sensitive,
            basename_only: cfg.basename_only,
            limit_text: cfg.max_results.to_string(),
            excluded_dirs_text: cfg.excluded_dirs.join(", "),
            cfg_draft: cfg.clone(),
            cfg,
            show_settings: false,
            settings_message: None,
            searching: false,
            status: "Listo.".to_string(),
            results: Vec::new(),
            current_limit: cfg_default_limit,
            last_elapsed: None,
            current_page: 1,
            progress: SearchProgress::new(),
            cancel_flag: None,
            tx,
            rx,
        }
    }
}

impl SearchApp {
    fn start_search(&mut self) {
        let limit: usize = self.limit_text.trim().parse().unwrap_or(0);
        if limit == 0 {
            self.status = "El máximo de resultados debe ser un número mayor que cero.".into();
            return;
        }
        if let Err(error) = self.cfg.validate() {
            self.status = format!("Configuración inválida: {}", error);
            return;
        }

        let params = SearchParams {
            dir: self.dir.trim().to_string(),
            pattern: self.pattern.clone(),
            content: self.content,
            case_sensitive: self.case_sensitive,
            limit,
        };
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();

        // Flag de cancelación nuevo para esta búsqueda concreta; el botón
        // "Detener" lo pondrá a `true` mientras `searching == true`.
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(Arc::clone(&cancel));
        self.progress.reset();
        let progress = Arc::clone(&self.progress);

        self.searching = true;
        self.status = "Buscando…".to_string();
        self.results.clear();
        self.current_limit = limit;
        self.current_page = 1;

        thread::spawn(move || {
            // `run_search` ya va enviando eventos por `tx` a medida que
            // encuentra resultados; aquí no hace falta recoger nada más.
            search::run_search(&params, &cfg, tx, cancel, progress);
        });
    }

    /// Pide que la búsqueda en curso se detenga. La cancelación es
    /// cooperativa: el hilo de búsqueda comprueba el flag entre archivo y
    /// archivo (y también antes de descender a cada carpeta), así que no
    /// se congela nada en la interfaz, pero puede tardar una fracción de
    /// segundo en pararse del todo mientras termina el archivo actual.
    fn stop_search(&mut self) {
        if let Some(flag) = &self.cancel_flag {
            flag.store(true, Ordering::Relaxed);
            self.status = "Deteniendo…".to_string();
        }
    }

    fn reset_settings_draft(&mut self) {
        self.cfg_draft = self.cfg.clone();
        self.excluded_dirs_text = self.cfg.excluded_dirs.join(", ");
        self.settings_message = None;
    }

    fn open_settings(&mut self) {
        self.reset_settings_draft();
        self.show_settings = true;
    }

    fn poll_search_events(&mut self) {
        // Se procesan todos los eventos que haya en el canal en este
        // frame (no solo uno), para no ir un frame por detrás cuando
        // llegan resultados muy rápido.
        while let Ok(event) = self.rx.try_recv() {
            match event {
                SearchEvent::Match(m) => {
                    if self.results.len() < self.current_limit {
                        self.results.push(m);
                    }
                }
                SearchEvent::Finished(summary) => {
                    self.searching = false;
                    self.cancel_flag = None;
                    self.current_page = 1;
                    if let Some(err) = summary.error {
                        self.status = format!("Error: {}", err);
                    } else {
                        self.last_elapsed = Some(summary.elapsed);
                        let mut msg = format!(
                            "{} resultado(s) de {} archivo(s) escaneados en {}.",
                            self.results.len(),
                            summary.scanned,
                            format_duration(summary.elapsed)
                        );
                        if summary.cancelled {
                            msg.push_str(" (búsqueda detenida por el usuario)");
                        } else if summary.truncated {
                            msg.push_str(" (se alcanzó el límite máximo de resultados)");
                        }
                        self.status = msg;
                    }
                }
            }
        }
    }

    fn save_settings(&mut self) {
        self.cfg_draft.excluded_dirs = self
            .excluded_dirs_text
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if let Err(error) = self.cfg_draft.validate() {
            self.settings_message = Some(format!("Configuración inválida: {}", error));
            return;
        }

        match self.cfg_draft.save() {
            Ok(path) => {
                self.cfg = self.cfg_draft.clone();
                self.dir = self.cfg.default_dir.clone();
                self.content = self.cfg.search_content;
                self.case_sensitive = self.cfg.case_sensitive;
                self.basename_only = self.cfg.basename_only;
                self.limit_text = self.cfg.max_results.to_string();
                self.settings_message =
                    Some(format!("Configuración guardada en {}", path.display()));
            }
            Err(e) => {
                self.settings_message = Some(format!("No se pudo guardar la configuración: {}", e));
            }
        }
    }

    fn settings_panel(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut close_requested = false;
        egui::Window::new("Configuración")
            .open(&mut open)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Estos valores se guardan en: {}",
                    AppConfig::config_file_path().display()
                ));
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Directorio por defecto:");
                    ui.text_edit_singleline(&mut self.cfg_draft.default_dir);
                });

                ui.horizontal(|ui| {
                    ui.label("Resultados máximos por defecto:");
                    let mut text = self.cfg_draft.max_results.to_string();
                    if ui.text_edit_singleline(&mut text).changed()
                        && let Ok(v) = text.parse()
                    {
                        self.cfg_draft.max_results = v;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Resultados por página:");
                    let mut text = self.cfg_draft.page_size.to_string();
                    if ui.text_edit_singleline(&mut text).changed()
                        && let Ok(v) = text.parse()
                        && v > 0
                    {
                        self.cfg_draft.page_size = v;
                    }
                });

                if self.cfg_draft.page_size > 500 {
                    ui.colored_label(
                        egui::Color32::from_rgb(235, 170, 40),
                        "⚠ Advertencia: Mostrar más de 500 resultados por página puede ralentizar el renderizado de la interfaz gráfica.",
                    );
                }

                ui.horizontal(|ui| {
                    ui.label("Tamaño máx. de archivo a leer (MB):");
                    let mut text = self.cfg_draft.max_file_size_mb.to_string();
                    if ui.text_edit_singleline(&mut text).changed()
                        && let Ok(v) = text.parse()
                        && v > 0
                    {
                        self.cfg_draft.max_file_size_mb = v;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Hilos (0 = automático):");
                    let mut text = self.cfg_draft.thread_count.to_string();
                    if ui.text_edit_singleline(&mut text).changed()
                        && let Ok(v) = text.parse()
                    {
                        self.cfg_draft.thread_count = v;
                    }
                });

                ui.checkbox(&mut self.cfg_draft.case_sensitive, "Distinguir mayúsculas/minúsculas por defecto");
                ui.checkbox(&mut self.cfg_draft.search_content, "Buscar en contenido por defecto");
                ui.checkbox(&mut self.cfg_draft.basename_only, "Mostrar solo nombre de archivo por defecto");
                ui.checkbox(&mut self.cfg_draft.include_hidden, "Incluir archivos ocultos en la búsqueda");

                ui.separator();
                ui.label("Carpetas excluidas (separadas por coma). Acelera mucho la búsqueda en proyectos grandes:");
                ui.text_edit_multiline(&mut self.excluded_dirs_text);

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Guardar").clicked() {
                        self.save_settings();
                    }
                    if ui.button("Restaurar valores por defecto").clicked() {
                        self.cfg_draft = AppConfig::default();
                        self.excluded_dirs_text = self.cfg_draft.excluded_dirs.join(", ");
                    }
                    if ui.button("Cerrar").clicked() {
                        close_requested = true;
                    }
                });

                if let Some(msg) = &self.settings_message {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, msg);
                }
            });
        if close_requested {
            open = false;
        }
        self.show_settings = open;
        if !open {
            self.reset_settings_draft();
        }
    }
}

impl eframe::App for SearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_search_events();
        if self.searching {
            // Fuerza repintado continuo mientras se busca, para refrescar
            // el estado apenas llegue el resultado por el canal.
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("🔎 Buscador de archivos");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ Configuración").clicked() {
                        if self.show_settings {
                            self.show_settings = false;
                            self.reset_settings_draft();
                        } else {
                            self.open_settings();
                        }
                    }
                });
            });
            ui.add_space(6.0);
        });

        if self.show_settings {
            self.settings_panel(ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Grid::new("search_form")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Directorio:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.dir).desired_width(420.0));
                        if ui.button("📂 Elegir…").clicked()
                            && let Some(path) = rfd::FileDialog::new().pick_folder()
                        {
                            self.dir = path.display().to_string();
                        }
                    });
                    ui.end_row();

                    ui.label("Buscar:");
                    let pattern_response = ui.text_edit_singleline(&mut self.pattern);
                    ui.end_row();

                    ui.label("Opciones:");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.content, "Buscar en contenido");
                        ui.checkbox(&mut self.case_sensitive, "Mayús/minús");
                        ui.checkbox(&mut self.basename_only, "Solo nombre");
                    });
                    ui.end_row();

                    ui.label("Máx. resultados:");
                    ui.add(egui::TextEdit::singleline(&mut self.limit_text).desired_width(80.0));
                    ui.end_row();

                    let enter_pressed = pattern_response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    ui.label("");
                    ui.horizontal(|ui| {
                        let button = ui.add_enabled(!self.searching, egui::Button::new("Buscar"));
                        if (button.clicked() || enter_pressed) && !self.searching {
                            self.start_search();
                        }
                        if self.searching {
                            // Se deshabilita en cuanto se pulsa una vez,
                            // para que no se pueda mandar la cancelación
                            // repetidamente mientras el hilo termina.
                            let stopping = self
                                .cancel_flag
                                .as_ref()
                                .map(|f| f.load(Ordering::Relaxed))
                                .unwrap_or(false);
                            if ui
                                .add_enabled(!stopping, egui::Button::new("⏹ Detener"))
                                .clicked()
                            {
                                self.stop_search();
                            }
                            ui.spinner();
                        }
                    });
                    ui.end_row();
                });

            ui.separator();
            if self.searching {
                let (scanned, current) = self.progress.snapshot();
                ui.label(&self.status);
                let current_str = current
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "…".to_string());
                ui.label(
                    egui::RichText::new(format!(
                        "{} archivo(s) revisados · {} coincidencia(s) · Explorando: {}",
                        scanned,
                        self.results.len(),
                        current_str
                    ))
                    .weak()
                    .monospace(),
                );
            } else {
                ui.label(&self.status);
            }
            ui.separator();

            // Lógica de paginación
            let page_size = self.cfg.page_size.max(1);
            let total_results = self.results.len();
            let total_pages = if total_results == 0 {
                1
            } else {
                total_results.div_ceil(page_size)
            };
            self.current_page = self.current_page.clamp(1, total_pages);
            let start_idx = if total_results == 0 {
                0
            } else {
                (self.current_page - 1) * page_size
            };
            let end_idx = (start_idx + page_size).min(total_results);

            // Barra de controles de paginación (estilo motor de búsqueda)
            if total_results > 0 {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Mostrando {} - {} de {} coincidencias (Página {} de {})",
                            start_idx + 1,
                            end_idx,
                            total_results,
                            self.current_page,
                            total_pages
                        ))
                        .color(egui::Color32::from_rgb(180, 200, 220)),
                    );

                    if total_pages > 1 {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    self.current_page < total_pages,
                                    egui::Button::new("Última ⏭"),
                                )
                                .clicked()
                            {
                                self.current_page = total_pages;
                            }
                            if ui
                                .add_enabled(
                                    self.current_page < total_pages,
                                    egui::Button::new("Siguiente ▶"),
                                )
                                .clicked()
                            {
                                self.current_page += 1;
                            }

                            // Botones numéricos de páginas alrededor de la actual
                            let max_numeric = 5;
                            let min_p = self.current_page.saturating_sub(2).max(1);
                            let max_p = (min_p + max_numeric - 1).min(total_pages);
                            let min_p = max_p.saturating_sub(max_numeric - 1).max(1);

                            for p in (min_p..=max_p).rev() {
                                let is_curr = p == self.current_page;
                                let btn = if is_curr {
                                    egui::Button::new(
                                        egui::RichText::new(format!("[ {} ]", p))
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    )
                                } else {
                                    egui::Button::new(format!("{}", p))
                                };
                                if ui.add_enabled(!is_curr, btn).clicked() {
                                    self.current_page = p;
                                }
                            }

                            if ui
                                .add_enabled(
                                    self.current_page > 1,
                                    egui::Button::new("◀ Anterior"),
                                )
                                .clicked()
                            {
                                self.current_page -= 1;
                            }
                            if ui
                                .add_enabled(
                                    self.current_page > 1,
                                    egui::Button::new("⏮ Primera"),
                                )
                                .clicked()
                            {
                                self.current_page = 1;
                            }
                        });
                    }
                });
                ui.separator();
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                if total_results > 0 {
                    for m in &self.results[start_idx..end_idx] {
                        let file_name = m
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| m.path.display().to_string());

                        let full_path_str = m.path.display().to_string();

                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                let title = if self.basename_only {
                                    format!("📄 {}", file_name)
                                } else {
                                    format!("📄 {}", full_path_str)
                                };
                                ui.label(egui::RichText::new(title).strong());

                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui
                                        .button("📂 Abrir ubicación")
                                        .on_hover_text("Abre la carpeta y resalta el archivo en el Explorador")
                                        .clicked()
                                    {
                                        crate::util::open_file_location(&m.path);
                                    }
                                    if ui
                                        .button("▶ Abrir")
                                        .on_hover_text("Abre o ejecuta este archivo con su aplicación predeterminada")
                                        .clicked()
                                    {
                                        crate::util::open_file(&m.path);
                                    }
                                });
                            });

                            if self.basename_only {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Ubicación:").weak());
                                    if ui
                                        .link(&full_path_str)
                                        .on_hover_text("Haz clic para abrir en el Explorador de archivos")
                                        .clicked()
                                    {
                                        crate::util::open_file_location(&m.path);
                                    }
                                });
                            }

                            if let Some(snippet) = &m.snippet {
                                ui.add_space(2.0);
                                ui.label(egui::RichText::new(snippet).weak().monospace());
                            }
                        });
                    }

                    // Navegación adicional al pie de la lista si hay varias páginas
                    if total_pages > 1 {
                        ui.add_space(8.0);
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label(format!("Página {} de {}", self.current_page, total_pages));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .add_enabled(
                                        self.current_page < total_pages,
                                        egui::Button::new("Última ⏭"),
                                    )
                                    .clicked()
                                {
                                    self.current_page = total_pages;
                                }
                                if ui
                                    .add_enabled(
                                        self.current_page < total_pages,
                                        egui::Button::new("Siguiente ▶"),
                                    )
                                    .clicked()
                                {
                                    self.current_page += 1;
                                }
                                if ui
                                    .add_enabled(
                                        self.current_page > 1,
                                        egui::Button::new("◀ Anterior"),
                                    )
                                    .clicked()
                                {
                                    self.current_page -= 1;
                                }
                                if ui
                                    .add_enabled(
                                        self.current_page > 1,
                                        egui::Button::new("⏮ Primera"),
                                    )
                                    .clicked()
                                {
                                    self.current_page = 1;
                                }
                            });
                        });
                    }
                }
            });
        });
    }
}
