# Selling Backer: license keys

How it works: a buyer pays on the store page (FastSpring or Paddle). The store calls a small key issuer, which
signs the buyer's email with a private key and returns a key like `BACKER-AEAQC-AAAJ6-…`. The buyer pastes it into
Settings, License. The app checks the signature with the matching public key, built in, then **activates** the key
online (below). One key covers all 1.x versions and works on 2 computers. Before a key is entered the app is a full 14-day trial;
after that backups stop, while history, Verify and restore keep working.

Code: `crates/backer-license` (key format and checking, `activation.rs` for activation tokens), `crates/backer-agent/src/licensing.rs` (trial and state),
`tools/key-issuer/` (the issuer: `issuer.mjs` core, `lambda.mjs` AWS entry point, `template.yaml` SAM deployment), `crates/backer-license/src/bin/backer-keygen.rs` (the seller's tool).

## One-time setup

1. **Make the signing key pair** on your own computer, outside the repository:
   `cargo run -p backer-license --features issue --bin backer-keygen -- init ~/backer-signing-key.txt`
   It prints the public key. Keep the private key file secret and backed up. Anyone who has it can make keys.
2. **Put the public key in the app**: replace the contents of `crates/backer-license/public_key.hex` with the printed
   64 characters (the public key is safe to commit), then build and release. Until then the app rejects every key.
3. **Set the store address**: put the store's checkout URL in `crates/backer-license/purchase_url.txt`.
4. **Deploy the issuer** (AWS Lambda with a Function URL; cost is effectively zero at this volume):
   1. Store the three secrets as SecureString parameters in SSM Parameter Store, in the region you will deploy to:
      `/backer/license/signing-key` (the private key file's contents), `/backer/license/issuer-user` and
      `/backer/license/issuer-password` (choose them; the store sends them with each request).
      `aws ssm put-parameter --name /backer/license/signing-key --type SecureString --value "$(cat ~/backer-signing-key.txt)"`
   2. `cd tools/key-issuer && sam build && sam deploy --guided` (AWS SAM CLI). The Node 22 runtime includes the AWS SDK v3, so
      there is nothing to install. The function may read only those three parameters, and is limited to 5 concurrent runs.
   3. The deploy prints `IssuerUrl`. That address is what you give the store.
5. **FastSpring**: create the product, then under its license key fulfillment choose *Remote Server Request*.
   URL: the `IssuerUrl` from step 4. Method: POST. Parameters: `email` (customer email) and `quantity`. Security: the same
   username and password as in step 4. Output format: *Single-Line*. Use FastSpring's test mode to place a test order
   and confirm the key appears on the confirmation page and in the receipt email. (FastSpring's page does not
   spell out how the username and password are sent; the issuer accepts HTTP basic auth or form fields.)
   For Paddle: add a `transaction.completed` webhook that calls the same URL and emails the returned key.

## Day to day

- Gift or support key: `backer-keygen issue ~/backer-signing-key.txt someone@example.com`
- Check a key: `backer-keygen check <public-key-hex> <key>`
- A new major version (2.x) needs keys issued with major 2; change `MAJOR` in `tools/key-issuer/issuer.mjs` at that point.

## Activation: 2 computers per key

Keys are still signed and checkable offline, but a key only counts on 2 computers, so one posted on the internet stops
working for everyone after the first two. The license service is the `turtlebox-billing` Lambda in howardcrow3/highturtle
(`billing/src/backer-activation.mjs`, DynamoDB table `highturtle-backer-activations`), reached at
`https://turtlebox.studio/api/backer/v1/activate` and `/deactivate`. It signs with the same `BACKER_SIGNING_KEY` as the keys.

- **Activate**: entering a key sends it with a fingerprint of the computer (SHA-256 of the OS machine id, see
  `crates/backer-agent/src/machine.rs`) to the service. The service checks the key's signature, takes a place for that
  computer (a third computer gets `seats_full`), and returns a signed token bound to that key and computer. Existing
  0.7.0 keys need no migration: the key's row is made on first activation.
- **Offline use**: the app checks the token with the built-in public key. A token is good for 90 days and is renewed
  in the background after 7. A key that could not be confirmed (entered offline, or saved by a version before
  activation) keeps working for a 14-day grace period while the app keeps trying. Only a definite answer from the service
  (on 2 other computers, or deactivated) stops backups early, so a service outage never stops a backup.
- **Free a place**: Settings, License, *Deactivate this computer* (needs the internet), or the buyer signs in at
  turtlebox.studio/account and chooses *Free this seat* on a computer, or support runs the **Backer seat reset**
  workflow in howardcrow3/highturtle with the buyer's email.
- **Revoke** a key (refund, abuse): set `status` to `revoked` on its row in the activation table. The app stops at its
  next renewal, within 7 days.

## Honest limits

A key is tied to an email and counted per computer, so posting it online stops working after two computers, but a customer can
still lend it to one friend. Someone determined can also edit their own copy of the app, or copy a key's two
places to virtual machines with made-up machine ids; the check stops casual sharing, not a hostile cracker. Two units bought in one
order get identical keys (same email, same day), so they share the 2 computers.
