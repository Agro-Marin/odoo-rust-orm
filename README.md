# odoo-rust-orm — Rust Odoo ORM kernel, read path

Proof of concept for reimplementing the Odoo ORM kernel in Rust, targeting
**exact behavioral compatibility** with Odoo 19 (verified by shadow-diffing
against the Python ORM on a real database) and **performance**.

## Scope (M0 + M1)

- **Registry bootstrap from the database itself**: models, fields, `_order`,
  relational metadata, related-field paths, `ir.default` fallbacks, security
  data (groups, `ir.model.access`, `ir.rule`) — all from `ir_model*` tables
  cross-checked against `information_schema`. 305 models / ~8k fields +
  security in ~26 ms (Python registry: ~750 ms).
- **Domain → SQL compiler** replicating `odoo/orm/fields/_field_sql.py` and
  `domain/optimizations.py` v19 semantics:
  - falsy-value null-equivalence, `OR IS NULL` on negative operators,
    `not in` null handling, `=?`, empty-list optimization
  - **dotted paths** (`country_id.currency_id.name`) as id-subselects
  - **`any` / `not any`** with nested domains, m2o NULL semantics
    (`col IS NULL OR col NOT IN (...)`)
  - **unaccent parity**: the registry probes for a 1-argument `unaccent`
    exactly as `odoo/modules/db.py` does, and wraps both sides of ilike
    comparisons when present (plain `like` stays accent-sensitive, matching
    `_field_sql.py`)
  - **`child_of` / `parent_of`** via `parent_path` prefix matching when the
    walked link is `_parent_name`, else iterative BFS — including the
    self-referencing m2o case (result keyed on `id`, walking the field)
  - **related fields** expanded in domains, and as correlated scalar
    subqueries in SELECT / ORDER BY (fixes e.g. ordering by `res.users.name`)
  - **display-name searches**: `('partner_id','ilike',v)` and
    `('display_name','ilike',v)`, compiled the way Odoo compiles them --
    `_optimize_relational_name_search` moves the negation onto the `any` and
    keeps the inner operator positive, `_search_display_name` expands it into
    the model's `_rec_names_search` (or `[_rec_name]`). Compilable for 87 of a
    base+mail database's 181 models; the rest refuse, and the export says
    which is which. A DIRECT `display_name` leaf keeps its negation on the
    operator instead, because there is no `any` to move it onto and
    `NOT (col ILIKE ...)` drops the NULL rows that `col NOT ILIKE ... OR col
    IS NULL` keeps
  - many2one values filtered through the comodel's ACL **and record rules**,
    exactly as `Many2one.convert_to_read_multi` does: an unreadable target
    reads `False`, not `[id, name]`, while the name of a readable one is still
    rendered with `sudo()`
  - **ordering** as `_order_to_sql` does it: `NULLS FIRST/LAST` clauses,
    many2one chains to any depth (each hop joined from the previous alias, with
    Odoo's seen-set breaking cycles), DESC reversing the comodel's own order,
    the no-join shortcut for a comodel ordered by `id`, and `COALESCE(col,
    FALSE)` on a nullable boolean
  - **company-dependent fields** (`COALESCE(col->'cid', to_jsonb(fallback))`
    with `ir.default` fallbacks) in reads, filters and ordering
  - translated fields via `->> lang` (context lang validated vs `res_lang`,
    `COALESCE` fallback to en_US)
- **Security**: `ir.model.access` read checks and **record rules**: a small
  Python-expression evaluator for `domain_force` (literals, lists, dotted
  `user.…` chains resolved through stored/related m2o hops, `company_ids`),
  Odoo's combination semantics (AND globals, OR matching group rules),
  comodel rules injected into any-subqueries and x2many reads like
  `_search` does. Resolved rule ASTs cached per (uid, company, companies),
  and dropped when Odoo's `orm_signaling_*` watermark moves. A rule the
  evaluator cannot compile makes its model **refused**, never served
  unrestricted — see Known gaps. A field's `groups=` spec is evaluated exactly
  as `res.users.has_groups` does (a `!` token denies, `.` denies everyone, and
  negations alone allow whoever matches none of them), and a subquery runs the
  comodel's **ACL** before its rules, as `comodel._search` does. Rules are
  compiled for the models a request can REACH, not for all of them, and a model
  the reachability walk misses is refused rather than read as unrestricted.
- **Read methods**: `search_read` (m2o `[id, display_name]`, ordered x2many
  id lists — rule-filtered, **archived corecords excluded** unless the field's
  own `context` says otherwise, carrying the field's `domain=` and, for a
  one2many over a `many2one_reference`, its model-name filter, like Python),
  `search_count`,
  `_read_group` with **multiple groupby**, **date granularities**
  (`date:month/quarter/year/day` via `date_trunc`), aggregates
  `__count`/`sum`/`avg`/`min`/`max`/`count`/`count_distinct`, and m2o group
  labels rendered through the same access-checked path `search_read` uses.
- **Transport**: CLI (`query`, `run-corpus`, `bench`, `inspect`) and an axum
  HTTP server (`serve`, POST `/call`) over a pool whose connections own their
  prepared statements and are replaced when a backend goes away. Requests carry `uid` / `su` / `lang` /
  `allowed_company_ids` / `active_test`; default is **admin non-superuser**
  (rules apply). `dispatch` refuses a request carrying a parameter the chosen
  method does not implement, rather than answering a different question.

## Correctness: shadow-diff harness

`harness/corpus.json`: 263 cases across 16 models — every operator family,
all M1 features, security-sensitive models (`res.partner`, `ir.filters`,
`res.users.log`), each base shape swept across **superuser** and
**`active_test=False`**, plus explicit coverage for ordering chains,
translated fields and x2many field-level domains. The same corpus runs through
the Python ORM
(`odoo-bin shell`) and the Rust kernel; `diff.py` compares recursively.

**Vary the identity, or the corpus verifies one point.** The first 78 cases
set none of `uid` / `su` / `lang` / `allowed_company_ids` / `active_test`,
though `Request` accepts all five and every one changes the answer. Record
rules at a second identity — the headline feature — were checked by nothing,
and three fail-open defects lived there: `_inherits` parent rules were never
applied, self-referencing `any` subqueries dropped the comodel's rules, and
`_active_name` was hardcoded to `active`. The sweep found a fourth on its
first run (boolean `ORDER BY` needs `COALESCE(col, FALSE)`, or Postgres sorts
NULLs first under DESC).

**What the sweep generates was itself audited, 2026-08-31, by comparing what
the corpora emit against what `sqlgen.rs` compiles.** Three families were
implemented and compared by nothing:

| family | before | after |
|---|---|---|
| `child_of` / `parent_of` | **0 cases** in the 12,146-case sweep; 3 hand-written, plus ~2 per fuzz seed | 224 cases over 32 models, **68 substantive comparisons** over 13 |
| `not =like`, `not =ilike`, `=ilike` | `not =`-family in **no corpus at all**, `=ilike` in three hand-written cases | in the fuzzer's operator lists; 34 substantive comparisons per 1,200-case seed |
| `sum` `avg` `min` `max`, `count_distinct` | 12 hand-written cases over **3 models**; `count_distinct` **nowhere** | **833 substantive comparisons over 465 models** |

None of them found a defect, which is the outcome to hope for and not the
reason to have looked: an operator the kernel implements and no lane compares
is surface nobody is checking, and `child_of` is not compiled to SQL at all —
`resolve_hierarchy` walks the tree and rewrites the leaf as an `in`, so a walk
that stops one level early returns a plausible row set.

`diff.py` reports three things a plain pass count hides. A kernel **refusal**
is separated from a **wrong answer**: A refusal is
the designed fail-closed path — the shim catches it and runs the Python
original — and counting it beside a wrong answer, which nothing downstream can
recover from, makes the two indistinguishable.

It also separates **both sides erroring** into the two different things that
hides, and this is the part that was still wrong until 2026-08-31. Both sides
raising `AccessError` is real agreement, on the property this kernel most needs
to get right: it denied exactly where Python denied. Both sides raising
anything else — a model this database does not have, a field the case names
wrongly — is a case that never ran. Only the first is a pass. Counting them
together, and counting both inside PASS, is how a corpus stops testing anything
while its headline keeps climbing: the 867-model sweep read `PASS 8953/12146`
while **6,037 of those cases had produced no value on either side**. It now
reads

```
PASS 8884/12146  (COMPARED 2916, DENIED 5968)  REFUSED 3193  VACUOUS 69
```

— 2,916 comparisons of actual values, 5,968 agreed access denials, 3,193
kernel refusals the shim falls back from, and 69 cases that proved nothing.
The three partition the corpus, and `score()` is unit-tested in
`harness/test_shims.py` because this classification decides what every other
number in this file means.

And an identity can be named portably. `"uid": "other"` resolves on both sides
to the lowest active user that is neither OdooBot nor admin, so a case at a
non-admin identity runs on any database — a hardcoded uid that does not exist
makes both sides error, i.e. passes vacuously.

**Result: 275/275 with nothing failing**, re-verified 2026-08-28 against THREE
databases built from this workspace's `tpl_p314o19marin` template — base-only;
base+mail+account+uom; and **agromarin + enterprise** (129 modules, 790 models,
222 ruled models, 15,674 fields). All runs use a live-registry export.

**And that number depends on the DATA, not only on the database.** Case `c26`
reads `res.partner.tag_ids` and has passed everywhere; archive one tag and
it fails, in all three of its identity variants, because a x2many read used to
keep archived corecords. `diff.py` guards the database's identity and its DML
drift; nothing guarded that the database still CONTAINED the shapes the corpus
depends on. `harness/sweep_corpus.py` seeds them — see the battery below.

**Two of those lanes are not the same lane.** The registry sweep compares
routed-Python against original-Python, so every comparison it makes travels
through `rust_orm_shim` — which sends `groupby_labels=False`, gates out every
model whose read path is Python, and refuses several parameter shapes. It is
evidence about the HYBRID and says nothing about `odoo-poc query`,
`run-corpus` or `serve`. The shadow corpus IS kernel-direct, and is 275
hand-written cases over 16 models. The kernel sweep is the third thing —
kernel-direct and broad, at admin and at a seeded non-admin identity — and it
found two fail-open defects on its first run: a field with `groups=` read from
its column, and a subquery traversing into a model the reader has no access to.

The whole-registry sweep on the real database compares **19,554 query shapes,
14,264 of them routed to the kernel, with zero mismatches** across 785 models.
It is the run that matters: it found the only invalid SQL the kernel has
generated (a company-dependent field could not be grouped by, because its
bound parameter was emitted twice) and a rule domain the evaluator refused over
a `#` comment.

On the `ir_model` bootstrap the same corpus is **206/267 with 53 refusals and
zero wrong answers**: that registry cannot see an x2many's field-level domain,
so it declines rather than reading more rows than Python would. `Registry::
source` is what makes the difference sayable in code.

The harness kept catching real spec errors during development — v19's m2o
`read_group` ordering (raw id, not comodel `_order`), the self-referencing
m2o `child_of` semantics, `not in` null handling, int4 parameter width —
which is the methodology's point: Python Odoo is the executable spec.

It also showed that a corpus result is only as good as the database behind
it. The original result was obtained on a database without the `unaccent`
extension; on any database that *has* it (every one created from the
workspace template) the same corpus was 77/78, because Odoo wraps ilike
comparisons in `unaccent()` whenever the function exists and the kernel did
not. Re-run the corpus against a database whose extensions match the target
deployment, not just any database.

One command runs everything this file claims:

```sh
cargo build --release --workspace
harness/verify.sh --db <any odoo database>          # or --quick to skip the sweep
harness/verify.sh --db newdb --build base,mail,account   # create it first
```

| stage | what it proves |
|---|---|
| fork contract | every Odoo symbol the shims, the addon and the harness import or patch still exists in the checkout -- no database, no extension, seconds |
| shim units | the two Python shims, with no database: dsn parsing, the kill switch, datetime round trips |
| registry export | the export describes this database (it refuses to write one that does not) |
| shadow corpus | 275 cases byte-equal to the Python ORM, across identities and contexts |
| kernel sweep | every model, kernel-DIRECT, on a fixture it seeds itself |
| fuzz | seeded random domains, values sampled from the columns |
| registry sweep | every model, every identity, ~25k query shapes |
| hybrid (phase 1) | the real Odoo registry boots and its ORM runs on Rust connections |
| upstream suites | routing changes no upstream test result |
| cursor type layer | 20 Postgres types round-trip identically to psycopg |
| concurrency | several identities interleaved across threads, no cache cross-talk |
| replay | captured browser traffic fed back through the shim in shadow mode: routed share and divergences per (model, method); SKIP without a capture file |
| soak | sustained load against `serve`: every answer still right, RSS flat, still healthy after |

Exit code is 0 only if every stage that RAN passed; a stage that cannot run
(no Odoo, no venv) is reported `SKIP` and does not fake a pass. A stage reads
`diff.py`'s SUMMARY line rather than its last line, which used to be the detail
of a refusal — the designed fail-closed path reported as a failure — and the
registry sweep judges its MISMATCH lists rather than requiring every corpus
case to have run, because the corpus covers modules a given database need not
have and one of those raised out of the whole stage. Before this,
each stage was a remembered command line — which meant the results were
reproducible only by whoever had just run them, and CI could run none of it.

The individual pieces still work on their own:

```sh
echo "exec(open('harness/gen_expected.py').read())" | \
  .../odoo-bin shell -c "$RUSTPOC_ODOO_CONF" -d "$RUSTPOC_DB" --no-http
./target/release/odoo-poc run-corpus --file harness/corpus.json > actual.json
python3 harness/diff.py expected.json actual.json
```

Both sides stamp the database they ran against, and `diff.py` refuses to
compare baselines across databases — the unaccent lesson, enforced rather
than remembered. **An unstamped baseline is refused too.** The guard used to
read `if exp_db and act_db and exp_db != act_db`, which short-circuits to
"compare anyway" when either side carries no stamp; the pre-stamp list format
therefore sailed through it. Unstamped is comparable to nothing, not to
everything.

## Configuration

Nothing is pinned to one machine or one database. Every path derives from
`RUSTPOC_WORKSPACE` (default `/home/marin/Odoo`) and each value has its own
override: `RUSTPOC_DB`, `RUSTPOC_DSN`, `RUSTPOC_PGHOST`, `RUSTPOC_PGUSER`,
`RUSTPOC_ODOO_ROOT`, `RUSTPOC_ODOO_CONF`, `RUSTPOC_VENV`, `RUSTPOC_VENV_SITE`,
`RUSTPOC_HARNESS`. See `kernel/src/config.rs`. The `pythonX.Y` path component
is discovered, not hardcoded, so an interpreter upgrade doesn't break the
embedding bins, and `odoo_conf()` falls back to the only `*.conf` at the
workspace root when the venv is renamed.

Four tests assert the defaults resolve to something **usable** — `odoo_root()`
holds an `odoo-bin`, `odoo_conf()` is a real file, `venv_site()` contains
`psycopg`. Three of them had drifted to directories that never existed under
this layout, and nothing noticed because no test had ever looked at a default.

## Performance

267 cases, 20 iterations per run, on **agromarin + enterprise** — 129 modules,
790 models, and real volume (172k `res.partner`, 164k `mail.message`, 314 MB).
Python timed at the ORM layer (warm caches, `invalidate_all()` between
iterations); Rust timings include full JSON serialization. Both sides run the
same corpus through the same case runner (`harness/cases.py`), so neither is
timed doing less work. Median of 3 independent runs per side:

| | |
|---|---|
| median speedup | **3.01×** |
| Rust faster in | 252/257 cases |
| range | 0.79× … 125× |
| big wins | mail.message reads 68–125×, company-dependent filters 75× |
| losses | small `search_count` where fixed cost dominates (worst 0.79×) |
| registry + security load | ~170 ms for 950 models |

**Quote these with their error bars.** A single benchmark process is not
reproducible: identical code re-run drifts **41%** per case at the median on
the Python side and 14% on the Rust side — page cache, ORM cache fill order,
PG plan state. `speedup.py` takes the per-case median across runs and prints
the observed drift, so the number comes with its uncertainty.

```sh
python3 harness/speedup.py --python py1.json py2.json py3.json \
                           --rust r1.txt r2.txt r3.txt
```

The previous figure in this file (2.13×) was measured on `rustpoc_probe`, a
7-partner database that no longer exists, and carried the caveat that fixed
per-query cost dominated it. It did: on real volume the same corpus is 3.01×,
and the shape is different — the wins are where a scan actually happens.

### The hybrid, concurrently, over HTTP

This is the number M3 actually rests on, and until `rust_engine` existed it
could not be taken: every figure above is single-threaded and below the web
stack, and there was no way to put the kernel inside a real `odoo-bin` at all.

`harness/http_bench.py` drives JSON-RPC against a running prefork server
(`workers = 2`, 8 client threads, 20 s), restarting it between modes so
neither inherits the other's caches, discarding a warmup, running A/B/A/B, and
checking every response against a serial baseline taken before anything is
timed. Two fixtures, deliberately: 16 partners, where the read is nearly free
and fixed request cost dominates, and the same database after 120,009 partners
were created through the ORM.

| fixture | routing off | routing on | throughput | p50 |
|---|---|---|---|---|
| 16 partners | 1080 req/s, p50 6.74 ms | 1165 req/s, p50 6.20 ms | **+7.9%** | **−8.0%** |
| 120k partners | 250 req/s, p50 29.5 ms | 269 req/s, p50 27.5 ms | **+7.6%** | **−6.7%** |

Both workers reported **83–85% of read decisions routed, 0 errors**, and every
response matched the baseline byte for byte.

### Burn-in

`harness/burnin.sh --db <db> --password <admin pw>` reproduces this. It is not
a battery stage — it takes twenty minutes — and it exists because the numbers
below have to be re-derivable by someone who is not the person who first ran
them. It restarts the server for each leg, gives BOTH legs the same warm pass,
reads RSS and connections from the pids that actually serve, and refuses to
report OK if the routing leg routed nothing.

Five minutes of sustained load through the addon, `workers = 4`, 16 client
threads, with `rust_engine_verify_sample = 0.05` so one routed read in twenty
is ALSO answered by Python and compared while it serves:

| | |
|---|---|
| requests | 162,184 in 300 s (541 req/s), p50 28.9 ms, p99 52.0 ms |
| routed | **139,340 reads**, 83–84% of decisions, 0 errors |
| verified in flight | **6,843 double-executions, 0 divergences** |
| 500s / routing-path exceptions | 0 / 0 |
| connections | 7 → 11, bounded |

**Memory is Odoo's, not the kernel's.** Under identical protocol — same boot,
same 30 s warm pass, same 300 s measured leg — a worker grows **+38.4 MB with
routing off and +38.1 MB with it on**. The growth converges to ~253 MB either
way; it is the ORM's cache fill, and the kernel adds nothing measurable to it.
An earlier reading of "+80 MB with routing on" was against workers that had
not had the warm pass, which is the same asymmetry this file warns about
everywhere else.

**A module install is survived, with a real install rather than a simulated
signal.** `barcodes,digest` installed by a separate `odoo-bin -i` while the
server served traffic: both workers rebuilt their kernel from the reloaded
registry, **181 models -> 194**, and then answered `digest.digest` and
`barcode.rule` — models that did not exist when the kernel was built — at
`share=0.90, error=0`. Zero stale refusals, zero request exceptions, and the
answers were correct throughout. Both halves of the mechanism are in place and
the faster one wins: Odoo's `check_signaling` reloads the registry (which fires
the addon's hook) before the kernel's own `RegistryStale` check trips, so in
practice the kernel is rebuilt rather than ever refusing on a stale map.

**Workers recycle and come back routing.** `limit_request` defaults to 65,536,
so a worker is replaced after about two of these legs — not on memory, and
there is no memory message in the log. The replacements built their own
kernels through `KERNEL_FACTORY` and resumed at `share=0.83, error=0` without
anyone doing anything.

**Concurrency narrows the margin.** The same A/B protocol gives **+7.6%** at 8
threads over 2 workers and **+2.3%** at 16 threads over 4. Both are honest and
neither is "the speedup": past some concurrency the bottleneck is not the read.

**The per-query 3.01× does not appear end to end, and should not be expected
to.** A request pays for HTTP, session handling, routing, dispatch and JSON
serialization whether its rows come from Rust or from Python; the kernel
replaces one term in that sum. That the gain is ~8% in BOTH regimes is the
useful part of the result: on 120k rows these shapes still return bounded row
counts (`limit` 200/500, an indexed `like`), so a 59 KB response body is doing
more work than the scan is. A workload whose reads are genuinely large is where
the per-query figure would start to show, and this benchmark does not contain
one -- quoting ~8% as "the speedup of the kernel" would be as wrong as quoting
3.01×.

**These are single-threaded numbers.** `bench` loops in one thread on one
connection, which is the right shape for comparing per-query cost and the
wrong shape for the hybrid. Until 2026-08-28 the PyO3 boundary held the GIL
across every database wait — `RustConn::execute` took a `Python` token and
called `block_on` under it, and `RustKernel::dispatch` is a `#[pymethods]` fn,
so PyO3 held it for the whole dispatch. Under `workers = 0`, which is what
this conf uses, every Odoo request is a thread, so the hybrid had no
concurrency at all. Measured with four threads on four separate connections
and a 0.4 s server-side sleep:

| | before | after | psycopg |
|---|---|---|---|
| cursor path, 4 threads | 1.611 s | **0.403 s** | 0.404 s |
| kernel dispatch, 4 vs 1 thread | 3.69× | **1.34×** | — |

The serial floor was 1.60 s. `py.detach` now wraps every wait. The table above
is unaffected — `bench` and `serve` are pure Rust and never take a GIL — and
the concurrent measurement of the hybrid this paragraph used to say did not
exist is the section above. Reproduce the GIL numbers with
`./target/release/probe_audit gil` and `probe_audit kernel <export.json>`.

**Quote these with their error bars.** A single benchmark process is not
reproducible on a small database: identical code re-run drifts ~19% per case
at the median (Python side) and up to 75% at the tail — page cache, ORM cache
fill order, PG plan state. `speedup.py` takes the per-case median across runs
and prints the observed drift, so the number comes with its uncertainty.

Two reasons this is lower than the 3.9× recorded for M1 on the original
database, both legitimate:

1. **Scale.** `rustpoc_probe` is small (7 partners), so fixed per-query cost
   dominates and the interpreter overhead Rust avoids is a smaller share. The
   big wins survive exactly where they should — rule-heavy, many-row reads.
2. **The old comparison timed Python doing less work.** `bench_python.py`
   called `_read_group` without resolving many2one group labels, while the
   kernel returns `[id, display_name]`. Any m2o groupby therefore looked like
   a kernel loss (this was the recorded "one loss, c11 0.5×" — it is ~1.1×
   once both sides produce the same payload). Fixed; the benchmark now
   resolves them as `gen_expected.py` does.

The M1 observation still holds: turning on real Odoo semantics widens the gap,
because Python pays per-call rule application and domain optimization in the
interpreter while the kernel compiles cached, pre-resolved rule ASTs into the
same SQL. Caveat kept honest: mail.message's Python `_search` overrides
contribute to its slow side, and the kernel does not replicate model-level
Python overrides.

What made the difference, cumulative: prepared-statement cache (tokio-postgres
re-prepares by default), per-identity rule AST cache, typed binary params, and
keys written from precomputed fragments rather than allocated per row.

**The statement cache is bounded and invalidatable, and both matter.** A bound
list is one parameter per value, so `IN (...)` past Odoo's own
`IN_TO_ANY_THRESHOLD = 100` becomes `= ANY($1)` over one array: five queries at
lengths 3, 5, 7, 3, 5 used to prepare three statements, and 70,000 ids did not
report "too many parameters" — they wrapped the protocol's int16 count and the
server answered `invalid message length: parameters is not drained`. A plan
invalidated by DDL is dropped and retried instead of failing for the life of
the process, and `_Prepared.clear()` in the shim now routes Odoo's
`discard_cached_plans()` to the Rust caches instead of reporting success and
doing nothing.

**Record rules compile for the models a request can reach.** A `search_count`
on `res.company` at a ruled identity used to compile all 34 ruled models
(2.39 ms); it compiles 1 (0.03 ms). On agromarin + enterprise the eager set is
222.

**Where the time actually goes, measured.** The largest case in the corpus --
`res.partner` filtered by `email not ilike`, 165,888 rows, 7.3 MB of JSON --
spends **157 ms of its 196 ms in the query itself**. Decoding and serializing
every row is the other 39 ms, 0.23 us/row. Rows are decoded into a per-cell
`Json` before serialization (this file previously claimed there was no
intermediate value tree; there is one, per cell, because the many2one and
x2many post-passes rewrite cells in place). Removing it is worth at most 20% of
a case that size and nothing at all on a small read, so it stays.

These predate the lazy rule compilation and the statement-cache work below,
and are left as measured rather than adjusted: both can only have moved them
in one direction, and the number to quote is one somebody re-ran.

**Read the numbers with the cache scope in mind.** The table above comes from
`bench`, which builds one `Orm` and loops inside it. The statement cache used
to live on `Orm`, so the HTTP server and the PyO3 bridge — which build one per
request — re-prepared every query and never saw those numbers. The cache is
now owned by the connection (`StmtCache`, handed to `Orm::new`), which is also
where Postgres scopes prepared statements, so every caller sees them. Measured
on the probe database, reusing it saves **0.22 ms/query, ~67%** of a small
read:

```sh
cargo test --release -p odoo-kernel --test stmt_cache_bench -- --ignored --nocapture
```

Each dispatch also runs one signaling check (Odoo does the same once per
request), ~0.03 ms on a cached statement, and one savepoint so that a kernel
failure leaves the transaction usable for the Python fallback.

The connection-scoped statement cache only pays if connections survive. In the
hybrid they did not: the shim's `give_back` closed the connection, so Odoo's
one-connection-per-cursor became one *connect* per cursor and a cold statement
cache every time. There is a real free list now — `phase1_shell` reports 5
borrows, 1 connect, 4 reused.

Measured savepoint cost: **0.031 ms** for `SAVEPOINT` + `RELEASE` (2.3× a
trivial `SELECT 1`) — 10–15% of a small dispatch, negligible on a large one,
and well under the 0.22 ms/query the connection-scoped statement cache saves.
Nested-subtransaction pressure does not apply here: Postgres assigns XIDs
lazily, and 200 read-only savepoints in a row left the transaction with no
XID assigned at all, so the 64-subxid cache is never touched on the read
path.

## Architecture

```
kernel/src/registry.rs   ir_model* / live export + information_schema +
                         security -> Registry; unaccent probe; the Dynamic
                         snapshot (security + defaults + signaling watermark)
kernel/src/domain.rs     JSON prefix-notation domain -> AST
kernel/src/security.rs   domain_force expression evaluator + rule combination
kernel/src/sqlgen.rs     AST -> sea_query Condition (exact _field_sql.py
                         semantics), ExprCtx (lang/company), related
                         subqueries, order parsing
kernel/src/orm.rs        the engine: env resolution (uid/company/active_test),
                         access checks, signaling, per-identity rule cache,
                         StmtCache, hierarchy resolution, dispatch,
                         domain -> Condition
kernel/src/scan.rs       the reader: search_read/search_count/read_group,
                         display names, x2many, wire decoding. The seam is the
                         Condition -- scan.rs decides nothing about access,
                         orm.rs decides nothing about wire format
kernel/tests/            DB-free compilation tests + statement-cache benchmark
server/src/              odoo-poc CLI + axum; connection-owned StmtCache
engine-py/src/           PyO3 boundary: cursor (psycopg3 seam), registry
                         export, kernel bridge
engine-py/python/        the cr facade and the ORM routing shim
harness/                 corpus + Python shadow harness + diff + benchmarks
```

Runtime-mutable state is deliberately narrow: `Registry.dynamic` holds
security, company-dependent defaults and the signaling watermark behind one
atomically-swapped `Arc`, so a refresh cannot publish security that disagrees
with the watermark it was read at.

## Rebuilding the databases the numbers are measured on

Every figure in this file is measured on one of two fixtures, and until
2026-08-31 neither was written down: both were built by hand and existed only
on the machine that built them. `harness/fixture.sh` builds them.

```sh
harness/fixture.sh --db rustpoc_scale_lmmg  --scale            # what verify.sh needs
harness/fixture.sh --db rustpoc_audit_lmmg  --volume           # what the benchmarks need
harness/fixture.sh --db probe --scale --modules barcodes,bus   # bounded, for testing the loop
```

`--scale` is a convergence LOOP and not one `-i`, because a single install of
every uninstalled module does not install them all — the loader converges on a
subgraph and exits 0 with a large remainder. `--volume` creates its rows
through the ORM so the stored computes and indexes are a live database's, then
`VACUUM ANALYZE`s, because otherwise the first benchmark measures a planner
with no statistics. Both create the database THROUGH Odoo rather than with
`createdb`: the conf names a `db_template` whose collation and extensions
differ from `template1`'s, so a database made outside Odoo sorts differently
from every database made through it.

## Deploying it

`addons/rust_engine` is the deployment. Until it existed the kernel could only
be driven the inverted way -- a Rust binary embedding CPython, or a script
piped into `odoo-bin shell` -- and neither is a server. The addon is
server-wide rather than installable: it has no models, no views and no data,
and it patches `ConnectionPool` and `BaseModel` for the whole process, so
there is nothing for `-i rust_engine` to install.

```ini
[options]
addons_path = ...,/path/to/odoo-rust-orm/addons
server_wide_modules = base,web,rust_engine
rust_engine_db = mydb              ; the ONE database to route
rust_engine_mode = shadow          ; on | off | shadow  (default off)
rust_engine_report_seconds = 300   ; log the routed share; 0 is silent
```

`engine_py.so` has to be importable -- `PYTHONPATH`, or installed into the
server's environment.

Three things about that shape are worth stating because each of them was a
defect first:

- **The kernel is built from the LIVE registry**, not from an `export.json`.
  `engine_py.export_registry(registry)` runs against the registry serving the
  requests, so the metamodel cannot drift from it, and a registry reload -- a
  module installed or upgraded -- rebuilds the kernel through the same path.
- **It is built in the process that serves, not the one that installs.**
  `post_load` runs in the prefork MASTER and the workers are forked before any
  registry exists, so a kernel built from the master's registry is built after
  the fork that would have carried it. The shim takes a `KERNEL_FACTORY` and
  calls it in whichever process first reaches the gate.
- **Enabling it for one database leaves the others alone.** Connections for
  any other database are delegated to the psycopg pool they would have had and
  the gate declines them, so this is a per-database decision on a server that
  hosts many.

Measured on a real prefork server (`workers = 2`), driving `search_read` and
`search_count` over JSON-RPC: two workers reporting `routed=6 share=0.86` and
`routed=7 share=0.88`, `error=0`, and in shadow mode `shadow_ok=14
shadow_diff=0`. The same four HTTP responses are byte-identical with routing
off and on.

## `web_search_read`

The web client does not call `search_read`; every list, kanban and pivot goes
through `web_search_read`, which takes a **specification** per field instead of
a field list, renders many2one values as `{id, display_name}` or a bare id, and
returns `{"length": n, "records": [...]}` with `length` decided by Odoo's own
rule from the page size, the limit and `count_limit`. None of that needs a new
kernel method: it is `search_read` plus a translation on each side, and that is
what the shim routes.

The translation refuses -- and Python answers -- every shape it cannot express
exactly: a many2one asking for any sub-field beyond `display_name` or carrying
a `context`; an x2many asking for sub-fields, `order`, `limit` or `context`; a
`reference` or `properties` field with any spec at all; a field the model does
not have (Odoo drops those silently, the route declines instead); and a model
that overrides any of web's own read hooks (`web_read`, `_web_read`,
`_format_web_search_read_results`, `_screen_fields_spec`, the two resolvers),
because a kernel answer would skip the override. The `length` rule is Odoo's
verbatim: a page shorter than the limit is its own length, a full page needs a
count capped at `count_limit`, `force_search_count` in the context always
counts, and a count limit already reached by the page is the length. The count
is the kernel's `search_count` with the same limit.

