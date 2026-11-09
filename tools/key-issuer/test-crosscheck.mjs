// Cross-check: a key made by the issuer's code must verify in the Rust app. Prints "<pubkey-hex> <key>".
import { issueKey } from "./issuer.mjs";
import { generateKeyPairSync } from "node:crypto";
const { privateKey, publicKey } = generateKeyPairSync("ed25519");
const seed = privateKey.export({ format: "der", type: "pkcs8" }).subarray(-32);
const pub = publicKey.export({ format: "der", type: "spki" }).subarray(-32);
const key = await issueKey(seed.toString("hex"), "Buyer@Example.com", 20400);
console.log(pub.toString("hex"), key);
