# Windows code signing (Azure Trusted Signing)

The release workflow can sign the Windows MSI and the files inside it with Azure Trusted Signing. It is **off** until
you switch it on, so unsigned builds keep working while Azure identity validation is pending.

## Turn it on (after Azure finishes validating your identity)

1. In Azure, create a Trusted Signing account and a certificate profile (public trust). Note the account name,
   profile name and endpoint (for example `https://eus.codesigning.azure.net`).
2. Create an app registration (service principal) with the **Trusted Signing Certificate Profile Signer** role on the
   account. Make a client secret.
3. In the GitHub repository settings add:
   - Secrets: `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`
   - Variables: `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE`
   - Variable `WINDOWS_SIGNING` = `true` (this is the switch)
4. Cut a release. The build runs `trusted-signing-cli` through Tauri's `signCommand`, so the updater signature is made
   over the already signed MSI. Check the result: right-click the MSI, Properties, Digital Signatures.
5. Then update the Windows notes in the README and on turtlebox.studio (the SmartScreen warning text), and remove the
   "not code-signed" line from `docs/updates.md`.

SmartScreen reputation with Trusted Signing certificates builds up quickly, but the first downloads may still warn.
To switch it off again set `WINDOWS_SIGNING` to anything other than `true`.
