use thiserror::Error;

use crate::op::ScopeMode;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScopeError {
    #[error("a frame scope carries no measurements")]
    NotMeasured { mode: ScopeMode },
    #[error("a frame with no {dimension} is not a frame")]
    ZeroFrameDimension { dimension: &'static str },
    #[error("a frame of {expected} bytes was given {found}")]
    FrameSize { expected: usize, found: usize },
    #[error("a scope grid carries no {field}")]
    ZeroGrid { field: &'static str },
    #[error("a scope grid of {columns} columns and {bins} bins does not fit in memory")]
    GridOverflow { columns: u32, bins: u32 },
    #[error("a scope grid of {expected} counts was given {found}")]
    GridMismatch { expected: usize, found: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, ScopeError> {
        if width == 0 {
            return Err(ScopeError::ZeroFrameDimension { dimension: "width" });
        }
        if height == 0 {
            return Err(ScopeError::ZeroFrameDimension {
                dimension: "height",
            });
        }
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(usize::try_from(height).ok()?))
            .and_then(|pixels| pixels.checked_mul(3));
        match expected {
            Some(expected) if expected == pixels.len() => Ok(Self {
                width,
                height,
                pixels,
            }),
            _ => Err(ScopeError::FrameSize {
                expected: expected.unwrap_or(usize::MAX),
                found: pixels.len(),
            }),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixel(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let offset = ((y as usize * self.width as usize) + x as usize) * 3;
        (
            self.pixels[offset],
            self.pixels[offset + 1],
            self.pixels[offset + 2],
        )
    }
}

fn luma(red: u8, green: u8, blue: u8) -> u32 {
    let red = f64::from(red);
    let green = f64::from(green);
    let blue = f64::from(blue);
    (0.299 * red + 0.587 * green + 0.114 * blue)
        .round()
        .clamp(0.0, 255.0) as u32
}

const NEUTRAL_CHROMA: f64 = 1.0;

fn hue_bin(red: u8, green: u8, blue: u8, bins: u32) -> u32 {
    let luma = 0.299 * f64::from(red) + 0.587 * f64::from(green) + 0.114 * f64::from(blue);
    let u = (f64::from(blue) - luma) / 1.772;
    let v = (f64::from(red) - luma) / 1.402;
    if u.hypot(v) < NEUTRAL_CHROMA {
        return 0;
    }
    let hue = v.atan2(u).rem_euclid(std::f64::consts::TAU);
    ((hue * f64::from(bins) / std::f64::consts::TAU) as u32).min(bins - 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeData {
    mode: ScopeMode,
    components: u32,
    columns: u32,
    bins: u32,
    counts: Vec<u32>,
}

impl ScopeData {
    pub fn new(
        mode: ScopeMode,
        components: u32,
        columns: u32,
        bins: u32,
        counts: Vec<u32>,
    ) -> Result<Self, ScopeError> {
        if mode == ScopeMode::Frame {
            return Err(ScopeError::NotMeasured { mode });
        }
        let expected = grid_len(components, columns, bins)?;
        if expected != counts.len() {
            return Err(ScopeError::GridMismatch {
                expected,
                found: counts.len(),
            });
        }
        Ok(Self {
            mode,
            components,
            columns,
            bins,
            counts,
        })
    }

    pub fn mode(&self) -> ScopeMode {
        self.mode
    }

    pub fn components(&self) -> u32 {
        self.components
    }

    pub fn columns(&self) -> u32 {
        self.columns
    }

    pub fn bins(&self) -> u32 {
        self.bins
    }

    pub fn counts(&self) -> &[u32] {
        &self.counts
    }

    pub fn count(&self, component: u32, column: u32, bin: u32) -> Option<u32> {
        let index = self.index(component, column, bin)?;
        self.counts.get(index).copied()
    }

    pub fn panel(&self, component: u32) -> &[u32] {
        let stride = (self.columns as usize) * (self.bins as usize);
        let start = (component as usize) * stride;
        &self.counts[start..start + stride]
    }

    pub fn total(&self) -> u64 {
        self.counts.iter().map(|count| u64::from(*count)).sum()
    }

    fn index(&self, component: u32, column: u32, bin: u32) -> Option<usize> {
        if component >= self.components || column >= self.columns || bin >= self.bins {
            return None;
        }
        let stride = (self.columns as usize).checked_mul(self.bins as usize)?;
        let index = (component as usize)
            .checked_mul(stride)?
            .checked_add(column as usize * self.bins as usize)?
            .checked_add(bin as usize)?;
        Some(index)
    }
}

fn grid_len(components: u32, columns: u32, bins: u32) -> Result<usize, ScopeError> {
    if components == 0 {
        return Err(ScopeError::ZeroGrid {
            field: "components",
        });
    }
    if columns == 0 {
        return Err(ScopeError::ZeroGrid { field: "columns" });
    }
    if bins == 0 {
        return Err(ScopeError::ZeroGrid { field: "bins" });
    }
    let length = (components as usize)
        .checked_mul(columns as usize)
        .and_then(|length| length.checked_mul(bins as usize))
        .ok_or(ScopeError::GridOverflow { columns, bins })?;
    if length > (isize::MAX as usize) / size_of::<u32>() {
        return Err(ScopeError::GridOverflow { columns, bins });
    }
    Ok(length)
}

fn bump(counts: &mut [u32], component: u32, columns: u32, bins: u32, column: u32, bin: u32) {
    let stride = (columns as usize) * (bins as usize);
    let index = (component as usize) * stride + (column as usize) * (bins as usize) + bin as usize;
    if let Some(count) = counts.get_mut(index) {
        *count += 1;
    }
}

pub fn measure(
    mode: ScopeMode,
    frame: &Frame,
    columns: u32,
    bins: u32,
) -> Result<ScopeData, ScopeError> {
    if mode == ScopeMode::Frame {
        return Err(ScopeError::NotMeasured { mode });
    }
    let components = mode.components();
    let length = grid_len(components, columns, bins)?;
    let mut counts = vec![0u32; length];
    let last_column = columns - 1;
    let last_bin = bins - 1;
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let (red, green, blue) = frame.pixel(x, y);
            let column = x * columns / frame.width();
            let column = column.min(last_column);
            match mode {
                ScopeMode::Waveform => {
                    let bin = (luma(red, green, blue) * bins / 256).min(last_bin);
                    bump(&mut counts, 0, columns, bins, column, bin);
                }
                ScopeMode::Parade => {
                    for (component, value) in [red, green, blue].into_iter().enumerate() {
                        let bin = (u32::from(value) * bins / 256).min(last_bin);
                        bump(&mut counts, component as u32, columns, bins, column, bin);
                    }
                }
                ScopeMode::Histogram => {
                    bump(
                        &mut counts,
                        0,
                        columns,
                        bins,
                        0,
                        luma(red, green, blue).min(last_bin),
                    );
                }
                ScopeMode::Vectorscope => {
                    bump(
                        &mut counts,
                        0,
                        columns,
                        bins,
                        0,
                        hue_bin(red, green, blue, bins),
                    );
                }
                ScopeMode::Frame => {}
            }
        }
    }
    ScopeData::new(mode, components, columns, bins, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Frame {
        Frame::new(4, 2, fixture_pixels()).unwrap()
    }

    fn fixture_pixels() -> Vec<u8> {
        include_str!("../tests/golden/scope-frame.txt")
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .flat_map(|line| line.split_whitespace())
            .flat_map(|pixel| {
                (0..pixel.len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(&pixel[at..at + 2], 16).expect("hex byte"))
                    .collect::<Vec<u8>>()
            })
            .collect()
    }

    #[test]
    fn luma_reads_the_rec601_weights() {
        assert_eq!(luma(0, 0, 0), 0);
        assert_eq!(luma(255, 255, 255), 255);
        assert_eq!(luma(255, 0, 0), 76);
        assert_eq!(luma(0, 255, 0), 150);
        assert_eq!(luma(0, 0, 255), 29);
    }

    #[test]
    fn a_count_beyond_the_grid_answers_none() {
        let data = measure(ScopeMode::Waveform, &frame(), 2, 4).unwrap();
        assert_eq!(data.count(0, 1, 3), Some(1));
        assert_eq!(data.count(0, 2, 0), None);
        assert_eq!(data.count(1, 0, 0), None);
        assert_eq!(data.count(0, 0, 4), None);
    }

    #[test]
    fn a_grid_with_no_components_is_refused_by_name() {
        assert!(matches!(
            grid_len(0, 2, 4),
            Err(ScopeError::ZeroGrid {
                field: "components"
            })
        ));
    }

    #[test]
    fn a_pixelless_frame_answers_nothing() {
        let empty = Frame::new(1, 1, vec![0, 0, 0]).unwrap();
        assert_eq!(empty.pixel(0, 0), (0, 0, 0));
    }
}
