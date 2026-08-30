# Security

## Reporting a vulnerability

Report privately through GitHub: **[Security → Report a vulnerability](https://github.com/koskeller/majik/security/advisories/new)**.
Please don't open a public issue for anything exploitable.

Expect an acknowledgement within a few days. Majik is a small project — there is no bounty, but you
will be credited in the advisory and the release notes unless you'd rather not be.

## What's in scope

Majik is a desktop application with no server of its own. Worth reporting:

- **API keys.** Keys are held in the platform credential store (macOS Keychain, Windows Credential
  Manager, Secret Service on Linux). Anything that leaks one — into the library folder, a config
  file, a log line, a crash report, an outbound request to somewhere other than the provider it
  belongs to — is a vulnerability.
- **Untrusted input.** Media arrives from provider APIs and from files you import. Memory-safety
  bugs or path traversal reachable from a downloaded or imported file are in scope, including in
  the H.264 decoder.
- **The library folder.** Anything that writes outside the library folder you chose, or deletes
  files it shouldn't — nothing in Majik is meant to hard-delete anything.
- **Update and packaging.** Anything that would let a third party substitute a release artifact.

Out of scope: reports that need an attacker who already runs code as your user, and vulnerabilities
in the model providers themselves — report those to fal, Replicate or OpenRouter.

## Supported versions

The latest release. Majik is pre-1.0 and fixes go into the next release rather than being backported.
