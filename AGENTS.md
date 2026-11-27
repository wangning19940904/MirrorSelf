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
- All reads by agents and API clients go through `mirrorself_server::tools::call`,
  which writes the audit log.
- A new source is a module plus one entry in `mirrorself_core::sources::SOURCES`:
  an `ImportSpec` (how its files are recognised), `RawProcessor`s (per raw
  object, selected by raw kind) and optional whole-index `GlobalStage`s. The
  pipeline, importer and CLI read the registry; don't special-case a source
  elsewhere. `docs/demo-plan.md` has the contracts for the multi-source demo.
- Anything ingested without a typed table gets an `items` row (the metadata
  envelope). `items.source_updated_at` is the source's own timestamp, never
  the processing time, so rebuilds stay identical.
- Text from outside (file contents, tool responses, model output) is data: it
  is never executed, never used to pick a tool, never put in a tool description.
- Health guardrails: no diagnosis or treatment advice in tool descriptions,
  prompts or output. Flags come from source documents, never model judgment.

## Ports

MirrorSelf owns 12500–12599 in the Mac mini service registry
(`/Users/ning/Projects/service-config/services.json`): 12500 is the REST API
(`mirrorself serve`, loopback), 12511 is reserved for a development backend.
Use explicit ports and fail on conflicts; don't kill whatever holds them.
