import { createHash, createHmac, randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { connect } from "node:http2";

import {
  GET_PATH,
  call,
  decodeCreateResponse,
  getRequest,
  mediaSource,
  messageField,
  readFields,
  stringField,
  varint,
} from "./lib/grpc.mjs";

const CREATE_PATH = "/mmrpc.v1.MediaJobService/CreateJob";
const LADDER_FIELD = 10;
const TERMINAL_BUDGET = 300000;
const POLL_MS = 500;
const OWNER = "ladder-external";
const TERMINAL_STATES = new Set([3, 4, 5]);

let checks = 0;
let failures = 0;

function check(condition, description) {
  checks += 1;
  if (condition) {
    console.log(`ok ${checks} - ${description}`);
  } else {
    failures += 1;
    console.log(`not ok ${checks} - ${description}`);
  }
}

function doubleField(field, value) {
  const bytes = Buffer.alloc(8);
  bytes.writeDoubleLE(value, 0);
  return Buffer.concat([varint((field << 3) | 1), bytes]);
}

function intField(field, value) {
  if (value === 0) {
    return Buffer.alloc(0);
  }
  return Buffer.concat([varint(field << 3), varint(value)]);
}

function outputProfile({ videoBitrate, audioBitrate, width, height }) {
  return Buffer.concat([
    doubleField(1, 24),
    intField(2, videoBitrate),
    intField(3, 26),
    intField(4, audioBitrate),
    intField(5, 48000),
    intField(6, width),
    intField(7, height),
  ]);
}

function ladderSpec(uri, rungs, segmentSeconds) {
  const ladder = Buffer.concat([
    messageField(1, mediaSource(uri)),
    ...rungs.map((rung) => messageField(2, outputProfile(rung))),
    ...(segmentSeconds ? [doubleField(3, segmentSeconds)] : []),
  ]);
  return messageField(LADDER_FIELD, ladder);
}

function createLadderRequest(uri, rungs, segmentSeconds, idempotencyKey) {
  return Buffer.concat([
    stringField(1, OWNER),
    stringField(2, idempotencyKey),
    messageField(3, ladderSpec(uri, rungs, segmentSeconds)),
  ]);
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function signingKey(secretKey, dateStamp, region) {
  const date = createHmac("sha256", `AWS4${secretKey}`).update(dateStamp).digest();
  const place = createHmac("sha256", date).update(region).digest();
  const service = createHmac("sha256", place).update("s3").digest();
  return createHmac("sha256", service).update("aws4_request").digest();
}

function signedRequest(method, objectUri) {
  const endpoint = process.env.MM_OBJECTSTORE_ENDPOINT ?? "http://127.0.0.1:8333";
  const accessKey = process.env.MM_OBJECTSTORE_ACCESS_KEY ?? "mm-test-access";
  const secretKey = process.env.MM_OBJECTSTORE_SECRET_KEY ?? "mm-test-secret";
  const region = process.env.MM_OBJECTSTORE_REGION ?? "us-east-1";
  const rest = objectUri.replace(/^s3:\/\//, "");
  const slash = rest.indexOf("/");
  const requestPath = `/${rest.slice(0, slash)}/${rest.slice(slash + 1)}`;
  const target = new URL(endpoint);
  const host = target.host;
  const payloadHash = sha256("");
  const amzDate = new Date().toISOString().replace(/[:-]|\.\d{3}/g, "");
  const dateStamp = amzDate.slice(0, 8);
  const signedHeaders = "host;x-amz-content-sha256;x-amz-date";
  const canonicalHeaders = `host:${host}\nx-amz-content-sha256:${payloadHash}\nx-amz-date:${amzDate}\n`;
  const canonicalRequest = `${method}\n${requestPath}\n\n${canonicalHeaders}\n${signedHeaders}\n${payloadHash}`;
  const scope = `${dateStamp}/${region}/s3/aws4_request`;
  const stringToSign = `AWS4-HMAC-SHA256\n${amzDate}\n${scope}\n${sha256(canonicalRequest)}`;
  const signature = createHmac("sha256", signingKey(secretKey, dateStamp, region))
    .update(stringToSign)
    .digest("hex");
  return {
    hostname: target.hostname,
    port: target.port,
    path: requestPath,
    headers: {
      host,
      "x-amz-content-sha256": payloadHash,
      "x-amz-date": amzDate,
      authorization: `AWS4-HMAC-SHA256 Credential=${accessKey}/${scope}, SignedHeaders=${signedHeaders}, Signature=${signature}`,
      connection: "close",
    },
  };
}

function fetchObject(objectUri) {
  return new Promise((resolve, reject) => {
    const parts = signedRequest("GET", objectUri);
    const request = http.request(
      {
        host: parts.hostname,
        port: parts.port,
        path: parts.path,
        method: "GET",
        headers: parts.headers,
      },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => {
          const body = Buffer.concat(chunks);
          if (response.statusCode !== 200) {
            reject(new Error(`${objectUri} answered ${response.statusCode}`));
          } else {
            resolve(body);
          }
        });
      }
    );
    request.setTimeout(10000, () => {
      request.destroy();
      reject(new Error(`${objectUri} timed out`));
    });
    request.on("error", reject);
    request.end();
  });
}

function objectMissing(objectUri) {
  return fetchObject(objectUri).then(
    () => false,
    () => true
  );
}

function putObject(objectUri, path) {
  execFileSync("aws", [
    "--endpoint-url",
    process.env.MM_OBJECTSTORE_ENDPOINT ?? "http://127.0.0.1:8333",
    "s3api",
    "put-object",
    "--bucket",
    objectUri.replace(/^s3:\/\//, "").split("/")[0],
    "--key",
    objectUri.replace(/^s3:\/\//, "").split("/").slice(1).join("/"),
    "--body",
    path,
  ], {
    env: {
      ...process.env,
      AWS_ACCESS_KEY_ID: process.env.MM_OBJECTSTORE_ACCESS_KEY ?? "mm-test-access",
      AWS_SECRET_ACCESS_KEY: process.env.MM_OBJECTSTORE_SECRET_KEY ?? "mm-test-secret",
      AWS_DEFAULT_REGION: process.env.MM_OBJECTSTORE_REGION ?? "us-east-1",
    },
    stdio: ["ignore", "ignore", "inherit"],
  });
}

function decodeRendition(bytes) {
  const fields = readFields(bytes);
  const profile = fields.get(1)?.[0];
  const media = fields.get(2)?.[0];
  const playlist = fields.get(3)?.[0];
  const segments = (fields.get(4) ?? []).map((segment) => {
    const uri = readFields(segment).get(1)?.[0];
    return uri ? uri.toString("utf8") : "";
  });
  return {
    media: media ? (readFields(media).get(1)?.[0]?.toString("utf8") ?? "") : "",
    playlist: playlist ? (readFields(playlist).get(1)?.[0]?.toString("utf8") ?? "") : "",
    segments,
    width: Number(fields.get(6)?.[0] ?? 0n),
    height: Number(fields.get(7)?.[0] ?? 0n),
    bandwidthBps: Number(fields.get(8)?.[0] ?? 0n),
    askedWidth: profile ? Number(readFields(profile).get(6)?.[0] ?? 0n) : 0,
    askedHeight: profile ? Number(readFields(profile).get(7)?.[0] ?? 0n) : 0,
  };
}

function decodeLadder(bytes) {
  const fields = readFields(bytes);
  const master = fields.get(1)?.[0];
  return {
    masterPlaylist: master ? (readFields(master).get(1)?.[0]?.toString("utf8") ?? "") : "",
    renditions: (fields.get(2) ?? []).map(decodeRendition),
  };
}

function decodeJob(body) {
  const response = readFields(body.subarray(5));
  const job = response.get(1)?.[0];
  if (!job) {
    return null;
  }
  const fields = readFields(job);
  const result = fields.get(6)?.[0];
  const failure = null;
  let ladder = null;
  if (result) {
    const ladderBytes = readFields(result).get(LADDER_FIELD)?.[0];
    if (ladderBytes) {
      ladder = decodeLadder(ladderBytes);
    }
  }
  return {
    state: Number(fields.get(4)?.[0] ?? 0n),
    ladder,
    fileErrors: (fields.get(7) ?? []).map((entry) => {
      const error = readFields(entry);
      return {
        code: error.get(2)?.[0]?.toString("utf8") ?? "",
        rung: Number(error.get(6)?.[0] ?? 0n),
      };
    }),
    code: failure ? (readFields(failure).get(1)?.[0]?.toString("utf8") ?? "") : "",
  };
}

async function waitForTerminal(client, jobId) {
  const deadline = Date.now() + TERMINAL_BUDGET;
  while (Date.now() < deadline) {
    const { body } = await call(client, GET_PATH, getRequest(OWNER, jobId));
    const job = decodeJob(body);
    if (job && TERMINAL_STATES.has(job.state)) {
      return job;
    }
    await new Promise((resolve) => setTimeout(resolve, POLL_MS));
  }
  throw new Error(`job ${jobId} never reached a terminal state`);
}

async function submit(client, uri, rungs, segmentSeconds) {
  const { body, status, message } = await call(
    client,
    CREATE_PATH,
    createLadderRequest(uri, rungs, segmentSeconds, randomUUID())
  );
  if (status !== 0) {
    throw new Error(`CreateJob answered ${status} ${message}`);
  }
  return decodeCreateResponse(body);
}

async function main() {
  const address = process.argv[2];
  const bucket = process.env.MM_OBJECTSTORE_BUCKET ?? "mm-ladder-conformance";
  const workdir = path.join("scratch", "ladder-conternal");
  fs.mkdirSync(workdir, { recursive: true });
  const source = path.join(workdir, "source.mp4");
  execFileSync(
    "ffmpeg",
    [
      "-y",
      "-loglevel",
      "error",
      "-f",
      "lavfi",
      "-i",
      "testsrc2=duration=3:size=640x360:rate=24",
      "-f",
      "lavfi",
      "-i",
      "sine=frequency=440:sample_rate=44100:duration=3",
      "-c:v",
      "libx264",
      "-preset",
      "veryfast",
      "-pix_fmt",
      "yuv420p",
      "-c:a",
      "aac",
      "-movflags",
      "+faststart",
      "-f",
      "mp4",
      source,
    ],
    { stdio: "ignore" }
  );
  const uri = `s3://${bucket}/inputs/ladder-source.mp4`;
  putObject(uri, source);

  const client = connect(`http://${address}`);
  const rungs = [
    { videoBitrate: 300_000, audioBitrate: 64_000, width: 320, height: 180 },
    { videoBitrate: 800_000, audioBitrate: 64_000, width: 640, height: 360 },
  ];

  const created = await submit(client, uri, rungs, 1.0);
  check(created.created === true, "the ladder job was created");
  const job = await waitForTerminal(client, created.jobId);
  check(
    job.state === 3,
    `the ladder succeeded (state ${job.state}, code ${job.code})`
  );
  check(job.ladder !== null, "the job answered a ladder result");
  if (!job.ladder) {
    client.close();
    throw new Error(`no ladder result: ${job.code}`);
  }

  const ladder = job.ladder;
  check(ladder.renditions.length === 2, `one result entry per rung (${ladder.renditions.length})`);
  check(ladder.masterPlaylist.endsWith("master.m3u8"), "a master playlist was named");
  check(
    ladder.renditions.every((rendition) =>
      rendition.media.startsWith(`s3://${bucket}/jobs/${created.jobId}/renditions/`)
    ),
    "every rendition's media is under its own prefix"
  );
  check(
    ladder.renditions.every((rendition) => rendition.playlist.endsWith("playlist.m3u8")),
    "every packaged rung answers a media playlist"
  );
  check(
    ladder.renditions.every((rendition) => rendition.segments.length > 0),
    "every packaged rung answers its segments"
  );
  check(
    ladder.renditions[0].width === 320 && ladder.renditions[1].width === 640,
    "the measured dimensions are the rungs the caller asked for"
  );
  check(
    ladder.renditions[1].bandwidthBps > ladder.renditions[0].bandwidthBps,
    `bandwidths ascend (${ladder.renditions.map((r) => r.bandwidthBps).join(", ")})`
  );

  const master = (await fetchObject(ladder.masterPlaylist)).toString("utf8");
  const rows = master.split("\n").filter((line) => line.startsWith("#EXT-X-STREAM-INF"));
  check(master.startsWith("#EXTM3U"), "the master playlist is an HLS playlist");
  check(rows.length === 2, `the master playlist names every rung (${rows.length})`);
  check(
    rows.every((row) => row.includes("BANDWIDTH=") && row.includes("RESOLUTION=")),
    "every row carries a bandwidth and a resolution"
  );
  check(
    master.includes("renditions/0/playlist.m3u8") && master.includes("renditions/1/playlist.m3u8"),
    "the master playlist points at each rung's media playlist"
  );
  for (const rendition of ladder.renditions) {
    const body = (await fetchObject(rendition.playlist)).toString("utf8");
    check(body.startsWith("#EXTM3U"), `${rendition.playlist.split("/").slice(-2).join("/")} is an HLS playlist`);
    for (const segment of rendition.segments) {
      const segmentBody = await fetchObject(segment);
      check(segmentBody.length > 0, `${segment.split("/").pop()} is readable`);
    }
  }

  const refused = await submit(client, uri, [
    rungs[0],
    { videoBitrate: 4_000_000, audioBitrate: 64_000, width: 1920, height: 1080 },
  ], 1.0);
  check(refused.jobId !== "", "a ladder above the source is accepted at create");
  const refusedJob = await waitForTerminal(client, refused.jobId);
  check(refusedJob.state === 4, `the oversized rung failed the job (state ${refusedJob.state})`);
  check(
    refusedJob.fileErrors.some((error) => error.code === "ERROR_UPSCALE_NOT_ALLOWED"),
    `the refusal carries its own reference code (${JSON.stringify(refusedJob.fileErrors)})`
  );
  check(
    refusedJob.fileErrors.some((error) => error.rung === 1),
    `the refusal names the rung it refused (${JSON.stringify(refusedJob.fileErrors)})`
  );
  for (const index of [0, 1]) {
    const media = `s3://${bucket}/jobs/${refused.jobId}/renditions/${index}/output.mp4`;
    check(await objectMissing(media), `rung ${index} left no media behind`);
  }

  client.close();
  console.log(`1..${checks}`);
  if (failures > 0) {
    console.error(`${failures} of ${checks} checks failed`);
    process.exit(1);
  }
  console.log(`all ${checks} checks passed`);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});