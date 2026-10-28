/** Playground transport limit, measured against the UI's 256 MiB deployment.
 * Provider image counts and token limits are separate model capabilities.
 */
export const EMBEDDING_REQUEST_MAX_BYTES = 4 * 1024 * 1024;
export const EMBEDDING_REQUEST_LIMIT_LABEL = "4 MiB";

/** Matches TextEncoder's output length without allocating the encoded copy;
 * lone surrogates encode as U+FFFD (3 bytes). */
export function utf8ByteLength(text: string): number {
  let bytes = 0;
  for (let index = 0; index < text.length; index++) {
    const code = text.charCodeAt(index);
    if (code < 0x80) bytes += 1;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xd800 && code <= 0xdbff && (text.charCodeAt(index + 1) & 0xfc00) === 0xdc00) {
      bytes += 4;
      index++;
    } else bytes += 3;
  }
  return bytes;
}
