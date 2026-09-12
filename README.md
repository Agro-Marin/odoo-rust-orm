# odoo-rust-orm — Rust Odoo ORM kernel, read path

Proof of concept for reimplementing the Odoo ORM kernel in Rust, targeting
**exact behavioral compatibility** with Odoo 19 and **performance**. Differential
checks against Python validate the exercised read shapes; they do not establish
full ORM replacement readiness. During coexistence Python remains authoritative
for registry metadata, extension hooks, cache/compute state and write orchestration.
Rust executes eligible reads on the caller's transaction and refuses unsupported
shapes back to Python. Runtime contracts additionally check effects that response
JSON comparisons cannot see.

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
is surface nobody is checking. `child_of` on a `_parent_store` model is kept
in the query as `parent_path =like` prefixes since 2026-09-10 (as
`_operator_child_of_domain` writes it, through `any!` on a relational field);
the other direction and the non-stored case still walk the tree in
`resolve_hierarchy` and rewrite the leaf as an `in`, so a walk that stops one
level early returns a plausible row set.

`diff.py` reports three things a plain pass count hides. A kernel **refusal**
is separated from a **wrong answer**: A refusal is
the designed fail-closed path — the shim catches it and runs the Python
original — and counting it beside a wrong answer, which nothing downstream can
recover from, makes the two indistinguishable. **A database error under the
kernel is not a refusal either.** `run-corpus` stamps every error with a
`kind`: `internal` when the error chain carries a `tokio_postgres::Error` or
the text starts with `db error` (invalid SQL the kernel generated, a dropped
connection), `refusal` for everything else; `diff.py` reports an `internal`
error as a FAIL whatever Python did, and infers the kind from the text for
an actual file written before the field existed. `--json <path>` writes the
buckets (`failed`, `refused`, `skipped`, `rejected`, `denied`, the compared
count and the drift flag) so a stage reads structured output instead of
grepping the headline.

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
PASS 8884/12146  (COMPARED 2916, DENIED 5968)  REFUSED 3193  SKIPPED 69
```

— 2,916 comparisons of actual values, 5,968 agreed access denials, 3,193
kernel refusals the shim falls back from, and 69 cases this database cannot
run. The partitions cover the corpus, and `score()` is unit-tested in
`harness/test_shims.py` because this classification decides what every other
number in this file means.

**A case the database cannot run is decided before either side is asked.**
Until 2026-09-09 "both sides raised" was one bucket, VACUOUS, and the shadow
corpus carried 49 such cases against a base+mail database — every one naming
`account.account`, `uom.uom`, `product.category` or an `account` field on
`res.partner`, so on that database they proved nothing while the headline
read PASS. `gen_expected.py` now walks each case's model and every field path
it names (fields, domain leaves, groupby and aggregate heads, order terms,
dotted paths through their comodels) against the live registry and stamps
`skipped` with the reason; `diff.py` reports those as SKIPPED and **fails the
run if the kernel answers one** — a kernel answering for a model the export
does not carry is a wrong answer, not agreement. What is left of the old
bucket is REJECTED: Python raised for some other reason and the kernel
declined. The fuzz generator produces those on purpose (an inequality on a
boolean, `child_of` on a model with no parent, a non-stored x2many in a
domain), so both sides refusing is agreement that the request has no answer,
and it is reported beside REFUSED rather than inside PASS. The skipped cases
stay in the corpus because a database with `account` and `uom` runs them.

**And the headline said PASS while cases were failing.** `diff.py` printed
`PASS n/N` unconditionally, followed by the FAIL detail and exit 1, and every
stage in `verify.sh` reads the headline rather than the exit code — so a
shadow corpus, sweep or fuzz stage with a wrong answer in it was reported OK.
Measured with a one-case fixture whose two sides disagree: `PASS 0/1`, exit
1, stage OK. The headline is `FAIL n/N` whenever anything failed, and the
stages match either word. The first full battery under the corrected headline
read `fuzz FAIL seed1 seed2 seed3` — eight value mismatches that the PASS
headline had been reporting as OK for as long as the fuzz stage existed.
Three were kernel defects, fixed and pinned in `kernel/tests/pure.rs`:

- `!` over a dotted path flipped the comparison (`country_id.name != 'BE'`),
  which keeps `any` and drops every row whose head is unset. Odoo optimises
  the path into `country_id any [...]` first and negates THAT, so the result
  is `country_id not any [...]` and an unset head satisfies it. Measured on
  `ir.actions.act_window_close`: `['!', ('binding_model_id.transient', '=',
  False)]` is 158 rows in Python and was 3 in the kernel.
- `in` with a scalar wrapped it into a one-element list; Odoo's
  `_optimize_in_set` makes a falsy scalar the empty set, so `('write_uid.share',
  'in', False)` is FALSE. The kernel answered 842 rows where Python answers 0.
  Through a path the collapse belongs to the sub-domain: `('create_uid.share',
  'not in', False)` is `create_uid any [TRUE]`, which still requires the head
  to be set, and the first version of this fix made it TRUE outright — the
  fuzz stage caught that on the next run.
- A `properties` field in a domain was compiled as a plain column; Odoo
  raises "Missing property name". Refused now.

The other five were the fuzz generator's: an `order` on one column with
`limit`, where two equal rows may come back in either order and Odoo appends
no tiebreak of its own. Every fuzz order now ends in `, id`.

**`read_group` takes the whole `order` grammar now, not only the groupby
sequence.** Odoo's `_read_group_orderby` accepts a groupby spec or a
requested aggregate per term, each with a direction and a nulls placement,
and — only when an order is given at all — walks a many2one groupby through
its comodel's `_order`, with `ANY_VALUE()` around the comodel columns so they
need no GROUP BY entry. The kernel refused everything but the bare groupby
sequence, and the sequence case itself was refused at the database whenever
the comodel's order named a translated column (its bound parameters cannot be
matched to a GROUP BY entry). It mirrors all of it now — `__count desc`,
`amount:sum`, `model_id desc nulls last`, `create_date:day_of_week desc` —
and still refuses an order term that is neither a groupby nor a requested
aggregate, which Odoo would compute on the fly. Pinned as corpus cases
`ro1`–`ro8` on `ir.model.fields`, whose `model_id` comodel orders by a
translated name.

**Real web-client traffic, and the gate says why it declines.** The tours
under `mail`, `web` and `base` (Discuss, the chatter, the composer, the
template editor, user settings, favourites, field translation) were run
against a server carrying `rust_engine` in shadow mode: 12 tours green, every
routed call verified against Python, 0 divergences. What the first run also
showed was a routed share of 13 % with 547 declines and no way to say why —
the shim counted `fallback_gate` and threw the reason away. Every gate site
now records `(model, method, reason)` and the periodic report logs the top
eight, so a rollout can read what blocks the traffic rather than guess. On
the tours the top reasons were `discuss.channel.member._read_group:
aggregate id:array_agg`, `res.users.read: load=False` and the display-name
and security-write walls. The first two were shim and kernel gaps, closed in
the same commit: the kernel takes `array_agg`, `array_agg_distinct`,
`bool_and` and `bool_or` (with Odoo's `ORDER BY id`, its sorted-distinct
subselect, and `[]` / `False` for an empty group — but not over a `numeric`
column, whose members reach Python as `Decimal`s that a float array does not
equal: the registry sweep, which compares live Python objects rather than
JSON, caught exactly that on the first battery), orders by an unrequested
`__count` the way `_read_group_select` computes it, and the shim's
`_read_group` gate no longer refuses an order, a limit or an offset the
kernel handles. `read(load=False)` bares many2one ids exactly as
`_read_format` does for any load but `_classic_read`, and a many2one
group-by no longer needs the comodel's display name to be Python-free,
because `_read_group` hands back bare recordsets. On the same tours the last
report before the run ended read a routed share of 18 % with 268 declines,
against 13 % and 547 before, and what remains at the top is the
security-write taint the tests' own user creation causes. Pinned as corpus
cases `ag1`–`ag8`.

**Two companies, not one.** The scratch database carried a single company, so
`allowed_company_ids`, the bound company parameter behind every
company-dependent column, and the `company_id in company_ids` record rules
were compared only where every branch gives the same answer. It has a second
company now (`RUSTORM Second Co`), admin and the portal probe belong to both,
three partners live in it with a company-dependent `barcode` set per company,
and fifteen corpus cases (`cp1`–`cp15`) read, search, order and group that
column under each company selection, at a non-admin identity, and with a
company the user is not allowed in (denied on both sides). The fuzz generator
picks a company selection for a quarter of its cases from the companies admin
belongs to.

**A string among the members of a relational `in` is a display-name lookup.**
`_Relational._optimize_condition` turns `('country_id', 'in', ['Allemagne', 3])`
into `country_id any [display_name in ['Allemagne']] OR country_id in [3]`, and
the negative form into `country_id not any [display_name in [...]] AND
country_id not in [3]` — the sub-domain always carries the POSITIVE operator,
`not any` supplies the negation. The kernel refused a string member outright
("cannot convert 'Allemagne' for int4"), for many2one and x2many alike, and a
test pinned that refusal as if it were the contract. It mirrors the rewrite
now; the first version negated the sub-domain as well and answered the 16
German states where Python answers the 2,115 others, which the French probe
caught before it was committed. Pinned as corpus cases `tr1`–`tr20`, which
also cover searching, ordering, grouping and display-name matching translated
columns in `fr_FR` — for which the scratch database now carries the French
terms (`--load-language fr_FR`); before that every "translated" comparison
ran on the `en_US` fallback branch, where a wrong lookup and a right one read
the same.

**A fuzzer only finds what it generates, and this one had never generated
most of the domain grammar.** Until 2026-09-09 `fuzz_corpus.py` produced
`any` only with an empty sub-domain, `!` only over one leaf at the front of
a domain, dotted paths one level deep, `in` only with a list, `=` only with
a scalar, and never `=?`, never a leaf on `id` or `display_name`, never a
granularity in a `read_group`. It now builds a real tree (`!` over groups,
groups inside groups, nested `any [...]` two levels down, two-level paths
through a many2one), the scalar-where-a-list-is-expected and
list-where-a-scalar-is-expected shapes, `=?`, `id` and `display_name`
leaves including `child_of` on a hierarchical model, `nulls first/last`
orders, `:week`/`:day_of_week`/`:quarter` group-bys with a timezone, and
`sum/avg/min/max/count_distinct` aggregates, two-level group-bys with an
aggregate or group order, and offsets. The first run of the wider
grammar found one more kernel defect: `['!', ('write_date', '<=', False)]`
is TRUE in Python, because `DomainNot` optimises its child (`<= False` on a
datetime is FALSE) and negates the result, while the kernel negated the raw
leaf into `> False OR IS NULL`, which is FALSE too. `negate_leaf` now runs
the leaf through the same optimisation pass first. Pinned in
`kernel/tests/pure.rs`.


And an identity can be named portably. `"uid": "grouped"` and `"uid": "debug"`
name the users the sweep seeds (`rustorm_sweep_grouped`, in two groups whose
rules carry several top-level terms and a `restrict` beside a grant, with a
Mexico City timezone on its partner; `rustorm_sweep_debug`, holding
`base.group_no_one`), resolved by login on both sides, and the `ch_*` cases
run the review's probe corpus at them on every database: restrict
composition, field groups on every path, the debug group, `id > False`, bare
dates in three zones, related fields, NULL group cells, hierarchy. The sweep
also keeps `en_US` active and adds three restricted custom fields on
`res.country`. `"uid": "other"` resolves on both sides
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

On the `ir_model` bootstrap the same corpus is **0/279**, measured 2026-09-09:
`Registry::load` cannot tell which models override the read path, so it marks
every one `read_path_pure = false` and the kernel refuses every dispatch. The
bootstrap is the integrity check M3-PLAN describes and nothing more; `serve`
without `--export` answers nothing. (This paragraph used to claim 206/267 with
53 refusals, a figure from before `read_path_pure` existed.)

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
| replay | the web tours' own traffic (captured by `rust_engine_capture` during that stage; `RUSTORM_REPLAY` names another file) fed back through the shim in shadow mode: routed share and divergences per (model, method); SKIP only when the tours did not run, FAIL when nothing was compared or an unexpected native/shim error occurred; rolled-back tour identities are reported separately and are not counted as replayed |
| runtime contracts | real native connections check cache fidelity, SQL-failure recovery and breaker classification, stale exports, and the registry reload hook; required even under `--quick` |
| web tours (shadow) | the mail, web and base tours run by `odoo-bin` carrying `rust_engine` in shadow mode against the database: FAIL on a failed tour or a live divergence; reports the routed share and the gate count; SKIP under `--quick` or without `mail` installed (`RUSTORM_TOUR_TAGS` picks another set) |
| soak | sustained load against `serve` behind a per-run token: the shadow corpus's baseline (`expected.json`) re-asked at every identity it names, plus self-consistency probes at the seeded `other` identity; RSS growth after a warm-up bounded (`RUSTORM_SOAK_RSS_GROWTH`, default 20 %), still healthy after. `--uids` is refused against a server that pins its identity, because the uids would be silently ignored |

Exit code is 0 only if every stage that RAN passed; a stage that cannot run
(no Odoo, no venv) is reported `SKIP` and does not fake a pass — a script
says so with exit code 3 (`fork_contract.py`, `replay.py`), and any other
non-zero exit is a FAIL. Every stage runs under `timeout`
(`RUSTORM_STAGE_TIMEOUT`, default 1800 s) and expiry is a FAIL. Ports for
the tour server and the soak server are taken by binding one (a python
one-liner prints the first free port in 9401–9449 / 9450–9499), not by
reading `ss` and hoping. `harness/verify.sh --help` prints the header. A stage reads
`diff.py`'s SUMMARY line rather than its last line, which used to be the detail
of a refusal — the designed fail-closed path reported as a failure — and the
registry sweep judges its MISMATCH lists rather than requiring every corpus
case to have run, because the corpus covers modules a given database need not
have and one of those raised out of the whole stage. Before this,
each stage was a remembered command line — which meant the results were
reproducible only by whoever had just run them, and CI could run none of it.

The individual pieces still work on their own:

```sh
echo "import runpy; runpy.run_path('harness/gen_expected.py', init_globals={'env': env})" | \
  .../odoo-bin shell -c "$RUSTORM_ODOO_CONF" -d "$RUSTORM_DB" --no-http
