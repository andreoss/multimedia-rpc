pub const IDENTIFY_FORMAT: &str = "%m %w %h %[colorspace] %[depth] %b\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
        }
    }

    fn coder(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeMode {
    Fit,
    Exact,
    Fill,
}

fn written(format: ImageFormat, output: &str) -> String {
    format!("{}:{output}", format.coder())
}

pub fn identify_args(format: &str, input: &str) -> Vec<String> {
    ["identify", "-format", format, input]
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub fn resize_args(
    input: &str,
    width: u32,
    height: u32,
    mode: ResizeMode,
    format: ImageFormat,
    output: &str,
) -> Vec<String> {
    let geometry = match mode {
        ResizeMode::Fit => format!("{width}x{height}"),
        ResizeMode::Exact => format!("{width}x{height}!"),
        ResizeMode::Fill => format!("{width}x{height}^"),
    };
    let mut args = vec![input.to_string(), "-resize".to_string(), geometry];
    if mode == ResizeMode::Fill {
        args.extend([
            "-gravity".to_string(),
            "center".to_string(),
            "-extent".to_string(),
            format!("{width}x{height}"),
        ]);
    }
    args.push(written(format, output));
    args
}

pub fn convert_args(input: &str, format: ImageFormat, output: &str) -> Vec<String> {
    vec![input.to_string(), written(format, output)]
}

pub fn composite_args(overlay: &str, base: &str, format: ImageFormat, output: &str) -> Vec<String> {
    [
        "composite",
        "-gravity",
        "center",
        overlay,
        base,
        &written(format, output),
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}
