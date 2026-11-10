use std::path::Path;

pub fn open_with_default_app(path: &Path) -> Result<(), String> {
    open::that(path).map_err(|e| e.to_string())
}

pub fn open_in_browser(path: &Path) -> Result<(), String> {
    let url = path_to_file_url(path)?;
    webbrowser::open(&url).map_err(|e| e.to_string())
}

/// Extracts a single thumbnail frame (as JPEG bytes) from a video file using ffmpeg CLI.
pub fn extract_frame_bytes(path: &Path) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;

    let path_str = path.to_str().ok_or("Invalid path")?;
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-ss",
        "00:00:01",
        "-i",
        path_str,
        "-vframes",
        "1",
        "-f",
        "image2",
        "-c:v",
        "mjpeg",
        "pipe:1",
    ]);

    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

    let output = cmd.output().map_err(|e| format!("ffmpeg execution failed: {e}"))?;
    if output.status.success() && !output.stdout.is_empty() {
        Ok(output.stdout)
    } else {
        Err("ffmpeg returned non-zero or empty output".to_string())
    }
}

fn path_to_file_url(path: &Path) -> Result<String, String> {
    let absolute = path
        .canonicalize()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .replace('\\', "/");
    let absolute = absolute.strip_prefix("//?/").unwrap_or(&absolute);
    if absolute.starts_with('/') {
        Ok(format!("file://{absolute}"))
    } else {
        Ok(format!("file:///{absolute}"))
    }
}