./target/release/odoo-poc run-corpus --file harness/corpus.json > actual.json
python3 harness/diff.py expected.json actual.json --json diff.json
```

The scripts `odoo-bin shell` runs share `harness/_env.py` — the DSN
(`RUSTORM_DSN` with the database substituted, else host/user/dbname, the
same rule as `kernel/src/config.rs::dsn_for`), the harness directory, the
base environment (uid 2, not superuser, `en_US`) and the percentiles — and
find it beside themselves through `__file__`, which `runpy` provides and
a bare `exec(open(...).read())` does not; the `exec` form still works with
`RUSTORM_HARNESS` set, and falls back to the workspace layout. Their
outputs default to a per-process directory under `$RUSTORM_VERIFY_OUT` or
the system temp dir, never a fixed `/tmp/rustorm_*.json` two sessions would
overwrite.

Both sides stamp the database they ran against, and `diff.py` refuses to
compare baselines across databases — the unaccent lesson, enforced rather
than remembered. **An unstamped baseline is refused too.** The guard used to
read `if exp_db and act_db and exp_db != act_db`, which short-circuits to
"compare anyway" when either side carries no stamp; the pre-stamp list format
therefore sailed through it. Unstamped is comparable to nothing, not to
everything.

### Cursor parity: Odoo's own cursor suites, on both cursors

`phase2_tests` cannot gate the cursor, and the reason is structural rather
than an omission: it differences a baseline leg against a routed leg, and
what it toggles is ORM ROUTING. The db shim is installed in BOTH legs, so the
cursor is rust-backed on both sides of the comparison and a cursor regression
lands in the "pre-existing in both modes" bucket the gate ignores. Measured
by adding the module: `test_db_cursor` gives `baseline 44/379 -> routed
44/379`, which reads as clean.

```sh
harness/cursor_parity.sh --db <db>
```

So this puts the CURSOR on the compared axis: Odoo's own `test_db_cursor`,
run twice through the same bare-`unittest` harness -- once with the db shim
installed, once on plain psycopg -- and diffed BY TEST NAME, because a count
reads "one fixed, one new" as no change. Failures that are artifacts of the
runner rather than of a cursor appear on both sides and cancel.

    ran psycopg=378  rust=378
    not-ok both=18   ONLY-RUST=3   only-psycopg=0

**3 of Odoo's own cursor tests fail against the rust cursor and pass against
psycopg**, none the other way -- down from 61 on the first run, and every
drop came from the gate rather than from guessing:

    61  first run
    59  `FakeConnection.execute()` rejected psycopg's `prepare` keyword, which
        `odoo/db/lifecycle.py` uses on the CONNECTION (not the cursor) to
        reset a connection before it returns to the pool
    38  `RustCopy` implements COPY TEXT. It required `set_types`, which Odoo
        does not call for a text COPY, so every text COPY raised
        `0 column types were declared` -- 21 tests, the single biggest cluster
    36  a psycopg `Json` / `Jsonb` wrapper's own `dumps` is honoured. Odoo
        wraps a string bound for a json column in
        `Jsonb(s, dumps=_dump_json_verbatim)` -- "this is ALREADY json text"
        -- and reaching past the wrapper for `.obj` re-encoded the document
        as a json STRING: `jsonb_typeof` read `string` where psycopg gives
        `object`. Asking the wrapper for its own text and parsing that is
        right for every spelling, including `Json("abc")` genuinely meaning
        the json string `"abc"`, which is why the general fix is shorter than
        the special case would have been.
    29  the db shim runs the POOL's accounting instead of bypassing it (below)
    18  the db shim SUPPLIES the pool instead of replacing `borrow` (below)
    11  the prepared-statement cache is invalidated where psycopg invalidates
        it, plus `RustCopy.write`, `Cursor.scroll` and the pool's `_pool`
        stamp
     8  `info.transaction_status`, without which a raising rollback hook also
        cost a warm pooled connection; and a non-UTF-8 bytes query raising
        the server's own error instead of `UnicodeDecodeError`
     3  COPY errors reach the caller as psycopg error classes rather than
        bare `RuntimeError`s; an unencodable parameter raises the server's
        own `22P02`; and a database error carries its DIAGNOSTICS (below)

## `db_maxconn` was not enforced on the rust path

The db shim replaced `ConnectionPool.borrow` and `give_back` at the CLASS
level and returned a rust connection without touching the instance. So none
of what the pool does around the connection ran: not `_budget.acquire()`,
which is what bounds `db_maxconn`; not `_checkouts.track()`, which is what
can name who is holding them when it saturates; not the stats, the leak
warning, or the health probe. Every `ConnectionPool` in the process also
shared one module-global idle list, so a connection borrowed under one
pool's settings could be handed back out under another's.

Measured, `maxconn=2`, six borrows, each driven to a real backend with
`pg_backend_pid()`:

    psycopg   borrowed 2   PoolError at borrow #2   2 distinct backends
    rust      borrowed 6   no error                 6 distinct backends

    after     borrowed 2   PoolError at borrow #2   2 distinct backends

`max_connections` is a hard cluster-wide limit shared with every other
process, so a bound that is not enforced is not a local matter.

**The first instrument answered the adjacent question.** Counting rows in
`pg_stat_activity` from the shell's own cursor reported `opened: 0` for both
legs and would have retired the whole finding -- that count is served from
the transaction's cached stats snapshot, which never moves inside one
transaction. Asking each borrowed connection for its own `pg_backend_pid()`
is the direct instrument, and it is the one that separated 6 from 2.

**The fix closed 8 and opened 1, which is why the count moved by 7.**
Releasing the budget in a `finally` also released it for a SECOND
`give_back` of the same connection, crediting a permit nobody acquired --
`test_double_give_back_does_not_over_release`, a test that was green before.
Odoo's own `give_back` spends a pop-once marker (`_odoo_pool`) for exactly
this reason and the shim now spends one too. A net figure hides a
regression; the gate reports names, which is how this one was seen at all.

## The pool was the seam, not `borrow`

Patching `borrow` meant reimplementing everything the pool does around a
connection. Rebinding `odoo.db.pool._PsycopgPool` to a factory that returns
a rust-backed pool means implementing none of it: `ConnectionPool.borrow`,
`give_back`, the budget, the checkout tracker, the idle reaper,
`close_database` / `drain_database`, the stats and the session GUCs are
Odoo's own code, unmodified. The shim got smaller and closed 11 more tests,
and two of those were production paths nothing else would have found:

- `db_session_gucs` reached no rust-backed connection at all, so a
  deployment's `work_mem` / `statement_timeout` policy was silently absent.
- `drain_all()` iterates `_pools`, which was **empty**, so it was a no-op.
  `registry.py` calls it on every reload -- meaning after any module upgrade
  a pooled rust connection kept prepared plans built against the old schema,
  and the next query through one can fail `cached plan must not change
  result type`.

**THE FIRST RESULT WAS A PERFECT SCORE AND IT WAS MEANINGLESS.** A pool is
built once per dsn and cached, so rebinding the CLASS reaches only pools
created afterwards -- and the process already had one. Every borrow returned
a psycopg connection while the leg still called itself rust, and the gate
reported `ONLY-RUST=0`: psycopg compared with psycopg, in perfect agreement.
The honest number was 18. Both legs of the gate now refuse a rust leg whose
connection is not a `FakeConnection`, and the negative control confirms it
(`VACUOUS: the rust leg drew a Connection`).

**The counter would not have caught it.** The vacuous run reported
`connects=28` -- nonzero, because the tests that construct their own
`ConnectionPool` did get rust pools. A guard on "did the shim do anything"
passes; only "what is this connection" fails. The old design was immune to
this by accident, because patching `borrow` takes effect on pools that
already exist; the better design has an ordering dependency, and paying for
it with a guard is the trade.

## `db_host =` made the engine unable to open a single connection

Measured 2026-09-11 against this workspace's own `p314o19m.conf`, whose
`db_host` is empty: **every** borrow died with `invalid configuration: invalid
number of ports` before it opened a socket, so the engine could not arm at all
and the cursor-parity gate could not produce a leg.

`_dsn_with_kwargs` concatenates the armed dsn with Odoo's own connection
options, on the stated belief that "anything later in the string wins, as libpq
resolves it". libpq does. **tokio-postgres does not**: it ACCUMULATES `host` and
`port` and then requires the two counts to match. Odoo's `connection_info`
carries a port and, when `db_host` is unset, no host — libpq falls back to
`PGHOST` and the socket directory, which is exactly why the addon injects a host
into the armed dsn and Odoo's half has none. One host, two ports, and the
connector refuses.

With `db_host` set both halves carry both keywords, the counts match by
accident, and the identical code connects. That is why this survived: the
defect is invisible on any configuration that names its host.

The composition now resolves the duplicate itself rather than leaving it to a
connector that resolves it differently, so each keyword reaches the connector
once and the last value wins as the comment always claimed. Pinned by
`test_the_composed_dsn_names_every_keyword_once`, which fails against the old
composition with the two-port dsn in its message.

## PostGIS, and one class this transport cannot reach at all

The same question asked of the other extension in this workspace's template.
`geometry` and `geography` were reading back as raw bytes; psycopg has no
loader for either and returns the type's TEXT output, which for these two IS
the hex-encoded WKB -- byte for byte what arrives here in binary, so
rendering it as uppercase hex reproduces psycopg's string exactly. Both now
match.

Two limits found with them, and both are the transport rather than an
oversight:

**`geography` cannot be WRITTEN.** PostgreSQL has an implicit `text ->
geometry` cast and none for geography, and this cursor binds every parameter
in BINARY against the statement's resolved type -- so a text literal cannot
reach the server's input function the way psycopg's untyped parameter does.
`geometry` writes work only because that implicit cast exists. Writing
geography needs an EWKB encoder; it is covered read-only until then.

**`box2d` cannot be READ at all**: *no binary output function available for
type box2d*. tokio-postgres requests binary results for every column and
psycopg negotiates per type, so any type with `typsend = 0` is unreachable
here -- 8 of them in this database, of which `box2d` (returned by
`ST_Extent`) and `aclitem` are the only ones a query is likely to select.
That is a hard limit of the transport, not a missing decoder, and it is
recorded here rather than left to be rediscovered.

## pgvector: every write refused, every read raw bytes

`vector` is not a catalog type -- its oid is per-database, so it is matched
by NAME -- but it is in this workspace's database template and agromarin's AI
modules store embeddings in it. It had already cost this repo once, encoded
as TEXT into a binary COPY (`set_types` carries the scar in a comment). It
was also, until now:

    write   psycopg stores it     rust  42804, "column is of type vector
                                       but expression is of type text"
    read    psycopg '[4,5,6]'     rust  b"\x00\x03\x00\x00@\x80..."

psycopg has no loader for it either, and returns the TEXT form. This cursor
cannot ask for text format, so it decodes pgvector's binary -- `int16` dims,
`int16` unused, then that many big-endian `float4` -- and renders the same
string, and encodes the same format on the way in. Both directions now agree.

**A column type used by shipped modules had NO coverage at all**, and the way
it was found is worth more than the fix: a sweep of every column type present
in the database read `vector(1536)` from a real table and reported agreement.
That agreement was vacuous -- the column has no rows, so both cursors
returned `[]`. An empty result set is not a comparison. The sweep that found
the defect was the one that inserted a value first.

## A `date[]` column read back as raw binary bytes

The cursor's type layer decoded a scalar `date` and a `text[]` and handed
back RAW BINARY for ten other categories -- `numeric[]`, `date[]`, `time[]`,
`timestamp[]`, `timestamptz[]`, `bytea[]`, `oid[]`, `inet[]`, and scalar
`interval` and `uuid`. Not an error: a column of any of those read as
`b"\x00\x00\x00\x01..."`, and the fallback logged that it was handing
back raw bytes "as psycopg does for an unregistered type" -- which is not
what psycopg does for ANY of these. psycopg has loaders for all ten.

    numeric[]  psycopg [1.25, -3.5]              rust b"\x00\x00\x00\x01..."
    date[]     psycopg [date(2026, 9, 9)]        rust b"\x00\x00\x00\x01..."
    interval   psycopg timedelta(days=32, ...)   rust b"\x00\x00\x00\x00..."

Nine of the ten now decode identically to psycopg, including psycopg's own
month-to-30-days convention for `interval` and `uuid` built from its 16 raw
bytes -- neither needs a new dependency, both formats being fixed-width.
`inet` is the one left.

**The write side was guessing where the server knew.** A list of strings was
declared `text[]` from its first element, so inserting `["2026-01-31"]` into
a `date[]` column got 42804, *column is of type date[] but expression is of
type text[]*. Odoo passes dates, timestamps and numerics as strings, so that
is the normal case, not an exotic one. Leaving the parameter UNTYPED lets the
server infer the column's own type, and the element conversions then parse
into it -- the same conversions the scalar arms use.

**And the reason no gate saw any of it**: the type-layer probe's corpus named
`int4[]`, `text[]`, `bool[]` and `float8[]`, and every type it did not name
was uncovered. A type layer is only covered for the types its corpus
mentions. The corpus now carries the array of everything the scalar list
covers, plus a read-only section for the types this cursor can decode but not
yet encode -- 84 checks, and splitting the direction is what lets the gate
hold at zero while still covering the half that works.

## The prepared-statement cache outlived what it was prepared against

psycopg clears its cache when a command's STATUS TAG is `ROLLBACK` or starts
with `DROP ` (`psycopg/_preparing.py`), because a plan prepared against an
object a rollback or a drop removed is looked up internally by PostgreSQL and
fails. `ROLLBACK TO SAVEPOINT` reports the tag `ROLLBACK` and so clears;
`RELEASE SAVEPOINT` reports `RELEASE` and does not. This cursor cleared on
neither, so a savepoint rollback left plans behind.

tokio-postgres surfaces no status tag -- `execute` yields a row count and
nothing else -- so the rule is applied to the STATEMENT instead. The two
agree on everything Odoo issues and differ where a tag would still be
produced but the text does not start with the keyword: a `DROP` inside a DO
block or a function body, or one that is not the first statement of a batch.
Those under-clear, the same direction psycopg errs when a cache is cold, and
the divergence is written down rather than left to be discovered.

## A constraint violation stopped naming the field

`db_err` carried the SQLSTATE and the message and dropped everything else.
Odoo turns a constraint violation into a message naming the offending field,
and it does that from `exc.diag` -- `constraint_name`, `table_name`,
`column_name`, `message_detail` -- in `service/transaction.py`,
`orm/models/mixins/load.py` and `orm/models/mixins/schema.py`. None of those
raised when the diagnostics were absent; each degraded quietly to the raw SQL
text. One cursor test found it; nothing else in the workspace would have.

The diagnostics now travel with the error, and `Error.diag` is a read-only
property built from a live `PGresult`, so the shim raises a per-class
subclass carrying its own. `column_name` still comes back None for a
composite constraint -- which is correct, and is the whole reason Odoo's
`check_registry` lookup exists.

**Two of the three that remain are one unimplemented feature, not two
defects.**
psycopg's PIPELINE MODE defers results, so from the second statement in a
pipeline block `cursor.description` is None until the batch is flushed.
`pipeline()` is a `nullcontext` here, so every statement runs immediately and
`description` is always set. tokio-postgres has no equivalent of libpq's
pipeline mode: this is a feature to build, not a bug to fix.

The third is `set_types`, which now asks the ENCODER whether it could encode
a type rather than asking whether the oid RESOLVES -- the same predicate
`write_row` applies, so the two cannot drift. That took the disagreement with
Odoo's guard from 47 array types to 7: four this cursor cannot encode
(`cidr[]`, `interval[]`, `timetz[]`, `uuid[]`, none of which tokio-postgres
maps) and three it can encode though psycopg will not (`bpchar[]`,
`int2vector`, `oidvector`) -- and those three are the harmless direction,
since Odoo's guard sends them to text COPY and never asks. So **the defect
list is empty**, and what holds the floor is one missing feature and one
narrow type-set disagreement.

The pool classes are the interesting remainder:
`test_raising_prerollback_hook_keeps_the_connection` fails with *a hook bug
must not also cost a warm pooled connection*, which is a behavioural
difference with a cost attached.

**Ratcheted, not zero.** Closing 61 differences is a programme of work;
`harness/cursor_parity_baseline.json` holds the number and the gate matches
it EXACTLY, as the repo's other ratchets do, so fixing one lowers the
baseline in the same commit rather than banking slack. A leg that ran no
tests reports `CURSOR PARITY VACUOUS` and fails rather than passing. And
`test_a_baseexception_during_construction_returns_the_connection` is excluded
from BOTH legs, symmetrically and visibly: it raises a real
`KeyboardInterrupt` on purpose, Odoo's runner installs a handler for that and
a bare one does not, so it killed the whole leg and the suite reported
nothing at all.

### Byte parity: the lane the other lanes cannot be

Every comparison above -- the shadow corpus, the sweep, the fuzzer, the
replay lane, and the shim's own in-flight `shadow` verification -- diffs a
METHOD'S RESULT. None of them can see a difference in what the SERVER SENDS
around it, and on 2026-09-08 that gap had a real defect sitting in it for as
long as the shim has routed `web_search_read` (see §Burn-in). So:

```sh
harness/parity.sh --db <db> --password <admin pw> [--port N] [--models N]
```

It generates one set of RPC-shaped calls per model (`parity_corpus.py`:
`search_read`, `search_count`, two `web_search_read` shapes, `web_read_group`,
`name_search`, `web_read` over frozen ids), boots the server with routing
**off**, records every response BODY, boots it again with routing **on**, and
diffs the recordings byte for byte. `workers = 0`, so a leg's behaviour is one
kernel's rather than an average over however many workers served.

Three things make the OK readable rather than decorative:

- **The off leg is recorded TWICE** and a case whose two baseline passes
  disagree is reported as nondeterministic, not as a divergence. A `LIMIT`
  window over equal sort keys is not a routing defect, and an instrument that
  cannot tell the two apart is not an instrument. (Every ordering the corpus
  emits ends in `id` for the same reason; the double pass catches the rest.)
- **The routed count is checked.** If the `on` leg routed nothing it compared
  Python with Python, and the run reports `PARITY VACUOUS` and fails instead
  of `OK` -- the denominator-of-zero trap, in the one gate where it would be
  easiest to fall into.
- **The answered/raised split is printed.** A case that raises `AccessError`
  on both legs agrees about the access decision and nothing else; a corpus of
  nothing but those would pass while comparing no answer at all.

The JSON-RPC `id` is scrubbed before comparison -- the client chooses it and
the server echoes it -- and an error body is recorded like any other, because
one leg raising where the other answers is exactly what this is looking for.

**It drives ONE identity, the admin, and that is a deliberate scope rather
than an oversight.** What this lane adds over the sweep is the layer around
the answer -- envelope, serialisation, status -- and that layer does not vary
with who is asking; what DOES vary with identity is which rows come back, and
the sweep already runs its whole corpus at a second, portal identity. A
second identity here would double the run for the part that is already
covered. What it would genuinely add is a case where the two legs disagree
about an ACCESS decision, and nothing has been measured about that.

## Configuration

Nothing is pinned to one machine or one database. Every path derives from
`RUSTORM_WORKSPACE` (default `/home/marin/Odoo`) and each value has its own
override: `RUSTORM_DB`, `RUSTORM_DSN`, `RUSTORM_PGHOST`, `RUSTORM_PGUSER`,
`RUSTORM_ODOO_ROOT`, `RUSTORM_ODOO_CONF`, `RUSTORM_VENV`, `RUSTORM_VENV_SITE`,
`RUSTORM_HARNESS`. See `kernel/src/config.rs`; `harness/_env.py` mirrors the
DSN and harness-directory rules for the Python side. `RUSTORM_VERIFY_OUT`
is where every harness artifact goes, `RUSTORM_STAGE_TIMEOUT` bounds each
battery stage, and `RUSTORM_EXPORT_CMD` lets `serve` regenerate a stale
export. The `pythonX.Y` path component
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

The previous figure in this file (2.13×) was measured on `rustorm_probe`, a
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
response matched the baseline byte for byte -- a property the bench checks on
every request, and which caught the routed answer ordering its keys as the
caller asked (Python's `read()` answers scalars first, relational fields after)
and omitting the JSON-RPC envelope version `web_read` stamps; both now match.

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

**The read-heavy profile, measured 2026-09-11** (`RUSTORM_BENCH_PROFILE=heavy
harness/burnin.sh`, or `http_bench.py --profile heavy`: 5000 rows × 12 fields,
2000 rows by name, a whole-table count, two `read_group`s and a 1000-row
`web_search_read`, on 120k partners, 2 workers, 8 threads, 90 s legs):

| profile | routing off | routing on | throughput | p50 |
|---|---|---|---|---|
| default | 122.0 req/s, p50 62.3 ms | 134.5 req/s, p50 55.6 ms | **+10.2%** | **−10.8%** |
| heavy | 2.8 req/s, p50 2051 ms | 3.7 req/s, p50 1694 ms | **+32%** | **−17%** |

**On the whole tree, in the rollout's own setting** (`rustorm_5e_scale`, 1593
modules, 2 workers, 8 threads, 300 s legs, `rust_engine_verify_sample = 0.2`,
2026-09-11): routing off 13.8 req/s p50 586 ms, routing on 16.7 req/s p50
475 ms -- **+21 % throughput, −19 % p50** -- 6207 reads routed, 1097 of them
verified against Python with 0 divergences, 0 errors, 0 changed answers, and
the same RSS drift on both legs (+71 MB off, +85 MB on, over five minutes at
2.3 GB per two workers). That is the setting decision 1B names for staging.

Before the shim revived its rows with `fromisoformat` and warmed the cache in
one recordset walk, the heavy profile read **flat** (3.7 → 3.6 req/s): the
kernel answered the 5000-row read in 16 ms and `json.loads` in 10, then
`datetime.strptime` took 204 ms over 10000 values and the cache warm-up 156.
The per-query figure is real; a shim that re-parses every value in Python is
where it went. What is left in a heavy request is the web layer serialising
1.3 MB of JSON, which routing does not touch.

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

1. **Scale.** `rustorm_probe` is small (7 partners), so fixed per-query cost
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
harness/fixture.sh --db rustorm_scale_lmmg  --scale            # what verify.sh needs (admin/admin stays; --password to change it)
harness/fixture.sh --db rustorm_audit_lmmg  --volume           # what the benchmarks need
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
rust_engine_only = res.partner,res.users   ; route these models only (default: all)
rust_engine_except = ir.attachment ; never route these (wins over only)
rust_engine_breaker = 3            ; Python serves a model after N kernel errors (default 3; 0 disarms)
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

That measurement drives `search_read` and `search_count`, and a burn-in run on
2026-09-08 found what it does not cover. `harness/burnin.sh` had to be
repaired first: it APPENDED `server_wide_modules` to a conf that already
declared one, which configparser rejects outright, so it no longer booted in
this workspace. Once it did, `web_search_read` diverged, and not in its data:

    res.country.web_search_read   off: {id, jsonrpc, result, version}
                                  on : {id, jsonrpc, result}

`records` and `length` were equal element for element and `__version` inside
the result matched; the response ENVELOPE was missing a key. `web_search_read`
carries `@versioned` and stamps `__version` into its own result, which the
shim mirrored -- but Python ALSO stamps `version` on the envelope, and it
gets there as a side effect of the inner `records.web_read(specification)`
call, which carries `@versioned_envelope` and digests the record list alone.
A routed answer never makes that call. **The shadow comparison cannot see
this class of divergence at all**: it diffs the METHOD's result, and the
envelope is not in it -- 2,396 sampled verifications reported `diff=0`
through the whole run. What saw it was the bench's own baseline, comparing
response BYTES; what reported it was nothing, because `burnin.sh` scored only
the server-side counters and printed `BURNIN OK` over 308 changed answers.
Both halves are fixed: the shim stamps the envelope, and a changed answer is
now fatal to the burn-in.

The class is small enough to enumerate, so it was:
`grep -rnE "request\._[a-z_]+ *=" --include='*.py'` over `odoo/` and
`addons/` returns `_response_version` and `ir_qweb_assets`' four
`_esm_import_map_*` / `_esm_page_bundles` writes, and nothing else. The
second group is asset rendering, which no routed read reaches. So this was
the only request-scoped side effect on a read path the kernel serves, and
there is not a second one waiting.

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
rust_engine_capture = /path/to/capture.jsonl   ; or RUSTORM_CAPTURE in the env
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
RUSTORM_REPLAY=/path/to/capture.jsonl harness/verify.sh --db <db>
```

