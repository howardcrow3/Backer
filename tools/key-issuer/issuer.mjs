// Backer key issuer core. The store (FastSpring, or Paddle through a webhook) calls it when someone buys, and it
// answers with the customer's license key. It signs with a private key that lives only in AWS Parameter Store,
// never in the repo or the app. The key format must stay identical to crates/backer-license/src/lib.rs.
//
// Runs anywhere with Node 20+ (WebCrypto Ed25519): AWS Lambda uses lambda.mjs, tests use handleRequest directly.

const DOMAIN = new TextEncoder().encode("backer-license-v1\0");
const FORMAT = 1, EDITION_STANDARD = 1, MAJOR = 1, MAX_EMAIL = 80;

const hex = (s) => Uint8Array.from(s.match(/../g).map((h) => parseInt(h, 16)));

// RFC 4648 base32, no padding.
function base32(bytes) {
  const A = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  let bits = 0, value = 0, out = "";
  for (const b of bytes) {
    value = (value << 8) | b; bits += 8;
    while (bits >= 5) { out += A[(value >>> (bits - 5)) & 31]; bits -= 5; }
  }
  if (bits > 0) out += A[(value << (5 - bits)) & 31];
  return out;
}

export function payloadFor(email, issuedDay) {
  const e = new TextEncoder().encode(email.trim().toLowerCase());
  if (e.length === 0 || e.length > MAX_EMAIL) throw new Error("bad email");
  const p = new Uint8Array(8 + e.length);
  p.set([FORMAT, EDITION_STANDARD, MAJOR, (issuedDay >>> 24) & 255, (issuedDay >>> 16) & 255, (issuedDay >>> 8) & 255, issuedDay & 255, e.length]);
  p.set(e, 8);
  return p;
}

export async function issueKey(seedHex, email, issuedDay) {
  // Ed25519 private key from a 32-byte seed, wrapped as PKCS#8 for WebCrypto.
  const pkcs8 = new Uint8Array([0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20, ...hex(seedHex)]);
  const key = await crypto.subtle.importKey("pkcs8", pkcs8, { name: "Ed25519" }, false, ["sign"]);
  const payload = payloadFor(email, issuedDay);
  const msg = new Uint8Array(DOMAIN.length + payload.length);
  msg.set(DOMAIN); msg.set(payload, DOMAIN.length);
  const sig = new Uint8Array(await crypto.subtle.sign("Ed25519", key, msg));
  const raw = new Uint8Array(payload.length + sig.length);
  raw.set(payload); raw.set(sig, payload.length);
  return "BACKER-" + base32(raw).match(/.{1,5}/g).join("-");
}

export function constantTimeEqual(a, b) {
  const x = new TextEncoder().encode(a), y = new TextEncoder().encode(b);
  let d = x.length ^ y.length;
  for (let i = 0; i < Math.max(x.length, y.length); i++) d |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return d === 0;
}

function parseBody(body, contentType) {
  if (!body) return {};
  if ((contentType || "").includes("json")) { try { return JSON.parse(body); } catch { return {}; } }
  return Object.fromEntries(new URLSearchParams(body));
}

/**
 * req: { query: object, body: string, contentType: string, authorization: string|undefined }
 * secrets: { signingKeyHex, user, password }
 * Returns { status, text }.
 */
export async function handleRequest(req, secrets, nowMs = Date.now()) {
  const f = { ...(req.query || {}), ...parseBody(req.body, req.contentType) };
  // FastSpring passes the username and password either as HTTP basic auth or as parameters.
  let user = f.username ?? f.user ?? "", pass = f.password ?? f.pass ?? "";
  if (req.authorization?.startsWith("Basic ")) {
    const decoded = Buffer.from(req.authorization.slice(6), "base64").toString("utf8");
    const i = decoded.indexOf(":");
    [user, pass] = i < 0 ? [decoded, ""] : [decoded.slice(0, i), decoded.slice(i + 1)];
  }
  if (!secrets.user || !secrets.password || !constantTimeEqual(String(user), secrets.user) || !constantTimeEqual(String(pass), secrets.password)) {
    return { status: 401, text: "unauthorized" };
  }
  const email = f.email;
  if (!email) return { status: 400, text: "email required" };
  const qty = Math.min(Math.max(parseInt(f.quantity ?? "1", 10) || 1, 1), 50);
  const day = Math.floor(nowMs / 86400000);
  const keys = [];
  try { for (let i = 0; i < qty; i++) keys.push(await issueKey(secrets.signingKeyHex, String(email), day)); }
  catch { return { status: 400, text: "could not issue a key" }; }
  return { status: 200, text: keys.join("\n") + "\n" };
}
