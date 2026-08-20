use mm_ffmpeg::parse_encoders;

#[test]
fn the_encoder_list_is_read_from_the_tool_table() {
    let text = "\
Encoders:
 V..... = Video
 ------
 V....D libx264              libx264 H.264 / AVC
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder
 V..... h264_vaapi           VAAPI H.264 encoder
 A....D aac                  AAC encoder
";
    assert_eq!(
        parse_encoders(text),
        vec!["libx264", "h264_nvenc", "h264_vaapi", "aac"]
    );
}
