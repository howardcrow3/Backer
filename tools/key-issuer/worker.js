// Backer key issuer: a Cloudflare Worker that FastSpring calls when someone buys, and that answers with
// the customer's license key. It signs with the private key held as a Worker secret, so the key never
// lives in the repo or the app. Format must stay identical to crates/backer-license/src/lib.rs.
//
// Settings (Cloudflare dashboard, Worker, Settings, Variables and secrets):
//   SIGNING_KEY_HEX   secret  the 64-hex-character private key from `backer-keygen init`
//   ISSUER_USER       secret  username FastSpring sends (FastSpring, key generator, Security)
//   ISSUER_PASSWORD   secret  password FastSpring sends
//
// FastSpring sends: email, quantity (and any parameters you add). The response is one key per line.

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

function constantTimeEqual(a, b) {
  const x = new TextEncoder().encode(a), y = new TextEncoder().encode(b);
  let d = x.length ^ y.length;
  for (let i = 0; i < Math.max(x.length, y.length); i++) d |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return d === 0;
}

async function fields(request) {
  const url = new URL(request.url);
  const out = Object.fromEntries(url.searchParams);
  if (request.method === "POST") {
    const ct = request.headers.get("content-type") || "";
    if (ct.includes("json")) Object.assign(out, await request.json());
    else for (const [k, v] of await request.formData()) out[k] = String(v);
  }
  return out;
}

export default {
  async fetch(request, env) {
    const f = await fields(request).catch(() => ({}));
    // FastSpring passes the username and password either as HTTP basic auth or as parameters.
    let user = f.username ?? f.user ?? "", pass = f.password ?? f.pass ?? "";
    const basic = request.headers.get("authorization");
    if (basic?.startsWith("Basic ")) [user, pass] = atob(basic.slice(6)).split(/:(.*)/s);
    if (!env.ISSUER_USER || !constantTimeEqual(user ?? "", env.ISSUER_USER) || !constantTimeEqual(pass ?? "", env.ISSUER_PASSWORD ?? "")) {
      return new Response("unauthorized", { status: 401 });
    }
    const email = f.email;
    if (!email) return new Response("email required", { status: 400 });
    const qty = Math.min(Math.max(parseInt(f.quantity ?? "1", 10) || 1, 1), 50);
    const day = Math.floor(Date.now() / 86400000);
    const keys = [];
    try { for (let i = 0; i < qty; i++) keys.push(await issueKey(env.SIGNING_KEY_HEX, email, day)); }
    catch { return new Response("could not issue a key", { status: 400 }); }
    return new Response(keys.join("\n") + "\n", { headers: { "content-type": "text/plain; charset=utf-8" } });
  },
};
