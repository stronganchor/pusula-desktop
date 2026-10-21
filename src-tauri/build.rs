fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        panic!("Pusula Desktop supports only the x86_64-pc-windows-msvc target");
    }

    tauri_build::build()
}
