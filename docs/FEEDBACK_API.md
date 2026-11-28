# Feedback API (what the app sends, what the service must do)

Backer no longer carries a shared secret for feedback. The old design signed requests with an
HMAC key compiled into every build, which anyone can extract from the app. Now each install makes
its own Ed25519 key the first time it sends feedback and keeps the private half in the OS keychain.

## Request

`POST https://feedback.turtlebox.studio/v1/feedback`, body as before (JSON: app, appVersion,
platform, rating, message, category, email, anonymous). Headers:

| Header | Value |
| --- | --- |
| `x-ht-app` | `backer` |
| `x-ht-install` | the install's public key, 64 hex characters (this is the install id) |
| `x-ht-timestamp` | unix seconds |
| `x-ht-signature` | 128 hex characters: Ed25519 signature over the text below |

Signed text (UTF-8, no trailing newline): `backer.<x-ht-install>.<x-ht-timestamp>.<raw request body>`

## What the service must do (howardcrow3/highturtle `feedback/`)

1. Reject the request unless the signature verifies with the public key in `x-ht-install`.
   This proves the sender holds that install key and the body was not altered. It does not prove
   the app is genuine, because the key is self-made; that is why the next two steps matter.
2. Reject timestamps more than 5 minutes from the server clock (replay).
3. Rate-limit **per install id** (suggested: 5 per hour, 20 per day) and **per source IP**
   (suggested: 20 per hour), and cap the body at 10 KB. Answer 429 when over. The app shows
   "You've sent a lot of feedback recently" for 429.
4. Store the install id with the feedback so abuse can be traced and one install blocked.
5. Keep accepting the old `x-ht-signature` HMAC requests (no `x-ht-install` header) until the
   last build that sends them is out of use, then remove the shared `backer` entry from
   `/highturtle/feedback/app-keys`, rotating it so the extracted key stops working.

## Test vector

Seed `07` repeated 32 times, timestamp `1795694400`, body `{"app":"backer","rating":5}`; see the
test `a_request_is_signed_by_the_install_key_and_verifies` in
`crates/backer-license/src/feedback.rs`.
