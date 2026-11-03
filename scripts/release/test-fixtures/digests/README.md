# srtool digest test fixtures

Real srtool digests (srtool v0.18.3, from the Polkadot-stable2509 release build),
committed so CI can exercise the full-release path of `build-changelogs.sh` — the
jq digest assembly and the `runtimes.md.tera`/`runtime.md.tera` templates — without
building any runtime.

Used by `.github/workflows/release-check-changelog-generation.yml`, which points
the `*_DIGEST` environment variables here. Real release runs are unaffected: the
release workflow downloads fresh digests and sets those variables itself, and
local runs default to the (gitignored) drop-zone `scripts/release/digests/`.

The digest content does not need to be current — only its shape matters. Refresh
the files from any release's `release-notes-context` artifact if srtool's output
format ever changes.
