//! Updates from GitHub releases: find a newer release, download this
//! platform's build, check it against the release's SHA256SUMS, and install it
//! with a small script that runs after Frameback quits.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};
use sha2::{Digest, Sha256};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "florence-wolfe/frameback";
const CHECKSUMS: &str = "SHA256SUMS";

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    asset_name: String,
    asset_url: String,
    checksums_url: String,
}

/// Platform suffix and archive type of release assets.
fn platform() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        ("macos-arm64", "zip")
    } else if cfg!(target_os = "windows") {
        ("windows-x64", "zip")
    } else {
        ("linux-x64", "tar.gz")
    }
}

pub fn asset_name(version: &str) -> String {
    let (platform, ext) = platform();
    format!("Frameback-v{version}-{platform}.{ext}")
}

/// `1.2.3` or `v1.2.3`. Versions with a suffix (`1.2.3-rc.1`) are pre-releases
/// and never offered.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.strip_prefix('v').unwrap_or(version).split('.');
    let mut next = || parts.next()?.parse().ok();
    let parsed = (next()?, next()?, next()?);
    parts.next().is_none().then_some(parsed)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(c), Some(v)) if c > v)
}

/// Reads GitHub's latest-release JSON; Some if it's newer than `current` and
/// has a build for this platform plus checksums.
fn parse_release(json: &serde_json::Value, current: &str) -> Option<Release> {
    let tag = json["tag_name"].as_str()?;
    if !is_newer(tag, current) || json["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let version = tag.trim_start_matches('v').to_string();
    let asset_name = asset_name(&version);
    let assets = json["assets"].as_array()?;
    let url = |name: &str| {
        assets
            .iter()
            .find(|a| a["name"].as_str() == Some(name))?["browser_download_url"]
            .as_str()
            .map(str::to_string)
    };
    Some(Release {
        asset_url: url(&asset_name)?,
        checksums_url: url(CHECKSUMS)?,
        asset_name,
        version,
    })
}

fn get(url: &str) -> Result<ureq::http::Response<ureq::Body>> {
    Ok(ureq::get(url)
        .header("User-Agent", concat!("Frameback/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .call()?)
}

/// The newest release, if it's newer than this build.
pub fn check() -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut response = get(&url).context("checking for updates")?;
    let json: serde_json::Value = serde_json::from_str(&response.body_mut().read_to_string()?)?;
    Ok(parse_release(&json, VERSION))
}

/// The expected SHA-256 (hex) of `name` in a SHA256SUMS file.
fn expected_hash<'a>(checksums: &'a str, name: &str) -> Option<&'a str> {
    checksums.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then_some(hash)
    })
}

/// Downloads the release's build into `dir`, refusing it unless it matches SHA256SUMS.
pub fn download(release: &Release, dir: &Path) -> Result<PathBuf> {
    let checksums = get(&release.checksums_url)?.body_mut().read_to_string()?;
    let expected = expected_hash(&checksums, &release.asset_name)
        .with_context(|| format!("{} isn't in {CHECKSUMS}", release.asset_name))?
        .to_lowercase();

    std::fs::create_dir_all(dir)?;
    let path = dir.join(&release.asset_name);
    let mut file = std::fs::File::create(&path)?;
    let mut reader = get(&release.asset_url)?.into_body().into_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = reader.read(&mut buf).context("downloading update")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        let _ = std::fs::remove_file(&path);
        bail!("the download doesn't match its checksum");
    }
    Ok(path)
}

/// What an update replaces: the .app on macOS, the folder elsewhere. None for
/// development builds (run from cargo's target folder), which never update.
pub fn install_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    if cfg!(target_os = "macos") {
        // Frameback.app/Contents/MacOS/frameback
        let app = exe.parent()?.parent()?.parent()?;
        (app.extension()? == "app").then(|| app.to_path_buf())
    } else {
        exe.parent().map(Path::to_path_buf)
    }
}

/// Why the last update failed, left by the install script for the next launch.
fn failure_notice() -> PathBuf {
    std::env::temp_dir().join("frameback-update-failed.txt")
}