**A capture no longer has to come from somewhere else, which is why this
stage used to SKIP on every run anybody made.** `harness/parity.sh` arms the
capture on its OFF leg -- Python answering the whole parity corpus, one
realistic RPC call after another -- so a full `verify.sh` writes
`$OUT/parity/capture.jsonl` and the replay stage picks it up without a second
boot. `RUSTORM_REPLAY` still wins when it is set, and `--quick` skips parity,
so the stage SKIPs there exactly as before. It is not real users' traffic and
the checklist item that asks for that is still open; it is the difference
between a lane that runs and a lane that has never run.

First run: **2,776 calls replayed, shadow ok=1848, diff=0**, routed share
0.77, gate=1016, error=270. **Read `error` as "the kernel raised", not as
"the kernel failed"** -- the great majority of those 270 are an `AccessError`
that PYTHON RAISES TOO on the same call (`error` and `raised` match row for
row in the per-`(model, method)` breakdown), which is agreement about an
access decision and not a divergence; the rest are the compiler declining a
non-stored field. Zero of them are database errors, which is the number that
would mean something was wrong.

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

The identity-race probe requires the seeded restricted user and checks that
ordinary and sudo counts differ before starting concurrent calls. Equal answers,
worker errors, mismatches or incomplete execution fail the probe; they cannot
produce a successful isolation result. Replay has separate subprocess controls
for a real comparison, missing users, fully gated traffic and native failures.

