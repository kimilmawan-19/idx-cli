# CLAUDE.md

@AGENTS.md

`AGENTS.md` (imported above) is the shared guide for every agent. This file
adds only what is specific to working here with Claude Code.

## Verify

- `nix flake check -L` is the only gate CI runs; after it passes, don't repeat
  full cargo runs (`docs/CONVENTIONS.md`). While iterating, use
  `nix develop --command cargo test <filter>`.
- `UPDATE_GOLDEN=1 cargo test --test golden` rewrites every golden file. Right
  after it, `git status --porcelain crates/idx-cli/tests/golden` must list
  only `??` lines (new cases). A ` M` line means an existing contract changed:
  revert it, or keep it only if the change is intended and explained in the
  commit (AGENTS.md rule 7).
- To show that a new test guards a fix, revert the fix and watch it fail.

## Ownership data model

- A KSEI release is a **register** (named holders: XLSX/PDF) or an
  **archive** (ZIP/TXT: totals per investor type, stored as `KSEI AGGREGATE ...`
  rows). The kind is derived from the rows and never stored; see "Release
  kinds" in `docs/ARCHITECTURE.md`.
- A new query must not pick "the newest release" by date. Use
  `db::latest_holder_release` for holder-level reads and `db::ticker_release`
  for `ticker`. Test for aggregate rows with `is_aggregate_holder` in Rust and
  `db::sql_not_aggregate` in SQL (`GLOB`, because `LIKE` ignores ASCII case).
- Reads never write to the database file: `ownership sync` compares the
  installed file's hash with the manifest. So no schema migration on open and
  no index tables in `main` (the search index lives in the `temp` schema), and
  remember that `ensure_schema` also runs on read-only opens.

## Code

- Cut text by chars, never by byte offset (`truncate_chars` in
  `crates/idx-cli/src/output/table.rs`); a byte slice panicked inside a
  multibyte character.
- Every network call needs a timeout: curl invocations pass `--connect-timeout`
  and `--max-time` (see `curl_impersonate.rs`) and ureq agents set
  `timeout_global`.

## Cloud sessions (claude.ai/code)

- The proxy blocks GitHub tarball downloads, so `nix develop` and
  `nix flake check` need each flake input fetched as git:
  `--override-input <input> "git+https://github.com/<owner>/<repo>?rev=<rev>&shallow=1"`
  for `nixpkgs`, `flake-utils`, `flake-utils/systems` (`nix-systems/default`),
  `rust-overlay` and `crane`. Take owner, repo and rev from `flake.lock`.
  The check is slow: run it in the background, redirect output to a log,
  append `echo exit=$?`, and read the log. The job notification's exit code is
  the wrapper's, not the check's.
- IDX answers HTTP 403 to the container's IP, so the `ownership` cases of
  `scripts/live-smoke.sh` fail on old and new builds alike. Report them as
  403s, never as passes (AGENTS.md rule 7).
- `idx ownership sync` fails with `invalid peer certificate: UnknownIssuer`
  because its HTTP client does not trust the proxy's CA. Download the snapshot
  with curl, check its sha256 against the manifest, copy the manifest with
  `snapshot.download_url` set to the local file, and run
  `idx ownership sync --manifest <copy>`.

## Commits and PRs

- Conventional commits with an optional scope (`fix(ownership): ...`,
  `test(golden): ...`), one logical change each, each leaving fmt, clippy and
  tests green.
- A PR description follows the merged #1: changes grouped by theme, intentional
  contract or golden changes with their script impact, verification (flake
  check, then the live-smoke summary with IDX 403s called out), and what is
  deliberately left out.
