use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct PackageRequest {
    pub input: String,
    pub output_dir: PathBuf,
    pub segment_stem: PathBuf,
    pub segment_seconds: f64,
}

impl PackageRequest {
    pub fn new(
        input: String,
        output_dir: PathBuf,
        segment_stem: PathBuf,
        segment_seconds: f64,
    ) -> Self {
        Self {
            input,
            output_dir,
            segment_stem,
            segment_seconds,
        }
    }

    pub fn stream_location(&self) -> String {
        self.segment_stem
            .with_extension("frag.mp4")
            .to_string_lossy()
            .into_owned()
    }

    pub fn fragment_milliseconds(&self) -> u64 {
        (self.segment_seconds.abs() * 1000.0).round() as u64
    }

    pub fn produced_paths(&self) -> ProducedPaths {
        let stem = self
            .segment_stem
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.output_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().starts_with(&stem))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        let init = found
            .first()
            .cloned()
            .unwrap_or_else(|| self.segment_stem.clone());
        let segments = found.into_iter().skip(1).collect();
        ProducedPaths { init, segments }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducedPaths {
    pub init: PathBuf,
    pub segments: Vec<PathBuf>,
}

pub fn playlist_name() -> &'static str {
    "playlist.m3u8"
}

pub fn render_playlist(init: Option<&str>, segment_seconds: f64, segments: &[String]) -> String {
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    if let Some(init) = init {
        playlist.push_str(&format!("#EXT-X-MAP:URI=\"{init}\"\n"));
    }
    playlist.push_str("#EXT-X-TARGETDURATION:");
    playlist.push_str(&segment_seconds.ceil().to_string());
    playlist.push('\n');
    for name in segments {
        playlist.push_str("#EXTINF:");
        playlist.push_str(&format!("{segment_seconds:.3}"));
        playlist.push_str(",\n");
        playlist.push_str(name);
        playlist.push('\n');
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    playlist
}
