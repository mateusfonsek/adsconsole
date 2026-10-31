fn main() {
    // Repositório usado nos links da UI e na verificação de versão nova.
    let repo =
        std::env::var("ADSCONSOLE_REPO").unwrap_or_else(|_| "mateusfonsek/adsconsole".into());
    println!("cargo:rustc-env=ADSCONSOLE_REPO={repo}");
    println!("cargo:rerun-if-env-changed=ADSCONSOLE_REPO");
    tauri_build::build();
}