What the route does NOT settle is the one policy the fork itself leaves open
(the workspace's known-defects entry on `web_read`'s many2one degradation): for
a many2one target the user cannot read, Python's `web_read` serves
`{id, display_name}` through `sudo()` while the kernel's `search_read` serves
`False`, the way `read()` does. The two disagree there by design of each side,
and shadow mode is where that disagreement shows up as a divergence rather than
as a wrong answer.

Verified by `harness/test_shims.py` for the pure halves (the record
translation, the `length` rule, the spec planner's accept/refuse table) and by
the "load into odoo" stage in shadow mode for the whole path: three requests
against `res.country` -- a page under the limit, a full page whose length
needs a count, one whose count limit is reached -- all routed, byte-equal to
Python.

## Capturing and replaying real traffic

Every corpus above is one somebody wrote or generated. Real traffic is the
one kind of test no corpus can be: the web client's own reads, with the
specifications, contexts, orders and page sizes a browser actually sends.

```ini
rust_engine_capture = /path/to/capture.jsonl   ; or RUSTPOC_CAPTURE in the env
```

With that set, the addon wraps `odoo.service.model.call_kw` and appends one
JSON object per served read -- model, method, args, kwargs, uid, context -- for
`search_read`, `web_search_read`, `search_count`, `read`, `web_read`,
`read_group`, `web_read_group` and the two name searches, on the bound database
only. It sits on the service function rather than on a controller, so every
dispatcher is caught, and it sees the call before `context` is popped from
kwargs, so the replay can rebuild the same environment. Writes are never
recorded: a replayed write would change the database the next call reads. A
capture that cannot be written is logged and never fails the request.

```sh
RUSTPOC_REPLAY=/path/to/capture.jsonl harness/verify.sh --db <db>
```

`harness/replay.py` feeds the file back through the shim with routing in
shadow mode, so each call is answered by Python AND by the kernel and every
divergence is logged, and reports per `(model, method)` how many calls were
routed, compared equal, diverged, or were declined at the gate -- the routed
share of real traffic, which is the number a rollout is decided on. The stage
is `SKIP` without a capture file, never a silent pass, and `FAIL` on any
divergence.

Measured on the 1,569-module fixture: a threaded server with
`rust_engine_mode = on` and the capture armed, `http_bench.py` on two threads
for 8 s through `/web/dataset/call_kw`, then the capture replayed:

| model.method | calls | kernel dispatches | equal | diverged | gate |
|---|---|---|---|---|---|
| res.country.search_read | 21 | 22 | 22 | 0 | 0 |
| res.country.web_search_read | 17 | 34 | 17 | 0 | 0 |
| res.currency.search_read | 21 | 21 | 21 | 0 | 0 |
| res.partner.search_count | 20 | 20 | 20 | 0 | 0 |
| res.partner.search_read | 55 | 37 | 37 | 0 | 18 |
| res.partner.web_search_read | 18 | 0 | 0 | 0 | 18 |

152 captured, 152 replayed, 0 divergences. "Kernel dispatches" counts kernel
requests, not calls: a full `web_search_read` page costs a read and a count,
which is why `res.country.web_search_read` shows 34 for 17 calls. The
`res.partner` shapes declined at the gate all ask for `company_id`, and
`res.company` computes its display name in Python -- the same refusal the
corpus audit above attributes 15% of all refusals to.

And from a real browser -- Odoo's own JS tours (`/crm:TestUi`,
`/web:TestFavorite`, `/mail:TestMailActivityChatter`) run in a process with
the addon armed, so the capture is what the web client sent, specifications,
contexts and all:

