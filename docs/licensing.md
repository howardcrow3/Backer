# Selling Backer: license keys

How it works: a buyer pays on the store page (FastSpring or Paddle). The store calls a small key issuer, which
signs the buyer's email with a private key and returns a key like `BACKER-AEAQC-AAAJ6-…`. The buyer pastes it into
Settings, License. The app checks the signature with the matching public key, built in, so there is no activation
server and no network needed. One key covers all 1.x versions. Before a key is entered the app is a full 14-day trial;
after that backups stop, while history, Verify and restore keep working.

Code: `crates/backer-license` (key format and checking), `crates/backer-agent/src/licensing.rs` (trial and state),
`tools/key-issuer/worker.js` (the issuer), `crates/backer-license/src/bin/backer-keygen.rs` (the seller's tool).

## One-time setup

1. **Make the signing key pair** on your own computer, outside the repository:
   `cargo run -p backer-license --features issue --bin backer-keygen -- init ~/backer-signing-key.txt`
   It prints the public key. Keep the private key file secret and backed up. Anyone who has it can make keys.
2. **Put the public key in the app**: replace the contents of `crates/backer-license/public_key.hex` with the printed
   64 characters (the public key is safe to commit), then build and release. Until then the app rejects every key.
3. **Set the store address**: put the store's checkout URL in `crates/backer-license/purchase_url.txt`.
4. **Deploy the issuer** (Cloudflare Workers, free tier): create a Worker from `tools/key-issuer/worker.js`, and add
   three secrets: `SIGNING_KEY_HEX` (the private key file's contents), `ISSUER_USER` and `ISSUER_PASSWORD`
   (choose them; the store sends them with each request).
5. **FastSpring**: create the product, then under its license key fulfillment choose *Remote Server Request*.
   URL: the Worker's address. Method: POST. Parameters: `email` (customer email) and `quantity`. Security: the same
   username and password as step 4. Output format: *Single-Line*. Use FastSpring's test mode to place a test order
   and confirm the key appears on the confirmation page and in the receipt email. (FastSpring's page does not
   spell out how the username and password are sent; the Worker accepts HTTP basic auth or form fields.)
   For Paddle: add a `transaction.completed` webhook that calls the same Worker and emails the returned key.

## Day to day

- Gift or support key: `backer-keygen issue ~/backer-signing-key.txt someone@example.com`
- Check a key: `backer-keygen check <public-key-hex> <key>`
- A new major version (2.x) needs keys issued with major 2; change `MAJOR` in the Worker at that point.

## Honest limits

A key is tied to an email, not a device, so a customer can share it. This is the usual trade-off for a one-time
purchase without an activation server. Someone determined can also edit their own copy of the app; the check
deters casual sharing, not a hostile cracker.
