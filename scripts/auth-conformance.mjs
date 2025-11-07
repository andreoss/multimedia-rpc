import { connect } from "node:http2";
import { createHmac, generateKeyPairSync, randomUUID, sign } from "node:crypto";
import { writeFileSync } from "node:fs";

const base64url = (value) => Buffer.from(value).toString("base64url");

function mintEs256(privateKey, kid, claims) {
  const header = { alg: "ES256", typ: "JWT", kid };
  const signingInput = `${base64url(JSON.stringify(header))}.${base64url(JSON.stringify(claims))}`;
  const signature = sign("sha256", Buffer.from(signingInput), {
    key: privateKey,
    dsaEncoding: "ieee-p1363",
  });
  return `${signingInput}.${base64url(signature)}`;
}

function mintHs256(secret, claims) {
  const header = { alg: "HS256", typ: "JWT" };
  const signingInput = `${base64url(JSON.stringify(header))}.${base64url(JSON.stringify(claims))}`;
  return `${signingInput}.${base64url(createHmac("sha256", secret).update(signingInput).digest())}`;
}

function writeKeys(jwksPath, tokensPath) {
  const published = generateKeyPairSync("ec", { namedCurve: "prime256v1" });
  const unpublished = generateKeyPairSync("ec", { namedCurve: "prime256v1" });
  const jwk = published.publicKey.export({ format: "jwk" });
  writeFileSync(
    jwksPath,
    JSON.stringify({ keys: [{ ...jwk, kid: "conformance-1", alg: "ES256", use: "sig" }] }),
  );
  const now = Math.floor(Date.now() / 1000);
  const claims = (subject, lifetime) => ({
    sub: subject,
    iss: "conformance-issuer",
    iat: now,
    nbf: now - 5,
    exp: now + lifetime,
  });
  const live = mintEs256(published.privateKey, "conformance-1", claims("subject-owner", 300));
  const other = mintEs256(published.privateKey, "conformance-1", claims("other-owner", 300));
  const expired = mintEs256(published.privateKey, "conformance-1", claims("subject-owner", -60));
  const foreign = mintEs256(unpublished.privateKey, "conformance-1", claims("subject-owner", 300));
  const symmetric = mintHs256("a-shared-secret-the-issuer-never-publishes", claims("subject-owner", 300));
  const parts = live.split(".");
  const tampered = `${parts[0]}.${parts[1]}.${base64url(Buffer.alloc(64, 7))}`;
  writeFileSync(tokensPath, JSON.stringify({ live, other, expired, foreign, symmetric, tampered }));
}

function varint(value) {
  let remaining = BigInt(value);
  const bytes = [];
  while (remaining > 0n) {
    let byte = Number(remaining & 0x7fn);
    remaining >>= 7n;
    if (remaining > 0n) {
      byte |= 0x80;
    }
    bytes.push(byte);
  }
  return Buffer.from(bytes);
}

function stringField(field, value) {
  const body = Buffer.from(value, "utf8");
  return Buffer.concat([varint(field * 8 + 2), varint(body.length), body]);
}

function messageField(field, value) {
  return Buffer.concat([varint(field * 8 + 2), varint(value.length), value]);
}

function boolField(field, value) {
  return Buffer.concat([varint(field * 8), varint(value ? 1 : 0)]);
}

function readVarint(buffer, start) {
  let value = 0n;
  let shift = 0n;
  let index = start;
  for (;;) {
    const byte = buffer[index];
    value |= BigInt(byte & 0x7f) << shift;
    index += 1;
    if ((byte & 0x80) === 0) {
      break;
    }
    shift += 7n;
  }
  return { value, index };
}

function readFields(buffer) {
  const fields = new Map();
  let index = 0;
  while (index < buffer.length) {
    const key = readVarint(buffer, index);
    const field = Number(key.value >> 3n);
    const wire = Number(key.value & 7n);
    if (wire === 2) {
      const length = readVarint(buffer, key.index);
      const from = length.index;
      const to = from + Number(length.value);
      const existing = fields.get(field) ?? [];
      existing.push(buffer.subarray(from, to));
      fields.set(field, existing);
      index = to;
    } else if (wire === 0) {
      const value = readVarint(buffer, key.index);
      const existing = fields.get(field) ?? [];
      existing.push(value.value);
      fields.set(field, existing);
      index = value.index;
    } else {
      throw new Error(`unsupported wire type ${wire}`);
    }
  }
  return fields;
}

function probeSpec(uri) {
  const probeSpec = messageField(1, stringField(1, uri));
  return messageField(2, probeSpec);
}

function createRequest(bodyOwner, idempotencyKey, uri) {
  return Buffer.concat([
    stringField(1, bodyOwner),
    stringField(2, idempotencyKey),
    messageField(3, probeSpec(uri)),
  ]);
}

function getRequest(bodyOwner, jobId) {
  return Buffer.concat([stringField(1, bodyOwner), stringField(2, jobId)]);
}

function decodeCreateResponse(body) {
  const frame = body.subarray(5);
  const fields = readFields(frame);
  return {
    jobId: fields.get(1)?.[0]?.toString("utf8") ?? "",
    created: fields.get(3)?.[0] === 1n,
  };
}

function decodeJobOwner(body) {
  const frame = body.subarray(5);
  const response = readFields(frame);
  const job = response.get(1)?.[0];
  if (!job) {
    return "";
  }
  const fields = readFields(job);
  return fields.get(2)?.[0]?.toString("utf8") ?? "";
}

