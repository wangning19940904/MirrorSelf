# MirrorSelf — notes for coding agents

## Build and test

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs the same clippy and test commands on
Ubuntu for every pull request and every push to `main`; both must pass.

The 5-year performance check from milestone M1 (target: under 5 minutes on an
M-series Mac):

```bash
cargo build --release && ./target/release/mirrorself dev synth-apple-health --years 5 --out /tmp/ms-5y.zip && MIRRORSELF_HOME=/tmp/ms-perf ./target/release/mirrorself init && time MIRRORSELF_HOME=/tmp/ms-perf ./target/release/mirrorself import apple-health /tmp/ms-5y.zip
```

Checkup-report tests replay recorded Apple Vision OCR, so they run on Linux
too. On macOS, run the real provider against the fixtures with:

```bash
MIRRORSELF_LIVE_OCR=1 cargo test -p mirrorself-core --test reports live_apple_vision
```

The extraction eval (M2 acceptance: ≥ 95% after extraction, 100% after
review, no unflagged errors) runs in `cargo test`; for a per-file report:

```bash
cargo run -q -p mirrorself-cli -- dev eval-reports fixtures/reports/eval
```

The index uses SQLCipher (`rusqlite` with `bundled-sqlcipher`). On macOS it
builds against CommonCrypto or Homebrew OpenSSL; on Linux install the OpenSSL
development package.

## Conventions

- Never commit real personal data. Fixtures are synthetic: `fixtures/` and
  `mirrorself_core::synth`.
- Raw objects are immutable. Nothing downstream modifies or deletes them
  except a user-initiated crypto-delete.
- Every processor has a semantic version (`pipeline::Processor`). Bump it when
  its output changes; the next `process` re-runs it from raw.
- Derived ids are deterministic (`ids.rs`) so a rebuild reproduces results.
  Keep `m0_deleting_the_index_and_rebuilding_gives_identical_results` green.
- Schema migrations are forward-only (`crates/core/src/schema/NNNN_*.sql`,
  tracked by `PRAGMA user_version`). The `v_*` views are the agent-facing
  contract: add columns, never remove or rename without a major version.
- Every importer ships with a fixture and tests asserting exact values. When
  you change `fixtures/apple-health/export.xml`, update
  `crates/core/tests/acceptance.rs` with it.
- Checkup-report fixtures are generated: a spec (`fixtures/reports/**/*.json`,
  renderer input and ground truth) is drawn as a PDF, a scan and photos by
  `tools/report-fixtures/render.swift`, and OCR is recorded next to each file
  (`*.ocr.json`). After changing a spec, the renderer or the Swift OCR helper,
  run `tools/report-fixtures/regenerate.sh` on a Mac and commit the results.
  The eval set keeps only specs and recordings; its renders are git-ignored.
- OCR and extraction sit behind `models::ModelProvider`. Whatever a provider
  returns goes through `models::validate`, which decides what waits for
  review. Extraction rules: `models/rules.rs`; analytes, LOINC codes and units:
  `labs.rs` (check digits are tested).
- User decisions are ops (`ops.rs`): stored in the vault and `ops_log`,
  replayed by the `ops.replay` stage. Never write review state anywhere else.
- All reads by agents and API clients go through `mirrorself_server::tools::call`,
  which writes the audit log.
- Health guardrails: no diagnosis or treatment advice in tool descriptions,
  prompts or output. Flags come from source documents, never model judgment.
- Text extracted from documents is untrusted: return it only through
  `query::untrusted` (delimited), never as instructions or tool input.

## Ports

MirrorSelf owns 12500–12599 in the Mac mini service registry
(`/Users/ning/Projects/service-config/services.json`): 12500 is the REST API
(`mirrorself serve`, loopback), 12511 is reserved for a development backend.
Use explicit ports and fail on conflicts; don't kill whatever holds them. The
OCR helper is a subprocess, not a service, so M2 added no port.
