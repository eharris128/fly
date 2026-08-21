// Tests for the JS half of the control-socket frame codec. Its Rust twin
// (src-tauri/src/control/frame.rs) carries its own unit tests; the two are
// edited only together with docs/core-protocol.md, and these tests pin the
// JS side to the same wire facts: u32 LE length (excluding the kind byte),
// u8 kind, 0x01 JSON / 0x02 pane output (u64 LE id + bytes) / 0x03 pane
// input, 8 MiB frame cap, incremental reassembly.
import { describe, it, expect } from "vitest";
import { encodeJson, encodePaneInput, FrameReader } from "./protocol.js";

const KIND_JSON = 0x01;
const KIND_PANE_OUTPUT = 0x02;
const KIND_PANE_INPUT = 0x03;

function collectReader() {
  const got = { json: [], panes: [], errors: [] };
  const reader = new FrameReader(
    (msg) => got.json.push(msg),
    (paneId, bytes) => got.panes.push([paneId, Buffer.from(bytes)]),
    (err) => got.errors.push(String(err.message ?? err)),
  );
  return { reader, got };
}

/** Build a pane-output frame (server→client; no encoder exists client-side). */
function paneOutputFrame(paneId, bytes) {
  const head = Buffer.alloc(13);
  head.writeUInt32LE(8 + bytes.length, 0);
  head.writeUInt8(KIND_PANE_OUTPUT, 4);
  head.writeBigUInt64LE(BigInt(paneId), 5);
  return Buffer.concat([head, Buffer.from(bytes)]);
}

describe("encodeJson", () => {
  it("length field excludes the kind byte and round-trips through FrameReader", () => {
    const obj = { id: 7, cmd: "core/ping", args: null };
    const frame = encodeJson(obj);
    const body = Buffer.from(JSON.stringify(obj), "utf8");
    expect(frame.readUInt32LE(0)).toBe(body.length);
    expect(frame.readUInt8(4)).toBe(KIND_JSON);
    expect(frame.length).toBe(5 + body.length);

    const { reader, got } = collectReader();
    reader.push(frame);
    expect(got.json).toEqual([obj]);
    expect(got.errors).toEqual([]);
  });

  it("survives multi-byte UTF-8 (length is bytes, not chars)", () => {
    const obj = { text: "naïve — 🪰 café" };
    const frame = encodeJson(obj);
    expect(frame.readUInt32LE(0)).toBe(
      Buffer.byteLength(JSON.stringify(obj), "utf8"),
    );
    const { reader, got } = collectReader();
    reader.push(frame);
    expect(got.json).toEqual([obj]);
  });
});

describe("encodePaneInput", () => {
  it("writes u32 LE (8 + payload), kind 0x03, u64 LE pane id, exact bytes", () => {
    const bytes = Buffer.from([0x1b, 0x5b, 0x41, 0x00, 0xff]);
    const frame = encodePaneInput(42, bytes);
    expect(frame.readUInt32LE(0)).toBe(8 + bytes.length);
    expect(frame.readUInt8(4)).toBe(KIND_PANE_INPUT);
    expect(Number(frame.readBigUInt64LE(5))).toBe(42);
    expect(frame.subarray(13).equals(bytes)).toBe(true);
  });

  it("carries a pane id beyond 32 bits", () => {
    const big = 2 ** 40 + 3;
    const frame = encodePaneInput(big, Buffer.from("x"));
    expect(Number(frame.readBigUInt64LE(5))).toBe(big);
  });
});

describe("FrameReader", () => {
  it("parses pane-output frames and copies bytes out of the rolling buffer", () => {
    const { reader, got } = collectReader();
    const payload = Buffer.from("hello pane");
    reader.push(paneOutputFrame(9, payload));
    expect(got.panes).toHaveLength(1);
    const [paneId, bytes] = got.panes[0];
    expect(paneId).toBe(9);
    expect(bytes.equals(payload)).toBe(true);
    // A later push must not mutate the delivered copy (subarray aliasing).
    reader.push(paneOutputFrame(9, Buffer.from("XXXXXXXXXX")));
    expect(got.panes[0][1].equals(payload)).toBe(true);
  });

  it("reassembles frames delivered one byte at a time", () => {
    const { reader, got } = collectReader();
    const frames = Buffer.concat([
      encodeJson({ id: 1 }),
      paneOutputFrame(2, Buffer.from([0xaa, 0xbb])),
      encodeJson({ event: "pane://exit", payload: { paneId: 2 } }),
    ]);
    for (const byte of frames) reader.push(Buffer.from([byte]));
    expect(got.json).toEqual([
      { id: 1 },
      { event: "pane://exit", payload: { paneId: 2 } },
    ]);
    expect(got.panes).toEqual([[2, Buffer.from([0xaa, 0xbb])]]);
    expect(got.errors).toEqual([]);
  });

  it("drains multiple frames arriving in a single chunk", () => {
    const { reader, got } = collectReader();
    reader.push(Buffer.concat([encodeJson({ a: 1 }), encodeJson({ b: 2 })]));
    expect(got.json).toEqual([{ a: 1 }, { b: 2 }]);
  });

  it("holds a partial frame until the rest arrives", () => {
    const { reader, got } = collectReader();
    const frame = encodeJson({ whole: true });
    reader.push(frame.subarray(0, 3));
    expect(got.json).toEqual([]);
    reader.push(frame.subarray(3));
    expect(got.json).toEqual([{ whole: true }]);
  });

  it("rejects an oversize frame (8 MiB cap)", () => {
    const { reader, got } = collectReader();
    const head = Buffer.alloc(5);
    head.writeUInt32LE(8 * 1024 * 1024 + 1, 0);
    head.writeUInt8(KIND_JSON, 4);
    reader.push(head);
    expect(got.errors).toEqual(["oversize frame"]);
  });

  it("rejects a pane-output frame too short for the u64 id", () => {
    const { reader, got } = collectReader();
    const head = Buffer.alloc(5 + 4);
    head.writeUInt32LE(4, 0);
    head.writeUInt8(KIND_PANE_OUTPUT, 4);
    reader.push(head);
    expect(got.errors).toEqual(["short pane frame"]);
  });

  it("rejects an unknown frame kind", () => {
    const { reader, got } = collectReader();
    const head = Buffer.alloc(6);
    head.writeUInt32LE(1, 0);
    head.writeUInt8(0x7f, 4);
    reader.push(head);
    expect(got.errors).toEqual(["unknown frame kind 127"]);
  });

  it("reports malformed JSON through onError", () => {
    const { reader, got } = collectReader();
    const body = Buffer.from("{not json", "utf8");
    const head = Buffer.alloc(5);
    head.writeUInt32LE(body.length, 0);
    head.writeUInt8(KIND_JSON, 4);
    reader.push(Buffer.concat([head, body]));
    expect(got.errors).toHaveLength(1);
    expect(got.json).toEqual([]);
  });
});
