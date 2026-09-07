# M3 Plan — Odoo on a Rust Engine

Goal: invert the current architecture. Today Python Odoo is the server and the
Rust kernel is a side-car. After M3, **the Rust binary is the server** —
HTTP, sessions, transactions, ORM data engine, cache, scheduling — and it
**embeds CPython** (PyO3) to execute the one thing it cannot replicate:
Odoo's Python business logic (computes, overrides, onchange, controllers),
running unmodified against our 19.0-marin fork.

The strategic bet, validated by M0–M1: the kernel (SQL, rules, cache,
serialization) dominates cost and is where Rust wins 4–56×; the business
logic is huge but thin per-call, and can stay Python indefinitely.

---

## Architecture target

```
┌─ rust binary (per worker process) ─────────────────────────────┐
│  axum HTTP        sessions (JSON store)     cron scheduler     │
│  ── RPC dispatch ──────────────────────────────────────────    │
│  ORM engine: registry · domain→SQL · rules · field cache ·     │
│              prefetch · write/flush · dependency graph         │
│  tokio-postgres pool  (single source of connections/txns)      │
│  ── PyO3 boundary ─────────────────────────────────────────    │
│  embedded CPython: odoo addons (methods, computes, overrides,  │
│  controllers, QWeb, safe_eval) on a `odoo.*`-compatible shim   │
│  whose data primitives call back into the Rust engine          │
└────────────────────────────────────────────────────────────────┘
   × N worker processes (prefork; GIL is per-process)
   + 1 supervisor process (socket, worker lifecycle, signaling)
```

Key inversion: Python overrides still run — `sale.order.write()` override
chains down its MRO until `BaseModel.write()`, which is now a Rust
primitive. Business code doesn't know the engine changed.

## Non-negotiable design decisions

1. **One connection owner.** All DB access flows through the Rust pool.
   Python's `cr.execute()` becomes a PyO3 call into the same transaction
   the Rust engine uses. This kills the entire class of snapshot-mismatch
   bugs that makes naive embedding unsafe.
2. **Registry fidelity by construction.** Don't parse addon Python from
   Rust. Boot Odoo's own registry build inside the embedded interpreter
   once per worker start, then export the complete metamodel (models,
   fields with compute/depends/related/store attrs, `_table` overrides,
   `_inherits`, access/rules) across the boundary into the Rust registry.
   The M0 trick (bootstrap from ir_model) remains as an integrity check.
3. **Prefork, not free-threading.** One GIL per worker process, N workers —
   same operating model as today's Odoo, zero exotic CPython dependencies.
   Rust async I/O inside each worker already beats a Python worker's
   throughput; per-interpreter-GIL / free-threaded CPython are upgrades to
   evaluate later, not foundations to depend on.
4. **Fallback is in-process, verified, and shrinking.** Every ORM entry
   point has a Rust implementation OR delegates to the Python original.
   The shadow harness decides which — per model × method — and the
   allowlist grows monotonically. No silent divergence: unsupported
   constructs fail into delegation, never guess.
5. **Odoo's own test suite is the acceptance oracle.** RPC-replay diffing
   validated reads; writes and computes are validated by making upstream
   `test_orm` / module test tags pass against the hybrid, plus dual-DB
   state diffing for business flows.

## Phases

### Phase 1 — Process & boundary foundation (~2–3 weeks)
Rust binary embeds CPython, boots the Odoo registry in it, and owns the DB.

- `rust-main`: initialize interpreter (pyo3 `Python::initialize`), import
  odoo from the 19.0-marin fork with config, run `Registry.new(db)`.
- Registry export: walk `registry.models` in Python, emit a typed metamodel
  (msgpack/json) → Rust `Registry` v2. Diff against the ir_model bootstrap
  from M0 to catch export gaps.
- Unified cursor: a Python `cr` facade whose `execute/fetch*` call Rust
  (sync bridge: runtime handle + block-in-place; one dedicated connection
  per open transaction). Odoo's `sql_db` monkey-patched to hand these out.
- Exit criteria: `odoo shell`-equivalent REPL inside the hybrid runs ORM
  code end-to-end on Rust-owned connections; registry export == M0
  registry (modulo known deltas, documented).

### Phase 2 — Read engine under real Odoo (~3–4 weeks)
The M1 kernel becomes the `_search`/`_read_group`/fetch implementation
inside the hybrid process.

