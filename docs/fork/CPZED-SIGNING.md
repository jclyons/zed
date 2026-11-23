# CPZed local signing

`script/cpzed/bundle` signs with the self-signed identity **CPZed Local Signing**.

1. **Create the identity (user, once per machine):** Keychain Access > Certificate Assistant > Create a Certificate. Name `CPZed Local Signing`, type Self Signed Root, certificate type Code Signing. Confirm with `security find-identity -p codesigning`.
2. **Allow unattended signing (user, once per machine).** Otherwise every `codesign` shows a keychain "allow" dialog and an unattended build hangs. Run this in a plain Terminal (not a Zed thread, not a Claude session). `security` prompts for the login-keychain password, which only the user types; there is no `-k`, so it is never on a command line or in shell history. `-l` limits the change to keys labelled "CPZed Local Signing" (if Keychain Access labelled the private key differently, use the label shown under Keys):

   ```sh
   security set-key-partition-list -S apple-tool:,apple:,codesign: -s -l "CPZed Local Signing" ~/Library/Keychains/login.keychain-db
   ```

3. `bundle` probes with a 20 s test `codesign` of a tiny file and exits 10 with this pointer if it fails or hangs.

## Notes for `deploy`

- `bundle` exits 7 when another `cargo`/`rustc` is running (the check is global on the shared Studio). `deploy` must retry, not fail.
- `--skip-build` is for tests only (`CPZED_TEST_SKIP_BUILD=1`). The release `zed` binary must embed the full commit SHA being stamped, or `bundle` exits 11.

## Versions

`CFBundleShortVersionString` is `<cargo version>-cpzed.<n>`. `CFBundleVersion` is the 7-character commit SHA; the full SHA is in `Contents/Resources/cpzed-build.json`.
