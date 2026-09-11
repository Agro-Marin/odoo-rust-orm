# Rust ORM rebase validation — 2026-09-11

The 70 local commits ending at `0c078f6` were replayed onto remote main
`77dbd8f7dab2ab3e4db83d05a6cc831b21635d25`. No feature branch or merge commit
was created. The replay initially favored incoming local conflict hunks;
that was a mechanical starting point, followed by the semantic reconciliation
and testing recorded here. The other workspace repositories were not rebased
or pushed.

## Reconciled contracts

- Preserve the remote Rust 2024 toolchain/dependency updates, declared field
  falsy values, unknown-vs-false access-bypass metadata, and translated trigram
  accelerator. Restore the remote regression cases alongside local tests.
- Keep public-domain rejection of `any!`, while allowing trusted ORM-composed
  traversal nodes to retain their bypass semantics. Keep the local registry
  generation and checked-security-snapshot guards.
- Preserve remote pooled-connection integration without losing the local
  connection identity guard or reset behavior. Compare the full libpq identity,
  excluding Python driver arguments; avoid silently delegating every request
  because connection-policy options disappeared during normalization.
- Carry autocommit through the native connection for pool maintenance. Metadata
  lookups release transactions they opened, and native ORM dispatch refuses
  autocommit because it requires one repeatable-read snapshot.
- Stamp borrowed connections with their pool generation. Draining after DDL
  closes those connections on return instead of reusing their stale plans.
  The regression fails against the preceding release and passes after rebuilding.
- Preserve remote SQL diagnostics, COPY TEXT, pgvector/geometry handling and
  expanded array encoders. Keep local typed errors, connection runtime lifecycle,
  UTF-8-safe SQL scanners and generation-scoped prepared caches.
- Clear prepared statements at rollback/drop boundaries. Preserve TypeError for
  a Python value unsuitable for binary COPY. Let PostgreSQL infer string-array
  column types; untyped arrays use PostgreSQL's literal syntax, matching psycopg.
- Retain both cursor parity and HTTP byte parity. Apply the existing stage
  deadline, report missing verdicts as failures, avoid running replay twice,
  and require native calls in the browser leg to prevent Python/Python passes.
- Refresh the soak's Python baseline and registry after the mutating lanes.
  An initial complete run exposed `res.users.log.search_count` changing from
  three to six after browser logins; the old expected count caused 248 soak
  mismatches. The refreshed baseline measures the state the soak actually reads.

Some older refusal tests are superseded by supported features: display-name
comparisons and relational name operands now have positive parity coverage.
The caller boundary still rejects internal operators; the compiler is also used
for trusted internally composed nodes and is deliberately a different boundary.

## Verification

Runs use two owned disposable
Odoo databases (`codex_rustorm_rebase_20260911` and
`codex_rustorm_rebase_other_20260911`), initialized through Odoo. The database
and cursor parity controls are not comparisons against an empty fixture.

Static and unit checks passed: 195 Rust tests (six opt-in database/benchmark
tests ignored), 30 shim tests with no skips, 15 harness tests, Cargo formatting,
Clippy across all targets with warnings denied, and Ruff lint/format checks.
The full live run also checks the transaction, registry-generation, security,
redacted-cache, typed-error and quarantine contracts against real PostgreSQL.

The final full run passed every stage. Its summary and soak output are preserved
in [the validation transcript](harness/evidence/2026-09-11-rebase.txt).

| Check | Result |
| --- | --- |
| Kernel corpus | 3,366 values compared; 3,322 matching access denials; 1,180 refusals; 18 rejected inputs |
| Shadow corpus | 328 values compared; 5 matching denials; 28 refusals; 59 unavailable-model/field skips; 5 rejected inputs |
| Fuzz | Three seeds passed |
| Registry sweep | 13,627 query shapes compared, 12,059 routed |
| Cursor types | 96 checks, zero mismatches |
| Concurrency | Zero wrong answers |
| Browser | 12 tests, 188 native calls, zero divergences |
| Replay | 51 captured, 29 replayed; 22 missing-user skips; zero divergences |
| HTTP parity | 784 cases: 774 stable, 10 nondeterministic; zero divergences |
| Cursor parity | 379 cases per backend; 19 fail on both, 3 only under Rust |
| Soak | 44,811 requests after warm-up; zero errors or mismatches |

Fallbacks, rejected inputs and skips are not evidence of native value parity.
These checks establish the exercised coexistence contracts, not readiness for
full Python ORM replacement.

Reproduction after initializing owned databases with `base,web,mail` and `base`
respectively, using an Odoo config scoped to those databases:

```sh
cargo build --release --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTORM_ODOO_CONF=/path/to/test.conf RUSTORM_OTHER_DB=your_other_rustorm_db \
  bash harness/verify.sh --db your_rustorm_db --out /tmp/rustorm-validation
```

The cursor-parity baseline retains its documented three differences: two
pipeline-mode cases and one permissive COPY type-resolution case. This rebase
does not implement pipeline mode or claim complete psycopg equivalence.
