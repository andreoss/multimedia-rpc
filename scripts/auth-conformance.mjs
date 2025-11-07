import { connect } from "node:http2";
import { createHmac, generateKeyPairSync, randomUUID, sign } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";

import {
  ALREADY_EXISTS,
  CREATE_PATH,
  GET_PATH,
  NOT_FOUND,
  OK,
  UNAUTHENTICATED,
  call,
  createRequest,
  decodeCreateResponse,
  decodeJobOwner,
  getRequest,
  statusName,
} from "./lib/grpc.mjs";

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
  const noExpiry = mintEs256(published.privateKey, "conformance-1", {
    sub: "subject-owner",
    iss: "conformance-issuer",
    iat: now,
    nbf: now - 5,
  });
  writeFileSync(
    tokensPath,
    JSON.stringify({ live, other, expired, foreign, symmetric, tampered, noExpiry }),
  );
}

async function run(address, tokensPath) {
  const tokens = JSON.parse(readFileSync(tokensPath, "utf8"));
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
      `${passed ? "pass" : "FAIL"} ${name}: expected ${expected}, observed ${observed}${detail ? ` (${detail})` : ""}`,
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
  check(
    "a replayed key answers the original job",
    jobId,
    replayed.jobId,
    `status=${statusName(replay.status)}`,
  );
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
    ["a token that carries no expiry", tokens.noExpiry],
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