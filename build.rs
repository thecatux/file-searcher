use std::{env, io};
use winresource::WindowsResource;

fn main() -> io::Result<()> {
    // Solo ejecutar si estamos compilando para Windows
    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        WindowsResource::new()
            // La ruta puede ser absoluta o relativa a la raíz del proyecto
            .set_icon("assets/icon_256.ico")
            .compile()?;
    }
    Ok(())
}