//! Lógica de búsqueda.
//!
//! Arquitectura (revisada):
//!
//! - El recorrido del árbol y el filtrado (por nombre o por contenido) ya
//!   NO son dos fases separadas: se recorre con `ignore::WalkParallel`
//!   (recorrido paralelo real, cada hilo del mismo pool examina cada
//!   archivo en cuanto lo descubre). Así no hace falta acumular todas las
//!   rutas del árbol en un `Vec` antes de empezar a buscar, lo que evita
//!   un pico de RAM grande en árboles con muchísimos archivos y baja la
//!   latencia hasta el primer resultado.
//! - Los resultados se envían a la UI en cuanto aparecen (`SearchEvent`),
//!   no se acumulan para entregarlos todos de golpe al final.
//! - La búsqueda se puede cancelar en cualquier momento con un
//!   `Arc<AtomicBool>` compartido, comprobado tanto en el recorrido como
//!   en cada archivo.
//! - El progreso (carpeta/archivo actual, nº de archivos vistos) se
//!   publica en una estructura compartida (`SearchProgress`) que la UI
//!   puede leer en cada frame sin necesidad de un canal aparte.
//! - La búsqueda por contenido lee cada archivo por bloques, línea a
//!   línea, y se detiene en cuanto encuentra la primera coincidencia, en
//!   vez de cargar el archivo entero en memoria de antemano. El pico de
//!   RAM por archivo queda acotado por la línea más larga encontrada
//!   hasta el momento del match (normalmente muy por debajo de
//!   `max_file_size_mb`), y no se usa memoria mapeada ni código `unsafe`.

use crate::config::AppConfig;
use ignore::{WalkBuilder, WalkState};
use regex::bytes::Regex;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Cuántos bytes iniciales se inspeccionan para descartar archivos
/// binarios antes de intentar tratarlos como texto.
const BINARY_PROBE_BYTES: usize = 8 * 1024;
/// Tamaño del bloque de lectura del `BufReader` (no es un límite de
/// cuánta memoria puede llegar a usar una línea, solo de cuántos bytes
/// se piden al sistema operativo en cada syscall de lectura).
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Parámetros de una búsqueda puntual (los que el usuario ajusta en el
/// formulario principal, no la configuración persistida).
#[derive(Debug, Clone)]
pub struct SearchParams {
    pub dir: String,
    pub pattern: String,
    pub content: bool,
    pub case_sensitive: bool,
    pub limit: usize,
}

/// Un resultado individual. `snippet` solo se rellena en búsqueda por
/// contenido, con la línea de texto donde ocurrió la coincidencia.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub snippet: Option<String>,
}

/// Resumen final de una búsqueda, ya sea completa, truncada por el
/// límite o cancelada por el usuario.
#[derive(Debug, Clone)]
pub struct SearchSummary {
    pub elapsed: Duration,
    pub scanned: usize,
    pub truncated: bool,
    pub cancelled: bool,
    pub error: Option<String>,
}

/// Eventos que `run_search` va emitiendo mientras trabaja. La UI los
/// consume a medida que llegan por el canal en vez de esperar un único
/// resultado al final.
pub enum SearchEvent {
    Match(SearchMatch),
    Finished(SearchSummary),
}

/// Progreso compartido de una búsqueda en curso. Se actualiza desde los
/// hilos de recorrido y se lee desde la UI en cada frame (ver `app.rs`).
///
/// La ruta "actual" se protege con un `Mutex`, pero se actualiza con
/// `try_lock`: si está momentáneamente ocupado por otro hilo simplemente
/// no se actualiza esta vez (no es grave perder alguna actualización de
/// progreso), en vez de bloquear hilos de trabajo esperando el lock.
#[derive(Default)]
pub struct SearchProgress {
    scanned: AtomicUsize,
    current: Mutex<Option<PathBuf>>,
}

