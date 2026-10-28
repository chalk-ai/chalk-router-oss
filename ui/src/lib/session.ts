import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";

export const SESSION_COOKIE = "router_session";
export const SESSION_SECONDS = 15 * 60;
export interface Session {
  token: string;
  environment: string;
  inferenceKey?: string;
  expires: number;
}

export class AuthError extends Error {
  constructor(message: string, readonly status: number) { super(message); }
}

export function authConfig() {
  const secret = process.env.ROUTER_UI_SESSION_SECRET;
  const configuredOrigin = process.env.ROUTER_UI_ORIGIN;
  if (!secret || !/^[a-fA-F0-9]{64}$/.test(secret) || !configuredOrigin) {
    throw new AuthError("UI authentication is not configured", 503);
  }
  let url: URL;
  try { url = new URL(configuredOrigin); } catch { throw new AuthError("Invalid UI origin", 503); }
  if (url.origin !== configuredOrigin || (url.protocol !== "https:" &&
      !(url.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)))) {
    throw new AuthError("UI origin must be HTTPS (HTTP is allowed only on loopback)", 503);
  }
  return { key: Buffer.from(secret, "hex"), origin: url.origin, secure: url.protocol === "https:" };
}

export function sealSession(session: Session): string {
  const { key, origin } = authConfig();
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  cipher.setAAD(Buffer.from(origin));
  const ciphertext = Buffer.concat([cipher.update(JSON.stringify(session), "utf8"), cipher.final()]);
  const value = Buffer.concat([iv, cipher.getAuthTag(), ciphertext]).toString("base64url");
  if (value.length > 3800) throw new AuthError("Credentials exceed the session size limit", 400);
  return value;
}

export function openSession(value?: string): Session {
  const { key, origin } = authConfig();
  try {
    if (!value || value.length > 3800) throw new Error();
    const bytes = Buffer.from(value, "base64url");
    const decipher = createDecipheriv("aes-256-gcm", key, bytes.subarray(0, 12));
    decipher.setAAD(Buffer.from(origin));
    decipher.setAuthTag(bytes.subarray(12, 28));
    const session: Session = JSON.parse(Buffer.concat([
      decipher.update(bytes.subarray(28)), decipher.final(),
    ]).toString("utf8"));
    if (typeof session.token !== "string" || !session.token || typeof session.environment !== "string" ||
        !Number.isFinite(session.expires) || session.expires <= Date.now() ||
        session.expires > Date.now() + SESSION_SECONDS * 1000 ||
        (session.inferenceKey !== undefined && !/^ck-[\w-]+$/.test(session.inferenceKey))) throw new Error();
    return session;
  } catch { throw new AuthError("Sign in required", 401); }
}

export function sessionFromHeaders(headers: Headers): Session {
  const matches = (headers.get("cookie") ?? "").split(";").map(v => v.trim())
    .filter(v => v.startsWith(`${SESSION_COOKIE}=`));
  return openSession(matches.length === 1 ? matches[0].slice(SESSION_COOKIE.length + 1) : undefined);
}

export function checkMutation(request: Request) {
  const { origin } = authConfig();
  if (["GET", "HEAD", "OPTIONS"].includes(request.method)) return;
  if (request.headers.get("origin") !== origin || request.headers.get("sec-fetch-site") === "cross-site") {
    throw new AuthError("Same-origin requests are required", 403);
  }
  const contentType = request.headers.get("content-type")?.split(";")[0].trim().toLowerCase();
  if ((["POST", "PUT", "PATCH"].includes(request.method) || contentType) && contentType !== "application/json") {
    throw new AuthError("Content-Type must be application/json", 415);
  }
}
