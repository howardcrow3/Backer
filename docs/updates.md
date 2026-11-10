# Shipping app updates

How it works: the app asks an update address (a small `latest.json` file) whether a newer version exists. If so, a
banner offers **Install and restart**. The download is checked against a signature made with your updater key before
anything is replaced, so only builds you signed can be installed. The background agent is stopped for the install
(Windows cannot replace a running file) and starts again after the restart. Nothing is installed without a click.

The files live on AWS: a private S3 bucket behind CloudFront. GitHub Actions publishes to it when you release.

Code: `app/src-tauri/src/main.rs` (`check_update`, `install_update`), `app/ui/app.js` (banner, Settings),
`.github/workflows/ci.yml` (release job), `tools/update-hosting/template.yaml` (AWS).

## One-time setup

1. **Make the updater key pair** on your own computer (this is separate from the Apple certificate and the license key):
   `cd Backer/app/src-tauri && cargo tauri signer generate -w ~/.tauri/backer-updater.key`
   It asks for a password and prints the public key. Keep the private key file and its password safe and backed up.
   If it is lost, installed copies can never be updated again; if it leaks, anyone can ship code to your users.
2. **Put the public key in the app**: replace `REPLACE_WITH_UPDATER_PUBLIC_KEY` in `app/src-tauri/tauri.conf.json`
   (`plugins.updater.pubkey`) with the printed public key (safe to commit). Until then the app never checks for updates.
3. **GitHub secrets** (repository Settings, Secrets and variables, Actions): `TAURI_SIGNING_PRIVATE_KEY` (the private key
   file's contents) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
4. **Create the AWS hosting**: deploy `tools/update-hosting/template.yaml` with CloudFormation
   (`aws cloudformation deploy --template-file tools/update-hosting/template.yaml --stack-name backer-updates --capabilities CAPABILITY_IAM`).
   If the account already has the GitHub OIDC provider, add
   `--parameter-overrides CreateGitHubOidcProvider=false ExistingOidcProviderArn=<arn>`.
   The stack outputs three values:
   - `BucketName` → GitHub repository **variable** `UPDATE_S3_BUCKET`
   - `BaseUrl` → GitHub repository **variable** `UPDATE_BASE_URL`
   - `RoleArn` → GitHub **secret** `AWS_UPDATE_ROLE_ARN`
   (Optional variable `UPDATE_AWS_REGION` if the stack is not in us-west-2.)
5. **Set the update address in the app**: in `tauri.conf.json` set `plugins.updater.endpoints` to
   `["<BaseUrl>/latest.json"]`. A custom domain such as `updates.yourdomain.com` in front of CloudFront also works and
   is better long term, because the address is built into every installed copy.

## Releasing an update

Push a tag, for example `git tag v0.5.0 && git push origin v0.5.0` (or run the workflow by hand with `release_tag`).
The build takes the version from the tag, builds and signs both installers and the update packages, creates the GitHub
release, uploads the packages to `backer/<version>/`, and only then replaces `backer/latest.json`. Installed copies
see it the next time they check (when the window is open, at most every 6 hours, or on **Settings, Check for updates**).

Withdraw a bad release by re-uploading the previous `latest.json` (S3 versioning is not on; keep a copy).

## Limits to know about

- The first install of each person still comes from the installer. Updates only reach copies that already contain the
  updater and the public key, so the first release carrying this feature has to be installed by hand.
- Versions must increase (0.5.0 after 0.4.0). The app never downgrades.
- The Windows installer is not code-signed yet, so Windows may show a SmartScreen warning for the installer and for updates.
- Download page links that never change: `<BaseUrl>/download/Backer-macOS-arm64.dmg` and `<BaseUrl>/download/Backer-Windows-x64-setup.exe` (replaced on every release).
- The macOS update package is the same signed and notarized app as the DMG.
