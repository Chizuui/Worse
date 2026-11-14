//! `harness-host` binary: NDJSON JSON-RPC on stdin/stdout, logs on stderr.
//!
//! Flags: `--stdio` (default, accepted for symmetry), `--version`.
//! Log filter: env `HARNESS_HOST_LOG` (default `info`).

use std::time::Duration;

use tracing_subscriber::EnvFilter;

/// 修复原因：portable-pty 用 `LoadLibrary("conpty.dll")` 加载 ConPTY，Windows 的 DLL 搜索顺序
/// 在 PATH 之前会查找进程当前目录；若 harness-host 的 cwd 落在不可信目录（例如用户打开的仓库），
/// 该目录里放置的 `conpty.dll` 会被加载执行。工作区禁止 unsafe，无法调用 `SetDllDirectoryW("")`，
/// 因此把 cwd 切到自身二进制所在目录（本就排在搜索顺序第一位），让 cwd 不再引入新的搜索位置。
/// 所有 RPC 都携带绝对路径，PTY/git 子进程显式设置 cwd，不依赖进程 cwd。
fn harden_dll_search_dir() {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    else {
        tracing::warn!("could not resolve harness-host directory; cwd left unchanged");
        return;
    };
    if let Err(error) = std::env::set_current_dir(&dir) {
        tracing::warn!(%error, dir = %dir.display(), "could not move cwd to the binary directory");
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("harness-host {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--stdio" => {}
            other => {
                eprintln!("harness-host: unknown argument '{other}'");
                std::process::exit(2);
            }
        }
    }
    let filter =
        EnvFilter::try_from_env("HARNESS_HOST_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("harness-host")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "failed to start tokio runtime");
            std::process::exit(1);
        }
    };
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        "harness-host started"
    );
    if cfg!(windows) {
        harden_dll_search_dir();
        match harness_pty::conpty_dll() {
            Some(dll) => tracing::info!(dll = %dll.display(), "pty: sideloaded ConPTY"),
            None => {
                tracing::info!("pty: inbox ConPTY (no conpty.dll beside the binary or on PATH)");
            }
        }
    }
    let reason = runtime.block_on(harness_host::serve(std::io::stdin(), std::io::stdout()));
    // 阻塞线程中可能仍有不可取消的 IO（如挂起的网络盘 readdir），不等待它们，保证 2 秒内退出。
    runtime.shutdown_timeout(Duration::from_millis(200));
    let code = i32::from(reason == harness_host::ExitReason::ReadError);
    std::process::exit(code);
}
