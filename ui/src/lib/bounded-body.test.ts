import { describe, expect, it, vi } from "vitest";
import { BodyReadError, readBoundedBody } from "./bounded-body";

function streamedRequest(chunks: Uint8Array[], headers: HeadersInit = {}) {
  const cancel = vi.fn();
  let next = 0;
  const body = new ReadableStream<Uint8Array>({
    pull(controller) {
      if (next < chunks.length) controller.enqueue(chunks[next++]);
      else controller.close();
    },
    cancel,
  });
  return {
    request: new Request("http://localhost/", {
      method: "POST", headers, body, duplex: "half",
    } as RequestInit & { duplex: "half" }),
    cancel,
  };
}

const bytes = (text: string) => new TextEncoder().encode(text);
const text = (value: Uint8Array) => new TextDecoder().decode(value);

describe("bounded request body reader", () => {
  it("accepts the inclusive byte boundary and returns bytes unchanged across chunk splits", async () => {
    const input = ' {"text":"🌲"}\n';
    const encoded = bytes(input);
    const { request } = streamedRequest([
      encoded.slice(0, 11), encoded.slice(11, 12), encoded.slice(12),
    ]);
    expect(await readBoundedBody(request, encoded.byteLength)).toEqual(encoded);
  });

  it.each([new Headers(), new Headers({ "content-length": "1" }), new Headers({ "content-length": "untrusted" })])(
    "rejects actual bytes regardless of declared size: %j",
    async (headers) => {
      const { request, cancel } = streamedRequest([bytes('{"x":"'), bytes("abcdef"), bytes('"}')], headers);
      await expect(readBoundedBody(request, 8)).rejects.toMatchObject({ status: 413 });
      expect(cancel).toHaveBeenCalledOnce();
    },
  );

  it("sizes storage from Content-Length and grows past an understated or missing one", async () => {
    const small = streamedRequest([bytes("{}")], { "content-length": "2" });
    const read = await readBoundedBody(small.request, 4 * 1024 * 1024);
    expect(text(read)).toBe("{}");
    expect(read.buffer.byteLength).toBe(2);

    const large = "x".repeat(200 * 1024);
    for (const headers of [{}, { "content-length": "3" }] as HeadersInit[]) {
      const chunks = [bytes(large.slice(0, 100)), bytes(large.slice(100))];
      const { request } = streamedRequest(chunks, headers);
      expect(text(await readBoundedBody(request, 1024 * 1024))).toBe(large);
    }
  });

  it("handles tiny and empty chunks without accumulating per-chunk storage", async () => {
    const input = JSON.stringify({ text: "🌲".repeat(500) });
    const encoded = bytes(input);
    let offset = 0;
    let empty = true;
    const body = new ReadableStream<Uint8Array>({
      pull(controller) {
        if (offset === encoded.length) { controller.close(); return; }
        controller.enqueue(empty ? new Uint8Array() : encoded.subarray(offset, ++offset));
        empty = !empty;
      },
    });
    const request = new Request("http://localhost/", { method: "POST", body, duplex: "half" } as RequestInit & { duplex: "half" });
    expect(text(await readBoundedBody(request, encoded.byteLength))).toBe(input);
  });

  it("rejects an oversized Content-Length without reading the stream", async () => {
    const cancel = vi.fn();
    const request = {
      headers: new Headers({ "content-length": "99999999999999999999" }),
      body: { cancel },
    } as unknown as Request;
    cancel.mockResolvedValue(undefined);
    await expect(readBoundedBody(request, 10)).rejects.toMatchObject({ status: 413 });
    expect(cancel).toHaveBeenCalledOnce();
  });

  it("does not parse JSON and reports stream failures as 400", async () => {
    const parse = vi.spyOn(JSON, "parse");
    try {
      const malformed = streamedRequest([bytes('{"input":NaN}')]);
      expect(text(await readBoundedBody(malformed.request, 1024))).toBe('{"input":NaN}');
      expect(parse).not.toHaveBeenCalled();
    } finally {
      parse.mockRestore();
    }
    const request = new Request("http://localhost/", {
      method: "POST", duplex: "half",
      body: new ReadableStream({ start(controller) { controller.error(new Error("disconnected")); } }),
    } as RequestInit & { duplex: "half" });
    const failure = readBoundedBody(request, 10);
    await expect(failure).rejects.toBeInstanceOf(BodyReadError);
    await expect(failure).rejects.toMatchObject({ status: 400 });
  });
});
