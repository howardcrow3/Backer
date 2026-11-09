// AWS Lambda entry point (Function URL). Secrets come from SSM Parameter Store (SecureString), read once per
// warm container:  <SECRET_PREFIX>/signing-key   <SECRET_PREFIX>/issuer-user   <SECRET_PREFIX>/issuer-password
import { SSMClient, GetParametersCommand } from "@aws-sdk/client-ssm";
import { handleRequest } from "./issuer.mjs";

const ssm = new SSMClient({});
let cached;

async function secrets() {
  if (cached) return cached;
  const prefix = process.env.SECRET_PREFIX || "/backer/license";
  const names = ["signing-key", "issuer-user", "issuer-password"].map((n) => `${prefix}/${n}`);
  const r = await ssm.send(new GetParametersCommand({ Names: names, WithDecryption: true }));
  const v = Object.fromEntries((r.Parameters || []).map((p) => [p.Name.slice(prefix.length + 1), p.Value]));
  if (!v["signing-key"] || !v["issuer-user"] || !v["issuer-password"]) throw new Error("missing parameters under " + prefix);
  cached = { signingKeyHex: v["signing-key"].trim(), user: v["issuer-user"], password: v["issuer-password"] };
  return cached;
}

export const handler = async (event) => {
  const headers = Object.fromEntries(Object.entries(event.headers || {}).map(([k, v]) => [k.toLowerCase(), v]));
  const body = event.isBase64Encoded && event.body ? Buffer.from(event.body, "base64").toString("utf8") : event.body;
  let s;
  try { s = await secrets(); } catch (e) { console.error("config error", e.message); return { statusCode: 500, body: "not configured" }; }
  const r = await handleRequest({ query: event.queryStringParameters, body, contentType: headers["content-type"], authorization: headers["authorization"] }, s);
  // Never log the email or the key; only the outcome.
  console.log("issuer", r.status);
  return { statusCode: r.status, headers: { "content-type": "text/plain; charset=utf-8" }, body: r.text };
};
