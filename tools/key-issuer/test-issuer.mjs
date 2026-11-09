// Offline tests for the issuer core and the Lambda adapter's request handling. Run: node test-issuer.mjs
import assert from "node:assert/strict";
import { generateKeyPairSync } from "node:crypto";
import { handleRequest } from "./issuer.mjs";

const { privateKey } = generateKeyPairSync("ed25519");
const seed = privateKey.export({ format: "der", type: "pkcs8" }).subarray(-32).toString("hex");
const secrets = { signingKeyHex: seed, user: "shop", password: "s3cret:with:colons" };
const form = (o) => new URLSearchParams(o).toString();
const basic = (u, p) => "Basic " + Buffer.from(`${u}:${p}`).toString("base64");
const call = (req) => handleRequest({ contentType: "application/x-www-form-urlencoded", ...req }, secrets, 1_800_000_000_000);

// Authorised with basic auth (the password may contain colons).
let r = await call({ body: form({ email: "a@b.co", quantity: "2" }), authorization: basic("shop", "s3cret:with:colons") });
assert.equal(r.status, 200);
const lines = r.text.trim().split("\n");
assert.equal(lines.length, 2);
assert.ok(lines.every((k) => k.startsWith("BACKER-")));
// Authorised with form fields.
r = await call({ body: form({ email: "a@b.co", username: "shop", password: "s3cret:with:colons" }) });
assert.equal(r.status, 200);
// Wrong or missing credentials are refused and no key is returned.
for (const req of [{ body: form({ email: "a@b.co" }) }, { body: form({ email: "a@b.co" }), authorization: basic("shop", "nope") }, { body: form({ email: "a@b.co", username: "x", password: "y" }) }]) {
  r = await call(req); assert.equal(r.status, 401); assert.ok(!r.text.includes("BACKER"));
}
// Not configured means nothing is ever issued.
r = await handleRequest({ body: form({ email: "a@b.co" }) }, { signingKeyHex: seed, user: "", password: "" }); assert.equal(r.status, 401);
// Bad input.
r = await call({ body: form({ username: "shop", password: "s3cret:with:colons" }) }); assert.equal(r.status, 400);
r = await call({ body: form({ email: "x".repeat(90) + "@y.z", username: "shop", password: "s3cret:with:colons" }) }); assert.equal(r.status, 400);
// Quantity is clamped.
r = await call({ body: form({ email: "a@b.co", quantity: "9999", username: "shop", password: "s3cret:with:colons" }) });
assert.equal(r.text.trim().split("\n").length, 50);
console.log("issuer tests passed");