## Rolling it out

`RUSTORM_ROUTE` decides what the shim does with a call it could route:

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
nothing left to catch it. `RUSTORM_ROUTE_SAMPLE=0.01` (`rust_engine_verify_sample`)
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

`rust_engine_only` and `rust_engine_except` in the conf (`RUSTORM_ROUTE_ONLY` /
`RUSTORM_ROUTE_EXCEPT` for the bins) scope it to named models;
`rust_engine_breaker = N` (`RUSTORM_ROUTE_BREAKER`) stops routing a model after
N unexpected kernel errors, per MODEL rather than globally, so one bad shape
does not take the rest with it. Only typed `KernelRefused` errors (including
`KernelAccessDenied` and `KernelRegistryStale`) are exempt. PostgreSQL failures
arrive as `KernelDatabaseError`; unclassified native failures arrive as
`KernelInternalError`. Both count, as do unexpected Python exceptions. The
addon arms the breaker at 3 unless the conf says otherwise.

A shadow or sampled mismatch separately quarantines that model in the current
process, even when the error breaker is disabled. The current compared call
returns Python's answer; subsequent calls use Python until `reset_breaker(model)`
or a process restart. Other workers maintain their own quarantine. The process
stats expose `quarantined`; operators can use the database-backed mode switch
to stop routing across workers. Equality here checks return values, not every
cache effect; `harness/runtime_contract.py` checks internal relation-cache
fidelity as well.
`rust_orm_shim.stats()` reports what routing actually did, including the routed
share and a per-model error map, and `set_mode()` changes the mode without a
restart.