| model.method | calls | kernel dispatches | equal | diverged | gate | error |
|---|---|---|---|---|---|---|
| crm.lead.web_read_group | 9 | 1 | 1 | 0 | 0 | 9 |
| crm.stage.search_read | 4 | 4 | 4 | 0 | 0 | 0 |
| ir.module.module.search_count | 4 | 4 | 4 | 0 | 0 | 0 |
| ir.module.module.web_read_group | 3 | 3 | 3 | 0 | 0 | 0 |
| ir.module.module.web_search_read | 5 | 0 | 0 | 0 | 5 | 0 |
| mail.activity.type.search_read | 4 | 4 | 4 | 0 | 0 | 0 |
| crm.lead.web_read | 8 | 0 | 0 | 0 | 8 | 0 |
| res.partner.web_read | 2 | 0 | 0 | 0 | 2 | 0 |
| ir.filters.web_read | 2 | 2 | 0 | 0 | 2 | 0 |
| res.partner.web_name_search | 1 | 0 | 0 | 0 | 1 | 0 |

42 captured, 42 replayed, 0 divergences, routed share 0.43 of the 42 --
0.17 on the first replay, 0.31 once the kernel served `display_name` as a
requested field, 0.55 of 29 once the shim stopped gating the browser's
group order and the capped count and the kernel learnt when a many2one
groupby follows the comodel's `_order`, then 0.44 of 41 once `read` was
routed and the twelve `web_read` calls entered the denominator, and 0.43
of 42 once `name_search` was routed and the last call entered it. Of those,
the `crm.lead` and `res.partner` forms ask for Python computes and decline
at the gate, and the two `ir.filters` reads are of a record the tour
created and rolled back: the kernel returned nothing for it, and a
shortfall is a fallback, never an answer. That share is the honest reading of where the kernel stands
against a browser. `web_read_group` needs no route of its own -- web builds
it on `_read_group`, which is routed -- but what is left declines for real
reasons: `web_read` and `name_search` have no route yet; the Apps list asks
for `icon` and `icon_flag`, non-stored computes; and every `crm.lead`
grouping aggregates `company_currency`, a Python compute in this fork, with
the multi-currency `sum_currency` and `array_agg_distinct` functions the
kernel does not have. Those nine errors are the business-logic wall, refused
correctly. The same run found the two cursor defects recorded in the commit
that added this table: an empty list parameter declared `text[]`, and the
shim's psycopg side connection opened with the rust dialect; with the addon
armed the tours had lost a whole test class to them, and now match their
no-addon baseline.

