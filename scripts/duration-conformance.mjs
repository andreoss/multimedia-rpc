import { randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { connect } from "node:http2";

import {
  OK,
  call,
  messageField,
  readFields,
  stringField,
  varint,
} from "./lib/grpc.mjs";

const CREATE_PATH = "/mmrpc.videomerge.v1.VideoMergeService/CreateJob";
const UPLOAD_PATH = "/mmrpc.v1.MediaJobService/CreateUploadTarget";
const REQUESTER_CHAT_MESSAGE = 1;
const FIRST_SECONDS = 3;
const SECOND_SECONDS = 2;
const DURATION_TOLERANCE_MS = 1000;
const RECEIVE_BUDGET = 180000;

function intField(field, value) {
  if (value === 0) {
    return Buffer.alloc(0);
  }
  return Buffer.concat([varint(field * 8), varint(value)]);
}

function inputFile(sequenceOrder, mediaId) {
  return Buffer.concat([intField(1, sequenceOrder), stringField(2, mediaId)]);
}

function videoMergeJob({ requestId, requester, owner, inputs }) {
  return Buffer.concat([
    stringField(2, requestId),
    intField(3, requester),
    stringField(4, owner),
    ...inputs.map(([sequenceOrder, mediaId]) => messageField(6, inputFile(sequenceOrder, mediaId))),
  ]);
}

function createRequest(job, idempotencyKey, callbackQueueUrl) {
  return Buffer.concat([
    messageField(1, job),
    stringField(2, idempotencyKey),
    messageField(3, stringField(1, callbackQueueUrl)),
  ]);
}

function decodeCreate(body) {
  const fields = readFields(body.subarray(5));
  return {
    jobId: fields.get(1)?.[0]?.toString("utf8") ?? "",
    created: fields.get(4)?.[0] === 1n,
  };
}

function uploadTargetRequest(owner) {
  return Buffer.concat([stringField(1, owner), intField(2, 900)]);
}

function upload(url, payload) {
  const target = new URL(url);
  return new Promise((resolve, reject) => {
    const request = http.request(
      {
        host: target.hostname,
        port: target.port,
        path: `${target.pathname}${target.search}`,
        method: "PUT",
        headers: {
          "content-type": "application/octet-stream",
          "content-length": payload.length,
          connection: "close",
        },
      },
      (response) => {
        response.resume();
        response.on("end", () =>
          response.statusCode === 200
            ? resolve(response.statusCode)
            : reject(new Error(`the store answered ${response.statusCode}`)),
        );
      },
    );
    request.setTimeout(10000, () => {
      request.destroy(new Error("the upload did not answer within the read budget"));
    });
    request.on("error", reject);
    request.end(payload);
  });
}

function sqs(endpoint, target, params) {
  const url = new URL(target, endpoint);
  const body = new URLSearchParams(params).toString();
  return new Promise((resolve, reject) => {
    const request = http.request(
      {
        host: url.hostname,
        port: url.port,
        path: url.pathname,
        method: "POST",
        headers: {
          "content-type": "application/x-www-form-urlencoded",
          "content-length": Buffer.byteLength(body),
          connection: "close",
        },
      },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () =>
          response.statusCode === 200
            ? resolve(Buffer.concat(chunks).toString("utf8"))
            : reject(new Error(`the queue answered ${response.statusCode}`)),
        );
      },
    );
    request.setTimeout(10000, () => {
      request.destroy(new Error("the queue did not answer within the read budget"));
    });
    request.on("error", reject);
    request.end(body);
  });
}

async function createQueue(endpoint, name) {
  const xml = await sqs(endpoint, "/", {
    Action: "CreateQueue",
    QueueName: name,
    Version: "2012-11-05",
  });
  const match = xml.match(/<QueueUrl>([^<]+)<\/QueueUrl>/);
  if (!match) {
    throw new Error(`the queue creation answered no url: ${xml}`);
  }
  return match[1];
}

async function receive(endpoint, queueUrl) {
  const xml = await sqs(endpoint, queueUrl, {
    Action: "ReceiveMessage",
    MaxNumberOfMessages: "1",
    WaitTimeSeconds: "1",
    Version: "2012-11-05",
  });
  const match = xml.match(/<Body>([\s\S]*?)<\/Body>/);
  return match ? match[1] : null;
}

async function deleteQueue(endpoint, queueUrl) {
  await sqs(endpoint, queueUrl, { Action: "DeleteQueue", Version: "2012-11-05" });
}

