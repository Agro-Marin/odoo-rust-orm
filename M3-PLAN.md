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
   security, orm), `server/` (rustorm CLI + axum), `engine-py/` (PyO3
   boundary: export, cursor, bins). Harness still 78/78.
2. **Shared caches** ✅ — `orm::Caches` (rule + env) shared via Arc across
   Orm instances; statement cache stays per-connection.
3. **Registry export v2** ✅ — `export_registry` bin dumps the full live
   Python registry (320 models — 15 more than the ir_model bootstrap could
   see, e.g. custom `_table` models) via an env-bound model walk;
   `Registry::from_export` merges it with information_schema.
   `rustorm --export FILE run-corpus`: **78/78** — the live registry is
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
(`rustorm_probe`, 77 modules from `tpl_p314o19marin`). All fixed, all covered
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
`RUSTORM_WORKSPACE` with per-value environment overrides, and discovers the
`pythonX.Y` component rather than pinning it. Re-derived on `rustorm_probe`:

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

## The burn-in ran, and scored the wrong number (2026-09-08)

Third item of that list. Two things had to be repaired before it could answer
anything, and the second is the interesting one.

**It would not boot.** `harness/burnin.sh` appended
`server_wide_modules = base,web,rust_engine` to a conf that already declared
one, and a duplicate key in one section is a hard `malformed configuration
file` from configparser, so both legs died before Odoo started. This is not
"it never ran" -- the 300 s figures in README §Burn-in came out of it -- it is
that it stopped working when this workspace's conf grew a
`server_wide_modules = base,rpc,web` line of its own. Fixed by appending
`rust_engine` to whatever the conf declares, which also stops it silently
dropping `rpc`.

It then found a divergence, on a 73-module / 315-model fixture,
2 workers, 8 threads, 90s per leg, `--sample 0.05`:

    off   42,216 requests   468.9 req/s   p50 15.48 ms   errors 0
    on    47,418 requests   526.8 req/s   p50 14.03 ms   errors 308
          routed 54,798   verified 2,396   diff 0   500s 0   routing-bugs 0

`diff=0` and `errors=308` in the same line is the whole finding. Every one of
the 308 was `ANSWER CHANGED for res.country.web_search_read`, ~5.2 % of that
call's requests -- the verification sample rate, because a sampled request
returns PYTHON's answer and the two were not byte-equal. The records were
identical; the response ENVELOPE was missing its `version` key, which Python
stamps as a side effect of the inner `records.web_read(specification)` call
that a routed `web_search_read` never makes.

Three things are worth carrying out of it, and only the first is a bug:

- **The shadow verification cannot see a request-scoped side effect.** It
  diffs the METHOD's result; the envelope is not in it. 2,396 sampled
  verifications said `diff=0` while 0.65 % of all responses differed. Any future divergence of this shape is equally invisible to it.
- **The gate scored the wrong number.** `burnin.sh` read the server-side
  counters and printed `BURNIN OK` over 308 changed answers, because the
  bench's own error count was only ever displayed, never scored -- and
  `http_bench.py` printed `errors[:5]`, the first five OCCURRENCES, so a run
  whose first five shared one kind hid every other kind behind them. Both
  fixed: errors are now counted by kind, `answer_changed` is broken out of
  the total, and a changed answer is fatal.
- **This was almost certainly reported before and read by nobody.** Not
  measured -- those runs' artifacts are gone -- but three facts constrain it:
  `res.country.web_search_read` has been in the bench's `CALLS` since the
  first commit, `errors[:5]` and the unscored total have been there just as
  long, and the envelope stamp itself predates this project (the
  `@versioned_envelope` on `web_read` landed 2026-06-08). So any earlier leg
  run with `--sample > 0` would have printed the same lines, and the README's
  own table records that run's requests, routed reads and server-side errors
  while quietly omitting the bench's. A number a gate prints but does not
  score is not evidence; it is decoration.

## A byte-parity lane, and what a negative control is worth (2026-09-08)

The envelope divergence above was found by ONE call shape in an eight-shape
bench, by accident, because the sampling rate happened to make it visible. So
the lane it should have been found by now exists: `harness/parity.sh` boots
the server with routing off and again with it on, drives one set of
RPC-shaped calls per model through real HTTP, and diffs the RESPONSE BYTES.
It is the only comparison in this repo that is not of a method's result --
see README §"Byte parity: the lane the other lanes cannot be".

**It was validated by removing the fix**, which is the only way to know a
green gate is measuring anything. With the shim's one-line envelope stamp
commented out and the release rebuilt, on a 73-module database:

    cases 1387   stable 1379   nondeterministic 8   answered 1351   DIVERGED 265
    on leg routed 1079 reads
    PARITY FAILED  265 diverged

All 265 are `web_search_read`, every one of them the missing `version` key,
across every model the kernel routes. Restore the line, rebuild, rerun:
`PARITY OK`.

Two things that measurement settled, neither of which was obvious:

- **The blast radius was 19 % of the corpus, not one call.** The burn-in saw
  it through 1 of its 8 shapes and 5.2 % of that shape's requests, which
  reads like an edge case. It was every routed `web_search_read` on every
  model, and only the sampling rate kept the number small.
- **The nondeterminism guard earned its place on the first run.** Eight cases
  over `res.users.log` and `res.device.log` differ between the two baseline
  passes, and the mechanism is the harness itself: each recording pass
  authenticates once, and a login writes a `res.users.log` row.
  `search_count` reads **147, then 148, then 149** across the three passes.
  Without the double baseline those would have been three false divergences
  in the gate's very first report -- and a gate that cries wolf on its first
  run is a gate nobody reads twice.

## The third defect of the same shape, and it needed a scenario built (2026-09-08)

`falsy_value` and the response envelope were both one shape: **a fact Odoo
declares per FIELD that the kernel derived, or ignored, instead of reading.**
Asking what else has that shape found a third, and this one was a wrong answer
with record rules in it.

`bypass_search_access` says that a subquery THROUGH a field runs with the
comodel's ACL and record rules turned off -- `_optimize_any_with_rights`
rewrites `any` to `any!` when a field declares it, and
`_search(bypass_access=True)` skips both. **90 fields of this 73-module
database declare it**: every mail-thread `message_ids` and `activity_ids`,
every `attachment_ids`, `res.users.partner_id`,
`account.move.line.move_id`, `product.product.product_tmpl_id`. The kernel
neither exported nor modelled it and compiled the comodel's rules into every
such subquery, answering with fewer rows than Python.