## Serving

`uid` and `su` arrive in the request BODY, so an unauthenticated caller could
name any identity it liked — `{"uid":1,"su":true}` returned superuser rows, and
nothing in the code said whether that was intended. Two modes now, and the
server says which one it is in:

| `RUSTORM_SERVE_TOKEN` | behaviour |
|---|---|
| set | requests must carry `X-Rustorm-Token`; a caller that presents it may name an identity, because it had to be told the secret |
| unset | `serve` **refuses to start** unless `RUSTORM_SERVE_PINNED_UID=<uid>` names the one identity every request runs as (non-superuser; `uid`/`su` in the body are ignored). It used to default to uid 2 — the administrator on every Odoo database, which reads everything — and called that "cannot be used to impersonate" |

Errors are typed by status so a caller and the soak harness can tell them
apart: 403 for an access denial (the body carries `access denied`), 422 for a
kernel refusal (the shim's fallback path), 500 for a kernel defect — a
database error under the kernel, or a panic, which is caught and poisons the
connection it ran on rather than dropping the socket — 503 when no pooled
connection is available within the acquire timeout (the reconnect of a dead
pooled connection failing is that too, not a dead connection handed out) or
the registry is stale, 504 when the request exceeded its timeout, 400 for a
body axum cannot parse (an unknown key is one: `Request` denies them). Every
error body carries `kind`, `internal` or `refusal`. `/health` reports `auth`
(`{"mode":"token"}` or `{"mode":"pinned","uid":N}`) so a harness can tell
whether the identities it names would be honoured.

The token is compared as two keyed-SipHash digests of fixed size, so neither
a length mismatch nor an early differing byte returns sooner.

**A stale registry is refused until the export is fresh.** When
`orm_signaling_registry` moves, re-parsing the same export file rebuilds the
same stale model map, so the server answers 503 (`/health` reads `stale`)
until the file's mtime changes — or, with `RUSTORM_EXPORT_CMD` set to a
command that regenerates it, runs that command and rebuilds in place, then
retries the request.

This is a PoC transport, not a session layer: it authenticates the *caller*, not
a user, and there is no login, no cookie and no CSRF story. It is enough that
the demo cannot be pointed at anything and asked for superuser.

Each request runs in one read-only `REPEATABLE READ` transaction
(`dispatch_in_transaction`); `bench` takes the same path, so its numbers
include the `BEGIN`/`COMMIT` round trips `serve` pays (`--json` prints them
as a document). A dispatch that returns an error is followed by an explicit
`ROLLBACK` on the leased connection before it goes back to the pool; one
that neither returns nor errors — cancelled by the request timeout, or
unwound by a panic — has its connection poisoned by a drop guard, so the
next request on that slot runs on a fresh connection outside the leaked
snapshot (`kernel/tests/db_probe.rs` pins that guarantee end to end). Without it a `search_read` was 2 + m + x implicit
transactions — the signalling probe, the scan, one `display_names` per many2one,
one `x2many_ids` per x2many — so the rows and the names rendered for them could
come from different instants. The hybrid does NOT take that path: there the
transaction is Odoo's, opened by `RustConn::ensure_tx`.

The per-identity caches are bounded (`MAX_IDENTITIES`), evicting superseded
watermarks first — those are dead weight, since no request can reach them
again. Before that, a long-lived process serving many users in many companies
grew without bound, and a database whose watermark never moves grew forever.

## Observability

Nothing is on by default. `RUSTORM_LOG` takes a standard `tracing` `EnvFilter`
string; `POC_TRACE=1` remains an alias for `odoo_kernel::sql=debug`.

```sh
RUSTORM_LOG=odoo_kernel=debug          # every subsystem, one line per decision
RUSTORM_LOG=odoo_kernel::scan=debug    # one subsystem
RUSTORM_LOG=warn,odoo_kernel::refusal=debug,odoo_kernel::dispatch=debug   # why calls fall back
RUSTORM_LOG=odoo_kernel=debug,odoo_kernel::domain=trace                   # + every leaf rewrite
```

**This surface is temporary.** It was widened in one pass to carry a code
quality, performance and lifecycle campaign, and it comes out again when that
campaign ends — see *Removing the campaign logging* below.

| target | what it reports |
|---|---|
| `odoo_kernel::dispatch` | one span per request, the request shape, then ok/refused with duration — and the **reason**, which is the line that explains a routing miss |
| `odoo_kernel::refusal` | every `refuse!`, with the **source line** that decided it. Aggregated over a corpus this is the capability backlog |
| `odoo_kernel::access` | every `deny_access!`, same shape, plus the ACL and field-level grants at `trace` |
| `odoo_kernel::sql` | statement, rows, params, whether it had to be prepared, prepare and execute ms, cache size |
| `odoo_kernel::rules` | per-identity compile: models seen, restricted, refused, hierarchy queries, ms; per-model outcome and every rule-domain name hop at `trace` |
| `odoo_kernel::signal` | a security refresh, a watermark stamp, or a registry bump refused |
| `odoo_kernel::registry` | load and refresh phases with counts and ms, and the capability line of a model whose read path Python overrides |
| `odoo_kernel::env` | identity resolution: uid, companies, groups, lang, and the two timezones that decide which day a bare date names |
| `odoo_kernel::domain` | at `trace`, every leaf rewrite `optimize_leaf` performs — the layer where a wrong answer is decided |
| `odoo_kernel::compile` | subquery construction, path normalisation, the trigram accelerator, `IN` vs `= ANY`, the company-dependent guard, the ORDER BY join chain |
| `odoo_kernel::scan` | the column plan, and the per-phase ms of the main query, the many2one labels and each x2many |
| `odoo_kernel::hierarchy` | `child_of` / `parent_of`: parent_path prefix match or a row-by-row walk, seeds, memo hits |
| `odoo_kernel::cache` | prepared-statement and per-identity cache evictions |
| `odoo_kernel::connect` | TLS mode, whether the host is authenticated, connect ms |
| `odoo_kernel::pool` | (server) pool fill, waits, saturation, poisoning |
| `odoo_kernel::http` | (server) one line per call with status, kind and ms |
| `odoo_kernel::cursor` | (hybrid) **every statement Odoo's own Python ORM runs**, with the query/decode split |
| `odoo_kernel::copy` | (hybrid) COPY streams: binary or text, rows |
| `odoo_kernel::bridge` | (hybrid) kernel build, generation, staleness, GIL-detached ms per dispatch |
| `odoo_kernel::export` | (hybrid) the live-registry export walk |

Levels are used consistently: `info` is lifecycle, `debug` is one line per
operation or decision, `trace` is per-item (per leaf, per name hop, per
statement). A disabled callsite is an atomic load and a branch, and the field
expressions are only evaluated once it is enabled — a benchmark with
`RUSTORM_LOG` unset is indistinguishable from one built without any of it
(p50 0.130 ms either way over 1,000 calls on base+mail).

### The Python half

`engine-py/python/` and `addons/rust_engine/` log through Odoo's own logger
rather than `tracing`:

| logger | what it reports |
|---|---|
| `odoo.rust_kernel.routing` | mode and sample changes, kernel build, fallbacks |
| `odoo.rust_kernel.gate` | one line per call the gate turned away, with the reason. `GATE_REASONS` is the same thing aggregated; this says *which call* |
| `odoo.rust_kernel.call` | one line per routed call: wire sizes, kernel ms, and the flush and cache-warm the shim does around it |
| `odoo.rust_kernel.pool` | which DSNs are intercepted and which are delegated to psycopg, pool fill, fork handling, drains |
| `odoo.addons.rust_engine` | arming, the connector DSN it built, the periodic routing report |

### Removing the campaign logging

The campaign surface is mechanically identifiable, which is the point:

```sh
grep -rn 'target: "odoo_kernel::' kernel/src server/src engine-py/src   # the rust half
grep -rn '_gate_logger\|_call_logger' engine-py/python                 # the python half
```

Four of the targets predate the campaign and stay: `dispatch`, `sql`, `rules`,
`signal`. The rest, and the `trace`-level events under the four, are the
campaign's and come out with it. `odoo_kernel::refusal` and
`odoo_kernel::access` are produced by the `refusal!` and `deny_access!` macros
in `kernel/src/error.rs`, so removing them is two edits and not a sweep.

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

**Two switches, both have to be open.** `RUSTORM_LOG` decides what the kernel
emits at all; the Odoo logger decides what is then printed. A kernel line goes
missing when either is shut, and `RUSTORM_LOG` unset means `warn`:

```sh
RUSTORM_LOG=odoo_kernel=debug odoo-bin … --log-handler odoo.rust_kernel:DEBUG
RUSTORM_LOG=odoo_kernel::domain=trace odoo-bin … --log-handler odoo.rust_kernel:TRACE
```

The bridge carries the **dispatch span** into the message, so an event from the
compiler or the reader names the request it belongs to:

```
DEBUG odoo.rust_kernel.scan: [dispatch model=res.partner method=search_read uid=Some(Id(2)) su=false]
      search_read plan model=res.partner columns=3 x2many=0 … rules_ms=1.455 cond_ms=0.072
```

`TRACE` is level 5, below Python's DEBUG. `addLevelName` alone does not make it
selectable — `odoo/logutils.py` resolves a `--log-handler name:LEVEL` with
`getattr(logging, LEVEL, logging.INFO)`, and it runs before any addon is
imported, so `:TRACE` read as INFO. `rust_engine` registers the level and
re-applies the entries that name it when it arms, which is what makes the line
above work.

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

## Corrected 2026-09-09: five wrong answers no corpus had asked about

Each of these was found by reading the fork's ORM source against `sqlgen.rs`,
then measured on a base+mail database before it was fixed. None was a
refusal; every one was a plausible wrong answer.

- **Rule domains ran in the caller's identity.** Odoo compiles an `ir.rule`
  domain with `model.sudo().with_context(active_test=False)`
  (`odoo/orm/runtime/backend.py`, `_prepare_postgres_search_query`), so a
  traversal inside a rule meets no comodel rules, no comodel ACL and no
  active filter. The kernel compiled rules with the request's compiler. With a
  portal rule `states via ANY country` beside a country rule `US/MX only`,
  Python counted 2131 states and the kernel 94 -- and admin, whose own
  reachable set walked through the same country rule, got 94 too.
  `Compiler::compile_rules` now compiles a rule node as superuser with
  `active_test=False`; three tests in `kernel/tests/pure.rs` pin the three
  differences, and `sweep_corpus.py` seeds both rules on the portal identity
  so the registry sweep keeps asking.
- **An empty `_rec_name` rendered `model,id`.** `_compute_display_name` is
  `str(value) if value else False`; `model,id` is the branch for a model with
  NO `_rec_name`. A `mail.template` with a NULL name read `false` from Python
  and `"mail.template,10"` from the kernel. `scan::display_name_cell` renders
  all three sites (`search_read`, many2one labels, group labels) the same way,
  and a model with no `_rec_name` at all now answers `model,id` instead of
  refusing.
- **A many2one or date `_rec_name` kept the default display name.** `finalize`
  dropped the field but not `display_name_default`, so `mail.notification`
  (`res_partner_id`), `ir.default` (`field_id`), `res.currency.rate` (a date)
  and 15 more on base+mail would have labelled as `model,id` where Python
  renders the target. `Registry::normalize_model` withdraws the default when a
  declared `_rec_name` is not a text or selection column, and is pure so it
  is tested.
- **Read-path hooks the export never looked at.** The workspace overrides
  `_field_to_sql` in 12 models, `_order_to_sql` / `_order_field_to_sql` in 5,
  `_read_group_select` / `_read_group_groupby` / `_read_group_orderby` in 24,
  `fetch` / `search_fetch` / `search` / `_fetch_query` in 13, and
  `_get_display_name_visible_ids` in `hr.employee`; `read_path_pure` checked
  five method names. `hr.employee._field_to_sql` renames a field,
  `account.account._order_to_sql` reads the context, `stock.quant`
  substitutes a `sum`. The export now lists `impure_read_methods` over the
  fetch family and `_field_to_sql` too, and the kernel checks only what its
  own method runs into (`Model::overridden_for`: Python's `search_read`,
  `search_count` and `_read_group` never call `read()`, so `res.users`'
  self-read `read()` override no longer refuses its searches; the shim's gate
  makes the same per-method distinction); `order_pure` refuses ordering
  (through the many2one order chain too) and `read_group_pure` refuses
  grouping, each only where it applies; `display_name_access_pure` refuses a
  label the kernel would hide where Python may show it. `TRANSPARENT_HOOKS`
  in `export.rs` names the overrides that were read and found to dispatch on
  one field and delegate the rest -- the mail activity mixin, the properties
  mixin, `res.device`, the user-favorite mixin, `crm.lead`, `hr.employee`
  (`_field_to_sql`, and hr_appraisal's `_search` / `fetch` on
  `next_appraisal_date`), `document.document`, the analytic mixin,
  `account.analytic.account`, `res.groups` (`_search` sorts in Python only
  on `full_name`) and geoengine's `Base`, whose two read_group hooks act on
  `geo_*` aggregates alone and hook every geometry field (a scope may be a
  callable of the model) -- and the kernel forgets those fields
  (`hooked_fields`), so on base+mail 172
  of 181 models still serve reads and `res.partner` stays routed, and on a
  428-model hr+crm+project+stock+mrp database the stricter check costs
  nothing the old one served: `calendar.event` and `project.task` override
  `_read_group` outright, `mail.message` overrides `fetch`, and
  `stock.quant` is deliberately NOT in the table because its
  `_read_group_select` answers NULL for the STORED `inventory_quantity` under
  a context flag the kernel never sees. An override not in that table makes
  its hook impure; the table grows by someone reading the override.
- **Display names are declared, not guessed.** A model whose
  `_compute_display_name` or `_search_display_name` is Python is refused for
  `display_name` unless the fork declares what the kernel may do:
  `_display_name_column` (stored text columns, or one delegated through a
  stored many2one under `_inherits`, coalesced in order) IS the display name
  whenever the context carries none of `_display_name_context_keys`, guarded
  by `_display_name_column_guard` (a nameless row renders a translated
  placeholder the column carries in English only); `_display_name_search_default`
  says the search is the default composition under the same condition; and
  `_display_name_search_exact` names stored fields whose exact match answers
  an `in` / `ilike` outright when any row matches (`res.users` and `login`),
  which the shim resolves in `_request` before dispatch -- a hit becomes
  `("id", "in", ids)`, a miss the composition over the name fields -- while
  the kernel refuses such a leaf it did not see resolved. `res.partner`
  (`complete_name` guarded by `name`), `res.company` (`("code", "name")`) and
  `res.users` (`name`) declare today; the partner display-name SEARCH still
  falls back because its `_rec_names_search` carries `phone_mobile_search`, a
  custom-search field resolved through the phone library.
- **`bypass_search_access=True`** (90 declarations in the workspace) makes
  Odoo open the traversal's subquery with `_search(bypass_access=True)`: no
  ACL, no rules. The kernel applied both. On base+mail every instance was a
  REFUSAL rather than a wrong answer -- `res.partner.user_ids any [...]` at a
  portal or internal identity tripped the rule-recursion guard, and the other
  bypass fields point at models refused for their own reasons -- so what was
  measured is refusal → correct answer (portal 1/1, internal 3/3). The
  narrowing shape is read from `comodel_rules`, not observed; it needs a
  bypass field whose comodel is ruled without recursing, which base+mail does
  not have. Exported per field; `comodel_rules` honours it.
- **A routed `read()` dropped archived x2many members under
  `active_test=False`.** Odoo's `search_read` deletes `active_test` from the
  context before it reads (`search.py`), so an x2many there never lists
  archived corecords, and the kernel's `x2many_ids` was built to that shape.
  `read()` and `web_read` keep the caller's context, and with
  `active_test=False` the archived corecords are part of Python's answer:
  measured, `mail.activity.type.read(["mail_template_ids"])` gave `[10, 11]`
  from Python and `[10]` routed. Found while challenging the earlier x2many
  probes, which had all gone through `search_read`. The read route now sends
  `x2many_active_test` from its env, `search_count` and `read_group` refuse
  the knob, and the probe reads `[10, 11]` both ways.
- **Python answers an x2many from the cache, in the order it was written
  there.** Putting that read into the battery found it: right after a
  `create` with `(6, 0, ids)`, Python's `read()` returned the ids in the
  command's order and the kernel returned them in the comodel's `_order`, a
  shadow divergence on the same set. With the cache invalidated both engines
  agree on `_order`, so this is not the SQL; it is the cache the kernel
  cannot see. The gate now keeps a read in Python when the ORM cache holds
  that x2many for any record of the model in the transaction
  (`_x2many_cached`), which is exactly when Python would not touch the
  database either. The cost is a routed share drop for the second read of one
  x2many inside one request; the load stage checks both halves.

Three transport defects, each pinned by a database-backed test in
`kernel/tests/db_probe.rs` (`cargo test -p odoo-kernel --test db_probe --
--ignored`, with `RUSTORM_DB` set and `RUSTORM_TLS_DSN` pointing to an owned
TCP server with a self-signed certificate; missing TLS configuration now fails
an explicitly requested TLS test instead of returning success):

- **The cursor re-prepared a retyped statement on every execution.** A
  parameter the server infers to a type the cursor cannot bind (`interval`,
  `regclass`) is re-prepared as `text`, and that statement was cached under
  the bare SQL while every lookup used the typed key. Measured:
  `SELECT now() + %s::interval` prepared `s3`, `s4`, `s5` across three calls.
- **`Db::query`'s stale-plan retry could not succeed where it ran.** Both real
  callers are inside a transaction, `cached plan must not change result type`
  aborts it, and the retry's `prepare` died with `25P02`. The cache entry is
  now dropped and the error returned; `serve` retries the whole dispatch in a
  new transaction, and the hybrid falls back to Python for that one call.
- **A timed-out request leaked its snapshot.** `tokio::time::timeout` dropped
  the dispatch after `BEGIN ... READ ONLY`, the lease returned the connection
  with the transaction open, and the next request's `BEGIN` was a no-op
  warning that read inside the previous request's snapshot (measured: request
  two saw 0 rows where the table held 1). The connection is poisoned, its
  query cancelled, and the pool reconnects.