**The first two replays found two defects, neither in the route.** The bench
sent `args: [[]]` on every call, an empty positional no browser sends, and
never looked for a JSON-RPC error body: `search_read` is not `@api.model`, so
`call_kw` read that `[]` as record ids and the call worked by accident, while
`web_search_read` is and got `domain` twice -- reported as `errors: 0`. With
the bench honest, the next replay caught the shim itself: its replacements for
`search_read`, `search_count` and `create` carried none of the originals'
decorator stamps, so on any server with the shim installed a browser-shaped
`search_read` (`args: []`) answered *requires record ids as its first
positional argument*. Every earlier HTTP measurement had passed on the strength
of the bench's wrong argument. The shim now restamps each replacement with the
original's `_api_model`, `_api_private` and `_readonly`, and `test_shims.py`
asserts them.

## Rolling it out

`RUSTPOC_ROUTE` decides what the shim does with a call it could route:

| value | behaviour |
|---|---|
| `on` (default) | route it, fall back to Python on any refusal |
| `off` | route nothing; the kernel is loaded and idle |
| `shadow` | run BOTH, compare, log any divergence, and return PYTHON'S answer |

`shadow` is what makes a first deployment defensible: real traffic becomes the
comparison corpus -- the one kind of test no generated corpus can be -- at no
risk to the answer, for the price of a duplicated read. It is a rollout mode,
not a resting state. It found the truncated datetimes described above on its
first run, in a shape every serialized corpus had normalised away.