**Reading the code was not enough, and that is the part worth keeping.** A
first pass put eleven realistic traversals through the corpus differ at a
portal identity and got: 14 REFUSED for unrelated reasons (`mail.message`,
`ir.attachment`, `account.move.line`, `res.users` and `product.product` all
override the read path in Python; `res.partner.message_ids` carries a
Python-computed domain; `res.partner`'s rules recurse through `res.users`),
4 DENIED by ACL, and 4 COMPARED -- of which two were at `su`, where Odoo
bypasses anyway, and two were the control. **Not one comparison reached the
divergence.** The kernel's other refusals are dense enough that a plausible
case list bounces off all of them, and "I read the code and it looks wrong"
would have stayed a guess.

So the scenario was built: an internal user, a `res.partner` of type
`private` owned by somebody else (which the shipped rule "a private address
is its subject's" hides from them), and an `account.analytic.account`
pointing at it.

    visible_to_user = False          the user genuinely cannot read the partner
    account.analytic.account, ('partner_id.name','ilike',…), uid 197
        python [{id: 1}]   kernel []    search_read     <- diverged
        python 1           kernel 0     search_count    <- diverged
        python [{id: 1}]   kernel [{id: 1}]  at su      <- matched, the control

After the fix all three match. The flag is exported, honoured, and `any!`
bypasses on its own; a bootstrap registry, which cannot see the flag,
refuses a traversal whose comodel has rules rather than guessing in either
direction.

`sweep_corpus.py` now emits one such traversal per model at both identities
-- **72 cases, and on this fixture all 72 REFUSE.** That is worth stating
plainly rather than filed as coverage: every bypassing field here leads into
`mail.message`, `ir.attachment`, `account.move.line` or another comodel the
kernel declines for an unrelated reason, so the sweep emits the family and
still compares none of it. What the cases buy is that they exist and are
classified -- a change that makes one routable compares it from that day,
and an error or a panic in the path would surface now. The coverage that
actually holds this fix is the four unit tests and the scenario above.

**And seeding the scenario immediately found a fourth, unrelated defect.**
The registry sweep gained two fallbacks on `account.analytic.account`, on
`db error: ERROR: column account_analytic_line.auto_account_id does not
exist`. `line_ids` is a STORED one2many whose inverse,
`account.analytic.line.auto_account_id`, is a non-stored many2one with a
`search=` method -- so there is no column to join on. The kernel read the
one2many's own `store` flag, which says nothing about the inverse, emitted
the column and learned from PostgreSQL that it was not there. Reproduced at
`su`, which is what rules out the day's other changes: `su` short-circuits
before any of them. It now asks the comodel's field and refuses at compile
time -- through one helper that BOTH paths call, because fixing the filter
path alone left the registry sweep emitting the identical error: reading a
one2many resolves its inverse in `scan.rs`, not in the compiler, and the
second site was found only by re-running the battery rather than by
trusting the two hand-written cases that had just gone green. The shape had always been compiled and never reached, because the
sweep skips a model with no rows and this fixture had no analytic account
until one was seeded ten minutes earlier.

**The lesson is the fixture's, not the kernel's.** Four defects in one day
all lived where a comparison ran and agreed for want of a discriminating
row, a reachable path, or any row at all: none held `res_id = 0`, no corpus
case compared against `False` with an ordering operator, no identity could
both reach a bypassing field and be filtered by its comodel's rules, and
`account.analytic.account` was empty. A green diff is a statement about the
fixture at least as much as about the code -- and the cheapest way to find
the next one of these is to put a row where there was none.

## A fifth, and the first that was about speed pointing the wrong way (2026-09-08)

Four defects in, the question "what else does Odoo declare per field that the
kernel derives or ignores?" was not exhausted. Enumerating the field CLASSES
that override SQL generation -- `condition_to_sql` and friends across
`odoo/orm/fields/` -- gives a short list, and two entries on it had never been
looked at.

`Binary` with `attachment=True` compiles a condition to `EXISTS (SELECT 1 FROM
ir_attachment ...)` rather than to a column. 41 stored fields here. **Not a
defect**: the column does not exist, so the kernel refuses (`cannot traverse
non-stored res.partner.image_1920`) and the shim falls back. Measured, not
assumed.

`_String` ANDs a **trigram accelerator** onto every positive `like` / `ilike`
over a translated `index="trigram"` field. 11 trigram-indexed stored fields
here, of which the translated ones include `product.template.name`,
`account.account.name` and `account.analytic.account.name`. The kernel emitted
only the base condition -- which is CORRECT, and the corpus differ says so --
and that is exactly why nothing had ever noticed:

    account.analytic.account, ('name','ilike','a'), su    python == kernel   MATCH

The defect is not in the rows. The GIN index is declared over the accelerator's
own expression and nothing else can use it:

    gin (unaccent(jsonb_path_query_array(name, '$.*')::text) gin_trgm_ops)

Measured with `SET enable_seqscan = off`, which is what separates "the planner
preferred a scan" from "the index is unusable":

    with the conjunct     Bitmap Index Scan on product_template__name_index
    without it            Seq Scan   (with sequential scans DISABLED)

So on a large `product.template` the kernel was doing a sequential scan where
the Python it replaces does an index scan -- **slower than the thing it is
meant to be faster than**, on the path a product autocomplete takes, and
invisible to every correctness lane by construction.

`crate::trigram` ports Odoo's two pattern builders. The port was validated
BEFORE it was written: the algorithm was prototyped in Python and diffed
against `odoo.libs.sql.trigram` over 40,040 inputs -- 40,000 of them random
over an alphabet of wildcards, backslashes, quotes, tabs, newlines and
non-ASCII -- and it took three rounds to reach 0 mismatches. The two
behaviours that cost those rounds are worth keeping, because neither is
visible in the source at a glance:

- **Python's `$` matches before a single trailing newline**, so a segment ends
  there -- *unless* a backslash escaped that newline, in which case the match
  runs to the true end instead. This is a scanner state, not a `strip_suffix`.
- **A dangling backslash drops only the segment it ends.** `"trailing\"` gives
  `%` because its one segment fails; `'"xc\t\nzé_\'` keeps the segment before
  the `_` and drops only the last.

Doing it the other way round -- port first, test after -- would have shipped
a conjunct that drops rows on any pattern ending in a newline, and a
row-dropping conjunct is a correctness bug wearing a performance fix's
clothes. The unit tests are Odoo's own outputs, generated rather than
transcribed, after a first hand-typed table was wrong on `100%`.

**What it is worth, measured rather than asserted**, on 200,000 rows of the
same shape (a scratch table with the fixture's own index definition, since
`product_template` here is empty), and across selectivities because one
flattering number would be a lie by omission:

    pattern         rows matched   without    with
    c4ca4238a0b9               1   70.5 ms    0.7 ms    102x faster
    abc                    1,462   67.1 ms    8.3 ms      8x faster
    Produit                    0   71.9 ms   72.5 ms    neutral
    widget               200,000   71.7 ms  176.2 ms    2.5x SLOWER

The accelerator is NOT a universal win and is not meant to be. A pattern
matching most rows pays for a GIN scan that excludes nothing, and `Produit`
is the shape where the prefilter searches every translation and so matches
everything the base condition then rejects. **Odoo has precisely this
profile, because this is Odoo's conjunct** -- parity with Odoo's PLAN is the
goal, not a cleverer plan of our own that wins one workload and loses
another. What the change buys is that a selective search, which is what an
autocomplete is, stops being two orders of magnitude slower than the Python
it replaced.

The single-valued `in` -- which is what an `=` becomes -- is wired up too,
through `value_to_translated_trigram_pattern`, compared with LIKE rather
than ILIKE because the equality it accompanies is case-sensitive. Odoo's own
emitted SQL was the reference for where it is WITHHELD as much as for where
it applies: a value shorter than a trigram, a falsy operand, more than one
value, and `!=` are unaccelerated on both sides.

## The coverage audit that caught a bypass I had just written (2026-09-08)

With five defects found and fixed, the obvious next question was whether the
lanes cover what the compiler now does. So: which operators does `sqlgen`
implement, and which does no corpus exercise? Counted over the sweep corpus,
the curated corpus and three fuzz seeds:

    ilike 1591  != 1374  > 1311  not ilike 1147  = 1119  in 840  not in 748
    >= 560  not any 386  any 374  <= 339  < 322  child_of 317  parent_of 302
    like 205  not like 196  not =like 185  not =ilike 177  =ilike 175
    =like 172  =?  3

Every operator the compiler handles appears, **except `any!` and `not any!`
-- and `any!` was the one whose meaning I had changed that afternoon.**

7b17267 made `any!` bypass the comodel's access, reasoning that it is Odoo's
own spelling for exactly that: `_optimize_any_with_rights` rewrites `any` to
`any!` when the field declares `bypass_search_access`. That reasoning is
correct about where `any!` COMES FROM and wrong about where it can ARRIVE.
Odoo produces it inside the optimizer, after parsing, and `Domain()` rejects
it in an incoming domain:

    Domain([('parent_id', 'any!', [...])])
      ValueError: invalid item in domain

So honouring it turned a harmless extra spelling into a record-rule bypass a
CALLER COULD ASK FOR. Measured on the seeded scenario, at the identity that
cannot read the partner:

    ('parent_id','any', …)   python []                  kernel []
    ('parent_id','any!',…)   python raises ValueError   kernel RETURNED THE ROW

Routing off: an error. Routing on: the rows. Refused now in `domain::parse`,
the single door every domain and sub-domain comes through, and the
operator-granted bypass is deleted outright -- **no operator STRING can grant
a bypass any more; only the field's own declaration can.** The legitimate
path is unaffected: the field-declared bypass still finds the analytic
account at the same identity.

Three things worth carrying:

- **A change that makes the kernel MORE permissive than Odoo is a security
  change, whatever it was aiming at.** This one was aiming at completeness.
- **"Where does this value come from?" is not "where can this value arrive
  from?"** The first is answered by reading the producer; the second needs
  the parser, and the parser said no.
- **The coverage audit is what found it**, hours after the commit and before
  it went anywhere, and it found it by asking a question with no suspect in
  it: not "is this right?" but "what does nothing test?". The answer was one
  operator, and it was the one that had just changed meaning.

The same audit run over FIELD TYPES rather than operators: every type in the
registry is exercised by some lane except **`binary`** -- 127 fields, touched
by nothing. Chased, and it is clean: a filter (`= False`, `!= False`,
`search_count`) agrees with Python case for case, and READING a binary field
is refused (`field logo_web of unsupported type for read`) so the shim falls
back. No defect, which is the outcome to hope for and not the reason to have
looked -- the type was untested either way, and `sweep_corpus.py` now probes
the filter so it stops being a blind spot.

    types exercised   many2one 7640  integer 4025  char 4023  datetime 2704
                      boolean 1614  one2many 1197  selection 1027
                      many2many 732  float 316  text 282  date 183  html 88
                      monetary 59  many2one_reference 51  json 46
                      reference 7  properties_definition 6  vector 3
                      properties 2
    never exercised   binary (127 fields)   <- now probed

## The audit turned on the WRITE path, where being wrong is worse (2026-09-08)

Everything above audits reads. A read divergence returns wrong rows; a wrong
binary COPY encoding writes wrong bytes, and the bytes stay. So: which column
types can reach `RustCopy`, and which of them does it actually encode?

`set_types` mapped an unrecognised OID to `Type::TEXT`. Binary COPY carries
no type tags -- PostgreSQL reads whatever arrives as the column's own binary
format -- so that substitution does not fail, it writes the wrong bytes.
Demonstrated on `vector`, which this workspace has installed and uses
(`ai.embedding.embedding_vector`):

    COPY … (v vector) FROM STDIN (FORMAT BINARY), value "[1,2,3]"
      SQLSTATE 54000  vector cannot have more than 16000 dimensions

The first two bytes of the string were read as a dimension count. It errored
here; whether a different value would have been accepted as a valid-looking
vector was NOT demonstrated, and is not claimed.

**It was not reachable through Odoo.** `_can_dump_binary` asks psycopg
whether it has a binary dumper for every OID, and it still works under the
shim -- measured directly: `_can_dump_binary([vector])` is False, so a bulk
create falls back to text COPY and the cursor is never handed the OID. Of the
11 distinct column types in this database, psycopg refuses exactly one.

**So the safety rested on a coincidence nobody had checked**: that psycopg's
binary-dumper set is a SUBSET of the OIDs tokio-postgres knows. It is -- all
**58** of the 291 `pg_catalog` base types psycopg will dump resolve through
`Type::from_oid` -- and that is now a unit test rather than a coincidence.
`set_types` refuses an OID it cannot resolve, so the property no longer has
to hold for the cursor to be safe; it only has to hold for the cursor to be
USEFUL, and a failure is a loud error naming the oid.

**Odoo already had the contract and the test.** psycopg's `set_types` raises
for a type it cannot dump, and
`test_db_cursor.py::test_can_dump_binary_agrees_with_set_types_on_every_type`
asserts the guard and the cursor agree over every `pg_catalog` array type.
`phase2_allowlist.json` runs `test_expression` and `test_search` and nothing
else, so the rust cursor had never been held to it. The lane that would have
caught this exists, in Odoo, and runs against the other implementation.

**So it was run by hand, and two things came back that the fix does not
settle.** First, the test still fails, and the mismatch set says why:

    71 pg_catalog array types, 47 mismatches, all one direction
        psycopg=False  rust=True    47   int2vector, oidvector, _xml, point…
        psycopg=True   rust=False    0

Zero in the corruption direction. The 47 are types `Type::from_oid` RESOLVES
and `py_to_sql` cannot encode, so `set_types` accepts the declaration and
`write_row` fails with `COPY encode failed` -- loud, late, and unreachable,
since `_can_dump_binary` refuses every one of them and no binary COPY is
attempted. Closing it means duplicating `py_to_sql`'s coverage inside
`set_types`: a second list to keep in step, for cases that cannot arrive.
Not done, deliberately, and recorded here rather than left to look like an
oversight.

Second, and more useful: **`phase2_tests` could not gate this even with the
module added.** It differences a baseline leg against a routed leg, and what
it toggles is ORM ROUTING -- the db shim is installed in BOTH legs, so the
cursor is rust-backed on both sides of the comparison. A cursor regression
shows up identically in both and lands in the "pre-existing in both modes"
bucket the gate ignores. Measured: adding `test_db_cursor` gives
`baseline 44/379 fail -> routed 44/379 fail`, which the gate reads as clean.
A real cursor gate has to diff the rust cursor against PSYCOPG, and that is a
different harness from the one that exists -- the same shape as the byte
parity lane, which had to boot two servers because no in-process comparison
could see what it needed to see.

## A cursor gate, and 61 differences it found on its first run (2026-09-08)

The previous entry ended by saying a real cursor gate has to diff the rust
cursor against PSYCOPG rather than against itself. It exists now.

`harness/cursor_parity.sh` runs Odoo's own `test_db_cursor` twice through the
same bare-`unittest` harness -- once with the db shim installed, once on
plain psycopg -- and diffs by test NAME. The runner's own artifacts appear on
both sides and cancel, which is the whole reason the comparison is legible
where `phase2_tests`' 44-of-379 was not.

    ran psycopg=378  rust=378
    not-ok both=18   ONLY-RUST=61   only-psycopg=0   (59 after the fix below)

**61 of Odoo's own cursor tests fail against the rust cursor and pass against
psycopg**, and none go the other way. A classification, not a diagnosis:

    text COPY: no types declared (already a recorded gap)   19
    AssertionError                                          19
    other assertion detail                                   7
    TypeError                                                6
    AttributeError / ValueError / InFailedSqlTransaction      9
    RuntimeError (other)                                     1

    TestCopyFrom 6, TestIdlePoolReaper 6, TestCopyEncodingsAgree 5,
    TestSaturatedPoolNamesItsHolders 4, then 27 more classes with 1-3

Three things to carry:

- **None of the 61 came from the `set_types` change made an hour earlier.**
  Checked, not assumed: zero of the recorded tracebacks mention its new
  refusal message. The gate keeps the last traceback line per test precisely
  so that question can be answered without re-running anything, and asking it
  was not optional -- a new gate that lights up right after a change is the
  case where attributing by proximity is most tempting and most wrong.
- **The 19 are a gap that was already written down** and the other 42 are
  not. A number is not a discovery until it is decomposed; the largest
  remaining group is pool behaviour, where
  `test_raising_prerollback_hook_keeps_the_connection` fails with *a hook bug
  must not also cost a warm pooled connection*.
- **Ratcheted at 61 rather than gated at zero.** Closing them is a programme
  of work; what must not happen quietly is the number growing. Exact match,
  as the repo's other ratchets are, so a fix lowers the baseline in the same
  commit instead of banking slack.

And the same two guards this session kept needing: a leg that ran no tests
reports VACUOUS and fails rather than passing, and the one test that raises a
real `KeyboardInterrupt` on purpose is excluded from BOTH legs, symmetrically
and visibly -- without Odoo's own handler it escaped and killed the leg, and
a suite that reports nothing is worse than a suite missing one test.

**And the gate paid for itself before it was committed.** Classifying the 61
showed 22 were pool-related, and one shape was a concrete bug rather than a
vague difference:

    TypeError: FakeConnection.execute() got an unexpected keyword argument 'prepare'

The shim's CURSOR `execute` accepted `prepare=`; its CONNECTION `execute` did
not -- and `odoo/db/lifecycle.py` uses it on the connection, twice, to reset a
connection before it returns to the pool:

    conn.execute("DISCARD ALL", prepare=False)
    conn.execute(_RESET_SESSION_STATE_SQL, prepare=False)

That is the path that stops session state leaking between borrows. A shim
standing in for a psycopg connection has to match the CONNECTION's signature
and not only the cursor's. Fixed, and the number went 61 -> 59, which is also
the ratchet's first real exercise: being exact-match, it FAILED on the
improvement -- `59 fail only under the rust cursor, baseline 61: lower the
baseline in the same commit that fixed them` -- which is the behaviour that
stops a fix quietly banking slack for a later regression.

## The cursor gate driven down: 61 -> 3, and what each step cost (2026-09-08)

The gate from the previous entry stopped being a report and started being a
lever. Three fixes, each found by decomposing the number rather than by
reading the code, and each validated by Odoo's own differential tests.

**61 -> 59.** `FakeConnection.execute()` rejected psycopg's `prepare`
keyword. The shim matched the CURSOR's signature and not the CONNECTION's,
and `odoo/db/lifecycle.py` uses it on the connection to reset a returned
connection -- the path that stops session state leaking between borrows.

**59 -> 38.** `RustCopy` required `set_types` before any row, and Odoo does
not call it for a text COPY because there are no types to declare in that
format. So every text COPY through this cursor failed, including the public
`cursor.copy_from(..., binary=False)`. `RustCopy` now reads the format from
the STATEMENT -- where PostgreSQL reads it from too -- and encodes COPY TEXT
itself. 21 tests, the single biggest cluster.

**THE DETECTOR WAS WRONG FIRST, AND SURVIVED THE FIRST CHECK.** It looked for
`WITH`, and Odoo emits `COPY t (cols) FROM STDIN (FORMAT BINARY)` with no
`WITH` at all -- so every BINARY stream would have gone out with no header,
which PostgreSQL rejects outright as `22P04 COPY file signature not
recognized`. Running both formats in ONE process showed both passing, an
artefact of the temp table and transaction state they shared; run in
isolation, binary failed immediately. **"Both work" from a combined run was
not evidence**, and the isolated re-run is the only reason a whole-stream
failure did not ship.

**A sequence is an ARRAY, not JSON.** The first text encoder rendered a
Python list as `[1,2]`, which is `22P02 malformed array literal` against an
array column. Fixed to `{1,2}`, as psycopg renders it. **That fix did not
close its test and the count stayed at 38** -- it moved
`test_binary_and_text_write_identical_rows` from a crash to a data mismatch,
which is progress inside a test and not a pass, and is reported as such.

**And it made a defect in the BINARY path visible**, which is the third fix.
A JSON document passed as text to a `jsonb` column was re-encoded as a JSON
string instead of parsed.

    binary   f = '{"k": [1, 2]}'     a string
    text     f = {'k': [1, 2]}       a document

Odoo's own test says it in words -- *a JSON document passed as text must be
parsed, not re-encoded as a JSON string value*.

**38 -> 36.** The cause was not in the type-directed encoder, which was
right; it was one branch earlier. A psycopg `Json` / `Jsonb` WRAPPER carries
its own `dumps`, and Odoo wraps a string bound for a json column as
`Jsonb(s, dumps=_dump_json_verbatim)` -- the wrapper's way of saying *this is
ALREADY json text, ship it unchanged*. Reaching past the wrapper for `.obj`
and re-encoding it discarded that instruction, and `jsonb_typeof` read
`string` where psycopg gives `object`. Asking the wrapper for its own text
and parsing that is correct for every spelling at once, `Json("abc")`
genuinely meaning the json string `"abc"` included -- which is why the
general fix is shorter than the special case would have been, and why it
closed `test_binary_and_text_write_identical_rows` in the same stroke.

**I looked at the wrong branch first** and reported the type-directed encoder
as the site before re-reading; the symptom was right and the location was
not. The oracle for every step here was Odoo's own differential suite, which
exists to catch a text encoder disagreeing with the binary one. None of it
needed a check anybody had to invent.

## The shim was bypassing the pool, not backing it (2026-09-08)

`db_maxconn` was not enforced on the rust path. The db shim replaced
`ConnectionPool.borrow` and `give_back` at the class level and returned a
rust connection without touching the instance, so everything the pool does
AROUND the connection was skipped: `_budget.acquire()`, which is the bound;
`_checkouts.track()`, which is what names the holders when it saturates; the
stats, the leak warning, the health probe. One module-global idle list served
every pool in the process, so per-instance `maxconn` and `reap_idle_ttl` --
and the read/write versus readonly distinction -- meant nothing.

    maxconn=2, six borrows, each driven to a real backend
    psycopg   borrowed 2   PoolError at #2   2 distinct backends
    rust      borrowed 6   no error          6 distinct backends
    after     borrowed 2   PoolError at #2   2 distinct backends

**THE FIRST INSTRUMENT SAID THERE WAS NOTHING HERE.** Counting
`pg_stat_activity` rows from the shell's own cursor read `opened: 0` on both
legs, which retires the finding. That count is served from the transaction's
CACHED STATS SNAPSHOT and cannot move inside one transaction, so it was
answering a question about a snapshot rather than about connections. Asking
each borrowed connection for `pg_backend_pid()` is the direct instrument.
Same shape as the entry below: the reading was honest and about the wrong
thing, and only a second instrument aimed at the actual claim settled it.

**8 closed, 1 opened, net 7.** Releasing the budget in a `finally` also
released it on a SECOND `give_back` of one connection, crediting a permit
nobody acquired -- `test_double_give_back_does_not_over_release`, green
before. Odoo's `give_back` spends a pop-once marker for exactly this and the
shim now spends one too. **A net count hides a regression**; the gate diffs
NAMES, which is the only reason it was visible rather than absorbed.

The other half of the seam is still open and it is the better fix: the shim
gives out connections without registering a per-DSN pool in
`ConnectionPool._pools`, so the reaper, `close_database`, `drain_database`
and the pooled `db_session_gucs` have nothing to act on. Rebinding the
module-global `_PsycopgPool` to a rust-backed pool would let Odoo's own
`borrow` run unmodified end to end, which is smaller than what is there now
rather than larger.

## The closed set, and the one type outside it that shipped anyway (2026-09-09)

After the array work, the useful question stopped being "which types are
broken" and became "which types can occur at all". Two answers, and they
differ:

**What Odoo's ORM can create is a CLOSED SET of ten.** Every stored field
class declares its `column_type`, and across the whole of `odoo/orm/fields/`
they are: `bool`, `date`, `int4`, `float8`, `numeric`, `varchar`, `text`,
`jsonb`, `timestamp`, `bytea`. All ten were already in the type probe's
corpus and all ten agree. That is a much stronger statement than a count of
passing cases -- it is coverage of the whole set, and it is re-derivable:

    grep -rh "_column_type = (" odoo/odoo/orm/fields/

**What a DATABASE holds is larger, and that is where `vector` was.** A sweep
of every column type present in the fixture found 28, including
`vector(1536)` from pgvector -- which is in this workspace's database
template and holds agromarin's embeddings. It was refusing every write with
42804 and returning raw bytes on every read.

**AND THE SWEEP THAT FOUND IT FIRST REPORTED IT CLEAN.** The first version
read existing rows: `SELECT col FROM table WHERE col IS NOT NULL LIMIT 3`.
The embedding column has no rows, so both cursors returned `[]` and the
comparison passed. **An empty result set is not a comparison** -- it is the
denominator-of-zero problem wearing the clothes of a passing test, inside my
own probe, three hours after writing the guard against the same shape in the
cursor gate. The version that found the defect inserted a value first.

The pattern now has three instances in one session: a gate that ran the wrong
thing (psycopg compared with psycopg), a gate that ran the right thing over
too little (the corpus that named five array types), and a gate that ran the
right thing over nothing at all (a column with no rows). All three print
exactly like success.

## A corpus covers the types it names, and nothing else (2026-09-09)

The cursor's type layer handed back RAW BINARY BYTES for ten type categories
-- `numeric[]`, `date[]`, `time[]`, `timestamp[]`, `timestamptz[]`,
`bytea[]`, `oid[]`, `inet[]`, and scalar `interval` and `uuid`. A `date[]`
column read as `b"\x00\x00\x00\x01..."`. Not an error, not a warning a
caller sees -- and the fallback's own log line said it was doing what psycopg
does for an unregistered type, which is wrong twice over: psycopg has loaders
for all ten, and for a genuinely unregistered type psycopg returns TEXT, not
binary.

**THE GATE THAT SHOULD HAVE CAUGHT THIS WAS GREEN, AND CORRECTLY SO.** The
type-layer probe compares the two cursors on a corpus of cases, and its
corpus named `int4[]`, `int8[]`, `text[]`, `bool[]` and `float8[]`. Those
five worked. Every type it did not name was uncovered, and "uncovered" and
"passing" print identically. This is the coverage twin of the vacuity problem
two entries up: there, the gate ran the wrong thing; here, it ran the right
thing over too little.

**The write path was guessing where the server knew.** A list of strings got
declared `text[]` from its first element, so inserting `["2026-01-31"]` into
a `date[]` column produced 42804. Odoo passes dates, timestamps and numerics
as strings -- that is the ORDINARY case. Leaving the parameter untyped lets
the server infer from the column and the element conversions parse into it.
The general lesson is the one the 22P02 fix reached from the other side: the
server's inference is authoritative wherever there is context, and a
client-side guess can only be right by luck or wrong by 42804.

**`set_types` now asks the encoder instead of describing it.** It resolved an
oid and called that "can encode", which is the resolve-versus-encode split
this repo already had a name for. It now boxes a NULL of that type through
`py_to_sql` and lets `to_sql_checked` apply the same `accepts` that
`write_row` will apply -- the predicate IS the encoder, so the two cannot
drift. Disagreement with Odoo's guard went 47 -> 7.

**What the corpus gained**: the array of everything the scalar list already
covered, and a READ-ONLY section for types this cursor decodes but cannot yet
encode. Splitting the direction is what lets the gate hold at zero while
covering the half that works -- the alternative was leaving them out, which
is exactly how they stayed broken.

## The errors were arriving stripped, and nothing raised about it (2026-09-09)

8 -> 3, and all five are one theme: an error crossing the rust/Python boundary
kept its text and lost everything a caller acts on.

**A failing COPY raised a bare RuntimeError.** `FakeCursor.copy` handed the
raw `RustCopy` to the caller, so every error from it arrived as
`RuntimeError("SQLSTATE:23505|...")` -- not catchable as
`psycopg.errors.UniqueViolation`, and carrying no `sqlstate`. That last part
is what `odoo.db.errors.has_reached_server` reads, so a COPY that failed ON
THE SERVER was booked as one that never reached it. Three tests, one wrapper.

**A DATABASE ERROR CARRIED NO DIAGNOSTICS, AND THAT ONE IS THE WORST OF THE
SESSION for how quietly it fails.** `db_err` kept the SQLSTATE and the
message and dropped `constraint`, `table`, `column`, `detail`. Odoo builds
its constraint messages from exactly those -- `exc.diag.constraint_name` in
`orm/models/mixins/schema.py`, `.table_name` in `service/transaction.py` and
`load.py`, `.message_detail` beside them. Not one of those raises when the
diagnostics are missing; each quietly degrades to the raw SQL text. So on the
rust path a unique violation stopped telling the user WHICH field clashed,
everywhere in the ORM at once, and no gate in this workspace could see it
except the one cursor test that reads `diag` directly.

**An unencodable parameter raised a Python TypeError**, which is not a
`psycopg.Error` and has no sqlstate. What the caller should get is the
server's `22P02`, and getting there took two wrong answers first:

    re-declare the parameter `text`     -> 42804 at PREPARE time; PostgreSQL
                                           will not cast text into an int
                                           column implicitly
    re-declare it UNKNOWN (untyped)     -> the server just re-infers int4
                                           (measured: client says Unknown,
                                           server answers Int4)

psycopg only gets 22P02 because it can send an UNTYPED TEXT parameter and let
the server parse it; tokio-postgres binds every parameter in binary against
the statement's resolved type and cannot express that. So the question cannot
be put to the server at all, and the honest move is the one already used for
undecodable query bytes: raise the answer the server would have given. The
TYPE NAME is asked of the server (`format_type`) rather than tabulated in the
shim, so the message reads `integer` and not `int4` -- byte-identical to
psycopg's.

**The pattern across all three**: the boundary was preserving what a human
reads and discarding what code branches on. A bare RuntimeError still prints
the SQLSTATE; a stripped diagnostic still prints the message. Everything that
was lost was lost to a `getattr(exc, ..., None)` that answers None, and None
is a valid answer -- so nothing anywhere raised.

## A hook bug was charged twice, and the residual is a feature not a defect (2026-09-09)

11 -> 8, and then the remainder stops being a list of bugs.

**`info.transaction_status` was missing, and `Cursor._is_connection_clean` is
its only reader in the fork.** That check is wrapped in `except Exception:
return False`, so an absent attribute did not raise anywhere visible -- it
silently answered "not clean", and every cursor whose rollback hook raised
ALSO lost its warm pooled connection. Odoo's own test says the intent in
words: *a hook bug must not also cost a warm pooled connection*. The
connection was in fact clean; `Cursor._rollback` rolls back in a `finally`, so
the rollback happens even when the hook raises. Only the reporting was
missing.

The mapping is IDLE or INTRANS from the connection's own transaction flag.
It cannot distinguish INTRANS from INERROR, and that is written down at the
attribute -- but the single consumer asks only whether the status is IDLE, so
the distinction is invisible to every reader in the fork today.

**A bytes query that is not UTF-8 raised the wrong CLASS of error.**
tokio-postgres takes the statement as a `&str`, so such bytes cannot reach
the server at all; `.decode()` raised `UnicodeDecodeError`, which is not a
`psycopg.Error`. A caller catching database errors saw nothing. Measured what
the server itself does with those bytes -- `CharacterNotInRepertoire`,
SQLSTATE 22021, `invalid byte sequence for encoding "UTF8": 0xff` -- and the
shim now raises exactly that. It is the server's own answer produced one hop
early, because the transport cannot carry the question. Measuring it beat
guessing: `DataError` and `ProgrammingError` were both plausible and both
wrong.

**THREE OF THE REMAINING EIGHT ARE ONE FEATURE THAT DOES NOT EXIST YET.**
psycopg's pipeline mode defers results, so from the second statement in a
pipeline block `cursor.description` is None. `pipeline()` is a `nullcontext`
here. tokio-postgres has no equivalent of libpq's pipeline mode, so this is
something to BUILD, and calling it three failures overstates how much is
broken while understating how much is absent. A ratchet counts tests, and a
test count cannot tell a defect from a gap -- the note has to.

## Four small parity gaps, each answered by reading psycopg rather than guessing (2026-09-09)

18 -> 11, and the method was the same every time: find the rule in psycopg's
own source, then implement THAT rule rather than the behaviour the test
happens to assert.

**The prepared-statement cache is the one that mattered.**
`psycopg/_preparing.py` clears the cache when a command's status TAG is
`ROLLBACK` or starts with `DROP `, because a plan prepared against an object
that a rollback or drop removed gets looked up internally by PostgreSQL and
fails. That single rule produces all three of the assertions the tests make:
`ROLLBACK TO SAVEPOINT` reports the tag `ROLLBACK` and clears, `RELEASE
SAVEPOINT` reports `RELEASE` and does not, `COMMIT` does not. Implementing
the three assertions separately would have been three special cases and no
coverage of `DROP` at all.

tokio-postgres exposes no status tag, so the rule reads the STATEMENT
instead. **Where an approximation diverges is worth writing down at the
point of approximation**: a `DROP` inside a DO block or a function body, or
one that is not the first statement of a batch, produces the tag but not the
leading keyword. Those under-clear -- the same direction psycopg errs when a
cache is cold -- and the note says so.

The other three were surface, not semantics: `RustCopy.write` (raw
passthrough, and it must mark the signature sent so a later `write_row` does
not splice a header mid-stream; psycopg appends the binary trailer only in
ROW mode, so a caller writing raw bytes owns its own trailer),
`Cursor.scroll`, and the `_pool` stamp psycopg's pool leaves on a lent
connection.

**`cargo build` is not evidence that the shim is valid Python.** The shim is
embedded with `include_str!`, so a botched edit that left an `if` body
followed by a bare `else` compiled perfectly and failed at import: `rust leg
produced no result`. The gate reported it immediately and legibly, which is
the only reason it cost a minute. Syntax-check the shim after editing it;
the build cannot.

## A perfect score, and the thing under test was not running (2026-09-09)

Replacing `ConnectionPool.borrow` was the wrong seam. The right one is the
POOL: rebind `odoo.db.pool._PsycopgPool` to a factory returning a rust-backed
pool, and `borrow`, `give_back`, the budget, the checkout tracker, the idle
reaper, `close_database` / `drain_database`, the stats and the session GUCs
are all Odoo's own code, unmodified. The shim got SMALLER and the gate went
29 -> 18. Two of the eleven were production paths:

  - `db_session_gucs` reached no rust-backed connection, so a deployment's
    `work_mem` / `statement_timeout` policy was silently not applied.
  - `drain_all()` iterates `_pools`, which was empty -- a no-op. `registry.py`
    calls it on every reload, so after any module upgrade a pooled rust
    connection kept prepared plans built against the OLD schema.

**THE FIRST RUN OF THAT FIX REPORTED `ONLY-RUST=0`.** Every one of the 378
agreeing exactly. It was false. A pool is built once per dsn and cached, so
rebinding the class reaches only pools created AFTER install, and the armed
process already holds one; every borrow returned a psycopg connection while
the leg went on calling itself rust. The gate was comparing psycopg with
psycopg and correctly reporting perfect agreement. The honest number was 18.

**A denominator of zero is suspicious on sight and a perfect score is not**,
which is what makes this the more dangerous shape. Every guard the gate
already had was satisfied: both legs ran 378, neither was truncated, the
`both=18` bucket was unchanged, `only-psycopg=0`. Nothing in the output was
wrong. The question none of it asked was whether the leg labelled "rust" had
held a rust connection.

**And the obvious guard does not work.** The vacuous run reported
`connects=28` -- NONZERO -- because the tests that build their own
`ConnectionPool` did get rust pools; only the ones going through the
process-wide pool silently fell back. "Did the shim do anything" passes.
Only "what class is this connection" fails, and that is now asserted in the
suite and re-checked in the gate, with a negative control that reproduces
the false zero on demand.

**The better design is the one that needed the guard.** Patching `borrow`
took effect on pools that already existed, so it could not fail this way --
by accident, not by design. Moving to the correct seam bought a much smaller
shim and an ordering dependency, and the guard is what the second is worth.

## A rebase landed mid-measurement, and the run was discarded (2026-09-08)

A peer rebased and pushed `19.0-marin` in odoo and enterprise while a battery
was reading the odoo checkout. HEAD moved FOUR times inside the run:

    baea56b347d9 -> a38be58f7727 -> 212f2b2b513e -> a3e7929e96f1

The run came back with sixteen stages OK and two red. **All eighteen are
uninterpretable and the run was discarded**, the green ones included: the
early stages measured one tree and the late ones measured another, so no
stage's verdict is about a tree that exists. A green stage against a tree
that is gone is not evidence either.

Two things worth keeping.

**`git status` answers the wrong question.** Both shared trees read CLEAN at
every single sample -- zero dirty files throughout -- while HEAD moved four
times. "Nobody has uncommitted work" and "it is safe to measure" are
different claims, and only the second one mattered. The check is whether HEAD
is STILL, and stillness has to be sampled over time rather than read once.

**A transient cross-repo break is worth reporting even when it evaporates.**
Mid-flight, `odoo.tools.view_validation.register_validator` was absent from
odoo while three enterprise modules still called it (web_cohort, web_grid,
web_map) -- an IMPORT failure, so it takes out every `odoo-bin` touching
those modules rather than surfacing as one red test. It resolved itself two
HEADs later. Reporting it cost one message; staying quiet would have cost the
machine if it had not. The report was hedged as a possible mid-flight sample
when it was sent, and corrected to exactly that once it cleared.

## Three instruments that answered the adjacent question (2026-09-08)

The rebase exchange produced a generalisation worth keeping, arrived at from
both sides. Three checks were consulted today, each a real instrument giving
a clean and confident answer -- to a question next to the one being asked:

    git status    answers "does anyone hold uncommitted work"
                  not     "is the tree still"
    git log       answers "did commits touch this path"
                  not     "did the content change"
    no conflict   answers "did the two sides edit the same line"
                  not     "is the number still true"

Each fails SILENTLY. There is no error, no marker, no empty result -- just a
confident answer to a question nobody asked. The peer's restated-figure case
has the cheapest tell of all: six of nine figures went stale through a merge
git called clean, with no conflict marker, because nothing had touched those
lines. The lines were never the subject. The tree was.

**And the same shape accounts for most of this session's own defects**, which
is why it is recorded here rather than in a message:

    BURNIN OK     answered "did the server's counters show a divergence"
                  not     "did the CLIENT get the same answer"      -> 308 missed
    set_types     answered "can this OID be resolved"
                  not     "can this type be encoded"                -> TEXT into
                                                                       a binary stream
    a green diff  answered "did the two sides agree"
                  not     "was there a row that could tell them apart"
                                                                    -> res_id = 0
                                                                       existed nowhere
    phase2_tests  answered "does routing change the outcome"
                  not     "does the CURSOR change the outcome"      -> 61 invisible

The correction is the same in every case and it is not "check more": it is to
say out loud what the instrument actually measures, and then ask whether that
is the claim being made. Every one of these was caught by writing the two
sentences down next to each other, and none by looking harder at the code.

## Phase 3 has a seam, and it is the fork's own (2026-09-12)

Phase 3 as written above starts with a typed columnar cache in Rust, then the
write primitives, then compute orchestration. That ordering assumed the engine
would have to build its own way into `create`/`write`/`unlink`, the way the
read path built its way in: by replacing methods on `BaseModel`.

It does not. `odoo/orm/runtime/backend.py` declares `StorageBackend` — twelve
methods and five capability flags through which every row read and every row
write in the ORM passes — and
`odoo/orm/tests/test_backend_dispatch_surface.py` pins it: fifteen sites
across nine files, each one annotated with what the in-memory branch does NOT
do, plus an assertion that the mixins hold no row I/O SQL of their own. Two
implementors ship, `PostgresBackend` and `InMemoryBackend`, so the port is
demonstrably not shaped around one backend.

**That is the Phase 3 boundary, already drawn and already tested.** It is also
exactly the line this plan's architecture sketch draws between the Rust engine
and the embedded interpreter: Python keeps the business logic, the cache and
the flush ORDERING; Rust owns what reaches the rows. Implementing the port is
therefore not a detour around Phase 3, it is Phase 3's first three bullets
approached from the side the fork supports.

What this changes about the plan:

- The write primitives do not need the Rust cache first. `update_rows` takes a
  column-group and rows from the flush and renders one statement; the cache
  decided what to flush, and that decision stays Python for now.
- Coverage is declared per method (`NATIVE`) rather than predicted per call.
  The read path's shim has to decide, in Python, whether the kernel supports
  the shape it was handed — the source of the drift the README records. A port
  method is armed or it is not.
- The port reports its own backlog. Every delegated call is counted with its
  reason, so "what moves next" is measured rather than argued.

Landed on 2026-09-12: the port itself, delegating everything, plus
`update_rows` armed. The README section "The persistence port, and the first
write the kernel owns" carries the detail, the verification and the one defect
it cost — a `threading.Lock` around the delegation counter, which serialised
every `fetch` and `search` in the process and surfaced as an intermittent
browser-tour failure rather than as a slow number.

Landed the same day: `create_rows` on its INSERT strategy. The COPY strategy
stays delegated on purpose -- it is the cursor's, already encoded by
`RustCopy` -- so the write path's two statements are both kernel-composed and
the third way rows arrive is Rust end to end already. `unlink_rows` is the
remaining write, and it is not a statement: it collects `ir.model.data` and
`ir.attachment` rows and runs the company-dependent `ir.default` cleanup,
which is ORM work the port would have to call back into.

`search` is implemented and verified exactly over the sweep corpus, and is
NOT armed. It started 1.4 to 1.6 times slower than Python and is at parity now
(0.93 to 1.09 across transaction lengths, within a noisy machine's margin):
the kernel reports the flush set, the watermark is read once per transaction
(a shortcut that trusted Python's registry sequences instead was unsound and
was removed), and a compile runs inside a savepoint only when it needs the
database. At the seam alone it is about a
third faster; the remaining cost is `optimize_full`, which runs in `_search`
before the port is asked. So the next gain for `search` is not in the port:
it is the kernel taking over the domain optimisation that precedes it, which
is the method-level question the shim already answers for its RPC calls.

Not started: `fetch`, which with `search` is the great majority of what the
port delegates.

## Readiness is a differential against Odoo's own suites (2026-09-13)

Installing the engine into the workspace venv put routing under real servers,
and the question stopped being "does the corpus agree" and became "does Odoo
still pass". Every test module installed on the probe databases was run twice,
with no engine and with routing on, and the failure sets diffed by name.

```
suite                               tests   routed   failing only under routing
test_orm, test_read_group,           1457     1390   0   (was 20 before the fixes; HTTP
  test_access_rights, test_search_panel, test_inherits     classes now run on both legs)
/base with HTTP, 0 failed either leg 4012      992   0   (was 4 cursor-parity residuals)
/web                                  460      417   0
/web with HTTP and JS suites, at       852     1365   0   (0 failed either leg; after
  grouped reads, stand-ins, pool fix)                    web_read_group, group_expand)
test_orm and the four above, again    1465     1399   0   (with the stand-in tests)
milestone, odoo 051049f3aeb1, pinned:
  test_orm and the four above         1486     1487   0
  /base with HTTP                     4032     1006   0   (0 failed either leg)
  /web with HTTP and JS suites         872     1381   1   (the aborted-transaction gap,
                                                         fixed in the next build)
  seven enterprise modules             811     2006   0   (5 failed, 1 error on both legs:
                                                         knowledge reds fixed after the pin,
                                                         sign and sale_planning tours)
/mail                                 429       90   0
/mail controllers over HTTP           167      386   0
24 smaller modules (ai, auth, bus,    702       61   0
  iap, sms, web views, ...)
approval, approval_app, base_install  829       10   0
/mail with HTTP (JS suites included)  718      985   0
enterprise: helpdesk, planning,       811     1561   0
  sale_planning, sale_subscription,
  timesheet_grid, knowledge, sign
2026-09-18, odoo bd9a5298f74f / enterprise 6c1e5964516 / agromarin b496ededd,
  siblings live, one 153- then 181-module database (53 enterprise):
project, calendar, appointment,      1625     1929   0   (1 failed + 1 error on both legs:
  knowledge, document, /base device                     document user-folder setUpClass,
                                                        knowledge invite-members query count)
crm, hr, account, document,          3255      946   0   (2 failed + 1 error on both legs:
  /base properties mixin                                the same setUpClass, two crm
                                                        lead-assign query counts; 5 account
                                                        analytic reports failed only routed
                                                        until engine 8030cb7, the cursor's
                                                        nested-list parameter)
sale, purchase, stock                2634     1992   0*  (2 failed on both legs, purchase
                                                        portal routes; *2 product-catalog
                                                        "add section" tours failed only in
                                                        the lane once; 4/4 rerun routed
                                                        alone and green on a second full
                                                        routed leg of the same lane -- a
                                                        one-off timing, not reproduced)
hr_payroll, l10n_mx_edi_payslip       257     1509   0   (24 failed + 9 errors on BOTH legs,
  (fresh 152-module database,                           on a closure the fork's lanes do not
   54 enterprise, 7 agromarin)                          run: wage and worked-day arithmetic
                                                        in hr_payroll's own computation
                                                        tests, two flows with no running
                                                        contract, a Mexican name format, a
                                                        dispersion permission, a memoryview
                                                        given a bool, a journal missing on a
                                                        structure, and §4's payslip tour --
                                                        the fork's, not the engine's)
```

The two later rows ran on odoo 1de39b4d2228 and enterprise a666e8d127a from
detached worktrees: two earlier /web runs in the shared checkout were
overtaken by other sessions' commits between the legs, which makes the legs
compare different trees, so a differential longer than the commit rate runs
pinned (`RUSTORM_ODOO_ROOT` and a conf whose addons_path names the worktrees).

Until 2026-09-13 the harness ran both legs with `--no-http`, so every HttpCase
class, tours and JS suites included, was skipped on both sides and cancelled
out: 18 classes in /base, 25 across the enterprise suites. Running them with a
server surfaced failures that exist with no engine at all, and they are the
fork's to fix, not the engine's. Most are fixed in odoo and enterprise
(action bindings stale after a server-action write, a COPY threshold below
its break-even, a filtered_domain round trip per rule, nested copies fetching
lines twice, partner-merge grouping over an arbitrary 20,000 of 110,542 pairs,
access-error group names in four queries, the JS mock server's missing
`result`, planning's context dates and template choices, knowledge's stale
FontAwesome selectors). What still fails on both legs of the enterprise run:

- knowledge `test_article_get_valid_parent_options` and
  `test_article_tree_panel_w_favorites`, two queries over each: the cost of
  filtered_domain evaluating a search-method field through its search, which
  the fork chose in 428ff95f5280 for parity with search();
- knowledge's portal tour: `knowledge.webclient` is a page built on
  `web.assets_backend` that no module's `dynamic_children` names, so its
  import map lacks `@odoo/o-spreadsheet` and the tour helpers;
- sign's two tours, which pick "Administrator" and meet "Mitchell Admin" on a
  database with demo data.

What that found, none of it visible to the corpora:

- **The Rust cursor was wrong in three ways a type probe did not name**: a
  tuple inside JSON written as its `str()`, a 20-digit JSON integer read as a
  float, and `%%` inside a quoted literal sent doubled. All three are
  psycopg's semantics now, and the type probe carries each shape.
- **A closed Rust connection kept its backend**, and one suite run exhausted
  the shared cluster.
- **Routed answers diverged where Python's rules are not the columns'**: a
  hidden many2one target through `web_read`, a `_check_access` override behind
  a label, a text column compared with a number, an html comparand, an unknown
  timezone.
- **Odoo itself had defects the differential exposed**: the mail access scans
  paged without ORDER BY (odoo 5fa7d6ec20c2), and grouped views checked label
  access once per group (odoo e8be5d08754b, about a third faster with or
  without the engine).

**The fork moves under the engine.** In one afternoon it added four members to
`StorageBackend`, dropped a parameter from `unlink_rows`, moved
`web_search_read` onto `search_fetch` and changed its envelope versioning, and
added a `_search` override to `mail.followers`. Each broke the port or the shim
outright on the armed database -- the `unlink_rows` change failed every
delete. The port now passes through what it only delegates and pins exact
signatures only where it composes SQL; an unknown protocol member is delegated
and counted. The shim's gates key on method identity, so an override that
appears at runtime is seen. The standing defence is running the suites above
after every sync, which `harness/orm_tests.sh` does for any tag set.

**What the engine does not yet own**, measured on recorded traffic:

- models whose read path is Python -- `_search` overrides on mail.message,
  mail.followers, mail.activity, ir.attachment, res.groups -- are the largest
  fallback class, and they are access logic, not queries;
- record rules evaluated in memory (`_check_access` through `filtered_domain`)
  are the largest Python cost left in the calls that do route;
- composing SQL in Python is not: `_field_to_sql` and `Query.select` are under
  a tenth of a replay, so a native `fetch` would buy a few percent.

The next step toward replacement is therefore access logic, not statements,
and one version of it has already been measured and set aside: answering the
rule half of `_check_access` as a `SELECT id ... WHERE id = ANY(ids) AND <rule>`
agreed with Python on 1,804 decisions and saved nothing, because the round trip
costs what the in-memory evaluation does (README, "Recorded traffic said the
routed path was slower, and why"). What remains open is evaluation without the
round trip -- the rule predicates over values the cache already holds, in
Rust -- and the mail access scans as one kernel-side join instead of chunked
Python passes. Either is verified by the differential above before it is armed.

**Grouped views were measured the same way on 2026-09-13, and the answer did
not change.** Routed `web_read_group` still reads 0.95x Python on recorded
traffic. A profile of its 286 calls puts about a third of the time in
`_get_display_name_visible_ids` -> `_filtered_display_name_access` ->
`filtered_domain`, which looks like the case the rule-check step missed:
fresh group records, so the in-memory rules fetch the fields they compare.
Replacing that helper with one rule-filtered `_search` over the group ids,
for comodels whose display-name visibility and `_check_access` are the base
ones, gave identical responses on all 286 calls and 1.025 s against 1.043 s.
The fetches move into the query rather than disappear, and the remaining time
is web's formatting over the groups. What would move grouped views is routing
`web_read_group` whole, as `web_search_read` is, which is a larger step than
the label check.

**Routing it whole did move it** (README, "`web_read_group` routes whole"):
routed calls went from 0.95x to 0.39x Python, with 0 differences in replay and
every-user. But the traffic that capture held grouped every model by one
many2one, and real kanban views do not look like that. Twelve tour classes
route 0.09 of their calls. What stands in the way is `group_expand` on stage
columns and `fill_temporal` on graphs, which are the grouping work left, and
two overrides that rewrite arguments -- `project.task._read_group` renames a
`triage_id` groupby, `helpdesk.ticket._search` rewrites a `ticket_ref` order --
which refuse every grouped call on those models because the gate keys on the
method, not on what the override touches.

### Re-verified on the pushed tips (2026-09-18 evening)

The six replayed commits were verified against trees 30 (odoo) and 45
(enterprise) commits older than the ones they now sit on. On the pushed tips,
odoo `b39df298cdf2` / enterprise `347828acc10`, siblings live, engine `9c3d13e`:

    test_orm + the four ORM test modules   1508    630   0   (0 failed either leg)
    /document, Python + JS over HTTP        516    180   0   (1 error either leg:
                                                              TestDocumentsUserFolder
                                                              setUpClass, the fork's)
    test_read_group alone                   149          (0 failed)

### Hash map after the 2026-09-18 push

Pushed 2026-09-18 evening after a rebase onto origin, which rewrote every odoo and enterprise hash cited in this session's notes and commit bodies. Old -> new, mapped by `git patch-id --stable`:

    odoo        7fbcf93b365e -> ff37d8f6bafc   (group_by_sql / order_by_sql)
    odoo        7d179f01c3f2 -> 3148b2570cab   (base, calendar declarations)
    odoo        bd9a5298f74f -> 45f81c4b2ba5   (value_sql, guideline)
    odoo        2792038b5cdb -> 1700ca96ddc1   (non-stored aggregate)
    odoo        82c9b8ed9a12 -> b39df298cdf2   (document is_folder)
    enterprise  6c1e5964516  -> 347828acc10    (knowledge order_by_sql)

A commit body citing the left column is citing a hash no fresh clone has; the
right column is what resolves.

## The write differential, and the first full battery it prompted (2026-09-18, night)

`harness/write_diff.py` and `write_diff_compare.py` (engine `c2dae2d`): every
model the generator can build, six rows across the value shapes a type has,
a field-by-field write, a uniform write, a per-row string write, an unlink of
the odd rows, then every stored scalar column read back with raw SQL -- once
with the port routing and once without, paired by creation order. OK only
when every cell agrees AND every model saw native creates. A positive control
corrupts the last write of a char column and must be reported on every row.

    rustorm_fe_wide (410 models)   157 models   469 rows   1939 cells   0 mismatches
    positive control               3 faulted rows, 3 reported
    verify.sh stage (probe db)     50 models   148 rows    664 cells   0 mismatches

Three corrections the build made to the plan: the armed leg must arm the port
itself (a shell never runs the addon's post_load for its own database, and the
first leg compared Python with Python -- refused, correctly, by the compare);
a fault on the create alone is overwritten by the per-row string writes before
the readback, so the control rides `update_rows` too; `parent_path` spells the
row's own id and is excluded. 253 of 410 models are skipped with their reason
in the dump -- unique constraints on computed defaults, required fields with
no fillable value, validation and check constraints -- a generator worklist,
not a port question.

**The battery it was added to had not run in full since 2026-09-13, and the
full run found two things nothing else had** (engine `7452d97`):

- the three battery binaries (`phase1_shell`, `phase2_tests`, `phase2_verify`)
  had been running on psycopg since the db shim's layer learned to be off
  (`caabfb4`): `install()` without `set_active(True)`, under a line saying
  "all connections are rust-backed". Two of the three passed that way. The
  registry sweep was the one that did not, at 0 routed shapes;
- **cursor parity read ONLY-RUST 11 against a baseline of 0, and the rust leg
  aborted the interpreter**: `tcache_thread_shutdown(): unaligned tcache chunk
  detected`, 2 runs in 3, under the fork's real-race test. The fork's cursor
  contract had moved 2026-09-14..17 (pipeline arming, savepoint refused in a
  block, session reset and liveness probe on libpq's simple query, permits on
  dead backends) and the shim's `pgconn` was the shared adapter connection's
  handle -- so every cursor return reset a connection nobody used, and six
  returning threads drove one libpq PGconn at once. No `unsafe` anywhere in
  the engine; the corruption was libpq's, reached through a Python attribute.
  Four defects in all (the reset seam, pipeline mode as a nullcontext, a
  terminated backend raising the socket's error instead of the server's
  FATAL, a dropped cursor pinned by its own error's `__context__` traceback);
  write-up in `agromarin-knowledge/research/2026-09-18-rust-cursor-parity-regressions.md`.

    cursor parity   ran psycopg=387 rust=387   both=20   ONLY-RUST 11 -> 0
    race test       3 of 3 clean (was SIGABRT 2 of 3)
    verify.sh       every stage OK on rustorm_fe_probe (mail, contacts)

A baseline of zero is a contract, and this is what it buys: eleven
regressions named on the first run after the fork moved, one of them memory
corruption that no assertion would have caught.

## A thirty-minute prefork burn-in per leg, routing on (2026-09-19)

`harness/burnin.sh --workers 4 --threads 16 --seconds 1800 --sample 0.05` on
`rustorm_fe_burn` (139 modules with demo: mail, crm, project, sale, calendar),
engine `e09f172`, odoo `b39df298cdf2`:

    leg   requests    req/s   p50     p95     p99     errors  routed   verified  divergences
    off   1,434,247    797   19.7ms  26.7ms  30.4ms   0
    on    2,762,956   1535   10.1ms  13.6ms  17.8ms   0      284,190  12,032    0

    BURNIN OK   284190 reads routed, 0 fell back to python, 500s=0,
                routing-bugs=0, answers-changed=0; no worker recycled

Every worker held a 0.97 routed share for the whole leg. The req/s ratio
understates the gain: with `--sample 0.05` one read in twenty is answered
twice. This is the first multi-worker run longer than five minutes with the
mode on, and the first since the cursor-contract repair of 2026-09-18; the
soak stage in `verify.sh` drives the kernel's own HTTP server, not odoo-bin,
so it could not have measured this. Still open: hours rather than minutes,
and a write-heavy profile -- the bench profile is reads.

## A write profile for the burn-in, and the port under prefork (2026-09-19)

`http_bench.py --profile writes` cycles create / write / read / unlink on
`crm.lead` plus one company search, per client thread, so nothing
accumulates and the "on" leg drives the persistence port's `create_rows` and
`update_rows` under prefork rather than only the routed reads. The addon's
periodic report gains a `rust port:` line, because the first run could not
say whether one create had gone native. 4 workers, 8 threads, 300 s per leg:

    leg   requests   req/s   p50      p99      errors   routed   verified  divergences
    off    85,205    284    25.8ms   63.2ms    0
    on     82,708    276    26.6ms   66.3ms    0       37,275   1,844     0

    port, summed over the workers' last reports (on leg; four HTTP workers
    at ~14,000 creates and ~9,300 updates each, two cron workers with the rest):
      create_rows 60,360   update_rows 46,039   native   500s=0   fallbacks=0

Reads-and-writes throughput is flat between the legs, as expected: a form
save's cost is the flush machinery, the mail thread and the access checks,
and the port replaces only the INSERT and the UPDATE. What the run
establishes is narrower and was unmeasured: thousands of native creates and
updates per worker under load, with the fork's own crm and mail overrides in
the path, and nothing raised. The first attempt of this run wrote `phone`,
a field this fork's `crm.lead` no longer has, and read 25% request errors on
BOTH legs -- a profile bug, and a reminder that a write bench's errors are
first suspected of the bench.

## The routing gain measured with the verify sample off (2026-09-19)

`burnin.sh --workers 4 --threads 16 --seconds 120 --sample 0`, three
profiles, on a 7-module demo database (mail, crm, project, sale, calendar);
engine `33f65fb`, odoo `dab5239bd19d`. Same server binary and conf, restarted
between legs, warm-up discarded, 0 errors on every leg.

    profile   leg   requests   req/s   p50      p95      p99      routed    gain
    default   off    97,696     814   19.1ms   25.8ms   29.7ms
              on    210,357   1,753    8.8ms   11.1ms   12.9ms   274,944   2.15x, p50 -54%
    heavy     off    71,191     593   26.3ms   33.2ms   37.0ms
              on    172,479   1,437   10.8ms   13.6ms   15.3ms   227,683   2.42x, p50 -59%
    writes    off    33,018     275   56.0ms   87.8ms  101.1ms
              on     36,023     300   51.2ms   83.0ms   96.0ms    18,863   1.09x, p50 -8%

The write profile is a form save: create, write, read, unlink. Its cost is
the flush, the mail thread and the access checks, all still Python; the port
replaces the INSERT and the UPDATE and buys nine percent. The two read
profiles are what the engine owns, and the gain is the whole request
including HTTP, session, dispatch and JSON on both sides.

## Phase 3 opens with a profile, and the first obvious move measured zero (2026-09-19)

`cProfile` over 200 form-save cycles on `crm.lead` (create, write, read,
unlink, flush), in process, pure Python, on a 4-module demo database:

    27 ms per cycle, of which
      driver round trips (26 statements)             17%
      domain optimisation (`optimize_full`, 5,700 calls)   14%
      orm/models (the write pipeline)                 14%
      orm/fields (field access, compute dispatch)     11%
      python computes (mail thread, crm)              27% cumulative
      16 internal `search()` calls per cycle         40% cumulative

**Arming the port's native `search` changes none of it.** `search_bench.py`
reads native 0.88x Python per search, so it was armed and the cycle measured
again: 4,001 of 4,025 internal searches answered natively, 28.3 ms against
28.1. The fork's `_search` runs `domain.optimize_full(self)` BEFORE it calls
`backend.search`, so the 14% of domain work stays in Python whichever side
compiles the SQL, and the port adds a JSON hop for the rest. Reverted; the
port arms `create_rows` and `update_rows` as before. The milestone that
would move a save is therefore **the kernel compiling the raw domain before
Python optimises it** -- what the routed `search_read` already does -- which
is a fork API question (`_search` asking the backend first) and not a port
switch. Worth about a quarter of the cycle by this profile; the rest is the
write pipeline and the computes, which is Phase 3 proper.

Two defects the attempt found are kept:

- **the kernel build deadlocked on itself once the port could search.**
  Building the kernel exports the registry; the export reads the company
  and the company-dependent fallbacks through the ORM; those reads reached
  the port, which asked `_ensure_kernel` for the kernel being built, under
  a lock the same thread held. One second of CPU in twenty minutes, every
  connection idle. `building()` marks the thread for both build paths (the
  addon's registry hook and `_ensure_kernel`) and a read from it is served
  by Python;
- **every unlink tainted the transaction as a security write.**
  `ir.default.discard_records` runs `stale.unlink()` on every unlink of any
  model, and an EMPTY `ir.default` unlink marked the cursor dirty, after
  which every routed read and every port search in that transaction fell
  back to Python. Measured: 4,385 of 4,463 port searches delegated for that
  reason in the profile; in a server, any request that unlinks anything
  lost routing for its remainder. An empty recordset writes nothing and no
  longer taints.

## Phase 3, first milestone landed: the kernel compiles the raw domain (2026-09-19)

The fork's `_search` now asks `backend.search_raw` before `optimize_full`
(odoo `0cba695f8c52`), and the port answers it from the kernel, armed. Same
profile as above, 200 form-save cycles on `crm.lead`, in process:

                              python     engine      
    per cycle                 27.8 ms    24.2 ms     -13%
    domain work per cycle      4.0 ms     1.4 ms
    internal searches                    4,001 of 4,025 compiled in the kernel

Arming the port's `search` on the optimised domain had measured zero the
same morning; the difference is only WHERE the port is asked. The battery's
search stage now reads 2,991 native and matched (was 2,701) with 80
delegated, and the gate with the A/B differential is green on it.

What is left of a save, from the same profile: the driver's 26 round trips,
the write pipeline in orm/models, field access and compute dispatch in
orm/fields, and the Python computes -- Phase 3 proper, in that order of
size.
