#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SegmentError {
    #[error("the stream carries a box header of {0} bytes, which is shorter than a header")]
    ShortHeader(usize),
    #[error("a box declares {size} bytes but the stream carries {available}")]
    TruncatedBox { size: usize, available: usize },
    #[error("the stream carries no fragment to segment")]
    NoFragments,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segmented {
    pub init: Vec<u8>,
    pub segments: Vec<Vec<u8>>,
}

fn box_kind(bytes: &[u8], start: usize) -> Result<[u8; 4], SegmentError> {
    let end = start + 8;
    if end > bytes.len() {
        return Err(SegmentError::TruncatedBox {
            size: 8,
            available: bytes.len().saturating_sub(start),
        });
    }
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&bytes[start + 4..end]);
    Ok(kind)
}

fn box_size(bytes: &[u8], start: usize) -> Result<usize, SegmentError> {
    if start + 8 > bytes.len() {
        return Err(SegmentError::TruncatedBox {
            size: 8,
            available: bytes.len().saturating_sub(start),
        });
    }
    let declared = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
    Ok(match declared {
        0 => bytes.len() - start,
        1 => {
            if start + 16 > bytes.len() {
                return Err(SegmentError::ShortHeader(16));
            }
            u64::from_be_bytes(bytes[start + 8..start + 16].try_into().unwrap()) as usize
        }
        other => other,
    })
}

pub fn top_level_boxes(bytes: &[u8]) -> Result<Vec<Fragment>, SegmentError> {
    let mut boxes = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if offset + 8 > bytes.len() {
            return Err(SegmentError::TruncatedBox {
                size: 8,
                available: bytes.len() - offset,
            });
        }
        let size = box_size(bytes, offset)?;
        if size < 8 || offset + size > bytes.len() {
            return Err(SegmentError::TruncatedBox {
                size,
                available: bytes.len() - offset,
            });
        }
        boxes.push(Fragment {
            start: offset,
            end: offset + size,
        });
        offset += size;
    }
    Ok(boxes)
}

pub fn split_fragments(bytes: &[u8]) -> Result<Segmented, SegmentError> {
    let boxes = top_level_boxes(bytes)?;
    let mut init = Vec::new();
    let mut segments = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    for fragment in &boxes {
        let kind = box_kind(bytes, fragment.start)?;
        match &kind {
            b"ftyp" | b"moov" | b"styp" | b"sidx" | b"free" => {
                if segments.is_empty() && current.is_empty() {
                    init.extend_from_slice(&bytes[fragment.start..fragment.end]);
                    continue;
                }
                current.extend_from_slice(&bytes[fragment.start..fragment.end]);
            }
            b"moof" | b"mdat" => current.extend_from_slice(&bytes[fragment.start..fragment.end]),
            _ => current.extend_from_slice(&bytes[fragment.start..fragment.end]),
        }
        if &kind == b"mdat" && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if segments.is_empty() {
        return Err(SegmentError::NoFragments);
    }
    Ok(Segmented { init, segments })
}
impl Segmented {
    pub fn names(&self, start: usize) -> Vec<String> {
        (0..self.segments.len())
            .map(|index| format!("seg{:05}.m4s", index + start))
            .collect()
    }

    pub fn write_to(&self, directory: &std::path::Path) -> std::io::Result<Written> {
        std::fs::create_dir_all(directory)?;
        let init = directory.join("init.mp4");
        std::fs::write(&init, &self.init)?;
        let mut segments = Vec::with_capacity(self.segments.len());
        for (index, segment) in self.segments.iter().enumerate() {
            let path = directory.join(format!("seg{index:05}.m4s"));
            std::fs::write(&path, segment)?;
            segments.push(path);
        }
        Ok(Written { init, segments })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub init: std::path::PathBuf,
    pub segments: Vec<std::path::PathBuf>,
}
