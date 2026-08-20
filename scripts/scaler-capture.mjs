import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { connect } from "node:http2";

import {
  CREATE_PATH,
  OK,
  call,
  createRequest,
  scrapeText,
  statusName,
} from "./lib/grpc.mjs";

const [backlog] = process.argv.slice(2);

function waitFor(stream, prefix, deadlineMs = 60000) {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    let seen = "";
    const inspect = (chunk) => {
      seen += chunk.toString("utf8");
      for (const line of seen.split("\n")) {
        if (line.startsWith(prefix)) {
          resolve(line.slice(prefix.length).trim());
          return true;
        }
      }
      if (Date.now() - started > deadlineMs) {
        reject(new Error(`the daemon never announced ${prefix}; it said: ${seen}`));
        return true;
      }
      return false;
    };
    stream.on("data", (chunk) => {
      if (!inspect(chunk)) {
        setTimeout(() => inspect(Buffer.alloc(0)), 200);
      }
    });
    setInterval(() => {
      if (Date.now() - started > deadlineMs) {
        reject(new Error(`the daemon never announced ${prefix}; it said: ${seen}`));
      }
    }, 1000);
  });
}

const daemon = spawn("target/debug/mmrpcd", ["server"], {
  env: {
    ...process.env,
    MM_BIND_ADDR: "127.0.0.1:0",
    MM_METRICS_BIND_ADDR: "127.0.0.1:0",
    MM_METRICS_INTERVAL_SECS: "1",
    MM_MIGRATE: "on",
    MM_DATABASE_URL: process.env.MM_DATABASE_URL ?? "postgres://mm:mm@127.0.0.1:5433/mm",
    MM_OBJECTSTORE_ENDPOINT: process.env.MM_OBJECTSTORE_ENDPOINT ?? "http://127.0.0.1:8333",
    MM_OBJECTSTORE_BUCKET: process.env.MM_OBJECTSTORE_BUCKET ?? "mm-scaler",
    MM_OBJECTSTORE_ACCESS_KEY: process.env.MM_OBJECTSTORE_ACCESS_KEY ?? "mm-test-access",
    MM_OBJECTSTORE_SECRET_KEY: process.env.MM_OBJECTSTORE_SECRET_KEY ?? "mm-test-secret",
    MM_OBJECTSTORE_REGION: process.env.MM_OBJECTSTORE_REGION ?? "us-east-1",
  },
  stdio: ["ignore", "pipe", "inherit"],
});

try {
  const metricsAddress = await waitFor(daemon.stdout, "metrics ");
  const serviceAddress = await waitFor(daemon.stdout, "listening ");
  console.log(`service: ${serviceAddress}`);
  console.log(`metrics: ${metricsAddress}`);

  const client = connect(`http://${serviceAddress}`);
  const before = Number(
    (await scrapeText(metricsAddress))
      .split("\n")
      .find((line) => line.startsWith("mm_jobs_pending "))
      ?.split(" ")[1] ?? "0",
  );
  const wanted = Number(backlog ?? 5);
  const created = [];
  for (let index = 0; index < wanted; index += 1) {
    const uri = `s3://mm-scaler/inputs/backlog-${randomUUID()}.mp4`;
    const answer = await call(client, CREATE_PATH, createRequest("scaler-owner", randomUUID(), uri));
    if (answer.status !== OK) {
      throw new Error(`create answered ${statusName(answer.status)}: ${answer.message}`);
    }
    created.push(answer);
  }
  client.close();
  console.log(`created ${created.length} jobs with no worker running`);

  let observed = before;
  for (let attempt = 0; attempt < 30 && observed < before + created.length; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 500));
    const body = await scrapeText(metricsAddress);
    observed = Number(
      body
        .split("\n")
        .find((line) => line.startsWith("mm_jobs_pending "))
        ?.split(" ")[1] ?? "0",
    );
  }
  console.log(`mm_jobs_pending before the backlog: ${before}`);
  console.log(`mm_jobs_pending after the backlog: ${observed}`);

  const body = await scrapeText(metricsAddress);
  for (const line of body.split("\n")) {
    if (line.startsWith("mm_jobs_pending") || line.startsWith("mm_claim_latency_seconds_count")) {
      console.log(line);
    }
  }
  if (observed < before + created.length) {
    console.error(`scaler: the backlog was not reported (${observed} of ${before + created.length})`);
    process.exit(1);
  }
} finally {
  daemon.kill("SIGTERM");
  setTimeout(() => {
    daemon.stdout.destroy();
    process.exit(0);
  }, 500);
}