# Security & build provenance

Shuffle is a file manager — it has broad filesystem access, an integrated
terminal, and an auto-updater. So beyond Apple's code signing and notarization
(which prove *who* signed a build), every release also ships with **GitHub build
provenance attestations** that prove *what source* it was built from.

## Verify a download

Every `Shuffle.dmg` published on the [Releases page][releases] is built in CI and
attested. Before installing, you can cryptographically confirm the DMG you
downloaded was built by this repo's release workflow from a specific commit:

```sh
gh attestation verify Shuffle.dmg -R WizenPainter/shuffle
```

(Requires the [GitHub CLI][gh] 2.49+. `gh auth login` once if you haven't.)

A successful result shows the commit SHA, the workflow that built it
(`.github/workflows/release.yml`), and confirms the DMG's SHA-256 digest matches
the signed attestation. If verification fails, **do not install** — the file
doesn't match anything this repository built.

You can also inspect the raw attestation:

```sh
gh attestation verify Shuffle.dmg -R WizenPainter/shuffle --format json
```

### What this proves (and what it doesn't)

- **Apple notarization** proves the binary was signed by the holder of the
  Developer ID certificate and scanned by Apple — i.e. *who* built it.
- **Build provenance** proves the DMG bytes came out of this repository's public
  CI workflow, tied to a specific commit — i.e. *what code* went into it.

Together they close the gap where a source audit couldn't confirm the shipped
binary matched the source. They do **not** replace reading the code — they let
you trust that the artifact corresponds to code you (or others) can review.

## How releases are built

Releases are produced by [`.github/workflows/release.yml`][workflow] on a tag
push (`v*`), not from a maintainer's laptop. The workflow:

1. builds the universal (arm64 + x86_64) binary from source,
2. signs it with the Developer ID certificate and notarizes it with Apple,
3. calls [`actions/attest-build-provenance`][attest] to record the attestation,
4. publishes the DMG as the "latest" release.

The local `release.sh` / `make_dmg.sh` scripts still work for emergency or
offline builds, but only the **CI** path produces attestations — prefer tagging.

## Reporting a vulnerability

Please report security issues privately via [GitHub Security Advisories][advisory]
(Security → Report a vulnerability) rather than a public issue.

---

## Maintainer: one-time CI setup

The release workflow needs these repository secrets
(**Settings → Secrets and variables → Actions**). None of them ever appear in
logs or in the repo.

**Signing**

| Secret | What it is |
| --- | --- |
| `MACOS_CERT_P12_BASE64` | Developer ID Application cert **+ private key**, exported from Keychain Access as a `.p12`, then `base64 -i DeveloperID.p12 \| pbcopy` |
| `MACOS_CERT_PASSWORD` | the password you set when exporting that `.p12` |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: Jaime Guzman (Z69U4AQSH3)` |

**Notarization — pick ONE set.**

App Store Connect API key (recommended for CI — scoped and revocable):

| Secret | What it is |
| --- | --- |
| `AC_API_KEY_ID` | the key's Key ID |
| `AC_API_ISSUER_ID` | the issuer UUID (App Store Connect → Users and Access → Integrations → App Store Connect API) |
| `AC_API_KEY_P8_BASE64` | the downloaded `AuthKey_XXXX.p8`, `base64`-encoded |

…or Apple ID + app-specific password:

| Secret | What it is |
| --- | --- |
| `APPLE_ID` | your Apple ID email |
| `APPLE_APP_PASSWORD` | an app-specific password from appleid.apple.com |
| `APPLE_TEAM_ID` | your Developer ID team, e.g. `Z69U4AQSH3` |

### Cutting a release

```sh
# 1. bump `version` in Cargo.toml, then:
git commit -am "vX.Y.Z: …"
git push
# 2. tag it — the workflow builds, notarizes, attests, and publishes:
git tag vX.Y.Z && git push origin vX.Y.Z
```

The tag must match `Cargo.toml`'s version or the build fails fast.

[releases]: https://github.com/WizenPainter/shuffle/releases/latest
[workflow]: .github/workflows/release.yml
[gh]: https://cli.github.com
[attest]: https://github.com/actions/attest-build-provenance
[advisory]: https://github.com/WizenPainter/shuffle/security/advisories/new