And in the shim:

- `flush_all()` before every routed read flushed every dirty record in the
  transaction, so a pending record that fails a constraint aborted the
  transaction inside a read Python would have answered. The shim now flushes
  what Odoo's own query would -- `_DependencyCollector` over the domain, the
  rule domain, the order and the requested fields -- and falls back to
  `flush_all()` for a shape the collector does not model.
- The gate cache survived registry reloads: a module installed at runtime that
  adds `web_read` to a model this cache called clean kept it routed.
  `forget_gates()` runs on every kernel rebuild -- measured through the
  `Registry.new` hook: a planted verdict is gone and the kernel is a new
  object after one reload.
- The flush change is measured, not argued: with a valid pending write on
  `res.country` and a pending `res.currency` rename the unique index rejects,
  the old shim answered a `res.country` read with `InFailedSqlTransaction`
  where Python answers it, and the new shim answers it AND returns the
  pending value (`phone_code` 999), so the targeted flush flushed.
- `_web_spec_plan` refuses binary, reference, properties and non-stored
  computes before the round trip; `_read_group` refuses unknown aggregate
  functions before it; `_restamp` copies every `_api*` stamp rather than
  three named ones; the `Registry.new` hook accepts positional arguments.

## Six more wrong answers, from reading `domain/optimizations.py`

Odoo rewrites a domain before any SQL is made of it, and the kernel compiled
the raw leaf. Measured on base+mail against Python, 22 shapes:

    ('vat_label', 'like', '')        python 251   kernel  52   -- TRUE in Odoo
    ('vat_label', 'not like', '')    python   0   kernel 199   -- FALSE in Odoo
    ('vat_label', '=like', '')       python 200   kernel   1   -- `= False`
    ('vat_label', 'like', '%')       python 251   kernel  52
    ('phone_code', '=?', 0)          python 251   kernel   0   -- any falsy value is TRUE
    ('state_ids', 'in', [0])         python 183   kernel   0   -- 0 is `False` on a relation

and nine shapes that refused where Odoo coerces: `'32'` against an integer,
`1` and `'false'` against a boolean, `> False` on a number (falsy) and on a
many2one (FALSE), `('code', '=', ['BE', 'FR'])`, `('code', 'in', 'BE')`,
`('x', '=', [])`. `Compiler::optimize_leaf` now mirrors `_operator_equal_as_in`,
`_optimize_in_set`, `_optimize_like_str`, `_optimize_numeric_comparand`,
`_optimize_relational_falsy_id`, `_optimize_boolean_in` and
`_optimize_inequality_against_null`, and the 22 shapes are in
`harness/corpus.json`. Two stay refusals on purpose: `child_of` seeded with a
name resolves through a name search in Python, and `display_name =ilike ''`
becomes `display_name = False`, which the kernel does not compile.