pub fn take_failure_notice() -> Option<String> {
    let path = failure_notice();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(path);
    Some(text.trim().to_string()).filter(|t| !t.is_empty())
}

const MACOS_INSTALL: &str = r#"
fail() { echo "$1" > "$NOTICE"; [ "$RELAUNCH" = 1 ] && open "$APP"; exit 1; }
while kill -0 "$APP_PID" 2>/dev/null; do sleep 0.2; done
staging=$(mktemp -d) || fail "Couldn't make a folder to unpack the update."
ditto -x -k "$ARCHIVE" "$staging" || fail "Couldn't unpack the update."
new=$(find "$staging" -maxdepth 1 -name '*.app' | head -n 1)
[ -n "$new" ] || fail "The update has no app in it."
rm -rf "$APP.old"
mv "$APP" "$APP.old" || fail "Couldn't move $APP aside."
if mv "$new" "$APP"; then
    rm -rf "$APP.old" "$staging" "$ARCHIVE"
else
    mv "$APP.old" "$APP"
    fail "Couldn't put the update in place."
fi
[ "$RELAUNCH" = 1 ] && open "$APP"
"#;

const LINUX_INSTALL: &str = r#"
fail() { echo "$1" > "$NOTICE"; [ "$RELAUNCH" = 1 ] && nohup "$APP/frameback" >/dev/null 2>&1 & exit 1; }
while kill -0 "$APP_PID" 2>/dev/null; do sleep 0.2; done
staging=$(mktemp -d) || fail "Couldn't make a folder to unpack the update."
tar -xzf "$ARCHIVE" -C "$staging" || fail "Couldn't unpack the update."
new="$staging/Frameback"
[ -x "$new/frameback" ] || fail "The update has no Frameback in it."
rm -rf "$APP.old"
mv "$APP" "$APP.old" || fail "Couldn't move $APP aside."
if mv "$new" "$APP"; then
    rm -rf "$APP.old" "$staging" "$ARCHIVE"
else
    mv "$APP.old" "$APP"
    fail "Couldn't put the update in place."
fi
[ "$RELAUNCH" = 1 ] && nohup "$APP/frameback" >/dev/null 2>&1 &
"#;

const WINDOWS_INSTALL: &str = r#"
param($AppPid, $Archive, $App, $Notice, $Relaunch)
function Fail($why) { Set-Content -Path $Notice -Value $why; if ($Relaunch -eq '1') { Start-Process (Join-Path $App 'frameback.exe') }; exit 1 }
Wait-Process -Id $AppPid -ErrorAction SilentlyContinue
$staging = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
try { Expand-Archive -Path $Archive -DestinationPath $staging -ErrorAction Stop } catch { Fail "Couldn't unpack the update." }
$new = Join-Path $staging 'Frameback'
if (-not (Test-Path (Join-Path $new 'frameback.exe'))) { Fail "The update has no Frameback in it." }
$old = "$App.old"
if (Test-Path $old) { Remove-Item -Recurse -Force $old }
# Windows can't move a folder while a file in it is open; retry while handles close
for ($i = 0; $i -lt 50; $i++) { try { Rename-Item -Path $App -NewName (Split-Path $old -Leaf) -ErrorAction Stop; break } catch { Start-Sleep -Milliseconds 200 } }
if (Test-Path $App) { Fail "Couldn't move $App aside." }
try { Move-Item -Path $new -Destination $App -ErrorAction Stop } catch { Rename-Item -Path $old -NewName (Split-Path $App -Leaf); Fail "Couldn't put the update in place." }
Remove-Item -Recurse -Force $old, $staging, $Archive -ErrorAction SilentlyContinue
if ($Relaunch -eq '1') { Start-Process (Join-Path $App 'frameback.exe') }
"#;

/// Starts a detached script that waits for this process to exit, replaces
/// `install` with the build in `archive`, and (if `relaunch`) reopens it.
pub fn install_after_exit(archive: &Path, install: &Path, relaunch: bool) -> Result<()> {
    install_after(std::process::id(), archive, install, relaunch)
}

