import net from "node:net";
import { connect } from "node:http2";

export const CREATE_PATH = "/mmrpc.v1.MediaJobService/CreateJob";
export const GET_PATH = "/mmrpc.v1.MediaJobService/GetJob";
export const LIST_PATH = "/mmrpc.v1.MediaJobService/ListJobs";
export const OK = 0;
export const NOT_FOUND = 5;
export const ALREADY_EXISTS = 6;
export const FAILED_PRECONDITION = 9;
export const UNAUTHENTICATED = 16;
export const INVALID_ARGUMENT = 3;
export const UNIMPLEMENTED = 12;

export function varint(value) {
  let remaining = BigInt(value);
  const bytes = [];
  for (;;) {
    let byte = Number(remaining & 0x7fn);
    remaining >>= 7n;
    if (remaining > 0n) {
      byte |= 0x80;
    }
    bytes.push(byte);
    if (remaining === 0n) {
      break;
    }
  }
  return Buffer.from(bytes);
}

export function stringField(field, value) {
  const body = Buffer.from(value, "utf8");
  return Buffer.concat([varint(field * 8 + 2), varint(body.length), body]);
}

export function messageField(field, value) {
  return Buffer.concat([varint(field * 8 + 2), varint(value.length), value]);
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

export function readFields(buffer) {
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
    } else if (wire === 1 || wire === 5) {
      index = key.index + (wire === 1 ? 8 : 4);
    } else {
      throw new Error(`unsupported wire type ${wire}`);
    }
  }
  return fields;
}

export function mediaSource(uri) {
  return messageField(1, messageField(1, stringField(1, uri)));
}

export function probeSpec(uri) {
  return messageField(2, messageField(1, mediaSource(uri)));
}

export function createRequest(bodyOwner, idempotencyKey, uri) {
  return Buffer.concat([
    stringField(1, bodyOwner),
    stringField(2, idempotencyKey),
    messageField(3, probeSpec(uri)),
  ]);
}

export function getRequest(bodyOwner, jobId) {
  return Buffer.concat([stringField(1, bodyOwner), stringField(2, jobId)]);
}

export function listRequest(bodyOwner) {
  return stringField(1, bodyOwner);
}

export function decodeCreateResponse(body) {
  const fields = readFields(body.subarray(5));
  return {
    jobId: fields.get(1)?.[0]?.toString("utf8") ?? "",
    created: fields.get(3)?.[0] === 1n,
  };
}

export function decodeJobOwner(body) {
  const response = readFields(body.subarray(5));
  const job = response.get(1)?.[0];
  if (!job) {
    return "";
  }
  return readFields(job).get(2)?.[0]?.toString("utf8") ?? "";
}

export function call(client, path, message, authorization) {
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

export function statusName(status) {
  switch (status) {
    case OK:
      return "OK";
    case INVALID_ARGUMENT:
      return "INVALID_ARGUMENT";
    case NOT_FOUND:
      return "NOT_FOUND";
    case ALREADY_EXISTS:
      return "ALREADY_EXISTS";
    case FAILED_PRECONDITION:
      return "FAILED_PRECONDITION";
    case UNIMPLEMENTED:
      return "UNIMPLEMENTED";
    case UNAUTHENTICATED:
      return "UNAUTHENTICATED";
    default:
      return `grpc-status ${status}`;
  }
}

export function scrapeText(address) {
  return new Promise((resolve, reject) => {
    const [host, port] = address.split(":");
    const socket = net.connect({ host, port: Number(port) });
    const chunks = [];
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error("the scrape did not answer within the read budget"));
    }, 5000);
    socket.setTimeout(5000, () => {
      clearTimeout(timer);
      socket.destroy();
      reject(new Error("the scrape did not answer within the read budget"));
    });
    socket.on("connect", () => {
      socket.write("GET /metrics HTTP/1.0\r\nhost: local\r\n\r\n");
    });
    socket.on("data", (chunk) => chunks.push(chunk));
    socket.on("end", () => {
      clearTimeout(timer);
      resolve(Buffer.concat(chunks).toString("utf8"));
    });
    socket.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}