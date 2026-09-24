//! Self-updater: `ollama-shepherd update [--check]`
//!
//! Checks the latest GitHub release of this repo, downloads the matching
//! asset for the current platform, and replaces the running binary.
//! (Windows trick: the running exe can be *renamed* but not overwritten,
//! so we rename current → `.old`, put the new one in place, and exit.)

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

const REPO: &str = "gianni-bischoff/OllamaShepherd";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const TIMEOUT: Duration = Duration::from_secs(30);

/// Asset produced by the release pipeline for the current platform.
fn target_asset() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("ollama-shepherd-x86_64-pc-windows-msvc.zip")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("ollama-shepherd-x86_64-unknown-linux-gnu.tar.gz")
    } else {
        None
    }
}

/// "v0.2.1" → (0, 2, 1)
fn parse_version(v: &str) -> (u64, u64, u64) {
    let t = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let mut it = t.split('.');
    let major = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

fn current_version() -> (u64, u64, u64) {
    parse_version(VERSION)
}

/// Fetch the latest release: (tag, asset_url).
fn latest_release() -> Result<(String, String), String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = ureq::get(&url)
        .set("User-Agent", "ollama-shepherd-updater")
        .set("Accept", "application/vnd.github+json")
        .timeout(TIMEOUT)
        .call()
        .map_err(|e| format!("cannot reach GitHub: {e}"))?;

    let json: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("bad response from GitHub: {e}"))?;

    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("response missing tag_name")?
        .to_string();

    let asset_name = target_asset().ok_or("unsupported platform")?;
    let download = json
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(asset_name))
                .and_then(|a| a.get("browser_download_url"))
                .and_then(|u| u.as_str())
        })
        .ok_or(format!(
            "release {tag} has no asset '{asset_name}' — pipeline may still be running"
        ))?
        .to_string();

    Ok((tag, download))
}

/// Stream a URL to a file.
fn download(url: &str, dest: &std::path::Path) -> Result<(), String> {
    let resp = ureq::get(url)
        .set("User-Agent", "ollama-shepherd-updater")
        .timeout(Duration::from_secs(300))
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut reader = resp.into_reader();
    let mut file =
        std::fs::File::create(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    std::io::copy(&mut reader, &mut file).map_err(|e| format!("download failed: {e}"))?;
    file.flush().ok();
    Ok(())
}

fn exe_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot locate own binary: {e}"))
}

fn run_cmd(cmd: &str, args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| format!("failed to run {cmd}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with {status}"))
    }
}

pub fn run(check_only: bool) -> Result<(), String> {
    println!("🐑 ollama-shepherd v{VERSION} — checking for updates…");

    let (tag, url) = latest_release()?;
    let latest = parse_version(&tag);

    if latest <= current_version() {
        println!("✓ already up to date (v{VERSION} ≥ {tag})");
        return Ok(());
    }

    println!("⬆ update available: v{VERSION} → {tag}");
    if check_only {
        println!("  run `ollama-shepherd update` to install");
        return Ok(());
    }

    let asset = target_asset().unwrap_or_default();
    let exe = exe_path()?;
    let dir = exe.parent().ok_or("cannot resolve install dir")?;

    // work dirs live next to the binary so same-volume renames are atomic
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let archive = dir.join(format!(".shepherd-update-{uniq}.pkg"));
    let stage = dir.join(format!(".shepherd-update-{uniq}"));

    println!("⇣ downloading {asset}…");
    download(&url, &archive)?;

    // extract
    std::fs::create_dir_all(&stage).map_err(|e| format!("cannot stage: {e}"))?;
    if cfg!(target_os = "windows") {
        run_cmd(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                &format!(
                    "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                    archive.display(),
                    stage.display()
                ),
            ],
        )?;
    } else {
        run_cmd("tar", &["xzf", &archive.to_string_lossy(), "-C", &stage.to_string_lossy()])?;
    }

    // locate extracted binary
    let new_name = if cfg!(target_os = "windows") {
        "ollama-shepherd.exe"
    } else {
        "ollama-shepherd"
    };
    let mut new_bin = None;
    if let Ok(rd) = std::fs::read_dir(&stage) {
        for e in rd.flatten() {
            if e.file_name() == new_name {
                new_bin = Some(e.path());
                break;
            }
        }
    }
    let new_bin = new_bin.ok_or("archive did not contain the binary")?;

    // swap
    if cfg!(target_os = "windows") {
        let old = exe.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).map_err(|e| format!("rename failed: {e}"))?;
        std::fs::rename(&new_bin, &exe).map_err(|e| {
            // best effort rollback
            let _ = std::fs::rename(&old, &exe);
            format!("install failed: {e}")
        })?;
        let _ = std::fs::remove_file(&old); // may fail while running; fine
    } else {
        #[cfg(unix)]
        {
            std::fs::set_permissions(
                &new_bin,
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .map_err(|e| format!("chmod failed: {e}"))?;
        }
        std::fs::rename(&new_bin, &exe).map_err(|e| format!("install failed: {e}"))?;
    }

    // cleanup
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&stage);

    println!("✓ updated to {tag}");
    println!("  release notes: https://github.com/{REPO}/releases/tag/{tag}");
    println!("  restart ollama-shepherd to use the new version.");
    Ok(())
}