`shadow` is not a resting state and `on` verifies nothing, so turning shadow
off replaces continuous verification with none: from that moment the kernel's
answer is the answer, and a divergence in a shape no corpus covered has
nothing left to catch it. `RUSTPOC_ROUTE_SAMPLE=0.01` (`rust_engine_verify_sample`)
is the middle -- in mode `on` it answers one call in a hundred from BOTH
engines and compares them, for 1% of a duplicated read. A sampled call returns
PYTHON'S answer, which makes the sample a safety net rather than telemetry:
when the two disagree the user gets the right rows and the log gets the
divergence.

**Stopping it does not need a restart.** `set_mode()` reaches one process and
a prefork server is several, so turning routing off across a running server
used to mean restarting it — a poor answer for a component whose whole
argument is that it fails closed, because failing closed per call is not the
same as being able to stop. Two `ir.config_parameter` rows override the
configuration file and every worker picks them up within one tick:

```sql
UPDATE ir_config_parameter SET value = 'off' WHERE key = 'rust_engine.mode';
```

`rust_engine.mode` takes `on|off|shadow` and `rust_engine.verify_sample` a
fraction; an absent key changes nothing, and an unparseable one is logged and
ignored rather than obeyed. Measured on a live `workers = 2` server: all three
processes reported `mode=off` within a tick, the routed counter froze at 29
across ten further calls, and the answers stayed correct throughout. Read by
its own psycopg connection, not through `Registry.cursor()` — with the db shim
installed that cursor is rust-backed, and an off switch must not depend on the
thing it turns off.

