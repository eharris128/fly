// The control-socket wire codec, JS side (docs/core-protocol.md — edited
// only together with core/src/control/). u32 LE length | u8 kind |
// payload; kinds: 0x01 JSON, 0x02 pane output (u64 LE id + bytes, server→
// client), 0x03 pane input (client→server).
'use strict';

const KIND_JSON = 0x01;
const KIND_PANE_OUTPUT = 0x02;
const KIND_PANE_INPUT = 0x03;
const MAX_FRAME = 8 * 1024 * 1024;

function encodeJson(obj) {
  const body = Buffer.from(JSON.stringify(obj), 'utf8');
  const head = Buffer.alloc(5);
  head.writeUInt32LE(body.length, 0);
  head.writeUInt8(KIND_JSON, 4);
  return Buffer.concat([head, body]);
}

function encodePaneInput(paneId, bytes) {
  const head = Buffer.alloc(13);
  head.writeUInt32LE(8 + bytes.length, 0);
  head.writeUInt8(KIND_PANE_INPUT, 4);
  head.writeBigUInt64LE(BigInt(paneId), 5);
  return Buffer.concat([head, Buffer.from(bytes)]);
}

/** Incremental frame parser over a stream of Buffers. */
class FrameReader {
  constructor(onJson, onPaneOutput, onError) {
    this.buf = Buffer.alloc(0);
    this.onJson = onJson;
    this.onPaneOutput = onPaneOutput;
    this.onError = onError;
  }

  push(chunk) {
    this.buf = this.buf.length === 0 ? chunk : Buffer.concat([this.buf, chunk]);
    for (;;) {
      if (this.buf.length < 5) return;
      const len = this.buf.readUInt32LE(0);
      if (len > MAX_FRAME) {
        this.onError(new Error('oversize frame'));
        return;
      }
      if (this.buf.length < 5 + len) return;
      const kind = this.buf.readUInt8(4);
      const payload = this.buf.subarray(5, 5 + len);
      this.buf = this.buf.subarray(5 + len);
      if (kind === KIND_JSON) {
        try {
          this.onJson(JSON.parse(payload.toString('utf8')));
        } catch (e) {
          this.onError(e);
          return;
        }
      } else if (kind === KIND_PANE_OUTPUT) {
        if (len < 8) {
          this.onError(new Error('short pane frame'));
          return;
        }
        const paneId = Number(payload.readBigUInt64LE(0));
        // Copy out: subarray views alias the rolling buffer.
        this.onPaneOutput(paneId, Buffer.from(payload.subarray(8)));
      } else {
        this.onError(new Error(`unknown frame kind ${kind}`));
        return;
      }
    }
  }
}

module.exports = { encodeJson, encodePaneInput, FrameReader };