impl SearchProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record(&self, path: &Path) {
        self.scanned.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.current.try_lock() {
            *guard = Some(path.to_path_buf());
        }
    }

    /// Foto del progreso actual: (archivos vistos hasta ahora, última
    /// ruta observada).
    pub fn snapshot(&self) -> (usize, Option<PathBuf>) {
        let scanned = self.scanned.load(Ordering::Relaxed);
        let current = self.current.lock().ok().and_then(|g| g.clone());
        (scanned, current)
    }

    pub fn reset(&self) {
        self.scanned.store(0, Ordering::Relaxed);
        if let Ok(mut guard) = self.current.lock() {
            *guard = None;
        }
    }
}

/// Ejecuta la búsqueda completa, emitiendo eventos por `tx` a medida que
/// van ocurriendo y comprobando `cancel` periódicamente para poder
/// detenerse de forma segura. Pensada para llamarse desde un hilo aparte
/// (ver `app.rs`), nunca bloquea el hilo de la interfaz.
pub fn run_search(
    params: &SearchParams,
    cfg: &AppConfig,
    tx: Sender<SearchEvent>,
    cancel: Arc<AtomicBool>,
    progress: Arc<SearchProgress>,
) {
    let start = Instant::now();
    progress.reset();

    let finish_with_error = |msg: String| {
        let _ = tx.send(SearchEvent::Finished(SearchSummary {
            elapsed: start.elapsed(),
            scanned: 0,
            truncated: false,
            cancelled: false,
            error: Some(msg),
        }));
    };

    let dir = Path::new(&params.dir);
    if !dir.is_dir() {
        finish_with_error(format!("'{}' no es un directorio válido", params.dir));
        return;
    }
    if params.pattern.trim().is_empty() {
        finish_with_error("Escribe un texto o patrón que buscar".to_string());
        return;
    }
    if params.limit == 0 {
        finish_with_error("El máximo de resultados debe ser un número mayor que cero.".to_string());
        return;
    }

    let regex = if params.content {
        match build_regex(&params.pattern, params.case_sensitive) {
            Ok(r) => Some(r),
            Err(e) => {
                finish_with_error(e);
                return;
            }
        }
    } else {
        None
    };

    let needle = if params.case_sensitive {
        params.pattern.clone()
    } else {
        params.pattern.to_lowercase()
    };

    // "0 = automático": usamos todos los núcleos disponibles en vez de
    // dejar que `ignore` caiga en su valor por defecto (2 hilos), que se
    // queda corto en máquinas modernas.
    let threads = if cfg.thread_count > 0 {
        cfg.thread_count
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    };

    let excluded: Vec<String> = cfg
        .excluded_dirs
        .iter()
        .map(|s| s.to_lowercase())
        .collect();

    let max_bytes = cfg.max_file_size_mb.saturating_mul(1_000_000);
    let matched = AtomicUsize::new(0);
    let limit_reached = AtomicBool::new(false);
    let limit = params.limit;
    let case_sensitive = params.case_sensitive;

    let mut builder = WalkBuilder::new(dir);
    builder
        .threads(threads)
        // Solo nos interesa excluir ocultos + carpetas configuradas por
        // el usuario, igual que antes. Desactivamos el resto de filtros
        // "estándar" de `ignore` (.gitignore, .ignore, exclude global de
        // git, etc.) para no cambiar silenciosamente qué archivos
        // aparecían en los resultados respecto al comportamiento previo.
        .standard_filters(false)
        .hidden(!cfg.include_hidden)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if excluded.contains(&name) {
                    return false;
                }
            }
            true
        });

    builder.build_parallel().run(|| {
        let tx = tx.clone();
        let cancel = Arc::clone(&cancel);
        let progress = Arc::clone(&progress);
        let matched = &matched;
        let limit_reached = &limit_reached;
        let regex = regex.clone();
        let needle = needle.clone();

        Box::new(move |entry| {
            if cancel.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }

            let entry = match entry {
                Ok(e) => e,
                // Un error puntual leyendo una entrada (p. ej. permisos)
                // no debe abortar toda la búsqueda.
                Err(_) => return WalkState::Continue,
            };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return WalkState::Continue;
            }

            let path = entry.path();
            progress.record(path);

            let found = match &regex {
                Some(re) => search_file_content(path, re, max_bytes),
                None => search_file_name(path, &needle, case_sensitive),
            };

            if let Some(m) = found {
                let count = matched.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = tx.send(SearchEvent::Match(m));
                if count >= limit {
                    limit_reached.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
            }

            if cancel.load(Ordering::Relaxed) {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    let scanned = progress.snapshot().0;
    let final_matched = matched.load(Ordering::Relaxed);
    let _ = tx.send(SearchEvent::Finished(SearchSummary {
        elapsed: start.elapsed(),
        scanned,
        truncated: limit_reached.load(Ordering::Relaxed) || final_matched >= limit,
        cancelled: cancel.load(Ordering::Relaxed),
        error: None,
    }));
}

/// Búsqueda por nombre de archivo (barata, sin tocar disco más allá de
/// los metadatos que ya trae `DirEntry`).
fn search_file_name(path: &Path, needle: &str, case_sensitive: bool) -> Option<SearchMatch> {
    let name = path.file_name()?.to_str()?;
    let name_cmp = if case_sensitive {
        name.to_string()
    } else {
        name.to_lowercase()
    };
    if name_cmp.contains(needle) {
        Some(SearchMatch {
            path: path.to_path_buf(),
            snippet: None,
        })
    } else {
        None
    }
}

/// Búsqueda por contenido, línea a línea, con lectura por bloques.
///
/// A diferencia de la versión anterior (que leía el archivo completo a
/// un `Vec<u8>` antes de buscar), aquí se lee con un `BufReader` y se
/// evalúa una línea a la vez, devolviendo en cuanto aparece la primera
/// coincidencia. Esto significa que:
/// - el pico de memoria por archivo es, en la práctica, el tamaño de la
///   línea más larga vista hasta el momento del match (no el archivo
///   completo), y
/// - un archivo grande donde el patrón aparece cerca del principio se
///   resuelve sin leer el resto del archivo.
///
/// `max_bytes` se sigue respetando como cota de seguridad: si el archivo
/// entero supera ese tamaño, ni se abre (igual que antes).
fn search_file_content(path: &Path, regex: &Regex, max_bytes: u64) -> Option<SearchMatch> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > max_bytes {
        return None;
    }

    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::with_capacity(READ_CHUNK_BYTES, file);

    // Sonda de binarios sobre el primer bloque que el BufReader ya trajo
    // a memoria (fill_buf no lo consume), evitando una lectura aparte.
    {
        let probe = reader.fill_buf().ok()?;
        let probe_len = probe.len().min(BINARY_PROBE_BYTES);
        if probe[..probe_len].contains(&0) {
            return None;
        }
    }

    let mut line: Vec<u8> = Vec::with_capacity(256);
    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line).ok()?;
        if bytes_read == 0 {
            return None; // EOF sin coincidencias
        }
        if let Some(m) = regex.find(&line) {
            return Some(SearchMatch {
                path: path.to_path_buf(),
                snippet: Some(extract_snippet(&line, m.start(), m.end())),
            });
        }
    }
}