- `BaseModel._search`, `_read_group`, `search_fetch`, `read` fetch path
  routed to the Rust engine (same transaction, registry v2), delegating to
  the Python originals off-allowlist (`_search` overrides, unsupported
  fields).
- Port the M1 shadow harness in-process: for every (model, method) run
  both engines, diff, emit the allowlist.
- Exit criteria: upstream `test_domain`, `test_search`, `test_read_group`
  tags green on the hybrid; RPC replay of captured web-client traffic
  byte-identical; measured speedup inside real Odoo requests.

### Phase 3 — Cache, write engine, recompute (~6–10 weeks; the beast)
Rust owns the field cache and the write path; Python computes are invoked
as callbacks.

- Typed columnar field cache in Rust (the design the ORM audit argues for:
  cache coherence and flush ordering enforced by construction — the
  rollback-poisoning and NewId-ordering bug classes become unrepresentable).
  Python recordset attribute access = PyO3 cache get + prefetch trigger.
- `create/write/unlink` primitives: constraint checks, `ir.model.data`
  touch, log-access columns, flush ordering, `modified()` dependency
  propagation from the exported `depends` graph.
- Compute orchestration: Rust marks-to-recompute and calls the Python
  compute methods batch-wise; related/company-dependent writes native.
- NewId/virtual records in the cache (prerequisite for onchange in P5).
- Exit criteria: upstream ORM write/compute/constraint test tags green;
  dual-DB flow diff green (confirm a sale order, post an invoice, run
  payroll-like batch on agromarin modules: identical row states modulo
  timestamps); no Python fallback on the write primitive itself.

### Phase 4 — HTTP takeover (~4–6 weeks)
The Rust binary becomes the addressable server.

- axum front: session store (existing JSON format), `/web/login`,
  `/web/dataset/call_kw` dispatch (Rust-native for engine methods, PyO3
  into model methods for the rest), static/assets serving, longpolling/bus.
- Python controllers (`@http.route`) behind a request-object shim; routes
  the shim can't honor yet are reverse-proxied to a legacy Python Odoo
  during transition (strangler pattern — same DB, same sessions).
- Registry/cache invalidation across workers via the orm_signaling tables.
- Exit criteria: stock web client fully usable against the hybrid alone
  for backoffice flows; browser tour tests green; proxy fallback list
  measurably shrinking.

### Phase 5 — Long tail & hardening (~6–8 weeks)
- onchange protocol (NewId cache + diff payloads), name_search,
  `_compute_display_name` dispatch into Python where overridden.
- Crons on the Rust scheduler invoking Python jobs; queue/bus.
- QWeb reports & website: keep Python (they run fine on the shim); later
  candidates for native QWeb.
