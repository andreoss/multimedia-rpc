import { connect } from "node:http2";
import { createHash, createHmac, randomUUID } from "node:crypto";
import http from "node:http";

import {
  FAILED_PRECONDITION,
  INVALID_ARGUMENT,
  NOT_FOUND,
  OK,
  call,
  messageField,
  readFields,
  stringField,
  varint,
} from "./lib/grpc.mjs";

const CREATE_PATH = "/mmrpc.videomerge.v1.VideoMergeService/CreateJob";
const CANCEL_PATH = "/mmrpc.videomerge.v1.VideoMergeService/CancelJob";
const GET_PATH = "/mmrpc.videomerge.v1.VideoMergeService/GetJob";
const LIST_PATH = "/mmrpc.videomerge.v1.VideoMergeService/ListUserJobs";
const UPLOAD_PATH = "/mmrpc.v1.MediaJobService/CreateUploadTarget";

const REQUESTER_CHAT_MESSAGE = 1;
const REQUESTER_STUDIO = 4;
const STITCH_COMPLETED = 1;
const STITCH_UNSPECIFIED = 0;

function intField(field, value) {
  if (value === 0) {
    return Buffer.alloc(0);
  }
  return Buffer.concat([varint(field * 8), varint(value)]);
}

function rawIntField(field, value) {
  return Buffer.concat([varint(field * 8), varint(value)]);
}

function enumFilter(values) {
  return Buffer.concat(values.map((value) => rawIntField(2, value)));
}

function inputFile(sequenceOrder, mediaId) {
  return Buffer.concat([intField(1, sequenceOrder), stringField(2, mediaId)]);
}

function videoMergeJob({ requestId, requester, owner, inputs, frameRate }) {
  return Buffer.concat([
    stringField(2, requestId),
    intField(3, requester),
    stringField(4, owner),
    ...inputs.map(([sequenceOrder, mediaId]) => messageField(6, inputFile(sequenceOrder, mediaId))),
    frameRate === undefined ? Buffer.alloc(0) : messageField(10, intField(1, frameRate)),
  ]);
}

function createRequest(job, idempotencyKey) {
  return Buffer.concat([
    messageField(1, job),
    stringField(2, idempotencyKey),
  ]);
}

function frame(body) {
  const message = body.subarray(5);
  const size = Buffer.alloc(5);
  size.writeUInt8(0, 0);
  size.writeUInt32BE(message.length, 1);
  return Buffer.concat([size, message]);
}

function decodeCreate(body) {
  const fields = readFields(body.subarray(5));
  return {
    jobId: fields.get(1)?.[0]?.toString("utf8") ?? "",
    requestId: fields.get(2)?.[0]?.toString("utf8") ?? "",
    createdAtMillis: Number(fields.get(3)?.[0] ?? 0n),
    created: fields.get(4)?.[0] === 1n,
  };
}

function decodeJob(body) {
  return decodeJobMessage(body.subarray(5));
}

function decodeJobMessage(body) {
  const fields = readFields(body);
  const inputs = (fields.get(6) ?? []).map((value) => {
    const input = readFields(value);
    return {
      sequenceOrder: Number(input.get(1)?.[0] ?? 0n),
      mediaId: input.get(2)?.[0]?.toString("utf8") ?? "",
    };
  });
  const output = fields.get(10)?.[0] ? readFields(fields.get(10)[0]) : null;
  return {
    jobId: fields.get(1)?.[0]?.toString("utf8") ?? "",
    requestId: fields.get(2)?.[0]?.toString("utf8") ?? "",
    requester: Number(fields.get(3)?.[0] ?? 0n),
    userId: fields.get(4)?.[0]?.toString("utf8") ?? "",
    status: fields.has(5) ? Number(fields.get(5)[0]) : null,
    inputs,
    frameRate: output?.has(1) ? Number(output.get(1)[0]) : null,
  };
}

function decodeCancel(body) {
  const fields = readFields(body.subarray(5));
  return {
    jobId: fields.get(1)?.[0]?.toString("utf8") ?? "",
    status: Number(fields.get(2)?.[0] ?? 0n),
    accepted: fields.get(3)?.[0] === 1n,
  };
}

