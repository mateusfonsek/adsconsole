//! App desktop do AdsConsole. `core` tem a lógica sem Tauri; `shell` liga a lógica
//! à janela, aos comandos e à barra de menu.

pub mod core;
mod shell;

/// Caminho dos tipos gerados para a UI, relativo a este crate.
const BINDINGS: &str = "../../ui/src/bindings.ts";

pub fn run() {
    let builder = shell::commands::builder();
    if std::env::args().any(|a| a == "--export-bindings") {
        export(&builder);
        return;
    }
    #[cfg(debug_assertions)]
    export(&builder);
    shell::run(builder, std::env::args().any(|a| a == "--demo"));
}

fn export(builder: &tauri_specta::Builder<::tauri::Wry>) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(BINDINGS);
    let ts = specta_typescript::Typescript::default()
        .header("// Gerado pelo tauri-specta. Não edite.\n");
    if let Err(e) = builder.export(ts, &path) {
        eprintln!("falha ao gerar {}: {e}", path.display());
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    /// Todo tipo exposto à UI precisa ser exportável para TypeScript.
    #[test]
    fn bindings_export() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bindings.ts");
        super::shell::commands::builder()
            .export(specta_typescript::Typescript::default(), &path)
            .unwrap();
        let ts = std::fs::read_to_string(path).unwrap();
        assert!(ts.contains("listPending"));
        assert!(ts.contains("app-event-msg"));
    }
}
