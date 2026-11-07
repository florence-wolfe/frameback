//! Files on the system clipboard. GPUI's clipboard covers text and images;
//! a video has to go on as a file so pasting into Finder or a chat app
//! attaches it.

use std::path::Path;

/// Replaces the clipboard with `path` as a file. Returns false where this
/// isn't supported yet.
#[cfg(target_os = "macos")]
pub fn copy_file(path: &Path) -> bool {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSURL};

    let Some(url) = NSURL::from_file_path(path) else { return false };
    let item = ProtocolObject::<dyn NSPasteboardWriting>::from_retained(url);
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    pasteboard.writeObjects(&NSArray::from_retained_slice(&[item]))
}

#[cfg(not(target_os = "macos"))]
pub fn copy_file(_path: &Path) -> bool {
    false
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    #[ignore = "replaces the system clipboard; run with -- --ignored"]
    fn copied_file_is_on_the_pasteboard_as_a_file() {
        let path = std::env::temp_dir().join("frameback-clipboard-test.mp4");
        std::fs::write(&path, b"test").unwrap();
        assert!(super::copy_file(&path));
        let out = std::process::Command::new("osascript")
            .args(["-e", "POSIX path of (the clipboard as «class furl»)"])
            .output()
            .unwrap();
        let pasted = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let (pasted, expected) = (std::fs::canonicalize(&pasted), std::fs::canonicalize(&path));
        std::fs::remove_file(&path).ok();
        assert_eq!(pasted.unwrap(), expected.unwrap());
    }
}