function decodeList(body) {
  const fields = readFields(body.subarray(5));
  return {
    jobs: (fields.get(1) ?? []).map(decodeJobMessage),
    nextPageToken: fields.get(2)?.[0]?.toString("utf8") ?? "",
  };
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

function deleteObject(mediaId) {
  return new Promise((resolve, reject) => {
    const endpoint = process.env.MM_OBJECTSTORE_ENDPOINT ?? "http://127.0.0.1:8333";
    const accessKey = process.env.MM_OBJECTSTORE_ACCESS_KEY ?? "mm-test-access";
    const secretKey = process.env.MM_OBJECTSTORE_SECRET_KEY ?? "mm-test-secret";
    const region = process.env.MM_OBJECTSTORE_REGION ?? "us-east-1";
    const rest = mediaId.replace(/^s3:\/\//, "");
    const slash = rest.indexOf("/");
    const path = `/${rest.slice(0, slash)}/${rest.slice(slash + 1)}`;
    const target = new URL(endpoint);
    const host = target.host;
    const payloadHash = sha256("");
    const amzDate = new Date().toISOString().replace(/[:-]|\.\d{3}/g, "");
    const dateStamp = amzDate.slice(0, 8);
    const signedHeaders = "host;x-amz-content-sha256;x-amz-date";
    const canonicalRequest = `DELETE\n${path}\n\nhost:${host}\nx-amz-content-sha256:${payloadHash}\nx-amz-date:${amzDate}\n\n${signedHeaders}\n${payloadHash}`;
    const scope = `${dateStamp}/${region}/s3/aws4_request`;
    const stringToSign = `AWS4-HMAC-SHA256\n${amzDate}\n${scope}\n${sha256(canonicalRequest)}`;
    const signature = createHmac("sha256", signingKey(secretKey, dateStamp, region))
      .update(stringToSign)
      .digest("hex");
    const request = http.request(
      {
        host: target.hostname,
        port: target.port,
        path,
        method: "DELETE",
        headers: {
          host,
          "x-amz-content-sha256": payloadHash,
          "x-amz-date": amzDate,
          authorization: `AWS4-HMAC-SHA256 Credential=${accessKey}/${scope}, SignedHeaders=${signedHeaders}, Signature=${signature}`,
          connection: "close",
        },
      },
      (response) => {
        response.resume();
        response.on("end", () => resolve(response.statusCode));
      },
    );
    request.setTimeout(10000, () => {
      request.destroy(new Error("the delete did not answer within the read budget"));
    });
    request.on("error", reject);
    request.end();
  });
}

function missingSibling(mediaId) {
  const parts = mediaId.split("/");
  parts[parts.length - 1] = `absent-${randomUUID()}`;
  return parts.join("/");
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

async function run(address) {
  const client = connect(`http://${address}`);
  client.on("error", (error) => {
    console.error(`merge-conformance: connection failed: ${error.message}`);
    process.exit(1);
  });
  const results = [];
  const check = (name, expected, observed, detail) => {
    const passed = expected === observed;
    results.push(passed);
    console.log(
      `${passed ? "pass" : "FAIL"} ${name}: expected ${expected}, observed ${observed}${detail ? ` (${detail})` : ""}`,
    );
  };
  const run = randomUUID();
  const owner = `conformance-owner-${run}`;
  const other = `conformance-other-${run}`;

  const targets = [];
  for (let index = 0; index < 2; index += 1) {
    const answer = await call(client, UPLOAD_PATH, uploadTargetRequest(owner), null);
    if (answer.status !== OK) {
      console.error(`merge-conformance: the upload target was refused: ${answer.message}`);
      client.close();
      process.exit(1);
    }
    const fields = readFields(answer.body.subarray(5));
    const url = fields.get(1)?.[0]?.toString("utf8") ?? "";
    const mediaId = fields.get(2)?.[0]?.toString("utf8") ?? "";
    const status = await upload(url, Buffer.from(`conformance fixture ${index} ${randomUUID()}`));
    check("a client PUTs bytes straight to the store", 200, status);
    targets.push(mediaId);
  }

  const requestId = randomUUID();
  const job = (request, key = requestId) =>
    createRequest(
      videoMergeJob({
        requestId: request,
        requester: REQUESTER_CHAT_MESSAGE,
        owner,
        inputs: [
          [1, targets[1]],
          [0, targets[0]],
        ],
      }),
      key,
    );

  const created = await call(client, CREATE_PATH, job(requestId), null);
  check("a reference create is accepted", OK, created.status, created.message);
  if (created.status !== OK) {
    client.close();
    process.exit(1);
  }
  const first = decodeCreate(created.body);
  check("the create answers a job id", true, first.jobId.length > 0, `len=${first.jobId.length}`);
  check("the create echoes the request id", requestId, first.requestId);
  check("the first create reports a new job", true, first.created);
  check("the create answers created_at", true, first.createdAtMillis > 0, `${first.createdAtMillis}`);

  const replay = await call(client, CREATE_PATH, job(requestId), null);
  const replayed = decodeCreate(replay.body);
  check("a replayed request id is accepted", OK, replay.status, replay.message);
  check("a replay answers the first job", first.jobId, replayed.jobId);
  check("a replay is not a new job", false, replayed.created);

  const read = await call(
    client,
    GET_PATH,
    Buffer.concat([stringField(1, first.jobId), stringField(2, owner)]),
    null,
  );
  check("the owner reads the job", OK, read.status, read.message);
  const job1 = decodeJobMessage(readFields(read.body.subarray(5)).get(1)[0]);
  check("the job answers its request id", requestId, job1.requestId);
  check("the job answers the requester it was created with", REQUESTER_CHAT_MESSAGE, job1.requester);
  check("the job answers no status while it is unsettled", null, job1.status);
  check(
    "the inputs read back in the order the caller named",
    JSON.stringify([
      { sequenceOrder: 0, mediaId: targets[0] },
      { sequenceOrder: 1, mediaId: targets[1] },
    ]),
    JSON.stringify(job1.inputs),
  );

  const foreign = await call(
    client,
    GET_PATH,
    Buffer.concat([stringField(1, first.jobId), stringField(2, other)]),
    null,
  );
  check("another owner reads the job as missing", NOT_FOUND, foreign.status, foreign.message);

  const absent = await call(
    client,
    GET_PATH,
    Buffer.concat([stringField(1, randomUUID()), stringField(2, owner)]),
    null,
  );
  check("an absent job reads as missing", NOT_FOUND, absent.status, absent.message);

  const cancelled = await call(
    client,
    CANCEL_PATH,
    Buffer.concat([stringField(1, first.jobId), stringField(2, owner)]),
    null,
  );
  check("the owner cancels the job", OK, cancelled.status, cancelled.message);
  const cancel = decodeCancel(cancelled.body);
  check("the cancel answers the job id", first.jobId, cancel.jobId);
  check("the cancel is accepted", true, cancel.accepted);
  check("a job that has not settled answers no stitch status", STITCH_UNSPECIFIED, cancel.status);

  const foreignCancel = await call(
    client,
    CANCEL_PATH,
    Buffer.concat([stringField(1, first.jobId), stringField(2, other)]),
    null,
  );
  check("another owner cannot cancel the job", NOT_FOUND, foreignCancel.status, foreignCancel.message);

  const list = await call(client, LIST_PATH, Buffer.concat([stringField(1, owner), intField(4, 10)]), null);
  check("the owner lists its jobs", OK, list.status, list.message);
  const page = decodeList(list.body);
  check("the list holds the created job", 1, page.jobs.length, `jobs=${page.jobs.length}`);
  check("the list answers no further page", "", page.nextPageToken);

  const otherList = await call(client, LIST_PATH, Buffer.concat([stringField(1, other), intField(4, 10)]), null);
  check("another owner lists nothing", 0, decodeList(otherList.body).jobs.length);

  const requesterMatch = await call(
    client,
    LIST_PATH,
    Buffer.concat([
      stringField(1, owner),
      rawIntField(3, REQUESTER_CHAT_MESSAGE),
      intField(4, 10),
    ]),
    null,
  );
  check(
    "a requester filter lists the jobs that carry it",
    true,
    decodeList(requesterMatch.body).jobs.length >= 1,
  );

  const requesterMiss = await call(
    client,
    LIST_PATH,
    Buffer.concat([
      stringField(1, owner),
      rawIntField(3, REQUESTER_STUDIO),
      intField(4, 10),
    ]),
    null,
  );
  check(
    "a requester filter lists nothing for a requester no job carried",
    0,
    decodeList(requesterMiss.body).jobs.length,
  );

  const filtered = await call(
    client,
    LIST_PATH,
    Buffer.concat([
      stringField(1, owner),
      enumFilter([STITCH_COMPLETED]),
      intField(4, 10),
    ]),
    null,
  );
  check(
    "a status filter the job does not carry lists nothing",
    0,
    decodeList(filtered.body).jobs.length,
    `status=${filtered.status}`,
  );

  const badFilter = await call(
    client,
    LIST_PATH,
    Buffer.concat([
      stringField(1, owner),
      enumFilter([STITCH_UNSPECIFIED]),
      intField(4, 10),
    ]),
    null,
  );
  check("a status the reference cannot settle on is refused", INVALID_ARGUMENT, badFilter.status, badFilter.message);

  const duplicateOrder = await call(
    client,
    CREATE_PATH,
    createRequest(
      videoMergeJob({
        requestId: randomUUID(),
        requester: REQUESTER_CHAT_MESSAGE,
        owner,
        inputs: [
          [0, targets[0]],
          [0, targets[1]],
        ],
      }),
      randomUUID(),
    ),
    null,
  );
  check(
    "a repeated sequence order is refused",
    INVALID_ARGUMENT,
    duplicateOrder.status,
    duplicateOrder.message,
  );

  const oneInput = await call(
    client,
    CREATE_PATH,
    createRequest(
      videoMergeJob({
        requestId: randomUUID(),
        requester: REQUESTER_CHAT_MESSAGE,
        owner,
        inputs: [[0, targets[0]]],
      }),
      randomUUID(),
    ),
    null,
  );
  check("fewer than two inputs is refused", INVALID_ARGUMENT, oneInput.status, oneInput.message);

  const absentMedia = missingSibling(targets[0]);
  const missingObject = await call(
    client,
    CREATE_PATH,
    createRequest(
      videoMergeJob({
        requestId: randomUUID(),
        requester: REQUESTER_CHAT_MESSAGE,
        owner,
        inputs: [
          [0, targets[0]],
          [1, absentMedia],
        ],
      }),
      randomUUID(),
    ),
    null,
  );
  check(
    "a create naming an absent object is refused before the job is accepted",
    FAILED_PRECONDITION,
    missingObject.status,
    missingObject.message,
  );

  const noRequester = await call(
    client,
    CREATE_PATH,
    createRequest(
      videoMergeJob({
        requestId: randomUUID(),
        requester: 0,
        owner,
        inputs: [
          [0, targets[0]],
          [1, targets[1]],
        ],
      }),
      randomUUID(),
    ),
    null,
  );
  check("a create naming no requester is refused", INVALID_ARGUMENT, noRequester.status, noRequester.message);

  const noKey = await call(
    client,
    CREATE_PATH,
    createRequest(
      videoMergeJob({
        requestId: "",
        requester: REQUESTER_STUDIO,
        owner,
        inputs: [
          [0, targets[0]],
          [1, targets[1]],
        ],
      }),
      "",
    ),
    null,
  );
  check("a create naming no request id and no key is refused", INVALID_ARGUMENT, noKey.status, noKey.message);

  const profiled = await call(
    client,
    CREATE_PATH,
    createRequest(
      videoMergeJob({
        requestId: randomUUID(),
        requester: REQUESTER_CHAT_MESSAGE,
        owner,
        inputs: [
          [0, targets[0]],
          [1, targets[1]],
        ],
        frameRate: 30,
      }),
      randomUUID(),
    ),
    null,
  );
  check("a create carrying an output profile is accepted", OK, profiled.status, profiled.message);
  if (profiled.status === OK) {
    const profiledId = decodeCreate(profiled.body).jobId;
    const profileRead = await call(
      client,
      GET_PATH,
      Buffer.concat([stringField(1, profiledId), stringField(2, owner)]),
      null,
    );
    check(
      "the job answers the frame rate it was created with",
      30,
      decodeJobMessage(readFields(profileRead.body.subarray(5)).get(1)[0]).frameRate,
    );
  }

  for (let index = 0; index < 1; index += 1) {
    await call(
      client,
      CREATE_PATH,
      createRequest(
        videoMergeJob({
          requestId: randomUUID(),
          requester: REQUESTER_CHAT_MESSAGE,
          owner,
          inputs: [
            [0, targets[0]],
            [1, targets[1]],
          ],
        }),
        randomUUID(),
      ),
      null,
    );
  }

  const paged = await call(client, LIST_PATH, Buffer.concat([stringField(1, owner), intField(4, 2)]), null);
  const firstPage = decodeList(paged.body);
  check("a full page holds the page size", 2, firstPage.jobs.length);
  check("a full page answers a token", true, firstPage.nextPageToken.length > 0);
  const token = Buffer.from(firstPage.nextPageToken, "utf8");
  check(
    "the page token is the base64 cursor this service speaks",
    true,
    /^[A-Za-z0-9_-]+$/.test(firstPage.nextPageToken),
    firstPage.nextPageToken,
  );
  const next = await call(
    client,
    LIST_PATH,
    Buffer.concat([
      stringField(1, owner),
      intField(4, 2),
      stringField(5, firstPage.nextPageToken),
    ]),
    null,
  );
  check("the token pages the list", OK, next.status, next.message);
  const secondPage = decodeList(next.body);
  check(
    "the second page holds what the first left",
    1,
    secondPage.jobs.length,
    `jobs=${secondPage.jobs.length} token=${token.length}b`,
  );
  check("the last page answers no further token", "", secondPage.nextPageToken);

  const badToken = await call(
    client,
    LIST_PATH,
    Buffer.concat([stringField(1, owner), intField(4, 2), stringField(5, "not a cursor!")]),
    null,
  );
  check("a cursor that is not one is refused", INVALID_ARGUMENT, badToken.status, badToken.message);

  const replayId = randomUUID();
  const replayMessage = createRequest(
    videoMergeJob({
      requestId: replayId,
      requester: REQUESTER_CHAT_MESSAGE,
      owner,
      inputs: [
        [0, targets[0]],
        [1, targets[1]],
      ],
    }),
    replayId,
  );
  const replayCreated = await call(client, CREATE_PATH, replayMessage, null);
  check("a job is created before its input is removed", OK, replayCreated.status, replayCreated.message);
  const replayJobId = decodeCreate(replayCreated.body).jobId;
  const removed = await deleteObject(targets[0]);
  check("the input object is removed from the store", 204, removed);
  const replayAfterRemoval = await call(client, CREATE_PATH, replayMessage, null);
  check(
    "a replay after the object is gone is accepted",
    OK,
    replayAfterRemoval.status,
    replayAfterRemoval.message,
  );
  const replayedAfterRemoval = decodeCreate(replayAfterRemoval.body);
  check(
    "a replay after the object is gone answers the first job",
    replayJobId,
    replayedAfterRemoval.jobId,
  );
  check(
    "a replay after the object is gone is not a new job",
    false,
    replayedAfterRemoval.created,
  );

  client.close();
  const failed = results.filter((passed) => !passed).length;
  console.log(`merge-conformance: ${results.length - failed} of ${results.length} checks passed`);
  process.exit(failed === 0 ? 0 : 1);
}

await run(process.argv[2]);