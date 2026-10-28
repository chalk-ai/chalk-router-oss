export class BodyReadError extends Error {
  constructor(message: string, readonly status: 400 | 413) {
    super(message);
    this.name = "BodyReadError";
  }
}

const INITIAL_CAPACITY = 64 * 1024;

/** Bound actual bytes, even for chunked requests, and return them unparsed so
 * a proxy forwards one copy; the upstream validates JSON itself.
 */
export async function readBoundedBody(request: Request, maxBytes: number): Promise<Uint8Array<ArrayBuffer>> {
  const tooLarge = () => new BodyReadError(`Request body exceeds the ${maxBytes} byte limit.`, 413);
  const contentLength = request.headers.get("content-length");
  const declared = contentLength && /^\d+$/.test(contentLength) ? Number(contentLength) : undefined;
  if (declared !== undefined && declared > maxBytes) {
    if (request.body) await request.body.cancel().catch(() => {});
    throw tooLarge();
  }

  const reader = request.body?.getReader();
  // One contiguous buffer bounds overhead from arbitrarily small or empty
  // chunks. Size it from the declared length; grow only when that is absent
  // or understated, so small requests never pay for the full limit.
  let buffer = new Uint8Array(Math.min(maxBytes, declared ?? INITIAL_CAPACITY));
  let bytes = 0;
  try {
    if (reader) {
      while (true) {
        const chunk = await reader.read();
        if (chunk.done) break;
        const needed = bytes + chunk.value.byteLength;
        if (needed > maxBytes) {
          await reader.cancel().catch(() => {});
          throw tooLarge();
        }
        if (needed > buffer.byteLength) {
          const grown = new Uint8Array(Math.min(maxBytes, Math.max(needed, buffer.byteLength * 2)));
          grown.set(buffer.subarray(0, bytes));
          buffer = grown;
        }
        buffer.set(chunk.value, bytes);
        bytes = needed;
      }
    }
    return buffer.subarray(0, bytes);
  } catch (error) {
    if (error instanceof BodyReadError) throw error;
    if (reader) await reader.cancel().catch(() => {});
    throw new BodyReadError("The request body could not be read.", 400);
  } finally {
    reader?.releaseLock();
  }
}
