pub mod commands;

use tauri_specta::Builder;

pub fn run(builder: Builder<tauri::Wry>) {
    let result = tauri::Builder::default()
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            Ok(())
        })
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("o AdsConsole parou com erro: {e}");
        std::process::exit(1);
    }
}