function call(client, path, message, authorization) {
  return new Promise((resolve, reject) => {
    const headers = {
      ":method": "POST",
      ":path": path,
      "content-type": "application/grpc",
      te: "trailers",
    };
    if (authorization) {
      headers.authorization = authorization;
    }
    const stream = client.request(headers);
    const chunks = [];
    let status = null;
    let message0 = "";
    const record = (received) => {
      if (received["grpc-status"] !== undefined) {
        status = Number(received["grpc-status"]);
        message0 = String(received["grpc-message"] ?? "");
      }
    };
    stream.on("response", record);
    stream.on("trailers", record);
    stream.on("data", (chunk) => chunks.push(chunk));
    stream.on("end", () => resolve({ status, message: message0, body: Buffer.concat(chunks) }));
    stream.on("error", reject);
    const frame = Buffer.alloc(5 + message.length);
    frame.writeUInt8(0, 0);
    frame.writeUInt32BE(message.length, 1);
    message.copy(frame, 5);
    stream.end(frame);
  });
}

const CREATE_PATH = "/mmrpc.v1.MediaJobService/CreateJob";
const GET_PATH = "/mmrpc.v1.MediaJobService/GetJob";
const OK = 0;
const NOT_FOUND = 5;
const ALREADY_EXISTS = 6;
const UNAUTHENTICATED = 16;

function statusName(status) {
  switch (status) {
    case OK:
      return "OK";
    case NOT_FOUND:
      return "NOT_FOUND";
    case ALREADY_EXISTS:
      return "ALREADY_EXISTS";
    case UNAUTHENTICATED:
      return "UNAUTHENTICATED";
    default:
      return `grpc-status ${status}`;
  }
}

async function run(address, tokensPath) {
  const tokens = JSON.parse(await import("node:fs").then((fs) => fs.readFileSync(tokensPath, "utf8")));
  const uri = `s3://conformance/inputs/probe-${randomUUID()}.mp4`;
  const client = connect(`http://${address}`);
  client.on("error", (error) => {
    console.error(`conformance: connection failed: ${error.message}`);
    process.exit(1);
  });
  const results = [];
  const check = (name, expected, observed, detail) => {
    const passed = expected === observed;
    results.push(passed);
    console.log(
      `${passed ? "pass" : "FAIL"} ${name}: expected ${expected}, observed ${observed}${
        detail ? ` (${detail})` : ""
      }`,
    );
  };

  const idempotencyKey = randomUUID();
  const created = await call(
    client,
    CREATE_PATH,
    createRequest("body-owner", idempotencyKey, uri),
    `Bearer ${tokens.live}`,
  );
  check(
    "a token signed by the published key is accepted",
    "OK",
    statusName(created.status),
    created.message,
  );
  if (created.status !== OK) {
    client.close();
    process.exit(1);
  }
  const decoded = decodeCreateResponse(created.body);
  const jobId = decoded.jobId;
  check("the job id is answered", true, jobId.length > 0, `len=${jobId.length}`);
  check("the first create reports a new job", true, decoded.created, `created=${decoded.created}`);

  const replay = await call(
    client,
    CREATE_PATH,
    createRequest("body-owner", idempotencyKey, uri),
    `Bearer ${tokens.live}`,
  );
  const replayed = decodeCreateResponse(replay.body);
  check("a replayed key answers the original job", jobId, replayed.jobId, `status=${statusName(replay.status)}`);
  check("a replayed key is not a new job", false, replayed.created, `created=${replayed.created}`);

  const ownerView = await call(
    client,
    GET_PATH,
    getRequest("body-owner", jobId),
    `Bearer ${tokens.live}`,
  );
  check(
    "the owner reads the job under the token subject, not the body field",
    "subject-owner",
    decodeJobOwner(ownerView.body),
    `status=${statusName(ownerView.status)}`,
  );

  const foreignView = await call(
    client,
    GET_PATH,
    getRequest("body-owner", jobId),
    `Bearer ${tokens.other}`,
  );
  check(
    "another subject reading the same job is not found",
    "NOT_FOUND",
    statusName(foreignView.status),
    foreignView.message,
  );

  const refused = [
    ["an unauthenticated call", null],
    ["a token signed by an unpublished key", tokens.foreign],
    ["a symmetric token", tokens.symmetric],
    ["an expired token", tokens.expired],
    ["a tampered signature", tokens.tampered],
  ];
  for (const [name, token] of refused) {
    const answer = await call(
      client,
      GET_PATH,
      getRequest("body-owner", jobId),
      token === null ? null : `Bearer ${token}`,
    );
    check(name, "UNAUTHENTICATED", statusName(answer.status), answer.message);
  }

  const replayOther = await call(
    client,
    CREATE_PATH,
    createRequest("body-owner", idempotencyKey, uri),
    `Bearer ${tokens.other}`,
  );
  check(
    "another subject replaying the key is refused",
    "ALREADY_EXISTS",
    statusName(replayOther.status),
    replayOther.message,
  );

  client.close();
  const failed = results.filter((passed) => !passed).length;
  console.log(`conformance: ${results.length - failed} of ${results.length} checks passed`);
  process.exit(failed === 0 ? 0 : 1);
}

const [mode, first, second] = process.argv.slice(2);
if (mode === "keys") {
  writeKeys(first, second);
} else if (mode === "run") {
  await run(first, second);
} else {
  console.error("usage: auth-conformance.mjs keys <jwks> <tokens> | run <address> <tokens>");
  process.exit(2);
}