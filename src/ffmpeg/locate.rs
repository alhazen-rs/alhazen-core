//! Finding the user's `ffmpeg` and what it can decode (`-version`, `-decoders`), cached per path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

/// The oldest ffmpeg we drive (`-hwaccel auto`, y4m output and the options we pass exist since 4.0).
pub const MIN_VERSION: (u32, u32) = (4, 0);

#[derive(Debug)]
pub struct FfmpegInfo {
    pub path: PathBuf,
    /// `None` for development builds (`N-12345-g…`), which are accepted as recent.
    pub version: Option<(u32, u32)>,
    pub decoders: HashSet<String>,
}

impl FfmpegInfo {
    pub fn has_decoder(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.decoders.contains(*n))
    }

    /// `-fps_mode` replaced `-vsync` in ffmpeg 5.1.
    pub fn passthrough_args(&self) -> [&'static str; 2] {
        match self.version {
            Some(v) if v < (5, 1) => ["-vsync", "passthrough"],
            _ => ["-fps_mode", "passthrough"],
        }
    }
}

/// Which `ffmpeg` programs to try, in order: `explicit`, else `$ALHAZEN_FFMPEG`, else `ffmpeg`
/// on `PATH` and then the standard install locations that `PATH` may lack.
pub fn candidates(explicit: Option<&Path>) -> Vec<PathBuf> {
    let env = std::env::var_os("ALHAZEN_FFMPEG").filter(|p| !p.is_empty()).map(PathBuf::from);
    candidates_for(std::env::consts::OS, explicit, env)
}

/// `candidates` for a given OS, explicit path and `$ALHAZEN_FFMPEG` value.
pub fn candidates_for(os: &str, explicit: Option<&Path>, env: Option<PathBuf>) -> Vec<PathBuf> {
    if let Some(p) = explicit {
        return vec![p.to_owned()];
    }
    if let Some(p) = env {
        return vec![p];
    }
    let mut out = vec![PathBuf::from("ffmpeg")];
    // Apps started from Finder/Dock get launchd's PATH, without Homebrew's or MacPorts' bin.
    if os == "macos" {
        out.extend(["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/opt/local/bin/ffmpeg"].map(PathBuf::from));
    }
    out
}

/// The first of `candidates(explicit)` that is a usable ffmpeg.
pub fn find(explicit: Option<&Path>) -> Option<Arc<FfmpegInfo>> {
    candidates(explicit).iter().find_map(|p| probe(p))
}

/// Runs `path -version` / `path -decoders` once per process for a usable ffmpeg; `None` if it is
/// missing, fails, or is older than [`MIN_VERSION`]. Failures are not remembered, so an ffmpeg
/// installed while the app runs is found by the next lookup.
pub fn probe(path: &Path) -> Option<Arc<FfmpegInfo>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<FfmpegInfo>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(path) {
        return Some(hit.clone());
    }
    let info = probe_uncached(path);
    match &info {
        Some(i) => log::info!("ffmpeg {:?} at {} with {} decoders", i.version, path.display(), i.decoders.len()),
        None => log::info!("no usable ffmpeg at {}", path.display()),
    }
    if let Some(i) = &info {
        cache.lock().unwrap().insert(path.to_owned(), i.clone());
    }
    info
}

fn probe_uncached(path: &Path) -> Option<Arc<FfmpegInfo>> {
    let version_text = run(path, "-version")?;
    let version = parse_version(&version_text)?;
    if version.is_some_and(|v| v < MIN_VERSION) {
        log::warn!("ffmpeg at {} is {version:?}; at least {MIN_VERSION:?} is required", path.display());
        return None;
    }
    let decoders = parse_decoders(&run(path, "-decoders")?);
    Some(Arc::new(FfmpegInfo { path: path.to_owned(), version, decoders }))
}

fn run(path: &Path, arg: &str) -> Option<String> {
    let mut cmd = Command::new(path);
    cmd.args(["-hide_banner", arg]).stdin(Stdio::null()).stderr(Stdio::null());
    super::process::no_window(&mut cmd);
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `Some(Some((major, minor)))` for releases, `Some(None)` for development builds, `None` if the
/// text is not ffmpeg's `-version` output.
pub fn parse_version(text: &str) -> Option<Option<(u32, u32)>> {
    let rest = text.lines().next()?.strip_prefix("ffmpeg version ")?;
    let v = rest.split_whitespace().next()?;
    // Release tags look like `6.1.1`, `n6.1`, `4.4.2-0ubuntu0.22.04.1`; git builds `N-112233-g…`.
    if v.starts_with("N-") || v.starts_with("git") {
        return Some(None);
    }
    let v = v.trim_start_matches('n');
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some(Some((major, minor)))
}

/// Decoder names from `ffmpeg -decoders` (lines after the `------` separator: ` V....D name  desc`).
pub fn parse_decoders(text: &str) -> HashSet<String> {
    text.lines()
        .skip_while(|l| !l.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let flags = it.next()?;
            (flags.len() == 6).then(|| it.next().map(str::to_owned))?
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_distro_and_git_versions() {
        assert_eq!(parse_version("ffmpeg version 9.0 Copyright (c) 2000-2026"), Some(Some((9, 0))));
        assert_eq!(parse_version("ffmpeg version n6.1.1 Copyright"), Some(Some((6, 1))));
        assert_eq!(parse_version("ffmpeg version 4.4.2-0ubuntu0.22.04.1 Copyright"), Some(Some((4, 4))));
        assert_eq!(parse_version("ffmpeg version 7 Copyright"), Some(Some((7, 0))));
        assert_eq!(parse_version("ffmpeg version N-112233-gdeadbeef Copyright"), Some(None));
        assert_eq!(parse_version("ffprobe version 6.0"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn parses_decoder_list() {
        let text = "Decoders:\n V..... = Video\n A..... = Audio\n ------\n V....D h264                 H.264 / AVC\n VFS..D hevc                 HEVC\n A....D aac                  AAC (Advanced Audio Coding)\n V....D libdav1d             dav1d AV1 decoder\n";
        let d = parse_decoders(text);
        assert!(["h264", "hevc", "aac", "libdav1d"].iter().all(|n| d.contains(*n)), "{d:?}");
        assert!(!d.contains("Video") && !d.contains("="));
    }

    #[test]
    fn missing_binary_is_none() {
        assert!(probe(Path::new("/nonexistent/ffmpeg-for-alhazen-core-tests")).is_none());
    }

    #[test]
    fn explicit_path_wins() {
        assert_eq!(candidates_for("linux", Some(Path::new("/opt/ff")), Some("/env".into())), [PathBuf::from("/opt/ff")]);
        assert_eq!(candidates_for("macos", None, Some("/env".into())), [PathBuf::from("/env")]);
    }

    #[test]
    fn macos_also_tries_homebrew_and_macports() {
        let c = candidates_for("macos", None, None);
        assert_eq!(c[0], PathBuf::from("ffmpeg"));
        assert!(c.contains(&PathBuf::from("/opt/homebrew/bin/ffmpeg")));
        assert!(c.contains(&PathBuf::from("/usr/local/bin/ffmpeg")));
        assert_eq!(candidates_for("linux", None, None), [PathBuf::from("ffmpeg")]);
    }
}