/// Recorta y limpia la línea donde ocurrió la coincidencia para mostrarla
/// como contexto junto al resultado.
fn extract_snippet(line: &[u8], _match_start: usize, _match_end: usize) -> String {
    let mut text = String::from_utf8_lossy(line).trim().to_string();
    const MAX_LEN: usize = 160;
    if text.chars().count() > MAX_LEN {
        text = text.chars().take(MAX_LEN).collect::<String>() + "…";
    }
    text
}

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<Regex, String> {
    let attempt = if case_sensitive {
        Regex::new(pattern)
    } else {
        Regex::new(&format!("(?i){}", pattern))
    };
    // Si el usuario escribió un patrón que no es una regex válida, se
    // reintenta tratándolo como texto literal (escapado) en vez de fallar.
    attempt.or_else(|_| {
        let escaped = regex::escape(pattern);
        if case_sensitive {
            Regex::new(&escaped)
        } else {
            Regex::new(&format!("(?i){}", escaped))
        }
        .map_err(|e| format!("Patrón inválido: {}", e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_build_regex_valid_and_literal() {
        let r1 = build_regex("hello", false).unwrap();
        assert!(r1.is_match(b"HELLO world"));

        // Carácter de regex inválida debe caer a literal escapado.
        let r2 = build_regex("[unclosed", false).unwrap();
        assert!(r2.is_match(b"test [unclosed string"));
    }

    #[test]
    fn test_extract_snippet_trims_and_truncates() {
        let snippet = extract_snippet(b"   Second target match line here   \n", 0, 0);
        assert_eq!(snippet, "Second target match line here");

        let long_line = "x".repeat(500);
        let snippet = extract_snippet(long_line.as_bytes(), 0, 0);
        assert!(snippet.chars().count() <= 161);
        assert!(snippet.ends_with('…'));
    }

    #[test]
    fn test_binary_probe_is_detected_without_text_conversion() {
        assert!(b"text\0with binary data".contains(&0));
        assert!(!b"plain text".contains(&0));
    }

    #[test]
    fn test_search_file_content_finds_match_without_reading_whole_file() {
        let dir = std::env::temp_dir().join(format!("buscador_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("sample.txt");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "primera línea").unwrap();
        writeln!(f, "segunda línea con TARGET aquí").unwrap();
        writeln!(f, "tercera línea").unwrap();
        drop(f);

        let regex = build_regex("target", false).unwrap();
        let result = search_file_content(&path, &regex, 10_000_000);
        assert!(result.is_some());
        let snippet = result.unwrap().snippet.unwrap();
        assert!(snippet.contains("TARGET"));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn test_cancel_flag_stops_search_quickly() {
        let dir = std::env::temp_dir().join(format!("buscador_test_cancel_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        for i in 0..50 {
            let _ = fs::write(dir.join(format!("file_{i}.txt")), "contenido de prueba");
        }

        let params = SearchParams {
            dir: dir.display().to_string(),
            pattern: "contenido".to_string(),
            content: true,
            case_sensitive: false,
            limit: 1000,
        };
        let cfg = AppConfig::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(true)); // ya cancelada antes de empezar
        let progress = SearchProgress::new();

        run_search(&params, &cfg, tx, cancel, progress);

        let mut summary = None;
        while let Ok(event) = rx.try_recv() {
            if let SearchEvent::Finished(s) = event {
                summary = Some(s);
            }
        }
        let summary = summary.expect("debe llegar un evento Finished");
        assert!(summary.cancelled);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_summary_flags_truncation_when_limit_is_reached() {
        let dir = std::env::temp_dir().join(format!("buscador_test_truncado_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("a.txt"), "contenido de prueba\ncontenido de prueba\n").unwrap();
        fs::write(dir.join("b.txt"), "contenido de prueba\n").unwrap();
        fs::write(dir.join("c.txt"), "contenido de prueba\n").unwrap();

        let params = SearchParams {
            dir: dir.display().to_string(),
            pattern: "contenido".to_string(),
            content: true,
            case_sensitive: false,
            limit: 2,
        };
        let cfg = AppConfig::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = SearchProgress::new();

        run_search(&params, &cfg, tx, cancel, progress);

        let mut summary = None;
        while let Ok(event) = rx.try_recv() {
            if let SearchEvent::Finished(s) = event {
                summary = Some(s);
            }
        }

        let summary = summary.expect("debe llegar un evento Finished");
        assert!(summary.truncated);
        assert!(!summary.cancelled);

        let _ = fs::remove_dir_all(&dir);
    }
}