function fixture(dir, name, seconds) {
  const file = path.join(dir, name);
  execFileSync(
    "ffmpeg",
    [
      "-y",
      "-f",
      "lavfi",
      "-i",
      `testsrc2=duration=${seconds}:size=320x240:rate=24`,
      "-f",
      "lavfi",
      "-i",
      `sine=frequency=440:sample_rate=44100:duration=${seconds}`,
      "-c:v",
      "libx264",
      "-preset",
      "veryfast",
      "-pix_fmt",
      "yuv420p",
      "-c:a",
      "aac",
      "-b:a",
      "128k",
      "-f",
      "mp4",
      "-movflags",
      "+faststart",
      file,
    ],
    { stdio: "ignore" },
  );
  return file;
}

function decodeStitch(body) {
  const fields = readFields(body);
  return {
    requestId: fields.get(1)?.[0]?.toString("utf8") ?? "",
    status: Number(fields.get(2)?.[0] ?? 0n),
    r2Uri: fields.get(4)?.[0]?.toString("utf8") ?? null,
    durationMs: fields.has(7) ? Number(fields.get(7)[0]) : null,
  };
}

async function run(address) {
  const endpoint = process.env.MM_QUEUE_ENDPOINT ?? "http://127.0.0.1:9324";
  const queueName = `mm-duration-${randomUUID().replace(/-/g, "")}`;
  const queueUrl = await createQueue(endpoint, queueName);
  const dir = path.join("scratch", "d06", "duration");
  fs.mkdirSync(dir, { recursive: true });
  const first = fixture(dir, "first.mp4", FIRST_SECONDS);
  const second = fixture(dir, "second.mp4", SECOND_SECONDS);

  const client = connect(`http://${address}`);
  client.on("error", (error) => {
    console.error(`duration-conformance: connection failed: ${error.message}`);
    process.exit(1);
  });

  const owner = `duration-owner-${randomUUID()}`;
  const media = [];
  for (const file of [first, second]) {
    const answer = await call(client, UPLOAD_PATH, uploadTargetRequest(owner), null);
    if (answer.status !== OK) {
      console.error(`duration-conformance: the upload target was refused: ${answer.message}`);
      client.close();
      process.exit(1);
    }
    const fields = readFields(answer.body.subarray(5));
    const url = fields.get(1)?.[0]?.toString("utf8") ?? "";
    const mediaId = fields.get(2)?.[0]?.toString("utf8") ?? "";
    await upload(url, fs.readFileSync(file));
    media.push(mediaId);
  }

  const requestId = randomUUID();
  const job = videoMergeJob({
    requestId,
    requester: REQUESTER_CHAT_MESSAGE,
    owner,
    inputs: [
      [0, media[0]],
      [1, media[1]],
    ],
  });
  const created = await call(client, CREATE_PATH, createRequest(job, requestId, queueUrl), null);
  if (created.status !== OK) {
    console.error(`duration-conformance: the create was refused: ${created.message}`);
    client.close();
    process.exit(1);
  }
  const jobId = decodeCreate(created.body).jobId;
  console.log(`duration-conformance: created job ${jobId}, waiting for its callback`);

  const deadline = Date.now() + RECEIVE_BUDGET;
  let stitch = null;
  while (Date.now() < deadline) {
    const body = await receive(endpoint, queueUrl);
    if (body === null) {
      continue;
    }
    const bytes = Buffer.from(body, "base64");
    const callback = readFields(bytes);
    const half = callback.get(2)?.[0];
    if (!half) {
      continue;
    }
    stitch = decodeStitch(half);
    break;
  }
  client.close();
  await deleteQueue(endpoint, queueUrl).catch(() => {});

  if (stitch === null) {
    console.error("duration-conformance: no callback arrived within the budget");
    process.exit(1);
  }

  const wanted = (FIRST_SECONDS + SECOND_SECONDS) * 1000;
  const present = stitch.durationMs !== null;
  const close = present && Math.abs(stitch.durationMs - wanted) <= DURATION_TOLERANCE_MS;
  console.log(
    `${close ? "pass" : "FAIL"} the callback carries the merged duration: ` +
      `expected ~${wanted}ms, observed ${stitch.durationMs}`,
  );
  console.log(
    `${stitch.r2Uri ? "pass" : "FAIL"} the callback names the merged output: ${stitch.r2Uri}`,
  );
  process.exit(close && stitch.r2Uri ? 0 : 1);
}

await run(process.argv[2]);
