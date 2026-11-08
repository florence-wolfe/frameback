//! Downloading videos from a URL with yt-dlp.
//!
//! Release builds bundle yt-dlp and an `ffmpeg` command beside the app's
//! executable (yt-dlp needs ffmpeg to merge the separate video and audio
//! streams most sites serve). Development builds use whatever is on PATH.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};

const PROGRESS_PREFIX: &str = "frameback-progress";

/// A tool bundled beside the executable, if this build has one.
fn bundled(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Fraction done from a progress line: downloaded bytes over the total (or
/// yt-dlp's estimate of it, when the size isn't known up front).
fn parse_progress(line: &str) -> Option<f32> {
    let mut fields = line.strip_prefix(PROGRESS_PREFIX)?.split_whitespace();
    let done: f64 = fields.next()?.parse().ok()?;
    let total = fields.filter_map(|f| f.parse::<f64>().ok()).find(|t| *t > 0.0)?;
    Some((done / total).clamp(0.0, 1.0) as f32)
}

/// yt-dlp's own explanation of a failure: its last `ERROR:` line.
fn error_message(stderr: &str) -> Option<&str> {
    stderr.lines().rev().find_map(|l| l.strip_prefix("ERROR: ")).map(str::trim)
}

/// Downloads `url` into `dir` and returns the file's path, or None if
/// cancelled. Partial files are kept in a temp folder, never in `dir`.
pub fn download(
    url: &str,
    dir: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(f32),
) -> Result<Option<PathBuf>> {
    // Each download gets its own folder for partial files, so two of the same
    // video can't collide, and whatever a cancel or failure leaves is removed.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = std::env::temp_dir().join(format!(
        "frameback-download-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    download_via(url, dir, &temp, cancel, progress)
}

/// Downloads using `temp` for partial files, and removes it afterwards.
fn download_via(
    url: &str,
    dir: &Path,
    temp: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(f32),
) -> Result<Option<PathBuf>> {
    let result = run(url, dir, temp, cancel, progress);
    let _ = std::fs::remove_dir_all(temp);
    result
}

fn run(
    url: &str,
    dir: &Path,
    temp: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f32),
) -> Result<Option<PathBuf>> {
    let yt_dlp = bundled("yt-dlp").unwrap_or_else(|| PathBuf::from("yt-dlp"));
    let mut command = Command::new(&yt_dlp);
    command
        .args(["--no-playlist", "--newline", "--quiet", "--progress"])
        .arg("--progress-template")
        .arg(format!(
            "download:{PROGRESS_PREFIX} %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s"
        ))
        .args(["--print", "after_move:filepath"])
        .arg("-P")
        .arg(dir)
        .arg("-P")
        .arg(format!("temp:{}", temp.display()))
        .args(["-o", "%(title).100B [%(id)s].%(ext)s"]);
    if let Some(ffmpeg) = bundled("ffmpeg") {
        command.arg("--ffmpeg-location").arg(ffmpeg);
        // The bundled ffmpeg finds the app's FFmpeg libraries through this.
        #[cfg(target_os = "linux")]
        if let Some(lib) = yt_dlp.parent().map(|dir| dir.join("lib")) {
            command.env("LD_LIBRARY_PATH", lib);
        }
    }
    // `--` so a URL can never be read as an option.
    command.arg("--").arg(url);
    let mut child = match command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("yt-dlp isn't installed (release builds include it; for development, install it and ffmpeg)")
        }
        Err(err) => return Err(err).context("starting yt-dlp"),
    };

    // Read both pipes off-thread, so this one can notice a cancel any time.
    let (lines_tx, lines) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stderr = child.stderr.take().unwrap();
    let stderr = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let mut path = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => match parse_progress(&line) {
                Some(fraction) => progress(fraction),
                None if !line.trim().is_empty() => path = Some(PathBuf::from(line.trim())),
                None => {}
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait().context("waiting for yt-dlp")?;
    let stderr = stderr.join().unwrap_or_default();
    if !status.success() {
        let why = error_message(&stderr).unwrap_or("yt-dlp failed");
        return Err(anyhow!("{why}"));
    }
    let path = path.filter(|p| p.is_file()).context("yt-dlp didn't report the downloaded file")?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_uses_the_total_or_its_estimate() {
        assert_eq!(parse_progress("frameback-progress 50 200 NA"), Some(0.25));
        assert_eq!(parse_progress("frameback-progress 50 NA 100"), Some(0.5));
        assert_eq!(parse_progress("frameback-progress 50 NA NA"), None);
        assert_eq!(parse_progress("/Users/me/Movies/clip [id].mkv"), None);
    }

    #[test]
    fn errors_come_from_the_last_error_line() {
        let stderr = "WARNING: slow\nERROR: [youtube] abc: Video unavailable\n";
        assert_eq!(error_message(stderr), Some("[youtube] abc: Video unavailable"));
        assert_eq!(error_message("WARNING: only a warning"), None);
    }

    #[test]
    #[ignore = "downloads from YouTube with yt-dlp; run with -- --ignored --nocapture"]
    fn downloads_a_video() {
        let dir = std::env::temp_dir().join(format!("frameback-download-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut last = 0.0;
        let path = download("https://www.youtube.com/watch?v=jNQXAC9IVRw", &dir, &AtomicBool::new(false), |p| last = p)
            .unwrap()
            .expect("not cancelled");
        println!("downloaded {}", path.display());
        assert!(path.starts_with(&dir), "{} not in {}", path.display(), dir.display());
        assert_eq!(last, 1.0, "progress reached the end");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "downloads from YouTube with yt-dlp; run with -- --ignored --nocapture"]
    fn cancelling_stops_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("frameback-cancel-test-{}", std::process::id()));
        let temp = dir.with_extension("partial");
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        // Cancel as soon as data starts arriving.
        let url = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
        let result = download_via(url, &dir, &temp, &cancel, |_| cancel.store(true, Ordering::Relaxed));
        assert!(result.unwrap().is_none(), "reported as cancelled");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "nothing in the destination");
        assert!(!temp.exists(), "partial files removed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