fn install_after(pid: u32, archive: &Path, install: &Path, relaunch: bool) -> Result<()> {
    let work = std::env::temp_dir().join("frameback-update");
    std::fs::create_dir_all(&work)?;
    // Nobody sees the script run; keep its output for diagnosing failures.
    let log = std::fs::File::create(work.join("install.log"))?;
    let relaunch = if relaunch { "1" } else { "0" };
    let mut command = if cfg!(target_os = "windows") {
        let script = work.join("install.ps1");
        std::fs::write(&script, WINDOWS_INSTALL)?;
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(script);
        c.arg("-AppPid").arg(pid.to_string()).arg("-Archive").arg(archive);
        c.arg("-App").arg(install).arg("-Notice").arg(failure_notice());
        c.arg("-Relaunch").arg(relaunch);
        c
    } else {
        let script = work.join("install.sh");
        let body = if cfg!(target_os = "macos") { MACOS_INSTALL } else { LINUX_INSTALL };
        std::fs::write(&script, body)?;
        let mut c = Command::new("/bin/sh");
        c.arg(script).env("APP_PID", pid.to_string()).env("ARCHIVE", archive);
        c.env("APP", install).env("NOTICE", failure_notice()).env("RELAUNCH", relaunch);
        c
    };
    command.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    detach(&mut command);
    command.spawn().context("starting the update installer")?;
    Ok(())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Its own process group, so quitting Frameback (or its terminal) doesn't stop it.
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_skip_prereleases() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("v0.2.0-rc.1", "0.1.0"), "pre-releases are never offered");
        assert!(!is_newer("garbage", "0.1.0"));
    }

    fn release_json(tag: &str, assets: &[&str]) -> serde_json::Value {
        let assets: Vec<_> = assets
            .iter()
            .map(|n| serde_json::json!({"name": n, "browser_download_url": format!("https://x/{n}")}))
            .collect();
        serde_json::json!({"tag_name": tag, "prerelease": false, "assets": assets})
    }

    #[test]
    fn release_needs_this_platforms_build_and_checksums() {
        let mine = asset_name("0.2.0");
        let json = release_json("v0.2.0", &[&mine, "SHA256SUMS"]);
        let release = parse_release(&json, "0.1.0").unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.asset_url, format!("https://x/{mine}"));

        assert_eq!(parse_release(&release_json("v0.2.0", &[&mine]), "0.1.0"), None, "no checksums");
        assert_eq!(parse_release(&release_json("v0.2.0", &["SHA256SUMS"]), "0.1.0"), None, "no build");
        assert_eq!(parse_release(&json, "0.2.0"), None, "not newer");
    }

    #[test]
    fn checksums_are_looked_up_by_file_name() {
        let sums = "aaa  Frameback-v1.0.0-windows-x64.zip\nbbb *Frameback-v1.0.0-macos-arm64.zip\n";
        assert_eq!(expected_hash(sums, "Frameback-v1.0.0-macos-arm64.zip"), Some("bbb"));
        assert_eq!(expected_hash(sums, "Frameback-v1.0.0-windows-x64.zip"), Some("aaa"));
        assert_eq!(expected_hash(sums, "other.zip"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "runs the real install script; -- --ignored"]
    fn install_script_swaps_the_app_after_the_process_exits() {
        let dir = std::env::temp_dir().join(format!("frameback-install-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = dir.join("Frameback.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("version"), "old").unwrap();
        // The update: a zipped Frameback.app with a new version file.
        let new = dir.join("new/Frameback.app");
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("version"), "new").unwrap();
        let archive = dir.join("update.zip");
        let zipped = Command::new("ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(&new)
            .arg(&archive)
            .status()
            .unwrap();
        assert!(zipped.success());

        // Stand-in for Frameback: a process that exits after a moment.
        let mut running = Command::new("sleep").arg("1").spawn().unwrap();
        install_after(running.id(), &archive, &app, false).unwrap();
        assert_eq!(std::fs::read_to_string(app.join("version")).unwrap(), "old", "waits for exit");
        running.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        // Done once the new app is in place and the old one is cleaned up.
        while std::fs::read_to_string(app.join("version")).unwrap_or_default() != "new"
            || dir.join("Frameback.app.old").exists()
        {
            assert!(std::time::Instant::now() < deadline, "update wasn't installed");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
