use mm_gst::{SegmentError, split_fragments, top_level_boxes};

fn box_of(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(payload);
    bytes
}

fn stream() -> Vec<u8> {
    let mut bytes = box_of(b"ftyp", b"isom");
    bytes.extend_from_slice(&box_of(b"moov", b"init"));
    bytes.extend_from_slice(&box_of(b"moof", b"one"));
    bytes.extend_from_slice(&box_of(b"mdat", b"first"));
    bytes.extend_from_slice(&box_of(b"moof", b"two"));
    bytes.extend_from_slice(&box_of(b"mdat", b"second"));
    bytes
}

#[test]
fn the_top_level_boxes_are_walked_in_order() {
    let boxes = top_level_boxes(&stream()).unwrap();

    assert_eq!(boxes.len(), 6);
    assert_eq!(boxes[0].start, 0);
    assert_eq!(boxes[0].end, 12);
    assert_eq!(boxes[1].start, 12);
}

#[test]
fn the_init_segment_holds_the_headers_and_each_segment_one_fragment() {
    let segmented = split_fragments(&stream()).unwrap();

    assert_eq!(segmented.init.len(), 24);
    assert_eq!(segmented.segments.len(), 2);
    assert_eq!(segmented.segments[0].len(), 11 + 13);
    assert_eq!(
        segmented.segments.iter().map(|s| s.len()).sum::<usize>() + segmented.init.len(),
        stream().len()
    );
    assert_eq!(segmented.segments[1].len(), 11 + 14);
}

#[test]
fn a_stream_without_a_fragment_is_refused() {
    let mut bytes = box_of(b"ftyp", b"isom");
    bytes.extend_from_slice(&box_of(b"moov", b"init"));

    assert_eq!(
        split_fragments(&bytes).unwrap_err(),
        SegmentError::NoFragments
    );
}

#[test]
fn a_stream_shorter_than_a_box_header_is_refused() {
    let bytes = vec![0u8; 4];

    assert_eq!(
        top_level_boxes(&bytes).unwrap_err(),
        SegmentError::TruncatedBox {
            size: 8,
            available: 4
        }
    );
}

#[test]
fn a_box_declaring_more_than_the_stream_carries_is_refused() {
    let mut bytes = (64u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(b"isom");

    assert_eq!(
        top_level_boxes(&bytes).unwrap_err(),
        SegmentError::TruncatedBox {
            size: 64,
            available: 12
        }
    );
}

#[test]
fn a_box_declaring_a_size_that_overflows_is_refused() {
    let mut bytes = box_of(b"free", b"");
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(&u64::MAX.to_be_bytes());

    assert_eq!(
        top_level_boxes(&bytes).unwrap_err(),
        SegmentError::TruncatedBox {
            size: usize::MAX,
            available: 16
        }
    );
}
