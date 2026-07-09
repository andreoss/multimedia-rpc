use std::path::PathBuf;
use std::process::Command;

const CREATE_RESPONSE: &str = "000000003c0a2430316131336330352d346465372d373333332d396531302d323539346165613738366262120b70726f62652d7265712d3218e99b95e093342001";
const GET_RESPONSE: &str = "00000000800a2430316131336330352d396130392d373033342d616465302d643364643134373662393461120b70726f62652d7265712d331801321f121d73333a2f2f6d6d2d6d657267652d636f6e66322f75706c6f6164732f6232210801121d73333a2f2f6d6d2d6d657267652d636f6e66322f75706c6f6164732f6138a0b496e09334";

fn script_library() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/lib/grpc.mjs")
        .canonicalize()
        .expect("the harness library is part of this repository")
}

fn decode(body: &str, source: &str) -> String {
    let library = script_library();
    let program = format!(
        "import {{ readFields }} from \"{library}\";\
         const fields = readFields(Buffer.from(\"{body}\", \"hex\").subarray(5));\
         console.log(JSON.stringify({source}));",
        library = library.display(),
        body = body,
        source = source
    );
    let output = Command::new("node")
        .arg("--input-type=module")
        .arg("-e")
        .arg(&program)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "node is not available to run the evidence harness ({error}); \
                 a harness this repository cannot execute is not evidence"
            )
        });
    assert!(
        output.status.success(),
        "the harness reader failed on bytes this service produced: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("the reader answers utf8")
}

#[test]
fn the_harness_reader_decodes_the_values_a_service_sends() {
    let create = decode(
        CREATE_RESPONSE,
        "({ created: fields.get(4)?.[0] === 1n, createdAtMillis: Number(fields.get(3)?.[0] ?? 0n) })",
    );

    assert!(
        create.contains("\"created\":true"),
        "a created flag the server sent true read as {create}"
    );
    assert!(
        !create.contains("\"createdAtMillis\":0"),
        "a timestamp the server sent read as nothing: {create}"
    );
    assert!(
        !create.contains("\"createdAtMillis\":24"),
        "the tag was read as the value: {create}"
    );
}

#[test]
fn the_harness_reader_decodes_the_job_shape_a_facade_sends() {
    let job = decode(
        GET_RESPONSE,
        "({ requester: Number(fields.get(3)?.[0] ?? 0n), \
          inputs: (fields.get(6) ?? []).map((value) => { \
            const input = readFields(value); \
            return { order: Number(input.get(1)?.[0] ?? 0n), media: input.get(2)?.[0]?.toString('utf8') }; \
          }) })",
    );

    assert!(job.contains("\"requester\":1"), "requester read as {job}");
    assert!(
        job.contains("\"order\":1"),
        "a sequence order read as {job}"
    );
    assert!(
        job.contains("s3://mm-merge-conf2/uploads/a"),
        "a media id read as {job}"
    );
}

#[test]
fn the_harness_writer_writes_a_zero_and_an_empty_string_as_they_are() {
    let library = script_library();
    let program = format!(
        "import {{ stringField, varint }} from \"{library}\";\
         console.log(JSON.stringify({{ \
           zero: varint(0).toString('hex'), \
           one: varint(1).toString('hex'), \
           empty: stringField(2, '').toString('hex') \
         }}));",
        library = library.display()
    );
    let output = Command::new("node")
        .arg("--input-type=module")
        .arg("-e")
        .arg(&program)
        .output()
        .expect("node runs the harness writer");
    assert!(
        output.status.success(),
        "the harness writer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let written = String::from_utf8(output.stdout).expect("utf8");

    assert!(
        written.contains("\"zero\":\"00\""),
        "a zero must be one byte, not an empty field: {written}"
    );
    assert!(
        written.contains("\"empty\":\"1200\""),
        "an empty string must carry its own zero length: {written}"
    );
}
