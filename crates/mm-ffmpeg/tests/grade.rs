use mm_ffmpeg::{GradeError, GradeTarget, TargetProfile, build_grade_args, parse_cube_table};

const INPUT: &str = "http://127.0.0.1:9/in.mp4";
const OUTPUT: &str = "scratch/out.mp4";

fn target() -> GradeTarget {
    GradeTarget {
        brightness: 0.1,
        contrast: 1.2,
        saturation: 1.3,
        gamma: 1.1,
    }
}

fn argument_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let position = args.iter().position(|arg| arg == flag).unwrap();
    &args[position + 1]
}

#[test]
fn a_grade_renders_each_adjustment_into_the_filter() {
    let args = build_grade_args(INPUT, &target(), None, &TargetProfile::default(), OUTPUT)
        .expect("the grade argv builds");
    assert_eq!(
        argument_after(&args, "-vf"),
        "eq=brightness=0.1:contrast=1.2:saturation=1.3:gamma=1.1,fps=24,format=yuv420p"
    );
    assert_eq!(argument_after(&args, "-c:v"), "libx264");
    assert_eq!(args.last().map(String::as_str), Some(OUTPUT));
}

#[test]
fn a_lookup_table_is_appended_after_the_adjustments() {
    let args = build_grade_args(
        INPUT,
        &target(),
        Some("scratch/a:b,c'd.cube"),
        &TargetProfile::default(),
        OUTPUT,
    )
    .expect("the grade argv builds");
    let filter = argument_after(&args, "-vf");
    assert!(
        filter.contains("lut3d=file='scratch/a\\:b\\,c\\'d.cube'"),
        "{filter}"
    );
}

#[test]
fn a_cube_table_is_read_by_its_declared_size() {
    let text =
        "TITLE test\nLUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
    assert_eq!(parse_cube_table(text), Ok(2));
}

#[test]
fn a_document_without_a_cube_size_is_refused() {
    assert_eq!(
        parse_cube_table("0 0 0\n1 1 1\n"),
        Err(GradeError::MissingLutSize)
    );
}

#[test]
fn a_cube_table_with_too_few_entries_is_refused() {
    assert_eq!(
        parse_cube_table("LUT_3D_SIZE 2\n0 0 0\n"),
        Err(GradeError::LutEntries {
            found: 1,
            expected: 8
        })
    );
}