`RUSTPOC_ROUTE_ONLY` and `RUSTPOC_ROUTE_EXCEPT` scope it to named models;
`RUSTPOC_ROUTE_BREAKER=N` stops routing a model after N kernel errors, per
MODEL rather than globally, so one bad shape does not take the rest with it.
`rust_orm_shim.stats()` reports what routing actually did, including the routed
share and a per-model error map, and `set_mode()` changes the mode without a
restart.

## Serving

`uid` and `su` arrive in the request BODY, so an unauthenticated caller could
name any identity it liked — `{"uid":1,"su":true}` returned superuser rows, and
nothing in the code said whether that was intended. Two modes now, and the
server says which one it is in:

| `RUSTPOC_SERVE_TOKEN` | behaviour |
|---|---|
| set | requests must carry `X-Rustpoc-Token`; a caller that presents it may name an identity, because it had to be told the secret |
| unset | `uid`/`su` in the body are **ignored** and every request runs as uid 2, non-superuser — the server cannot be used to impersonate |

This is a PoC transport, not a session layer: it authenticates the *caller*, not
a user, and there is no login, no cookie and no CSRF story. It is enough that
the demo cannot be pointed at anything and asked for superuser.

Each request runs in one read-only `REPEATABLE READ` transaction
(`dispatch_in_transaction`). Without it a `search_read` was 2 + m + x implicit
transactions — the signalling probe, the scan, one `display_names` per many2one,
one `x2many_ids` per x2many — so the rows and the names rendered for them could
come from different instants. The hybrid does NOT take that path: there the
transaction is Odoo's, opened by `RustConn::ensure_tx`.

