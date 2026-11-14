use std::path::Path;

/// Directory filled by `Worse/scripts/stage-sidecar-resources.mjs` and bundled
/// as the `host/` resource on desktop (see `tauri.{windows,macos,linux}.conf.json`).
const STAGED_HOST_DIR: &str = "resources/host";

fn main() {
    // tauri-build 会拒绝不存在的 resources 路径；未执行暂存脚本时用空目录占位，
    // 保证普通 `cargo build` 不依赖打包步骤（空目录不会产生任何资源文件）。
    if !Path::new(STAGED_HOST_DIR).is_dir() {
        if let Err(error) = std::fs::create_dir_all(STAGED_HOST_DIR) {
            println!("cargo:warning=failed to create {STAGED_HOST_DIR}: {error}");
        }
    }
    // Phase 6 (updater B): the production updater key is compiled in via option_env!.
    println!("cargo:rerun-if-env-changed=HARNESS_UPDATER_PUBKEY");
    tauri_build::build();
}
