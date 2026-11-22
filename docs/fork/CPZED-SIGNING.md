# CPZed local signing

`script/cpzed/bundle` signs with the self-signed identity **CPZed Local Signing**.

1. **Create the identity (user, once per machine):** Keychain Access > Certificate Assistant > Create a Certificate. Name `CPZed Local Signing`, type Self Signed Root, certificate type Code Signing. Confirm with `security find-identity -p codesigning`.
2. **Allow unattended signing (user, once per machine).** Otherwise every `codesign` shows a keychain "allow" dialog and an unattended build hangs. The USER types their login-keychain password; no session ever does:

   ```sh
   read -rs PW; security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
     -k "$PW" ~/Library/Keychains/login.keychain-db; unset PW
   ```

3. `bundle` probes with a 25 s test `codesign` of a tiny file and exits 10 with this pointer if it fails or hangs.

## Notes for `deploy`

- `bundle` exits 7 when another `cargo`/`rustc` is running (the check is global on the shared Studio). `deploy` must retry, not fail.
- `--skip-build` is for tests only (`CPZED_TEST_SKIP_BUILD=1`). The release `zed` binary must embed the full commit SHA being stamped, or `bundle` exits 11.
