//! Configuración de la aplicación: se guarda como JSON en un directorio
//! "config" ubicado junto al ejecutable, para que el usuario pueda
//! personalizar el comportamiento del buscador sin recompilar nada.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

const CONFIG_DIR_NAME: &str = "config";
const CONFIG_FILE_NAME: &str = "config.json";

fn default_page_size() -> usize {
    400
}

/// Parámetros configurables por el usuario y persistidos en disco.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    /// Directorio de búsqueda que se precarga al abrir la app.
    pub default_dir: String,
    /// Distinguir mayúsculas/minúsculas por defecto.
    pub case_sensitive: bool,
    /// Buscar en el contenido de los archivos en vez de en el nombre.
    pub search_content: bool,
    /// Mostrar solo el nombre del archivo en los resultados.
    pub basename_only: bool,
    /// Incluir archivos y carpetas ocultos en la búsqueda.
    #[serde(default)]
    pub include_hidden: bool,
    /// Máximo de resultados a devolver.
    pub max_results: usize,
    /// Cantidad de resultados por página en la vista de búsqueda.
    #[serde(default = "default_page_size")]
    pub page_size: usize,
    /// Tamaño máximo (en MB) de archivo a inspeccionar en búsqueda por contenido.
    pub max_file_size_mb: u64,
    /// Nombres de carpetas que se excluyen siempre del recorrido
    /// (mejora drásticamente el rendimiento en proyectos grandes).
    pub excluded_dirs: Vec<String>,
    /// Número de hilos a usar (0 = automático, según números disponibles).
    pub thread_count: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_dir: ".".to_string(),
            case_sensitive: false,
            search_content: false,
            basename_only: false,
            include_hidden: false,
            max_results: 300,
            page_size: 400,
            max_file_size_mb: 2,
            excluded_dirs: vec![
                ".git".into(),
                "node_modules".into(),
                "target".into(),
                "dist".into(),
                "build".into(),
                ".cache".into(),
                ".venv".into(),
            ],
            thread_count: 0,
        }
    }
}

/// Directorio "config" al lado del ejecutable actual.
fn config_dir() -> PathBuf {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(CONFIG_DIR_NAME)
}

fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE_NAME)
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_results == 0 {
            return Err("El máximo de resultados debe ser mayor que cero.".to_string());
        }
        if self.page_size == 0 {
            return Err("Los resultados por página deben ser mayores que cero.".to_string());
        }
        if self.max_file_size_mb == 0 {
            return Err("El tamaño máximo de archivo debe ser mayor que cero.".to_string());
        }
        if self.default_dir.trim().is_empty() {
            return Err("El directorio por defecto no puede estar vacío.".to_string());
        }
        Ok(())
    }

    /// Carga la configuración desde disco. Si no existe o está corrupta,
    /// devuelve (y no sobrescribe) los valores por defecto.
    pub fn load() -> Self {
        match fs::read_to_string(config_path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => AppConfig::default(),
        }
    }

    /// Guarda la configuración actual en `config/config.json`.
    pub fn save(&self) -> io::Result<PathBuf> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        let path = config_path();
        let text = serde_json::to_string_pretty(self)
            .map_err(io::Error::other)?;
        fs::write(&path, text)?;
        Ok(path)
    }

    pub fn config_file_path() -> PathBuf {
        config_path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.page_size, 400);
        assert_eq!(cfg.max_results, 300);
        assert_eq!(cfg.max_file_size_mb, 2);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_invalid_config_is_rejected() {
        let mut cfg = AppConfig {
            max_results: 0,
            ..AppConfig::default()
        };
        assert!(cfg.validate().is_err());

        cfg.max_results = 300;
        cfg.max_file_size_mb = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_backwards_compatibility_deserialization() {
        // JSON without page_size should deserialize with default page_size (400)
        let json = r#"{
            "default_dir": ".",
            "case_sensitive": false,
            "search_content": false,
            "basename_only": false,
            "max_results": 300,
            "max_file_size_mb": 2,
            "excluded_dirs": [".git"],
            "thread_count": 0
        }"#;

        let cfg: AppConfig = serde_json::from_str(json).expect("Failed to deserialize old config");
        assert_eq!(cfg.page_size, 400);
        assert!(!cfg.include_hidden);
    }
}
