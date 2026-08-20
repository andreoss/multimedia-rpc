use mm_gst::{VariantDescriptor, master_playlist_name, render_master_playlist};

fn variant(uri: &str, bandwidth: u32, width: u32, height: u32) -> VariantDescriptor {
    VariantDescriptor::new(uri, bandwidth, width, height).with_codecs("avc1.64001f,mp4a.40.2")
}

#[test]
fn a_master_playlist_names_every_rendition_in_ascending_bandwidth() {
    let rendered = render_master_playlist(&[
        variant("renditions/0/playlist.m3u8", 896_000, 640, 360),
        variant("renditions/1/playlist.m3u8", 2_596_000, 1280, 720),
    ])
    .expect("an ascending ladder renders");
    assert!(
        rendered.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("#EXT-X-INDEPENDENT-SEGMENTS\n"),
        "{rendered}"
    );
    let bands: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with("#EXT-X-STREAM-INF"))
        .collect();
    assert_eq!(bands.len(), 2, "{rendered}");
    assert!(bands[0].contains("BANDWIDTH=896000"), "{}", bands[0]);
    assert!(bands[1].contains("BANDWIDTH=2596000"), "{}", bands[1]);
    assert!(bands[0].contains("RESOLUTION=640x360"), "{}", bands[0]);
    assert!(bands[1].contains("RESOLUTION=1280x720"), "{}", bands[1]);
    assert!(
        bands[0].contains("CODECS=\"avc1.64001f,mp4a.40.2\""),
        "{}",
        bands[0]
    );
    assert!(
        rendered.contains("renditions/0/playlist.m3u8\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("renditions/1/playlist.m3u8\n"),
        "{rendered}"
    );
    assert!(
        rendered.trim_end().ends_with("#EXT-X-ENDLIST"),
        "{rendered}"
    );
}

#[test]
fn a_master_playlist_carries_an_average_bandwidth_when_the_rendition_has_one() {
    let rendered =
        render_master_playlist(&[variant("renditions/0/playlist.m3u8", 896_000, 640, 360)
            .with_average_bandwidth(800_000)])
        .expect("a ladder with one rendition renders");
    assert!(rendered.contains("AVERAGE-BANDWIDTH=800000"), "{rendered}");
}

#[test]
fn a_master_playlist_omits_codecs_when_the_rendition_names_none() {
    let rendered = render_master_playlist(&[VariantDescriptor::new(
        "renditions/0/playlist.m3u8",
        896_000,
        640,
        360,
    )])
    .expect("a ladder with no codecs renders");
    assert!(rendered.contains("BANDWIDTH=896000"), "{rendered}");
    assert!(!rendered.contains("CODECS="), "{rendered}");
}

#[test]
fn a_master_playlist_refuses_renditions_that_are_not_in_ascending_order() {
    let error = render_master_playlist(&[
        variant("renditions/1/playlist.m3u8", 2_596_000, 1280, 720),
        variant("renditions/0/playlist.m3u8", 896_000, 640, 360),
    ])
    .unwrap_err();
    assert!(error.to_string().contains("ascending"), "{error}");
}

#[test]
fn a_master_playlist_refuses_renditions_of_equal_bandwidth() {
    let error = render_master_playlist(&[
        variant("renditions/0/playlist.m3u8", 896_000, 640, 360),
        variant("renditions/1/playlist.m3u8", 896_000, 1280, 720),
    ])
    .unwrap_err();
    assert!(error.to_string().contains("ascending"), "{error}");
}

#[test]
fn a_master_playlist_refuses_a_rendition_with_no_uri_or_no_bandwidth() {
    assert!(render_master_playlist(&[]).is_err());
    let descriptor = VariantDescriptor::new("", 896_000, 640, 360);
    let error = render_master_playlist(&[descriptor]).unwrap_err();
    assert!(error.to_string().contains("uri"), "{error}");
}

#[test]
fn the_master_playlist_has_its_own_name_and_a_media_playlist_keeps_its_own() {
    assert_eq!(master_playlist_name(), "master.m3u8");
    assert_eq!(mm_gst::playlist_name(), "playlist.m3u8");
}