## A datetime groupby ignored the context timezone

`_read_group_groupby_temporal` converts a datetime to the context `tz`
(`timezone(tz, timezone('UTC', col))`) before truncating, when Postgres
knows the zone or Odoo's alias table maps it to one it knows; the kernel
truncated in UTC, and the shim never forwarded `tz`, which the web client
always sends. On three tags created around 2026-03-01 00:00 UTC:

    create_date:month, no tz                python Feb 1, Mar 2   kernel the same
    create_date:month, America/Mexico_City  python Feb 2, Mar 1   kernel (before) Feb 1, Mar 2

The kernel now takes `tz`, resolves it against `pg_timezone_names` and the
exported alias table (`Asia/Calcutta` groups as `Asia/Kolkata`; an unknown
zone groups in UTC, as Odoo does with a warning), applies it to datetime
groupbys only, and knows `hour`. `week` stays refused: its boundary depends
on the language's first weekday. The shim also declines any read carrying
`prefetch_langs`, under which a translated field reads as its whole
dictionary. The six shapes are in `harness/corpus.json`, seeded by the sweep.

## The full battery found two more, and the battery itself had three holes

`harness/verify.sh` without `--quick` had not run since the series began.
It found:

- **An x2many membership test by id, or by absence, runs as superuser in
  Odoo.** `_RelationalMulti.condition_to_sql` browses an id list, or
  searches `TRUE` for the `False` case, on `comodel.sudo().with_context(
  active_test=False)`: the field's own domain still applies, the comodel's
  record rules, ACL and active filter do not. The kernel applied all three.
  At the portal identity whose country rule allows two countries:

      ('country_ids', '!=', False)     python 8   kernel 0
      ('country_ids', '=', False)      python 0   kernel 8
      ('country_ids', 'in', [20])      python 3   kernel 0

  A domain value (`any`) keeps meeting the rules, as Odoo's does. The
  registry sweep is what caught it, at the one identity with a rule on the
  comodel; the direct corpus at admin could not.
- **A dotted inequality against False on a boolean target looped the
  compiler** into a stack overflow: the rewrite replaced `False` with the
  boolean's falsy value, which is `False`. Found by the fuzz seeds on their
  first run after the optimisation layer landed; the rewrite now declines
  when it would reproduce the leaf and the boolean refusal stands.
- The soak stage bound a fixed port and failed when a peer's test server
  held it; it picks a free one. Phase 1 compared one `res.partner` read
  against a baseline the registry sweep's own seeding had since outdated; it
  runs right after that baseline is written.

## Week and number granularities

`week` was refused because its boundary is the language's first weekday:
`get_lang(env).week_start` is 7 for `en_US` and 1 for `fr_FR`, and Odoo
truncates `col - INTERVAL '-d DAY'` to the Monday and adds the interval
back. The snapshot now carries every active language's `week_start`, a
request that names a language groups by its week, and one that names none
is refused rather than guessed, because Odoo then falls back to the
company's language, which the kernel does not know. The ten `*_number`
granularities are `date_part`s, returned as floats as Odoo returns them,
and `day_of_week` groups are ordered from the language's first weekday
(`mod(7 - week_start + dow, 7)`) -- selected as a hidden trailing column and
named by ordinal, because the expression carries the bound timezone and
Postgres cannot match two bindings of it between GROUP BY and ORDER BY.
Measured against Python with `en_US` and `fr_FR` active, with and without a
timezone: 16 shapes equal, in `harness/corpus.json`.

The battery on a 428-model database (hr, crm, project, stock, mrp) passed
every stage but phase 1, which read the pre-seeding baseline the
warm-computes stage writes to `harness/expected.json`; it reads the run's
own now.

## The connector speaks TLS

`db_sslmode = require` made the addon refuse to arm, correctly: the Rust
connector had no TLS and arming it would have downgraded the database's
connections to plaintext. `odoo_kernel::connect` now parses the dsn's
`sslmode` and `sslrootcert`, hands tokio-postgres a rustls connector, and
follows libpq's meaning of each mode: `require` encrypts without verifying
the certificate, `verify-full` verifies the chain against the platform
roots plus `sslrootcert` and checks the host name, `verify-ca` is refused
because the connector cannot check a chain without the name. The server,
its pool (reconnects and query cancellation included) and the embedded
cursor all connect through it. Measured against this cluster's snakeoil
certificate over TCP: `require` gives a TLSv1.3 session, `verify-full`
fails with "certificate not valid for name", both pinned by a database
probe that takes `RUSTORM_TLS_DSN`. Unix-socket connections are unchanged.

## `display_name` with `=`, `!=`, `in` and `not in`

`_search_display_name` answers those four the way it answers the like
family: one condition per `_rec_names_search` entry, joined by OR for a
positive operator and AND for a negative one, with an unset name (`False`,
`None`, `''`) in the values becoming a separate `name = False` conjunction,
negated for the negative operators. The kernel refused them. Five shapes on
`res.country` and `mail.template` now match Python and sit in the corpus;
inequalities stay refused.

`_read_group(limit=0)` returns every group -- it tests `if limit:` -- while
`_search(limit=0)` returns nothing; the kernel applied zero in both. Fixed
for read_group, and the four limit-and-offset shapes are in the corpus.

## `child_of` and `parent_of` seeded by a name

`_operator_hierarchy` turns a string seed into `display_name ilike` on the
comodel and searches it at the caller's identity -- a many2one's comodel
without the active filter, an x2many's under the field's context, `id`
under the request's -- and a many2many runs its id seeds through that
search too. The kernel refused any non-integer seed. It resolves them now
through the same display-name compiler, with the comodel's ACL and rules
unless the domain is a rule's own, and the memo still keys on the ids the
search produced. Six shapes on `ir.module.category` equal to Python at
admin and at an internal user; a model whose display name is Python's
stays refused.

## Compounding 2026-09-09: the improvements that were not defects

- **Language and company are bound parameters.** `->> 'fr_FR'` and
  `-> '1'` were interpolated into the statement text, so every language and
  every company owned a separate prepared statement and a separate plan, and
  the 256-entry statement cache filled with copies of one query. They are
  `$n` parameters now; a test pins that two identities build byte-identical
  SQL with different values. The English fallback stays literal.
- **Record rules are memoised per identity, not per request shape.** The
  cache was keyed on the set of models a request could reach, so two shapes
  on one identity recompiled every rule. One entry per identity accumulates
  models as requests reach them; the repo corpus went from 22 compile passes
  to 16 with identical answers. The key is the security-bearing signals only
  (`default`, `groups`, `stable`), so an asset or template rebuild neither
  evicts it nor reloads security -- those bumps are stamped, not acted on --
  and the active languages are part of the snapshot, so activating one no
  longer needs a restart.
- **Id lists over the threshold become one array parameter** in the
  many2one label and x2many reads as they already did in domains, so a page
  of 500 rows no longer prepares a 500-parameter statement of its own.
- **The x2many cache gate is precise.** The shim records the x2many fields a
  `create` or `write` touched in the transaction -- including the inverse
  x2many of a written many2one, which the ORM appends to rather than
  re-sorts -- and only those keep a read in Python. A cache filled by a read
  is in `_order` already and routes.
- **A routed read warms the ORM cache** the way Python's `read` and
  `search_read` do, so the attribute access that follows a routed read no
  longer re-queries what the kernel just answered. x2many values stay out on
  purpose: Python caches every corecord and filters archived ones at access
  time, while the kernel already filtered. The load stage checks that the
  fields of a routed read are in the cache and that a many2one resolves from
  it.
- **The kill switch keeps one connection per worker** instead of opening a
  psycopg connection every tick, and the dispatch span is entered through
  `Instrument` rather than held across the await.
- **Labels are fetched once per comodel**, not once per many2one field, so
  `user_id` and `create_uid` share one `res.users` query; the group closure
  of an identity is cached beside its companies instead of being rebuilt
  from the implied-group graph on every request; `serve` takes `--pool`,
  `--request-timeout-secs` and `--acquire-timeout-secs` instead of three
  constants.
- **`serve` refuses to start without `--export`** instead of serving a
  kernel that refuses everything. `Registry::load` remains for `inspect` and
  for the export's own cross-check.
- Redundancies removed: `Orm` held the client twice, `Model.has_active`
  restated `active_name`, `inverse_column` duplicated `o2m_inverse`,
  `query_cached` was a pass-through, and the order-by-related alias used a
  hard-coded depth of 16 to dodge the read alias family; it has its own
  prefix now. `project.task._read_group`, which only renames a `triage_id`
  groupby, joined the transparent-hook table.

## Known gaps

What is NOT on this list any more, because it was closed: the binary COPY
encoder (differentially tested against psycopg), running under `workers > 0`
(the tokio runtime and the connection free list both survive the fork now),
serving a host that has more than one database (others are delegated to
psycopg rather than broken), turning routing off without a restart
(`ir.config_parameter`), the absence of a concurrent end-to-end
measurement, and TLS in the connector (`require` and `verify-full`, see
above).

- **Python model overrides** (`_search`, `_compute_display_name`, computed
  fields): invisible to a registry built from the DB alone. This is the
  fundamental M2 problem — the business-logic layer. Fields declaring
  `search=` are detected via the live-registry export and refused; the
  `ir_model` bootstrap cannot see them, so it is not a safe primary source —
  `serve`, `query`, `run-corpus` and `bench` refuse to start without
  `--export`; `inspect` is the one command that reads the bootstrap.
- **25 of 222 ruled models on a real database** cannot have their rules
  compiled and fall back to Python — `account.move`, `account.move.line`,
  `sale.order`, `sale.order.line`, `hr.employee` among them. Every one is now
  the same cause: a rule interpolating a **non-stored Python compute**
  (`res.users.crm_team_ids`, `res.users.employee_id`,
  `res.company.country_id`). The first two also declare `search=`, so both the
  value and the domain leaf need Python. This is the measured size of the
  business-logic problem, not an estimate.
