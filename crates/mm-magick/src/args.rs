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

pub fn sprite_args(inputs: &[String], columns: u32, output: &str) -> Vec<String> {
    let mut args = vec!["montage".to_string()];
    args.extend(inputs.iter().cloned());
    args.push("-tile".to_string());
    args.push(format!("{}x", columns.max(1)));
    args.push("-geometry".to_string());
    args.push("+0+0".to_string());
    args.push(format!("jpg:{output}"));
    args
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

fn annotate_text(text: &str) -> String {
    text.replace('\\', "\\\\").replace('@', "\\@")
}

#[allow(clippy::too_many_arguments)]
pub fn title_args(
    text: &str,
    width: u32,
    height: u32,
    point_size: u32,
    colour: &str,
    background: Option<&str>,
    format: ImageFormat,
    output: &str,
) -> Vec<String> {
    let canvas = match background {
        Some(colour) => format!("xc:{colour}"),
        None => "xc:none".to_string(),
    };
    [
        "-size".to_string(),
        format!("{width}x{height}"),
        canvas,
        "-gravity".to_string(),
        "center".to_string(),
        "-font".to_string(),
        "DejaVu-Sans".to_string(),
        "-pointsize".to_string(),
        point_size.to_string(),
        "-fill".to_string(),
        colour.to_string(),
        "-annotate".to_string(),
        "+0+0".to_string(),
        annotate_text(text),
        written(format, output),
    ]
    .to_vec()
}