- Ops: metrics, SQL logging parity (`--log-sql`), worker recycling memory
  limits, upgrade path (`-u` still runs Python's loading — fine).
- Continuous shadow CI: replay corpus + test tags on every fork sync.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| GIL caps per-worker Python throughput | Prefork workers (as today); hot paths keep migrating to Rust, shrinking GIL time per request |
| Boundary conversion overhead eats wins | Batch-oriented API (recordset-level, never per-field-per-record calls); measured at every phase |
| Registry export misses semantics (context-dependent fields, dynamic models) | Export from the *live* Python registry + ir_model cross-check + test suite as oracle |
| psycopg vs tokio-postgres type edge cases (numeric, tz, bytea) | Phase 1 gate: run Odoo's low-level SQL tests on the unified cursor first |
| Odoo test suite pokes engine internals (`cr` attributes, `flush()` timing) | Budget shim-compat work per phase; some tests get documented waivers |
| Fork churn (19.0-marin syncs upstream) | Engine targets the ORM contract, not internals where avoidable; shadow CI on every sync |
| Segfault in native code kills a worker | Same blast radius as a Python worker OOM today: supervisor restarts; keep unsafe surface ~zero (PyO3 safe API) |

## Effort and staging honesty

Single senior engineer, focused: **~6–9 months** to end of Phase 4; Phase 3
carries most of the uncertainty. Two engineers compress mostly Phase 3–4.
Every phase exits with a standalone-valuable artifact:

- P1: Odoo running on Rust-owned connections (ops win: pooling, observability)
- P2: in-process read accelerator (the M1 4–56× inside real Odoo requests)
- P3: native write path (the ORM-audit bug classes retired by construction)
- P4: Rust server in front (throughput + the side-car sunset)

Stop-loss points are the phase gates: if P3's dual-DB diff can't get green
on the agromarin flows within budget, P2's accelerator is still a shipped,
maintained asset and the fallback architecture means nothing regresses.

## Phase 1 kickoff — spike results (2026-07-05)

Both gate spikes are DONE and PASS (they were `src/bin/spike_embed.rs` and
`src/bin/spike_cursor.rs`, deleted once `export_registry` and `phase1_shell`
superseded them; `git log` has them, pyo3 0.29 against the venv's Python 3.14.4).

**Spike A — embedding + registry export: PASS.**
- interpreter init 6 ms · `import odoo` 190 ms · full 87-module registry
  boot inside the embedded interpreter 1.26 s · metadata export ~1 ms/model.
- Metamodel diff (live Python registry vs M0 ir_model bootstrap) over
  res.partner / res.users / account.account / uom.uom: **5 diffs, all one
  class** — `image_*` binaries with `store=True` but no column
  (attachment-stored). Finding: the Phase 1 export schema must carry the
  binary `attachment` flag; everything else matched, including
  `_parent_name` (validated the uom `relative_uom_id` case) and `_order`.

**Spike B — unified cursor: PASS.**
- One Rust connection, one transaction, three participants: Python
  `cr.execute()` INSERT via the PyO3 cursor → the Rust ORM engine
  (search_count with rules, same connection) sees the uncommitted row →
  an observer connection does not → ROLLBACK restores all views. All
  asserted.
- Boundary + SQL round-trips: ~1 ms. The sync bridge is a plain
  `Handle::block_on` from the Python thread — no thread-per-transaction
  machinery needed at this scale. Revisit only if Phase 2 profiling says so.

**Decision:** Phase 1 design confirmed as written.

## Phase 1 — DONE (2026-07-05)

1. **Workspace restructure** ✅ — `kernel/` (registry, domain, sqlgen,
   security, orm), `server/` (odoo-poc CLI + axum), `engine-py/` (PyO3
   boundary: export, cursor, bins). Harness still 78/78.
2. **Shared caches** ✅ — `orm::Caches` (rule + env) shared via Arc across
   Orm instances; statement cache stays per-connection.
3. **Registry export v2** ✅ — `export_registry` bin dumps the full live
   Python registry (320 models — 15 more than the ir_model bootstrap could
   see, e.g. custom `_table` models) via an env-bound model walk;
   `Registry::from_export` merges it with information_schema.
   `odoo-poc --export FILE run-corpus`: **78/78** — the live registry is
   the verified primary metamodel source now. (Gotcha: export must walk
   `env[name]`, not registry classes — `_order`/`_rec_name` are properties
   on the class.)
4. **Unified cr facade** ✅ — `engine-py/src/cursor.rs` + `python/rust_db_shim.py`.
   Discovery: Odoo 19 runs psycopg **3** with server-side binding, so the
   perfect seam is faking the `psycopg.Connection`/`Cursor` pair under
   Odoo's real `sql_db.Cursor` (patched `ConnectionPool.borrow/give_back`);
   savepoints, hooks, logging, metrics all run unmodified. The Rust side
   translates `%s`/`%(name)s` placeholders, prepares (cached) to learn
   *inferred* param types, converts Python values per type (incl. jsonb via
   the psycopg `Json` wrapper, numeric via rust_decimal, generic array
   kinds — `name[]`, `"char"[]` as i8), and decodes rows to Python objects
   by column OID.

**Phase 1 exit test (`phase1_shell`): PASS.**
- Full 87-module registry boot with psycopg bypassed entirely: **1.28s**
  through Rust connections (native-parity).
- Real Odoo ORM `search_read` (admin, non-su, rules active) byte-matches
  the shadow harness expectation (case c21).
- Write path: `create()` with translated jsonb name → flush →
  INSERT..RETURNING → visible mid-transaction → rollback clean.

## Phase 2 — first milestone DONE (2026-07-05)

`phase2_verify` PASS and standalone harness still 78/78. Built:
- **kernel bridge** (`engine-py/src/kernel.rs`): RustKernel dispatches on
  the calling cursor's own connection/transaction (registry from the live
  export, shared caches).
- **routing shim** (`python/rust_orm_shim.py`): `search_read`,
  `search_count`, `_read_group` routed to the kernel with layered gates —
  method-override identity (`_search`/`read`/... below BaseModel),
  display-name fidelity (custom `_compute_display_name`, custom
  `display_name` compute, non-stored `_rec_name` like res.groups
  `full_name`), order fidelity (`_order_spec_clean`: non-stored order
  fields like account.account `code`, nested-m2o comodel ordering), plus
  transparent fallback on any kernel error. Wire values revived to
  in-process Python objects (datetimes, m2o tuples, browse records for
  read_group groups).
- **verifier** (`phase2_verify`): corpus original-vs-routed in the same
  transaction 78/78 (62 routed, rest correctly gated); whole-registry
  sweep: **273 models kernel-verified, zero mismatches**; allowlist written
  to harness/phase2_allowlist.json.
- cursor facade hardening from real traffic: psycopg3-style *declared*
  param types (unknown oid for strings; fixes polymorphic `unnest($1)`
  without aborting transactions), execute must use the prepared statement
  (not re-prepare by SQL text), numeric loads as float
  (`_NumericToFloatLoader` parity).

In-process timing (median): typical search_read 2.6–3.1×; gated models
1.0× (fallback, by design); small read_group 0.6× — `env.flush_all()` +
boundary overhead dominates sub-ms queries. Tuning items → Phase 2 cont.

**Upstream test gate (2026-07-05, `phase2_tests`)**: `test_expression` +
`test_search` (52 upstream tests) run inside the hybrid, one process per
mode for isolation. Routed == baseline on every test except `test_count`,
which `assertQueries`-snapshots the literal SQL text — the kernel's SQL is
semantically identical but textually different (`$N`/`IN` vs
`%s`/`IS TRUE`). **Waived** (the plan's predicted "tests poke engine
internals" category); 27 kernel-routed calls exercised during the run.
Shared failures in both modes (`setUpClass`, `test_rec_names_search`,
`test_20_x_active`) are environment-level (no demo data / regclass param
path), not shim-caused. Targeted flush landed
(`is_any_dirty` + `_compute_engine.pending`); exotic inferred param types
(regclass) re-prepare with TEXT declarations.

**Upstream gate round 2 (2026-07-05): PASS.** Fixes that got there:
- psycopg `sql.Literal.as_string(cnx)` needs an adaptation context: one
  real psycopg connection kept solely for client-side quoting (never
  queried); `FakeCursor.connection` / `FakeConnection.pgconn/adapters`.
- `ODOO_DISABLE_COPY=1`: the v19 bulk-create COPY path is switched to the
  byte-identical INSERT VALUES path until the cursor implements the COPY
  protocol (named backlog item). setUpClass then passes → **87 tests run**
  (was 44).
- The deeper suite caught two real gaps, both now gated to fallback:
  like-family operators on many2one (Python searches comodel display names
  with access rules; kernel compared the raw column) and **security
  metadata mutated mid-transaction** (kernel security is a boot snapshot:
  writes to ir.rule/access/groups/users/ir.default/company/lang/ir.model*
  taint that cursor via a WeakSet and stop routing for that transaction).
- Result: routed-extra = ∅ vs baseline (test_count SQL-snapshot waiver not
  even needed in the final run). Caveat recorded: upstream setUpClass
  creates res.users → taints the class-shared TransactionCase cursor → only
  2 routed calls inside the suite. Routing coverage evidence remains
  phase2_verify (78/78 corpus, 273 models, zero mismatch, re-confirmed).

## Correctness and cache audit (2026-07-26)

Six defects found by testing the claims above against a rebuilt database
(`rustpoc_probe`, 77 modules from `tpl_p314o19marin`). All fixed, all covered
by tests; the corpus is 78/78 on the rebuilt fixture.

1. **unaccent — silent wrong results.** The kernel hardcoded "the target DB
   has no unaccent extension". Odoo probes for it at runtime and wraps both
   sides of ilike comparisons. Every database from the workspace template has
   it, so every text ilike returned *fewer* rows than Odoo, ungated. On
   `res.country.state` ilike 'san': 35 rows vs Odoo's 39. This is why the
   original 78/78 was 77/78 once re-run on a template-derived database.
2. **Rule-cache staleness — security-relevant.** Rule ASTs were cached per uid
   for the process lifetime with no invalidation; the per-cursor taint in the
   shim cannot evict what other cursors cached. Demonstrated: after tightening
   a global `account.account` rule, a live server kept serving 51 records while
   a fresh process and Python both returned 0. Now `Orm::dispatch` checks the
   `orm_signaling_*` watermark (as Odoo does per request) and atomically swaps
   the security snapshot.
3. **Multi-company — silent wrong values.** `Request` carried no company, so
   `env.company` was always the user's default. Python returned 999.0 where the
   kernel returned 111.0 for a company-dependent field read in company 2. Now
   `allowed_company_ids` is carried through, resolved exactly as
   `Environment.company/companies` (including the fallback, where `env.company`
   is `user.company_id` and *not* `companies[0]`), unauthorized companies are
   rejected, and the rule cache is keyed by the full identity.
4. **Statement cache scope — the Phase 2 latency mystery.** The cache lived on
   `Orm`, which `http.rs` and `kernel.rs` build per request, so every query
   re-prepared; only `bench` (one `Orm`, loop inside) ever saw the cached path.
   Measured cost: 0.22 ms/query, ~67% of a small read. It now belongs to the
   connection. This is a strong candidate for the "small read_group 0.6×"
   recorded above — that conclusion should be re-measured before being trusted.
5. **Silent ORDER BY degradation.** `parse_order` dropped unresolvable terms
   and fell back to the raw FK column, returning correct rows in the wrong
   order with no signal — against this plan's own rule that unsupported
   constructs must fail into delegation. It now raises.
6. **Cursor bugs.** `%s` inside `--`/`/* */` comments was rewritten as a
   parameter; `RETURNING` was substring-matched, so `SET note = 'returning
   tomorrow'` was misrouted to the row-returning path. Both fixed and tested.

Also: the gate for m2o like-operators did not recurse into `any` sub-domains;
`env.companies` now excludes archived companies, matching
`res.users._get_company_ids`.

Method note worth keeping: a shadow corpus proves equivalence *on the database
it ran against*. Extensions, installed modules and data all move the answer.
Fixture provenance belongs in the result.

## Fail-closed rules + portable fixtures (2026-07-26, later)

**Rules now fail closed.** A rule the kernel could not compile was logged and
*dropped*, and the query then ran unrestricted. Demonstrated with an ordinary
Odoo domain the mini-evaluator cannot parse, `[('id','in',[1,2] + [3])]` on
`res.country`: Python returned 3 rows, the kernel returned **251**. Eight
models were already in that state on a stock 77-module database (`+` in the
domain, and rules referencing `all_group_ids`). `RuleSet` now separates
compiled domains from unevaluated ones, and `ensure_evaluated` is checked for
the queried model *and* for every comodel whose rules get injected into a
subquery — omitting a comodel rule widens the outer result just as badly.
Both paths now error, which in the hybrid means transparent Python fallback.

**Everything is re-derivable again.** The five embedding bins hardcoded
`sm_statemachine_test` and absolute paths, so none of the Phase 1/2 evidence
could be reproduced. `odoo_kernel::config` now derives every path from
`RUSTPOC_WORKSPACE` with per-value environment overrides, and discovers the
`pythonX.Y` component rather than pinning it. Re-derived on `rustpoc_probe`:

| gate | result |
|---|---|
| corpus (standalone) | 78/78 |
| `phase1_shell` | PASS — registry through Rust connections in 1.23s, c21 parity, write/rollback |
| `phase2_verify` | PASS — corpus 78/78 in-process, **257 models kernel-verified, zero mismatches** |
| `phase2_tests` | routed == baseline; routed-extra = ∅ |

The six fail-closed models show up in `phase2_verify`'s fallback list with
their exact reasons, i.e. the safety property is observable in the sweep.

**Fixture provenance is now enforced, not remembered.** `gen_expected.py`
stamps the baseline with its database and `has_unaccent`; `run-corpus` stamps
its output the same way; `diff.py` refuses to compare across databases and
`phase1_shell` skips (rather than fails) a parity check whose baseline came
from elsewhere. That is the unaccent lesson made structural.

**A cursor bug was hiding behind a documented waiver.** The round-2 note above
calls the shared `setUpClass` failure "environment-level". It was not: stock
psycopg runs the same suite 0-failed/0-error, while the Rust cursor raised
`cannot call jsonb_each on a non-object`. Cause: a Python `str` bound to a
json/jsonb parameter is JSON *text* — psycopg ships it untyped and Postgres
parses it on the cast — but `py_to_sql` wrapped it as a JSON string scalar.
Fixed; `test_expression` went from 44 run / 1 failure / 1 error to **87 run /
1 failure / 0 errors**, unmasking 43 tests.

## Running the last two failures to ground (2026-07-26, later still)

Both remaining failures were chased down. They had *different* causes, and the
first attempt to classify them was wrong — worth recording, because the wrong
method is seductive.

**Wrong method:** run the test under `odoo-bin --test-tags` (passes) and under
the hybrid (fails), conclude the hybrid is at fault. That is what "stock
psycopg is clean, so it's ours" amounts to, and it is invalid whenever the two
runs differ in more than the thing being tested. Here they also differed in
the *test runner*.

**Right method:** hold the runner fixed and vary only the stack. Running the
same test through a bare `unittest.TextTestRunner` inside `odoo-bin shell` —
i.e. stock psycopg, hybrid's runner — separates them cleanly.

- `test_rec_names_search`: **harness artifact, not ours.** It fails identically
  on stock psycopg under a bare unittest runner. `assertQueries` opens with
  `if not self.warm: return` (odoo/tests/common.py:802), so it behaves
  differently under Odoo's runner than under plain unittest. The query itself
  is byte-identical between stock and hybrid — both order by ir.model's
  mail-overridden `_order` and both wrap ilike in `unaccent()`. Left failing
  and visible rather than waived: a known artifact with evidence beats a
  silent exclusion. Fixing it properly means running the suite through Odoo's
  own runner inside the embedded interpreter.
- `test_20_x_active`: **ours, now fixed.** Passes on stock under the same bare
  runner, so the runner was excluded. `_get_fk_constraints` selects
  `pg_constraint.confdeltype`, type `"char"` (oid 18) — a 1-byte integer on
  the wire that psycopg surfaces as a one-character str. `cell_to_py` had no
  case for it, fell through to `try_get::<String>`, failed, and decoded it as
  `None`; the tuple comparison in `get_foreign_keys` then never matched,
  returning `[]`, and `[0]` raised IndexError deep inside `init_models`. The
  array conversion path already knew `"char"` is an i8; the row path did not.
  `test_search` is now 8 run / 0 failures / 0 errors.

Net upstream gate: `test_expression` 87 run / 1 failure (the artifact above) /
0 errors; `test_search` 8 run / 0 / 0; routed == baseline throughout.

## Deepening the sweep (2026-07-26, last)

The whole-registry sweep was broad but shallow: per model it ran
`search_count([])` and one `search_read([], limit=5)`. An empty domain
exercises no operator, no ordering and no grouping — which is how "273 models,
zero mismatches" coexisted with every `ilike` being compiled wrongly.

Shapes are now generated from each model's own stored fields: ilike and the
falsy `=`/`!=` pair on a char, inequality and `not in` on a number, a date
`!= False` plus a `date:month` grouping, a selection grouping, m2o `!= False`
/ dotted `.id in [...]` / grouping, an x2many read, and an ordered and an
offset read. **3,046 shapes across 267 models, 2,737 routed, zero mismatches**
— up from ~524 shallow ones. Each probe runs in its own savepoint, or the
first bad shape aborts the transaction and every later model fails for an
unrelated reason.

It found three kernel defects and one shim defect on its first run:

1. **Panic on incomplete m2m metadata.** `x2many_ids` and `x2many_subselect`
   used `.unwrap()` on `relation_table`/`column1`/`column2`. A panic inside the
   hybrid takes the worker down, where an error merely falls back to Python.
   Now `Field::comodel/inverse_column/m2m_columns` return errors.
2. **`GROUP BY NULL`.** Grouping by a m2o whose comodel `_rec_name` is not a
   direct column — `create_uid` → `res.users`, whose `name` is inherited from
   `res.partner` — emitted `NULL` as the label expression, which Postgres
   rejects ("non-integer constant in GROUP BY"). It now uses `read_expr` (which
   handles inherited/related rec_names), and synthesizes `"model,id"` only when
   there is genuinely no `_rec_name`, matching `_compute_display_name`.
3. **Fields with `search=`.** `mail.activity.mixin.activity_summary` is a
   stored varchar whose domain Odoo delegates to `_search_activity_summary`
   (which traverses `mail_activity`). The kernel compared the column and
   returned 81 rows where Python returned 0. The export now carries a
   `custom_search` flag and such fields are refused. `ir_model_fields` does not
   record `search=`, so only the live-registry export can see this — another
   reason the export is the primary metamodel source.
4. **Kernel SQL errors poisoned the caller's transaction.** Fallback is only
   transparent if the transaction survives the miss. A compile error raises
   before any SQL, but a malformed query reaches Postgres and aborts the
   transaction, after which the Python original fails too — turning a
   recoverable miss into a failed request. `_dispatch` now runs inside a
   savepoint.

Defect 2 is the sharpest argument for shape-generated sweeps: it was reachable
by *any* `read_group` on `create_uid`, one of the most common groupings in
Odoo, and no amount of re-running the old sweep would have found it.

## COPY protocol in the cursor (2026-07-26, last)

`ODOO_DISABLE_COPY=1` is gone. It was a real divergence, not just a speed
tradeoff: with it set, the hybrid ran a *different* write path than production
would, so the bulk-create path was never exercised at all.

v19's bulk create asks for `FORMAT BINARY`, which sounds like the expensive
option but is the cheap one here: `py_to_sql` already produces `ToSql` values
and `ToSql::to_sql` writes exactly the per-field binary encoding COPY expects.
`RustConn::copy` opens a tokio-postgres `CopyInSink`; `RustCopy` implements the
psycopg surface Odoo actually uses (`set_types`, `write_row`, context manager),
frames the binary header/trailer, and flushes at ~64 KiB rather than per row.

Evidence it is exercised and correct, not just tolerated: with
`ODOO_COPY_THRESHOLD=1`, forcing COPY for essentially every create in the
upstream suites, results are identical to the INSERT path — across translated
jsonb names, dates, m2o ints and booleans.

Phase 2 remaining:
m2o like-as-display-name-search in the kernel (removes that gate); dynamic
security reload (removes the taint gate, unlocks routing under mutating
tests); investigate shared jsonb_each baseline failures (translated
hierarchy-ilike on our cursor); route `search_fetch`/`read`; RPC replay.

## Phase 2 status (2026-09-04)

Of the "Phase 2 remaining" list above: `read` is routed (and through it
`web_read`), and RPC replay exists -- `rust_engine_capture` writes every read
`call_kw` serves, `harness/replay.py` feeds it back through the shim in shadow
mode, and the "replay" stage of `verify.sh` runs it. The other three are
still open: the m2o like-as-display-name search, dynamic security reload, and
the shared jsonb_each baseline failures.

What changed the picture was replaying real traffic. Every read method the
web client makes now has a route -- `search_read`, `search_count`,
`web_search_read`, `read`/`web_read`, `web_read_group` (through the routed
`_read_group`), `name_search`/`web_name_search` -- and the first replays of a
bench and of Odoo's own JS tours found nine defects in one day, none of them
visible to any synthetic lane: the shim had dropped the originals' `api.model`
stamps, the cursor typed an empty list as `text[]` and opened its psycopg side
connection with the rust dialect, an x2many with a Python-callable domain was
served unrestricted, a numeric average was taken over `float8`, the shim could
not serialise a `Domain`, and read_group ordered many2one groups by id where
Python follows the comodel's `_order` -- but only when an order is given,
which Python's own SQL settled after a first fix got the rule wrong.

Measured on a 1,569-module fixture: registry sweep 58,883 shapes / 0
mismatches; bench replay 152/152, routed share 0.88; browser-tour replay
42/42, 0 divergences, routed share 0.43; a 2-worker prefork burn-in at
--sample 0 gives ~14% throughput (off 27.2 req/s p50 319 ms, on 30.9 req/s
p50 258 ms) with 0 divergences. What still declines against a
browser does so for real reasons -- Python-computed fields and display names,
multi-currency aggregates -- which is the business-logic wall Phase 3 exists
for. Nothing in Phase 3 has started.

Before `on` in production, in order: a capture from real users replayed with
0 divergences; the many2one unreadable-target policy decided (Python's
`web_read` serves the name through sudo, the kernel serves False, by design of
each side); a burn-in on a prefork server under sampled verification; an
enterprise test lane in CI.