- **A field declaring `bypass_search_access` turns the comodel's access OFF
  for a subquery through it, and the kernel used to apply it anyway.** Odoo's
  `_optimize_any_with_rights` rewrites `any` to `any!` exactly when the field
  declares it, and `_search(bypass_access=True)` then skips the comodel's ACL
  check AND its record rules. **90 fields of a 73-module database declare
  it** -- every mail-thread `message_ids` and `activity_ids`, every
  `attachment_ids`, `res.users.partner_id`, `account.move.line.move_id`,
  `product.product.product_tmpl_id`. The kernel compiled the comodel's rules
  into the subquery regardless, so it answered with FEWER rows than Python:
  fail-closed, and still a wrong answer. Reproduced on a real database rather
  than argued -- an internal user who cannot read a private `res.partner`
  still finds, through Python, the `account.analytic.account` that points at
  it, because `partner_id` bypasses:

        account.analytic.account, ('partner_id.name','ilike',…), uid 197
            python  [{id: 1}]      kernel  []        <- before
            python  1              kernel  0         <- the same as search_count
            python  [{id: 1}]      kernel  [{id: 1}] <- after
        the same domain at su matched throughout, which is what says the
        record-rule path is the one that moved

  `bypass_search_access` is now exported per field and honoured. **`any!` is
  REFUSED, and briefly was not, which is the cautionary half of this entry.**
  It is Odoo's internal spelling for the same bypass -- the optimizer rewrites
  `any` to `any!` when the field declares the flag -- so honouring it looked
  like completeness. It is not: `Domain()` REJECTS `any!` in an incoming
  domain, so no domain reaching this kernel can legitimately carry one, and
  honouring it turned a harmless extra spelling into a rule bypass a CALLER
  COULD ASK FOR. Measured on the same scenario, at the identity that cannot
  read the partner:

        ('parent_id', 'any',  [('name','ilike',…)])   python []   kernel []
        ('parent_id', 'any!', [('name','ilike',…)])   python raises ValueError
                                                      kernel returned the row

  It is refused in `domain::parse`, the single door every domain and
  sub-domain comes through, and no operator string can grant a bypass any
  more -- only the field's own declaration can. Found by auditing which
  operators the compiler implements that no corpus exercises: `any!` was the
  only one, and it was the one that had just changed meaning.
  **The `ir_model` bootstrap cannot see
  it**, so a registry built that way carries `None` and REFUSES a traversal
  whose comodel has rules, rather than guessing: applying them answers with
  too few rows and skipping them with too many, and neither is safe to
  assume. A comodel with no rules is the same answer either way and still
  compiles.
- **A `like` over a translated `index="trigram"` field could not use the
  index Odoo built for it, and now does.** This is the one entry here that is
  about SPEED rather than answers, and it points the wrong way: the kernel
  was slower than the Python it replaces, on the path a product autocomplete
  takes. Odoo's `_String.condition_to_sql` ANDs a conjunct onto every
  positive `like` / `ilike`, and the GIN index is declared over that
  conjunct's expression and nothing else:

        gin (unaccent(jsonb_path_query_array(name, '$.*')::text) gin_trgm_ops)

  Measured on the fixture with `SET enable_seqscan = off`, which is what
  separates "the planner preferred a scan" from "the index cannot be used":

        with the conjunct     Bitmap Index Scan on product_template__name_index
        without it            Seq Scan   (with sequential scans DISABLED)

  What that is worth, `EXPLAIN ANALYZE` on 200,000 rows of the same shape --
  and reported across selectivities, because the flattering number alone
  would be a lie by omission:

        pattern         rows matched   without    with     
        c4ca4238a0b9               1   70.5 ms    0.7 ms   102x faster
        abc                    1,462   67.1 ms    8.3 ms     8x faster
        Produit                    0   71.9 ms   72.5 ms   neutral
        widget               200,000   71.7 ms  176.2 ms   2.5x SLOWER

  **The accelerator is not a universal win, and it is not supposed to be.**
  A pattern that matches most rows pays for a GIN scan that excludes nothing;
  `Produit` is the shape where the prefilter searches EVERY translation and
  so matches everything the base condition then rejects. Odoo has exactly
  this profile, because this is Odoo's conjunct -- and parity with Odoo's
  PLAN is the goal, not a plan of our own that is better on one workload and
  worse on another. What the fix buys is that a selective search, which is
  what an autocomplete is, stops being two orders of magnitude slower than
  the Python it replaced.

  The conjunct is implied by the condition it accompanies -- the array of
  every translation contains the one the base condition tests -- so it
  changes no row. `crate::trigram` ports Odoo's two pattern builders;
  `_TRIGRAM_PATTERN_RE`'s lookbehind is not expressible in Rust's `regex`, so
  the scanner is hand-written and was validated differentially against
  Odoo's own functions over **40,040** inputs (0 mismatches) before any of it
  was written in Rust. Two behaviours that cost three rounds to find and are
  in the tests: Python's `$` matches before a single trailing newline, so a
  segment ends there -- *unless* a backslash escaped it; and a dangling
  backslash drops only the segment it ends, not the whole pattern.
  A single-valued `in` -- which is what an `=` becomes -- is accelerated the
  same way, through `value_to_translated_trigram_pattern` and compared with
  LIKE rather than ILIKE, because the equality it accompanies is
  case-sensitive. Checked against Odoo's own emitted SQL, including where it
  is WITHHELD: a value shorter than a trigram, a falsy operand, more than one
  value, and `!=` are all unaccelerated on both sides.
- **`child_of` on a `_parent_store` model resolves to an id list where Odoo
  inlines a prefix match.** Both are correct and the corpus says so across
  224 cases; the plans differ. Odoo compiles
  `('id','child_of',N)` straight into the main query as
  `parent_path LIKE '1/2/16/%'`, one indexed predicate. The kernel's
  `resolve_hierarchy` rewrites every hierarchy leaf to `id in [...]` before
  compiling, which keeps the rest of the compiler simple and costs two extra
  round trips (fetch the prefixes, then fetch the matching ids) plus an `IN`
  list as long as the subtree. It DOES take the `parent_path` fast path --
  `can_use_parent_path` is true for all 7 `_parent_store` models here, so
  this is not the O(depth) walk it would otherwise be.
  **Not implemented, deliberately**: emitting the prefix match as a
  condition instead is a small change, and on this fixture the hierarchies
  are a handful of rows, so there is no measurement to justify it and no
  measurement to check it against. It belongs on a database with a deep
  category tree, which is where the `IN` list would start to hurt.
- **A one2many whose inverse is not a column is refused, and used to be a
  database error instead.** `store` on a one2many says its INVERSE is a
  column, and Odoo does not require that:
  `account.analytic.account.line_ids` inverts
  `account.analytic.line.auto_account_id`, a non-stored many2one with a
  `search=` method, and no such column exists. The kernel read the one2many's
  own flag, emitted `s0_account_analytic_line.auto_account_id`, and learned
  from PostgreSQL that it does not exist -- fail-safe, since the shim falls
  back on the error, but one wasted round trip per call and a database ERROR
  in the log for something the compiler can see. It now asks the COMODEL's
  field, through one `Field::o2m_inverse_column(owner, comodel)` that BOTH
  the filter path and the read path call -- fixing only the filter path left
  the registry sweep emitting the identical error, because reading a
  one2many resolves its inverse in `scan.rs` and not in the compiler. Found
  on 2026-09-08 by seeding the fixture's first `account.analytic.account`
  row: the shape had always been compiled and never reached, because the
  sweep skips a model with nothing in it.
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
  table, and a registry bump raises a typed `RegistryStale`. The embedded
  kernel remains refused once stale; it cannot rebuild from its retained old
  metadata. The Python registry lifecycle publishes a fresh kernel through
  the addon's `Registry.new` hook. Export files must carry `registry_sequence`
  matching the database; regenerate older unstamped exports. Embedded builds
  read metadata and the watermark in one repeatable-read snapshot. Each new
  kernel has a distinct statement-cache generation. Routed requests also carry
  their Python registry sequence; an old environment cannot use the replacement
  kernel. Requests whose SQL snapshot predates the shared security metadata
  refuse, and each dispatch retains the exact security snapshot it checked,
  including when unrelated cache signals race a security refresh. Standalone `serve` retains
  its explicit export-refresh command mechanism.
- `phase2_tests` drives the upstream suites with a bare
  `unittest.TextTestRunner`, not Odoo's runner, so `assertQueries`-style
  assertions behave differently (`assertQueries` no-ops when `self.warm` is
  false). Shared baseline failures, failed child processes and selections running no
  unskipped tests now fail this gate as well. The
  `test_rec_names_search` SQL expectation previously assumed base's ordering
  although mail replaces it; the sibling Odoo test now fixes its ordering
  explicitly to isolate the expression contract.
- `RustConn::copy` implements binary `COPY FROM STDIN` (the format v19's bulk
  create requests). Text/CSV format, `ON_ERROR`, and a custom writer are not
  implemented — nothing in the read or create path asks for them.
- A `search_read` with no `fields` is refused. Odoo returns every readable
  field there; enumerating them is a `fields_get` this kernel does not have.
- An inequality against an unset value follows the field's `falsy_value`, as
  `_optimize_inequality_against_null` does: `('id','>',False)` and
  `('write_date','<',False)` are FALSE, `('name','>',False)` compares with `''`
  and `('credit_limit','>',False)` with `0`. `id` is the one integer without a
  falsy value, and the kernel answered every row for it until 2026-09-10.
- A bare date against a datetime names a whole day in `env.tz` -- the
  context's `tz`, else the USER's (`Datetime._optimize_datetime_comparand`,
  `_value_to_datetime`): `<= '2026-09-10'` is `< 2026-09-11 00:00` local,
  `=` is a range. The kernel reads the user's tz per request (it lives on the
  partner and moves no cache signal), converts the local midnights through
  `chrono-tz`, and refuses a zone its table does not know or a midnight that
  is missing or doubled in that zone. Until 2026-09-10 it compared at UTC
  midnight; a first fix computed UTC boundaries whatever the user's zone,
  which the challenge run caught (Python 2, kernel 0 for a Mexico City user).
  `date.fromisoformat`'s padded form is required: `'2026-9-1'` raises in
  Python and is refused here.
- A non-stored related field in a domain compiles the way `_search_related`
  composes it: the leaf on the path's target, each hop wrapped in `any` (`any!`
  when the field is `compute_sudo`, which skips the comodel's rules), `hop =
  False` OR-ed in on every non-required many2one hop whenever an unset hop
  should match, and a negative operator with a set value as the negation of the
  positive form. Until 2026-09-10 the kernel spliced the path in and lost every
  row whose head is unset (41 vs 0 on `ir.actions.server`); seven shapes
  (`=`/`!=`/`in`/`not in`/`not ilike`/`!`, at admin and at a denied identity)
  are measured equal now.
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
