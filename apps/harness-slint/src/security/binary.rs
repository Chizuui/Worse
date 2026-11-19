//! osv-scanner acquisition (spec `docs/specs/harness/security-panel.md` §3.1): pinned release asset per platform,
//! SHA-256 verified against the release checksum file, installed under `<HARNESS_HOME>/bin/tools/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Pinned `google/osv-scanner` release this build knows the asset names and flags of.
pub const VERSION: &str = "2.7.1";

const BASE: &str = "https://github.com/google/osv-scanner/releases/download";
const INSTALL_LIMIT: Duration = Duration::from_secs(600);

/// Release asset for the running platform, e.g. `osv-scanner_windows_amd64.exe`.
pub fn asset_name() -> Result<String, String> {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "darwin",
        "linux" => "linux",
        other => return Err(format!("no osv-scanner build for {other}")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => return Err(format!("no osv-scanner build for {other}")),
    };
    let ext = if os == "windows" { ".exe" } else { "" };
    Ok(format!("osv-scanner_{os}_{arch}{ext}"))
}

fn exe_name() -> String {
    if cfg!(windows) {
        format!("osv-scanner-{VERSION}.exe")
    } else {
        format!("osv-scanner-{VERSION}")
    }
}

fn tools_dir(home: &Path) -> PathBuf {
    home.join("bin").join("tools")
}

/// Installed binary path, whether or not it exists.
#[must_use]
pub fn path(home: &Path) -> PathBuf {
    tools_dir(home).join(exe_name())
}

/// The installed binary, when a previous download left one behind.
#[must_use]
pub fn installed(home: &Path) -> Option<PathBuf> {
    let path = path(home);
    path.is_file().then_some(path)
}

/// Deletes only the pinned binary; `Ok(false)` when there was nothing to delete.
pub fn remove(home: &Path) -> Result<bool, String> {
    let path = path(home);
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path).map_err(|e| format!("cannot remove the scanner: {e}"))?;
    Ok(true)
}

// SHA-256 state letters are the FIPS 180-4 names (a..h, plus `t1`/`t2`); renaming them would hide the standard.
#[allow(clippy::many_single_char_names)]
#[allow(clippy::too_many_lines)]
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().fold(String::with_capacity(64), |mut out, word| {
        use std::fmt::Write as _;
        let _ = write!(out, "{word:08x}");
        out
    })
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("download failed: HTTP {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("download failed: {e}"))
}

fn client() -> Result<reqwest::Client, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("cannot start the download: {e}"))
}

/// Downloads, verifies and installs the pinned binary. Nothing is installed on any failure.
pub async fn ensure(home: &Path) -> Result<PathBuf, String> {
    if let Some(found) = installed(home) {
        return Ok(found);
    }
    let work = async {
        let asset = asset_name()?;
        let client = client()?;
        let bytes = fetch(&client, &format!("{BASE}/v{VERSION}/{asset}")).await?;
        let sums = String::from_utf8_lossy(
            &fetch(
                &client,
                &format!("{BASE}/v{VERSION}/osv-scanner_SHA256SUMS"),
            )
            .await?,
        )
        .into_owned();
        let expected = sums
            .lines()
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                let hash = parts.next()?;
                parts
                    .any(|field| field.trim_start_matches('*') == asset)
                    .then(|| hash.to_ascii_lowercase())
            })
            .ok_or_else(|| format!("{asset} is not in the checksum file"))?;
        if sha256_hex(&bytes) != expected {
            return Err(format!(
                "checksum mismatch for {asset}; nothing was installed"
            ));
        }
        install_bytes(home, &bytes)
    };
    tokio::time::timeout(INSTALL_LIMIT, work)
        .await
        .map_err(|_| "the scanner download did not finish within 10 minutes".to_owned())?
}

fn install_bytes(home: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let tools = tools_dir(home);
    std::fs::create_dir_all(&tools)
        .map_err(|e| format!("cannot create {}: {e}", tools.display()))?;
    let target = path(home);
    let part = target.with_extension("part");
    std::fs::write(&part, bytes).map_err(|e| format!("cannot write the scanner: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot make the scanner executable: {e}"))?;
    }
    std::fs::rename(&part, &target).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("cannot install the scanner: {e}")
    })?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 1000]),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn checksum_file_lines_match_the_asset() {
        // The real file is `<sha256>  <asset>`; `parts.any(...)` also accepts the `*<asset>` binary marker.
        let line = "7121dcf420aefbbf89a7f0050c4d3094e079ff93afd334dfc96f16b02ec85800  osv-scanner_windows_amd64.exe";
        let mut parts = line.split_whitespace();
        let hash = parts.next().unwrap_or_default();
        assert_eq!(hash.len(), 64);
        assert!(parts.any(|field| field.trim_start_matches('*') == "osv-scanner_windows_amd64.exe"));
        assert!(!line
            .split_whitespace()
            .skip(1)
            .any(|field| field.trim_start_matches('*') == "osv-scanner_linux_amd64"));
    }

    #[test]
    fn asset_name_covers_this_platform() {
        let name = asset_name();
        if cfg!(any(windows, target_os = "linux", target_os = "macos"))
            && cfg!(any(target_arch = "x86_64", target_arch = "aarch64"))
        {
            let name = name.unwrap_or_default();
            assert!(name.starts_with("osv-scanner_"));
            assert_eq!(
                std::path::Path::new(&name)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("exe")),
                cfg!(windows)
            );
        }
    }

    /// Network test, opt-in: `cargo test -p harness-slint -- --ignored download_and_verify`.
    #[test]
    #[ignore = "downloads the pinned release (~60 MB)"]
    fn download_and_verify_installs_the_pinned_release() {
        let home = std::env::temp_dir().join("harness-osv-test");
        let _ = std::fs::remove_dir_all(&home);
        let Ok(runtime) = tokio::runtime::Runtime::new() else {
            return;
        };
        let path = match runtime.block_on(ensure(&home)) {
            Ok(path) => path,
            Err(error) => panic!("ensure failed: {error}"),
        };
        assert!(path.is_file());
        assert_eq!(path, super::path(&home));
        assert!(path.to_string_lossy().contains(VERSION));
        // A second call reuses the installed binary instead of downloading it again.
        let Ok(again) = runtime.block_on(ensure(&home)) else {
            return;
        };
        assert_eq!(again, path);
        let _ = std::fs::remove_dir_all(&home);
    }
}
