# Contributing to Majik

Majik is [GPL-3.0-or-later](LICENSE). Contributions are welcome.

## Before a pull request

- `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
- Behaviour changes come with their tests in the same commit. [CLAUDE.md](CLAUDE.md) describes the
  architecture, the vocabulary and how the suites are written.
- Commit titles are imperative and capitalized, optionally `crate: Summary`.

## Copyright and the contributor agreement

Non-trivial pull requests need a signed contributor licence agreement before they can be merged.
There is nothing to sign up front — open the pull request and the maintainer will send you the
agreement. It grants Majik's maintainer a licence to your contribution broad enough to relicense it.
That allows two things: distributing Majik through channels whose terms conflict with the GPL (the
Mac App Store, for example), and offering a commercial licence alongside the GPL one.

CLAs have also been used to move projects off open source, so this promise comes with it:

**Majik will not be relicensed under a non-OSI-approved licence.** If the licence ever changes, it
changes to another licence approved by the Open Source Initiative, and every version released before
that change stays available under GPL-3.0-or-later. The client will never use a source-available
licence that reserves commercial use — BUSL, SSPL, the Elastic Licence or similar. Server-side
components that are not part of this repository are not covered by this promise.

If you would rather not sign, say so on the pull request. Small fixes — typos, a one-line
correction — are taken without one.

## What is and is not in this repository

This repository is the Majik desktop application and everything it needs to build. Any hosted
service Majik may later talk to is separate, is not covered by the GPL, and is not promised here.
