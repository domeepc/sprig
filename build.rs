// Only the desktop app (`--features ui`) needs a build step: Tauri reads its
// config and capabilities, which live in ui/ with the page.
fn main() {
    #[cfg(feature = "ui")]
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .config_path("ui/tauri.conf.json")
            .capabilities_path_pattern("ui/capabilities/**/*"),
    )
    .expect("tauri-build");
}