The per-identity caches are bounded (`MAX_IDENTITIES`), evicting superseded
watermarks first — those are dead weight, since no request can reach them
again. Before that, a long-lived process serving many users in many companies
grew without bound, and a database whose watermark never moves grew forever.

## Observability

Nothing is on by default. `RUSTPOC_LOG` takes a standard `tracing` `EnvFilter`
string; `POC_TRACE=1` remains an alias for `odoo_kernel::sql=debug`.

| target | what it reports |
|---|---|
| `odoo_kernel::dispatch` | one span per request, then ok/refused with duration — and the **reason**, which is the line that explains a routing miss |
| `odoo_kernel::sql` | statement, rows, params, whether it had to be prepared, ms |
| `odoo_kernel::rules` | per-identity compile: models seen, restricted, refused, hierarchy queries, ms |
| `odoo_kernel::signal` | a security refresh, or a registry bump refused |

The whole-registry sweep in `phase2_verify` runs at **every identity it can
find** — admin, admin-superuser, and any other active user, preferring a share
user — because one identity is one point of the space that decides the answer.
It compares **25,348 query shapes** that way on agromarin + enterprise (17,375
routed to the kernel, zero mismatches); the portal identity is what caught the
many2one disclosure below.

The probe shapes aim at what has broken rather than at breadth: the model's own
`_order` and that order reversed, a company-dependent field as both a filter and
a groupby, an x2many as a traversal and not only a read, two groupby terms. Every
probe that limits rows orders by a key that fully determines the sequence —
`website.page._order` is `website_id`, five of its six rows have it NULL, and
comparing two engines on an arbitrary permutation reports a mismatch that is
nothing at all.

In the hybrid these go into **Odoo's own logger** under
`odoo.rust_kernel.<target>` (`engine-py/src/logbridge.rs`), so
`--log-handler odoo.rust_kernel.rules:DEBUG` behaves like any other Odoo
logger. `eprintln!` to a worker's stderr reached no logfile and carried no
level, which is why a refused model — i.e. a silent fallback to Python — used
to be invisible in a running system.

## The cursor's type layer

Every Odoo statement goes through `RustConn`, and its conversion layer is the
part with the least natural coverage — the shadow harness exercises the
*kernel*, not the cursor. `probe_audit types` writes and reads 20 Postgres
types through both the shim and psycopg and requires the same Python object,
both directions:

```sh
PYTHONUNBUFFERED=1 ./target/release/probe_audit types   # 64 checks
```

Its first run found nine defects, one of which is the shape that matters most:
`cell_to_py` handled `int4[]`, `text[]` and `varchar[]` and let every other
array fall through to a String attempt that failed into `py.None()`, so
`int8[]`, `bool[]` and `float8[]` columns read as SQL NULL **with no error**.
The fallback now raises rather than inventing a NULL.

## The write path the shim owns

The kernel is a read engine and every write goes to Python -- with one
exception that is easy to miss. `COPY_THRESHOLD` in
`odoo/orm/models/mixins/_crud_common.py` is **10**, so any `create()` of ten
or more records is sent as a binary `COPY`, and with the db shim installed
that stream is encoded by `RustCopy`: the PGCOPY header, a big-endian int16
field count per row, an int32 length prefix per value, `-1` for NULL, a
trailer. A wrong byte in any of those does not raise. It shifts where the
server thinks the next value starts, and the row lands in the table wrong.

`harness/copy_path.py` is differential against psycopg, because a corrupted
write is equally corrupt on both sides of a read comparison: the same values
are created twice, once through each cursor, and the stored rows are compared
column by column by an independent connection. Fifteen cases chosen for where
a length-prefixed encoder goes wrong -- the empty string (length 0, which is
not NULL), unicode, a 70 KB value that forces a mid-row flush, both booleans,
zero and negative integers, dates at both ends of the range, a float that is
not binary-representable -- across varchar, text, bool, int4, date, numeric,
the timestamps Odoo writes itself, and jsonb, which is how this fork stores
every translated field.

It also PROVES THE PATH RAN, by counting the COPY streams the shim opened. A
create that fell below the threshold, or fell back to `INSERT`, would pass
every comparison in the file while exercising nothing -- the same shape as the
dsn defect that left the kernel armed and unused.

## Why the routed share is what it is, and why widening it was declined

**Read the refusals with the right denominator.** A first pass at this counted
every case where the KERNEL errored, found 62% of them to be access denials,
and concluded the gap was mostly cases nobody could answer. That was wrong:
those are cases where **Python errored too**, which is agreement on an access
decision, not a refusal. Restricted to what a refusal actually is -- the kernel
declined and **Python answered** -- 3,193 cases on the 867-model database:

| share | reason |
|---|---|
| 65.7% | display_name is computed or searched in Python |
| 14.6% | a subquery's target model defines `_search` in Python |
| 11.3% | the model overrides the read path |
| 4.3% | a traversal through a non-stored field |
| 2.9% | record rules that could not be compiled |
| 0.7% | an access decision |

So the gap is real and it is display_name. It is also **concentrated in two
models**: `res.users` is 66.6% of those refusals and `res.company` 15.4%, with
a 116-model tail behind them.

Both of those genuinely run Python, which is what settles it.
`res.users._search_display_name` **executes a search on `login` and rewrites
the domain to `('id','in', ids)`** — a query, mid-compile.
`res.company._search_display_name` branches on `user_preference` being in the
context, and its compute is `company.code or company.name`, which SQL can
render exactly.

