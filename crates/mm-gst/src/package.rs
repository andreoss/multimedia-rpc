use thiserror::Error;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantDescriptor {
    uri: String,
    bandwidth_bps: u32,
    average_bandwidth_bps: Option<u32>,
    width: u32,
    height: u32,
    codecs: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MasterPlaylistError {
    #[error("a master playlist needs at least one rendition")]
    NoRenditions,
    #[error("rendition {uri:?} has no uri")]
    MissingUri { uri: String },
    #[error("rendition {uri:?} has no bandwidth")]
    MissingBandwidth { uri: String },
    #[error("rendition {uri:?} names no resolution")]
    MissingResolution { uri: String },
    #[error("renditions are not in ascending bandwidth: {previous} then {next}")]
    NotAscending { previous: String, next: String },
}

impl VariantDescriptor {
    pub fn new(uri: &str, bandwidth_bps: u32, width: u32, height: u32) -> Self {
        Self {
            uri: uri.to_string(),
            bandwidth_bps,
            average_bandwidth_bps: None,
            width,
            height,
            codecs: None,
        }
    }

    pub fn with_average_bandwidth(mut self, average_bandwidth_bps: u32) -> Self {
        self.average_bandwidth_bps = Some(average_bandwidth_bps);
        self
    }

    pub fn with_codecs(mut self, codecs: &str) -> Self {
        self.codecs = Some(codecs.to_string());
        self
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn bandwidth_bps(&self) -> u32 {
        self.bandwidth_bps
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

pub fn master_playlist_name() -> &'static str {
    "master.m3u8"
}

pub fn render_master_playlist(
    variants: &[VariantDescriptor],
) -> Result<String, MasterPlaylistError> {
    if variants.is_empty() {
        return Err(MasterPlaylistError::NoRenditions);
    }
    let mut playlist = String::from(
        "#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
",
    );
    for (index, variant) in variants.iter().enumerate() {
        if variant.uri.is_empty() {
            return Err(MasterPlaylistError::MissingUri {
                uri: variant.uri.clone(),
            });
        }
        if variant.bandwidth_bps == 0 {
            return Err(MasterPlaylistError::MissingBandwidth {
                uri: variant.uri.clone(),
            });
        }
        if variant.width == 0 || variant.height == 0 {
            return Err(MasterPlaylistError::MissingResolution {
                uri: variant.uri.clone(),
            });
        }
        if index > 0 && variants[index - 1].bandwidth_bps >= variant.bandwidth_bps {
            return Err(MasterPlaylistError::NotAscending {
                previous: variants[index - 1].uri.clone(),
                next: variant.uri.clone(),
            });
        }
        playlist.push_str("#EXT-X-STREAM-INF:BANDWIDTH=");
        playlist.push_str(&variant.bandwidth_bps.to_string());
        if let Some(average) = variant.average_bandwidth_bps {
            playlist.push_str(",AVERAGE-BANDWIDTH=");
            playlist.push_str(&average.to_string());
        }
        playlist.push_str(&format!(",RESOLUTION={}x{}", variant.width, variant.height));
        if let Some(codecs) = &variant.codecs {
            playlist.push_str(",CODECS=\"");
            playlist.push_str(codecs);
            playlist.push('"');
        }
        playlist.push('\n');
        playlist.push_str(&variant.uri);
        playlist.push('\n');
    }
    playlist.push_str(
        "#EXT-X-ENDLIST
",
    );
    Ok(playlist)
}