The declarative surface behind them is small. Of **334**
`_compute_display_name` definitions across four repos, **3** are a plain column
expression; of **76** models declaring `_rec_names_search`, **60** also
override a display_name hook, leaving **16** the kernel could compile from the
declaration alone. The one pattern with real volume is the **67** computes
built from an f-string over columns, and rendering those means reproducing
CPython's `str()` in SQL — an unset many2one is `False` and renders as
`"False"`, a float by `repr`, a date by its own `__str__`. A mismatch there is
not an error; it is a wrong label shown to a user.

Against a measured ~8% end-to-end gain, widening coverage buys a fraction of
8%. The reason to decline is not that the work is hard: it is that the
correctness argument would rest on imitating an interpreter rather than on a
proof, which is the opposite of this kernel's posture. `res.company` is the one
case where a proof is available, and it is one model.

## Known gaps

What is NOT on this list any more, because it was closed: the binary COPY
encoder (differentially tested against psycopg), running under `workers > 0`
(the tokio runtime and the connection free list both survive the fork now),
serving a host that has more than one database (others are delegated to
psycopg rather than broken), turning routing off without a restart
(`ir.config_parameter`), and the absence of a concurrent end-to-end
measurement.

- **Python model overrides** (`_search`, `_compute_display_name`, computed
  fields): invisible to a registry built from the DB alone. This is the
  fundamental M2 problem — the business-logic layer. Fields declaring
  `search=` are detected via the live-registry export and refused; the
  `ir_model` bootstrap cannot see them, so it is not a safe primary source —
  `serve` takes `--export` and warns loudly when it has none.
- **25 of 222 ruled models on a real database** cannot have their rules
  compiled and fall back to Python — `account.move`, `account.move.line`,
  `sale.order`, `sale.order.line`, `hr.employee` among them. Every one is now
  the same cause: a rule interpolating a **non-stored Python compute**
  (`res.users.crm_team_ids`, `res.users.employee_id`,
  `res.company.country_id`). The first two also declare `search=`, so both the
  value and the domain leaf need Python. This is the measured size of the
  business-logic problem, not an estimate.
- Rules the mini-evaluator cannot compile make the model **refused**, not
  silently served without its rules: queries error and the routing shim falls
  back to Python. The evaluator handles `+` on lists and resolves
  `user.all_group_ids` / `user.group_ids` from the security snapshot (they are
  the transitive group closure it already computes). What remains unsupported
  is rules traversing non-stored computed fields (`discuss.channel.is_member`)
  — 3 models on a stock 77-module database, down from 8.
- `web_search_read`, `read` (and through it `web_read`) and `web_read_group`
  (through the routed `_read_group`) and `name_search` (and through it
  `web_name_search`) are served; the session layer is not; no writes/computes **by
  the kernel** — but see "The write path the shim owns" above: the db shim
  encodes every bulk `create()` of ten or more records as binary COPY, which
  is a write path even though the kernel is not on it, and it is now covered
  differentially against psycopg.
- Security staleness is detected through Odoo's signaling tables, so it
  inherits Odoo's own contract: a change written by **raw SQL** bumps no
  signal and is invisible until restart — exactly as it is to a second Odoo
  worker. Changes made through the ORM are picked up on the next dispatch,
  verified against a live server for `ir.rule`, `company_id` and
  `company_ids` writes. (Test this only through a path that calls
  `registry.signal_changes()` — `service/transaction.py` does, a bare
  `cr.commit()` in `odoo-bin shell` does not, and mistaking the second for the
  first makes the mechanism look broken when it is not.)
- A **registry** signal is reported, not absorbed. `refresh_dynamic` rebuilds
  security and stamps the new watermark, so routing a module install through
  it consumed the signal while `models` / `langs` / `has_unaccent` stayed
  stale — and the process never noticed again. The vector is partitioned by
  table, and a registry bump raises a typed `RegistryStale`. **The holder then
  rebuilds**: both `serve` and the embedded kernel replace the model map, drop
  the caches derived from it, and retry the request once. Refusing without
  recovering was correct and useless — every later request failed until
  someone restarted the process.
- `phase2_tests` drives the upstream suites with a bare
  `unittest.TextTestRunner`, not Odoo's runner, so `assertQueries`-style
  assertions behave differently (`assertQueries` no-ops when `self.warm` is
  false). One upstream test (`test_rec_names_search`) fails under it on the
  *stock* psycopg stack too, so it is a harness artifact rather than a kernel
  or cursor defect — left visible instead of waived.
- `RustConn::copy` implements binary `COPY FROM STDIN` (the format v19's bulk
  create requests). Text/CSV format, `ON_ERROR`, and a custom writer are not
  implemented — nothing in the read or create path asks for them.
- A `search_read` with no `fields` is refused. Odoo returns every readable
  field there; enumerating them is a `fields_get` this kernel does not have.
- An inequality against an unset value is refused. Odoo's answer is
  type-dependent -- `('id','>',False)` and `('write_date','<',False)` both
  optimize to FALSE while `('name','>',False)` is kept and compared -- and a
  kernel that guesses at that is wrong on every row of whichever case it
  guessed wrong.
- A model whose `_rec_name` IS `display_name` cannot have a display-name search
  compiled: it would search display_name to answer display_name. Odoo breaks
  that loop in Python (`display_name.no_error`); here it is a refusal, and a
  32-level cap on the compiler makes any other runaway one too rather than a
  stack overflow.
- A Postgres type the cursor has no decoder for -- `geometry` is the one this
  workspace has -- reaches Python as the raw wire BYTES, with one warning per
  type. psycopg does the same for an unregistered type. It is not a parsed
  value, and code that expects one will not get it.
- `RustConn::execute` decodes whole result sets into Python tuples eagerly;
  there is no `fetchmany` streaming path, so a very large scan holds the rows
  twice (Rust `Vec<Row>` + the Python list). Measured on the kernel's own
  path -- 165,888 rows, 7.3 MB out -- peak RSS is **+6.4 MB** over the
  registry-only baseline, so the doubling is real but small at this scale;
  it is a question for a scan an order of magnitude larger, not this one.
- The routing shim no longer models what the kernel supports.
  `_order_spec_clean` and `_domain_clean` are gone: the kernel raises on every
  case they covered, so the fallback happens through the error path instead of
  being predicted in Python where it could drift. What remains genuinely needs
  the Python registry — method overrides and `_compute_display_name`.
- The `ir_model` bootstrap cannot see `read_path_pure`, `display_name_default`,
  a field's `groups=` or its `context`, so it refuses everything that depends
  on them. That is the honest reading of what it knows, and one more reason
  `--export` is not optional in spirit.
- Model-level Python overrides are now REFUSED rather than served: 9 of a
  base+mail database's 181 models define `_search` / `read` / `_read_group`,
  and 26 compute `display_name` in Python. The price is measured -- 32 of the
  275 corpus cases stop routing, all of them `mail.message`, `res.groups` and
  `res.users` -- and it is the same set the shim already declines to route in
  production.

## Next steps (M2 candidates)

1. `web_search_read` + session auth → point the stock web client at it.
2. Replay-based shadow testing: capture real `call_kw` traffic, diff at scale.
3. The business-logic wall: PyO3-embedded Python for model overrides, with
   the Rust kernel as the data engine underneath.
