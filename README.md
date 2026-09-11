# odoo-rust-orm — the Rust ORM that replaces Odoo's Python one

**The goal is replacement.** This kernel is built to take over Odoo's ORM
outright: `odoo/odoo/orm/` is what it supersedes, not what it defers to.
`M3-PLAN.md` holds the target architecture — the Rust binary is the server, and
it embeds CPython for one job only, running addon business logic on the Rust
engine underneath.

Two requirements are non-negotiable the whole way there: **exact behavioral
compatibility** with Odoo 19 and **performance**. Differential checks against
Python are how both are held — every read shape the kernel serves is diffed
against Python's answer, and runtime contracts check the effects a response-JSON
comparison cannot see.

The read path is the stage that is built, measured and serving; the write path
has started, at the fork's own persistence port, with the update and the insert on it
(see "The persistence port" below). **Coexistence is the migration mechanism,
not the destination**: while it lasts Python stays authoritative for registry
metadata, extension hooks, cache/compute state and write orchestration, Rust
executes eligible reads on the caller's transaction, and any shape it cannot
answer exactly refuses back to Python. Those checks
establish the shapes they exercise and no more — the routed share and every
remaining refusal are measured below, with a reason attached, because the
refusal list is the replacement backlog.

## Built and measured today (M0 + M1): the read path

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

`harness/corpus.json`: 425 cases across 29 models (255 `search_read`, 90 `search_count`, 80 `read_group`;
`harness/corpus_stats.py` counts the file, which is where every figure about it
should come from) — every operator family,
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

**Result: 275/275 with nothing failing** (the corpus of the time; it holds
425 cases today), re-verified 2026-08-28 against THREE
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
evidence about the HYBRID and says nothing about `rustorm query`,
`run-corpus` or `serve`. The shadow corpus IS kernel-direct, and is 275
hand-written cases over 29 models. The kernel sweep is the third thing —
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

**A conf that has been armed for a deployment names a DIFFERENT database, and
the battery used to fail three stages over it.** `rust_engine_db` arms the
addon for exactly one database; a workspace conf carrying a deployment block
names that one, not the database `--db` was pointed at. Every stage needing a
routed call then gates on "another database" and reports `routed=0`, and
`replay gate controls`, `load into odoo` and `copy encoder` fail for a reason
that has nothing to do with the code. Measured on `p314o19m.conf` armed for
`rustorm_5e_scale`. The battery now derives a copy armed for its own database
and says so on the first line:

```
note: the conf arms rust_engine for 'rustorm_5e_scale'; this run uses a copy armed for 'rustorm_probe_odoo6b'
```

The tours stage had always derived its own conf for exactly this reason; the
rest of the battery does it now too.

| stage | what it proves |
|---|---|
| fork contract | every Odoo symbol the shims, the addon and the harness import or patch still exists in the checkout -- no database, no extension, seconds |
| shim units | the two Python shims, with no database: dsn parsing, the kill switch, datetime round trips |
| registry export | the export describes this database (it refuses to write one that does not) |
| shadow corpus | 425 cases byte-equal to the Python ORM, across identities and contexts |
| kernel sweep | every model, kernel-DIRECT, on a fixture it seeds itself |
| fuzz | seeded random domains, values sampled from the columns |
| registry sweep | every model, every identity, ~25k query shapes |
| hybrid (phase 1) | the real Odoo registry boots and its ORM runs on Rust connections |
| upstream suites | routing changes no upstream test result |
| cursor type layer | 20 Postgres types round-trip identically to psycopg |
| concurrency | several identities interleaved across threads, no cache cross-talk |
| replay | the web tours' own traffic (captured by `rust_engine_capture` during that stage; `RUSTORM_REPLAY` names another file) fed back through the shim in shadow mode: routed share and divergences per (model, method); SKIP only when the tours did not run, FAIL when nothing was compared or an unexpected native/shim error occurred; rolled-back tour identities are reported separately and are not counted as replayed |
| runtime contracts | real native connections check cache fidelity, SQL-failure recovery and breaker classification, stale exports, and the registry reload hook; required even under `--quick` |
| every user | every user of the database, archived and other companies' included, reads every table-backed model through each routed method, routed and not: many2ones and x2manys, counts, names, groups |
| orm test modules | with `RUSTORM_ORM_TEST_DB` set, Odoo's test_orm, test_read_group, test_access_rights, test_search_panel and test_inherits run with no engine and with routing on; a test failing only under routing fails the stage |
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
./target/release/rustorm run-corpus --file harness/corpus.json > actual.json
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

## Ordering by a company-dependent many2one asked PostgreSQL for `jsonb = integer`

Found 2026-09-11 by the fuzz stage on a 162-module fixture, one case in 1,500,
and it predates this branch — the base commit fails identically:

```
ERROR: operator does not exist: jsonb = integer
HINT: No operator matches the given name and argument types.
```

A company-dependent field stores its value in a `jsonb` keyed by company, so a
company-dependent **many2one** keeps its id inside that jsonb. `order_terms`
took the FK as the raw column (`col(alias, &field.name)`) and `OrderJoin`
carried a column NAME, so the LEFT JOIN onto the comodel compared the jsonb to
`id`. The statement dies before it runs — an internal error, not a refusal, so
the caller's transaction aborts rather than falling back to Python.

The groupby itself was always right: `read_group` reads its group expression
through `ctx.read_expr`, which handles company-dependent. **Only the ORDER BY
was wrong, which is why it needs an order term to reproduce** — and it hit
`search_read` just as hard as `read_group`, on any order naming such a field.

`OrderJoin` carries an `Expr` now instead of a column name, and the FK comes
from `order_expr`, which is what every other reader of the field already used.
Pinned by `ordering_by_a_company_dependent_many2one_reads_it_out_of_its_jsonb`,
which also asserts a plain many2one still joins on its own column, and which
fails against the old FK.

**base+mail could not reach this.** It has no company-dependent many2one whose
comodel is ordered by something other than `id`, so all three fuzz seeds passed
there. The same seeds on 162 modules found it on the second. That is the same
lesson as the census: a fixture decides what a gate can see.

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
tests reports `CURSOR PARITY VACUOUS` and fails rather than passing. No test
is excluded from either leg.

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

267 cases (the corpus of the time; it holds 425 today), 20 iterations per run, on **agromarin + enterprise** — 129 modules,
790 models, and real volume (172k `res.partner`, 164k `mail.message`, 314 MB).
Python timed at the ORM layer (warm caches, `invalidate_all()` between
iterations); Rust timings include full JSON serialization. Both sides run the
same corpus through the same case runner (`harness/cases.py`), so neither is
timed doing less work. Median of 3 independent runs per side:

| | |
|---|---|
| median per-case speedup | **3.01×** |
| Rust faster in | 252/257 cases |
| range | 0.79× … 125× |
| big wins | mail.message reads 68–125×, company-dependent filters 75× |
| losses | small `search_count` where fixed cost dominates (worst 0.79×) |
| registry + security load | ~170 ms for 950 models |

The median is a median of per-case ratios, unweighted: a 0.05 ms `res.country`
read and a 196 ms `res.partner` scan count the same, and the corpus is
dominated by the small reads. It is not a throughput figure. `speedup.py` now
prints the time-weighted ratio beside it — total Python time over total Rust
time on the same cases, which is what a workload shaped like the corpus would
see — and how many cases each side dropped before the intersection, since
`bench_python.py` drops a raising case without a word. The weighted figure was
not measured for this table; the range is the honest content.

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
server/src/              rustorm CLI + axum; connection-owned StmtCache
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

`off` is off for **both** layers. The mode always governed routing; it did not
govern the connection layer, and `db_shim.install()` at `post_load` rebound the
pool class for every later borrow whatever the mode said — so with
`rust_engine_mode = off` no read was routed and every query still ran on
tokio-postgres. The db shim now has its own `ACTIVE` flag, set through
`rust_db_shim.set_active` from the same mode by `rust_engine._set_mode` — at
start (`_apply_config`) and on every kill-switch tick (`_apply_params`): while
it is off the pool factory builds psycopg pools, while it is on it builds rust
ones (`on` and `shadow` both need them — the kernel runs inside the caller's
transaction), and each switch closes the armed database's pools so the next
borrow goes through the factory again. Connections already checked out keep
working and are closed on return rather than pooled.

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
| set | requests must carry `X-Rustorm-Token`; a caller that presents it may name a `uid`, because it had to be told the secret. `su` is **refused with 403** unless the server was started with `RUSTORM_SERVE_ALLOW_SU=1`, and `/health` reports `allow_su`: the secret says the caller may pick an identity, not that one shared string should be a superuser read of the whole database. `verify.sh` starts the soak server with the opt-in, because that lane replays the corpus at every identity against a disposable database |
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

A connection whose transaction could not be ENDED is discarded, not pooled.
A failed `COMMIT`/`ROLLBACK` used to be a `warn!` and the connection went back
to the pool in a state nobody could name — the next request on it could run
inside the previous snapshot, or inside an aborted transaction. It now
surfaces as a typed `TxEndFailed` (a database error to the shim, not a
refusal), the server poisons the connection instead of issuing a second
`ROLLBACK` on it, and the next `acquire` replaces it — the same path the
request timeout and a dropped `TxGuard` already take.

The token is compared as two keyed-SipHash digests of fixed size, so neither
a length mismatch nor an early differing byte returns sooner.

**A stale registry is refused until the export is fresh.** When
`orm_signaling_registry` moves, re-parsing the same export file rebuilds the
same stale model map, so the server answers 503 (`/health` reads `stale`)
until the file's mtime changes — or, with `RUSTORM_EXPORT_CMD` set to a
command that regenerates it, runs that command and rebuilds in place, then
retries the request.

This transport is not the session layer and does not yet try to be: it
authenticates the *caller*, not a user, and there is no login, no cookie and no
CSRF story. Sessions arrive with M3, where the Rust binary is the server. Until
then the bar this has to clear is narrower — it cannot be pointed at an
arbitrary database and asked for superuser.

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

The default filter is `warn`, so **`warn` and `error` are on and everything
below them is off** — that is deliberate, and two `error` lines depend on it
(the internal invariants below, which are defects rather than refusals and must
not need a flag to be seen). `RUSTORM_LOG` takes a standard `tracing`
`EnvFilter` string; `POC_TRACE=1` remains an alias for
`odoo_kernel::sql=debug`.

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
| `odoo_kernel::config` | which value every setting resolved to and whether the environment or the default chose it — each one has a silent fallback, and a wrong one connects to the wrong database or imports another environment's packages and reports success |
| `odoo_kernel::pool` | (server) pool fill, waits, saturation, poisoning |
| `odoo_kernel::http` | (server) one line per call with status, kind and ms |
| `odoo_kernel::cursor` | (hybrid) **every statement Odoo's own Python ORM runs**, with the query/decode split |
| `odoo_kernel::copy` | (hybrid) COPY streams: binary or text, rows |
| `odoo_kernel::bridge` | (hybrid) kernel build, generation, staleness, GIL-detached ms per dispatch |
| `odoo_kernel::export` | (hybrid) the live-registry export walk |

The harness binaries (`export_registry`, `phase1_shell`, `phase2_tests`,
`phase2_verify`, `probe_audit`) install their own stderr subscriber first, so
`RUSTORM_LOG` reaches them too. Without it nothing on their path set a
subscriber until `RustKernel::build` did, by which time the registry export had
already run and logged into one that did not exist — the boot/export split
below was simply not observable:

```
INFO odoo_kernel::export: registry booted             db=… ms=1534.9
INFO odoo_kernel::export: exported the live registry  bytes=893027 ms=32.5
```

**The phases account for the dispatch.** At `debug`, one `search_read` prints
its whole time budget and the parts sum to the total, so a slow read is
attributable without a profiler:

```
dispatch: preamble …                signal_ms=0.251 env_ms=0.768
scan:     search_read plan …        rules_ms=0.016 cond_ms=0.004
scan:     search_read read …        main_ms=0.801 decode_ms=0.279 labels_ms=0.001
                                    x2many_ms=0.012 serialise_ms=0.110
dispatch: ok                        ms=2.487
```

2.24 of 2.49 ms named; the remainder is span overhead and the registry lookups.
Adding a phase to the reader without adding it to that line is what makes the
sum stop working, so keep them together.

Levels are used consistently: `info` is lifecycle, `debug` is one line per
operation or decision, `trace` is per-item (per leaf, per name hop, per
statement). A disabled callsite is an atomic load and a branch, and the field
expressions are only evaluated once it is enabled — a benchmark with
`RUSTORM_LOG` unset is indistinguishable from one built without any of it
(p50 0.130 ms either way over 1,000 calls on base+mail). Read that as ruling
out a LARGE regression and not a small one: three passes a side, not
interleaved, and on the small fixture. Interleave control/instrumented pairs
and report the per-pair ratio if you ever need a tighter bound.

### The Python half

`engine-py/python/` and `addons/rust_engine/` log through Odoo's own logger
rather than `tracing`:

| logger | what it reports |
|---|---|
| `odoo.rust_kernel.routing` | mode and sample changes, kernel build, fallbacks |
| `odoo.rust_kernel.gate` | one line per call the gate turned away, with the reason. `GATE_REASONS` is the same thing aggregated; this says *which call* |
| `odoo.rust_kernel.call` | one line per routed call: wire sizes, kernel ms, and the flush and cache-warm the shim does around it |
| `odoo.rust_kernel.pool` | which DSNs are intercepted and which are delegated to psycopg, pool fill, fork handling, drains, and **a connection whose `close()` raised** — suppressing that is right, since a pool must not fail its caller on it, but it has left a backend on the server |
| `odoo.addons.rust_engine` | arming, the connector DSN it built, the periodic routing report |

Two silent paths were made loud rather than merely logged. A parameter the
cursor has no encoder for is **stringified** and the server asked to cast it —
psycopg's own behaviour for an unregistered adapter, and also the shape that
produced this transport's worst defects, so it now reports once per (python
type, pg type). And `_taint` / `_note_written` in the ORM shim are the safety
net that records what this transaction wrote so the gate can refuse a stale
read; they swallowed a `TypeError` from an unhashable cursor, which records
nothing and leaves the gate seeing a clean transaction. That failure now warns,
because it is more serious than the thing it guards.

### The cursor-parity floor could not tell "fixed" from "not exercised"

It compared a COUNT. On a base+mail fixture all three recorded differences fail
under **psycopg too** — they leave `only_rust` for the both-legs bucket — so the
gate read `ONLY-RUST=0` against a floor of 3 and asked for the floor to be
lowered. That is the shape of an improvement with nothing improved, and
lowering it would have banked a number measured on a fixture that never reaches
those paths. The battery's documented quick path (`--build base,mail`) hits this
every time.

The baseline names the three now, and the gate says which of them moved:

```
baseline entry now failing under BOTH (moved, not fixed): …test_can_dump_binary_agrees_with_set_types…
baseline entry now failing under BOTH (moved, not fixed): …test_pipeline_enters_on_the_second_statement
baseline entry now failing under BOTH (moved, not fixed): …test_pipeline_nesting_does_not_re_enter_the_mode
CURSOR PARITY OK   (379 tests; 0 of 3 baseline differences still only-rust,
                    3 moved into the both-legs bucket on this fixture, 0 not
                    exercised; 22 fail identically on both)
```

Only a baseline entry that **passes under the rust cursor while the suite still
runs it** asks for the floor to come down, and the gate says so by name. It
still fails on a rust-only failure that is not in the baseline, it still
refuses to score a leg whose connection is not a `FakeConnection`, and it now
also refuses a baseline whose count and names disagree — a file that lies to
whoever reads only one half. All four are negative-controlled.

### The refusal census, and what it took to make it mean something

`odoo_kernel::refusal` carries the source line, so the backlog is a group-by
rather than a set of hand-written regexes:

```sh
RUSTORM_LOG=odoo_kernel::refusal=debug,odoo_kernel::access=debug \
  ./target/release/rustorm --db <db> --export <export.json> \
  run-corpus --file <sweep_corpus.json> 2> census.log
```

Over the 24,199-case sweep on a 162-module fixture (account, sale, stock,
project, hr, purchase, crm, mail — 557 models):

```
  refusal=4221  access=10110  over 27 sites   <- 23 refusal sites + 4 access ones
  1128  orm.rs:1215       <model> overrides the read path in Python (_field_to_sql)
   815  sqlgen.rs:1483    res.users lets an exact match on login take precedence over display_name
   450  sqlgen.rs:1049    <field> defines a custom search method
   372  sqlgen.rs:1469    <model> defines its display name in Python
   357  sqlgen.rs:281     cannot traverse non-stored <field>
   316  sqlgen.rs:1389    the subquery traverses <model>, which defines `_search` in Python
   283  security.rs:257   record rules on <model> could not be evaluated
   143  scan.rs:1119      grouping by a comodel row this identity may not read
    …
```

**STATE THE FIXTURE, because a census does not scale — a small one hides whole
classes.** The same corpus generator on base+mail (33 modules, 8,254 cases)
gives 1,241 refusals over 13 sites, and the difference is not a factor:

| | base+mail | 162 modules |
|---|---|---|
| refusals | 1,241 | 4,221 |
| distinct **refusal** sites | 13 | 23 |
| refusal sites the smaller fixture never reached | — | **13** |

Two of the top eight at scale are **absent** from the small fixture entirely —
`record rules could not be evaluated` (283) and `grouping by a comodel row this
identity may not read` (143) — and `defines a custom search method` moves from
21 hits at rank 8 to 450 at rank 3. A backlog built on base+mail would have put
effort into the wrong three things and never seen the record-rule class at all.
The tail is where it shows worst: an unparsable `domain_force`, an `_inherits`
field missing from the registry, a one2many whose inverse has no column — each
a handful of hits, none reachable without the modules that declare them.

**The first census, on base+mail, read 5,726 refusals and 4,700 of them were
not refusals.** The reachability walk asks *"does this request name groupby
fields?"* and *"does this leaf path resolve?"* by calling the demanding form and
discarding the error — so a plain `search_read` filed one refusal for having no
groupby (62% of the census) and every `display_name` leaf filed one for not
being a registry field (19%). The two loudest rows of the work list were the
engine asking itself questions.

A question gets its own form next to the demand, and neither refuses: `lookup`
beside `get`, `groupby_names_seen` beside `groupby_names`, `normalize_path_seen`
beside `normalize_path`, `parse_nested` beside `parse`. The same corpus then
read **1,241** against the **1,233** refused cases `harness/verify.sh` counts
through a completely separate path — and that agreement, not the number itself,
is what made it worth acting on. Cross-check a census against something that
was not derived from it before believing any of it.

Four rules fall out of it, and they are the ones to keep. **A census needs
both of the first two and neither implies the other** — a count can be inflated
by questions nobody asked, or deflated by sites that refuse and never report,
and each failure looks fine from the other axis:

- **Null control: a corpus that is fully handled must log zero refusals.** That
  is what finds a speculative caller. This census failed it.
- **Scale: a census is a property of its fixture, not of the engine.** Thirteen
  of the twenty-three sites above are unreachable on base+mail, so a backlog
  built there is confidently wrong about its own top three. Say which fixture a
  census came from, and re-measure before acting on a ranking.
- **Completeness: every path that can fail must report somewhere.** `refuse!` /
  `refusal!` / `deny_access!` cover their sites by construction, so what matters
  is the paths that BYPASS them — a bare `bail!` or `anyhow!` is invisible to
  every target. `test_every_kernel_failure_path_reports_somewhere` pins the 14
  that exist: two kernel invariant violations that are defects rather than
  refusals and log at `error` (visible under the default `warn` filter), the
  dsn rejections, and the server's, each answered by `handle_call`, which logs
  status and kind. A fifteenth fails the test until somebody decides which it
  is.
- **A helper that reports why it stopped must report WHERE**, as
  `concat!(file!(), ":", line!())`, and let the caller decide whether that is a
  refusal (`refusal_at!`). Wrapping it in one `map_err` collapses every cause
  the helper has onto one row and the census silently loses resolution.

### Removing the campaign logging

The campaign surface is mechanically identifiable, which is the point:

```sh
grep -rn 'target: "odoo_kernel::' kernel/src server/src engine-py/src   # the rust half
grep -rn '_gate_logger\|_call_logger' engine-py/python                 # the python half: all campaign
grep -rn '_logger\.' engine-py/python/rust_db_shim.py                  # mixed — read the table below
```

`odoo.rust_kernel.gate` and `odoo.rust_kernel.call` are campaign loggers
outright: delete the two module-level handles and every use goes with them.
`odoo.rust_kernel.pool` in `rust_db_shim.py` is **mixed** — the pool-fill and
interception lines are campaign, the teardown and `close_db` ones are the
permanent fix in the table below. `odoo.rust_kernel.routing` predates the
campaign.

`addons/rust_engine/__init__.py` is the third place and the one a grep for
loggers does not obviously point at: its arming summary, the connector-dsn
keywords and the export-versus-build split in `_build_kernel` are campaign.
`_open_trace_level()` is campaign **infrastructure** rather than a fix — it
exists because Odoo resolves a `--log-handler name:LEVEL` before any addon
loads, so `:TRACE` read as INFO; once the `trace` events are gone nothing needs
it, and until then removing it makes the finest level unreachable rather than
merely quiet.

**What the campaign FOUND does not come out with it.** The removal is of
scaffolding, not of fixes. These are permanent and a grep-and-delete pass takes
them if nobody is looking:

| stays | why it is not scaffolding |
|---|---|
| the dsn de-duplication (`_dsn_with_kwargs`) | without it the engine cannot open a connection when `db_host` is unset |
| the `jsonb = integer` join (`OrderJoin` carrying an `Expr`) | ordering by a company-dependent many2one killed the statement |
| the question/demand pairs and their tests | `lookup`, `groupby_names_seen`, `normalize_path_seen`, `parse_nested` |
| the battery's derived conf, the parity gate's named baseline | two gates that could not report what they were asked |
| the two `error!` lines on the internal invariants | **they look exactly like campaign lines** and are the only report those two paths have |
| `_close_quietly` and the unhashable-cursor warnings | a suppressed teardown failure leaks a backend; a failed taint lets a routed read answer from before a write |

Four of the targets predate the campaign and stay: `dispatch`, `sql`, `rules`,
`signal`. The rest, and the `trace`-level events under the four, are the
campaign's and come out with it. `odoo_kernel::refusal` and
`odoo_kernel::access` are produced by the `refusal!`, `refusal_at!` and
`deny_access!` macros in `kernel/src/error.rs`, so removing them is three edits
and not a sweep.

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

**Declined here does not mean conceded.** Replacement does not need SQL that
imitates CPython's `str()`; it needs that Python to run on this engine, which is
exactly what M3 embeds an interpreter to do. So this gap closes by moving the
boundary rather than by widening the compiler, and the refusal census above is
the list of what moving it has to cover.

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

- **Non-stored related fields are read as SQL only where Odoo reads them as
  SQL.** `_traverse_related_sql` allows the correlated subquery for `env.su`,
  `compute_sudo` and `inherited` fields, and computes every other related
  field in Python under the caller's own access. The subquery carries no ACL
  and no rules, so the kernel used to read the comodel unfiltered for exactly
  the fields Odoo would not; it now refuses them for a non-superuser caller
  (in `search_read` fields, `_read_group` groupbys and aggregates, and
  `ORDER BY`), and the export carries `inherited` beside `compute_sudo` so it
  can tell. A domain leaf on such a field is unaffected: `related_search`
  already follows `search_related` and applies the comodel's rules.
- **Domain nesting is capped at 100 structural levels**, as Odoo's
  `MAX_DOMAIN_NESTING` caps it, and counted the same way: a run of the same
  n-ary operator is one level (the parser flattens it, as `DomainNary` does)
  and `!!x` is `x`. Before that, `Compiler::MAX_DEPTH` counted only `any`
  subqueries, and a flat prefix domain of ten thousand `"!"` tokens — well
  inside the 256 KiB request body — built a tree that deep and aborted the
  process on the recursion; the parser refuses it now, before any walk exists.
- **Python model overrides** (`_search`, `_compute_display_name`, computed
  fields): invisible to a registry built from the DB alone. This is the
  fundamental M2 problem — the business-logic layer. Fields declaring
  `search=` are detected via the live-registry export and refused; the
  `ir_model` bootstrap cannot see them, so it is not a safe primary source —
  `serve`, `query`, `run-corpus` and `bench` refuse to start without
  `--export`; `inspect` is the one command that reads the bootstrap.
- **The fork's `ir.rule.composition` is read where it exists, assumed `grant`
  where it does not.** The loader asks `pg_attribute` for the column through
  `'ir_rule'::regclass` before selecting it; a stock Odoo database has no such
  column, and selecting it unconditionally failed the whole security load
  there. Tested against a live PostgreSQL with and without the column
  (`kernel/tests/db_rules.rs`, ignored unless `RUSTORM_TEST_DSN` is set).
- **25 of 222 ruled models on a real database** cannot have their rules
  compiled and fall back to Python — `account.move`, `account.move.line`,
  `sale.order`, `sale.order.line`, `hr.employee` among them. Every one is now
  the same cause: a rule interpolating a **non-stored Python compute**
  (`res.users.crm_team_ids`, `res.users.employee_id`,
  `res.company.country_id`). The first two also declare `search=`, so both the
  value and the domain leaf need Python. This is the measured size of the
  business-logic problem, not an estimate.
- **`edit_translations` is served from Python.** The shim sends `env._lang`
  rather than `context['lang']`; the two agree except under
  `edit_translations` / `check_translations`, where Odoo reads translated
  columns through a `_xx_XX` pseudo-language the kernel's `res_lang` check
  refuses — which is the fallback that mode needs. Before, the plain language
  was served there and the translation editor showed translated values where
  Odoo shows the sources.
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

## The references were the engine: kernel sweep, fuzz and cursor parity read an engine against itself

**Correction to every battery reading in this file taken through `verify.sh`
in this workspace until 2026-09-12**, including the figures in the two
commits before this one. The kernel still holds -- measured again below
against a pure Python reference -- but the earlier readings did not show
that.

`verify.sh` rewrites the conf's `rust_engine_db` to the database it verifies,
because stages such as the replay controls and the copy encoder need the
engine armed there. Every stage shared that conf, so the stages that produce
the REFERENCE a comparison is made against loaded the addon too:

```
                                  what it claimed      what it ran        measured
gen_expected (kernel sweep,       Python answers       routing ON, on     3,601 calls routed to the
fuzz, shadow corpus, soak)                             the kernel          kernel in one sweep baseline;
                                                                           666 in one fuzz seed's
cursor parity, psycopg leg        psycopg cursor       FakeConnection     type(env.cr._cnx) under the
                                                                           armed conf
copy encoder, psycopg batch       psycopg COPY         the rust cursor    same conf
```

So for most of what those stages called compared, the "expected" side was the
kernel, through whatever `engine_py` the process imported -- often the venv's
copy from the day before -- and the cursor gate compared the rust cursor with
the rust cursor, reading `ONLY-RUST=0` and "3 moved into the both-legs
bucket", which is the shape of an improvement and was agreement with itself.

It surfaced as a fuzz case whose "Python" answer moved between batteries:
`('currency_id.decimal_places', '=?', 0)` read 0 as the expected value in two
of them and 246 in a third, each from a baseline process that had routed
hundreds of calls through a kernel. Which engine answered that one case in
each is not recoverable from the logs. What Python itself answers, probed
directly with routing off, is 0 -- and the kernel had a real defect on that
shape as well (next section), which a reference that moved could not show.

**The fix is structural, not a flag.** `verify.sh` writes a second conf,
`python.conf`, with `rust_engine` out of `server_wide_modules` and every
`rust_engine_*` key removed, and runs the reference-producing stages on it:
the four `gen_expected` sites, both legs of cursor parity (the rust leg
installs its shim explicitly, as it always did), and the copy encoder, which
installs its shim after writing its psycopg batch. A stage that wants the
engine installs it; a stage that wants Python gets a process without it.

Each also proves it now, rather than relying on the conf:

- `gen_expected.py` turns routing off before its first case and refuses to
  write a baseline if the shim's counter moved while it ran.
- `cursor_suite.py` refuses to write a psycopg leg that drew a
  `FakeConnection`, the mirror of the check its rust leg always had, and
  `cursor_parity.sh` refuses to score one without the marker.
- `copy_path.py` refuses to write its reference batch through the rust
  cursor.

Measured again on committed trees only -- detached `odoo` and `enterprise`
worktrees at their HEADs, because the shared trees held another session's
uncommitted `ir.cron` rework the database schema did not match:

```
kernel sweep     7,679/9,097   4,021 compared against pure Python, 0 mismatches
fuzz             3 seeds pass  against pure Python baselines
cursor parity    379 tests     ONLY-RUST 3 -- exactly the three the baseline
                               names: two pipeline-mode cases and one COPY
                               type resolution -- on an honest psycopg leg
```

The cursor baseline was right all along: it had been measured on a setup that
did not arm the engine. The "3 moved, 0 only-rust" readings were the ones that
were wrong.

**A related path bug, fixed with it.** `parity.sh` and `cursor_parity.sh`
read `RUSTORM_ODOO` where every other stage reads `RUSTORM_ODOO_ROOT`, and
`write_sql_contract.py` read the workspace tree unconditionally, so a battery
pointed at a worktree ran those stages against the shared tree. All three honour
`RUSTORM_ODOO_ROOT` now.

## `=?` on a dotted path was decided outside the traversal

`('currency_id.decimal_places', '=?', 0)` on `mail.tracking.value`, counted
at superuser: Python 0, kernel 232. Odoo's `DomainCondition._optimize_step`
splits a relational dotted path into `currency_id any [decimal_places =? 0]`
at the BASIC level, before any operator optimisation, so the `=?` folds to
TRUE INSIDE the sub-domain and the whole condition reads "the currency is
set". The kernel applied the `=?` rewrite to the leaf first and folded the
whole condition to TRUE -- every row.

The fuzzer's seed 3 had generated it, and it read as intermittent: failed,
passed, failed across three batteries on one database. It is deterministic in
the data. The case disagrees exactly when some rows have the relation unset,
and `mail.tracking.value` grows between batteries, because the write-path
stage writes tracked fields.

The fix splits first, as Odoo does. A second test compiles twenty operator
and value pairs, the ones whose leaf can fold to a constant, both dotted and
in their `any` form and requires them equal. `=?` was the only one that was
not.

## Recorded traffic said the routed path was slower, and why

Every speed figure above was the kernel alone, a synthetic corpus, or an
HTTP burn-in. `harness/traffic_bench.py` replays calls Odoo actually served --
the byte-parity stage records 1,792 of them, `web_search_read`,
`web_read_group`, `search_read`, `search_count`, `name_search`, `web_read` --
in one process, with the method shim routing and with it off, interleaved.
Its first reading, on committed `odoo` and `enterprise` worktrees:

```
                     calls   routing off   routing on
web_search_read        584       2.705 s      2.658 s    0.98x
web_read_group         286       0.805 s      3.176 s    3.95x
search_read            292       0.358 s      0.265 s    0.74x
web_read               138       0.300 s      0.383 s    1.27x
name_search            230       0.272 s      0.314 s    1.15x
search_count           292       0.160 s      0.212 s    1.32x
all                   1792       4.601 s      7.008 s    1.52x
```

The workspace conf ships with routing on. Every answer was exact.

**`web_read_group` browsed each group on its own.** The routed `_read_group`
rebuilt each many2one group value as `env[comodel].browse(id)`, a recordset
whose prefetch set is itself. `web_read_group` then reads the groups' names,
and each name was a fetch of one record's one field: routed, the replay ran
6,008 statements where Python ran 678, 5,372 of them single-field fetches.
Python's `_read_group_postprocess_groupby` gives every group of a column the
column's values as prefetch ids; the shim does the same now. A runtime
contract routes a two-country `_read_group` and requires the groups to share a
prefetch set and their names to cost at most two queries; with the fix
reverted it fails.

```
                     routing off   routing on      after the fix
web_read_group           1.256 s      1.192 s          0.95x
all                      6.193 s      5.645 s          0.91x
```

(The machine was loaded differently between the two readings; compare within
a row.) Measured again after the unsound registry shortcut was removed from the
dispatch path: all calls 0.94x, `search_read` 0.65x, `web_search_read` 0.91x,
`web_read_group` 1.02x, `name_search` 1.02x. `search_count` at 1.15x and `web_read` at 1.06x are still slower
routed. A routed call pays a savepoint, a signalling read, the query and a
release, four round trips where Python's own statement is one; the port's
`search` removed the middle two, and the dispatch path has not been given the
same treatment.

**A profile pointed at record rules, and the timers said otherwise.** Under
`cProfile`, `_check_access` was 32% of the replay and `filtered_domain` --
evaluating the caller's rules against records in memory -- most of that. A
prototype answered the rule half of `_check_access('read')` natively, as
`SELECT id FROM t WHERE id = ANY(ids) AND <the kernel's rule WHERE>`, flushing
the columns the kernel reports. It agreed with Python on all 1,804 decisions
it made and saved nothing measurable: 4.99 s against 4.83 s, 4.99 against
4.79, 5.19 against 5.32. The profiler had doubled the replay's wall time, and
that overhead lands on exactly the many small Python calls a predicate
evaluation is made of. Boundary timers, with no profiler, read the replay as:

```
_check_access                    22%   most of it ACL lookup and recordset work
backend.fetch, with its SQL      20%
all SQL execution                19%
_search                           9.5%
backend.search compile            5%
_read_format                      4.5%
optimize_full                     3.5%
```

So the rule check was not made native, and the port's methods, one at a time,
bound what they can save at a few percent of a request. The gain is in whole
calls leaving Python, which is what the method shim does -- once its own
round trips are paid for.

**The totals hid that routed calls already win.** Each method's line mixed
the calls the shim served with the calls it refused and handed to Python, so
a refusal's cost read as the kernel's. The bench now records, per call,
whether the routed leg reached the kernel, and splits every method into its
routed and fallback calls; it names the five slowest fallbacks too. On
committed worktrees:

```
                     calls   all     routed           fallback
web_search_read        584   0.93x   484   0.56x      100   1.03x
web_read_group         286   1.01x   254   1.01x       32   1.04x
search_read            292   0.62x   252   0.51x       40   1.16x
web_read               138   1.07x   110   1.04x       28   1.16x
name_search            230   0.95x   140   0.71x       90   1.08x
search_count           292   1.00x   260   0.93x       32   1.06x
all                   1822   0.94x
```

Four `iap.account.web_search_read` calls of about half a second each are
most of the `web_search_read` total on both legs. They fall back, and their
time is `iap.account`'s own `web_read` override, not the ORM's. A fallback
pays the refused dispatch on top of Python's call, 3 to 16 percent.

## An extension built before its sources is refused

The same bench, run from a shell that imported the venv's `engine_py`, read
routed `web_read_group` at 4.48x again: the defect fixed two sections above.
The shims are compiled into the extension with `include_str!`, so a build that
predates a change to them imports cleanly and serves the old code. The venv's
copy was a day older than the prefetch fix, and the workspace conf arms routing
with it.

`engine-py/build.rs` now checksums the sources the extension is built from --
the workspace manifest and lock, `kernel/src`, `engine-py/src` and the
embedded Python modules -- into `engine_py.__source_crc__`, with the cargo
profile in `__profile__`. At startup `rust_engine` computes the same checksum
over its own checkout and, on a mismatch, an unstamped build or a debug build,
logs why at ERROR and leaves the server entirely on Python. This is the check
the fork applies to `odoo_rust`, and for the same reason: an absent extension
is slow, a stale one is wrong. An addon deployed without its checkout has
nothing to compare with and arms as before; `RUSTORM_SKIP_FRESHNESS_CHECK=1`
bypasses it.

```
venv engine_py.so    refusing to arm for rustorm_o31: ... predates the source stamp
target/release       rust_engine armed for rustorm_o31 in mode 'on'
```

Two shim tests pin it. One requires the extension under test to carry the
checkout's checksum, so the battery's first stage fails by name when
`target/release` is older than the tree. The other copies the inputs, edits an
embedded module and requires the build to be refused.

`harness/install_engine.sh` builds the extension and installs it into the
workspace venv by rename, so a running server keeps the file it mapped, then
asks the addon's own check whether the installed build is fresh. Every change
to the engine's sources needs it, or servers on the venv serve from Python.

## Routed `web_search_read` redacted many2one targets `web_read` keeps

`read()` and `web_read` answer a many2one whose target the user may not read
differently. `read()` goes through `Many2one.convert_to_read_multi`, which
redacts that target to `False`. `web_read` reads with `load=None`, keeping the
raw foreign key, then `_web_read_resolve_many2one` returns the id for a plain
spec, and `{"id": id}` when a name was asked for, adding the name only where
`_filtered_display_name_access` allows it.

The routed `web_search_read` built its many2ones from the kernel's label,
which is `read()`'s answer, and the routed `read(load=None)` took the id out
of the same redacted label. Every user of the probe database read every
table-backed model's first four stored many2ones, routing on and off:

```
                                    routed calls   disagreeing
web_search_read, plain and named          1,018            124
search_read and read()                      760              0
```

`res.users.company_id`, `res.country.currency_id`, `ir.actions.report.binding_model_id`
and `create_uid`/`write_uid` on half the base models were `False` routed and
an id in Python. No stage saw it, because every corpus reads as users who can
see those targets. With `rust_engine_verify_sample` above zero such a call
would also have quarantined the model.

The kernel's `search_read` takes two new lists. Fields in `raw_many2one` come
back as the bare foreign key with no label query. Fields in
`unredacted_many2one` keep their label, but a target the label query hid comes
back as its id instead of `False`. The shim chooses per column:

- **A plain many2one** is the raw id, which is what `web_read` returns.
- **A named many2one to a comodel the kernel can name** keeps the label when
  the target is visible, which is `web_read`'s answer too. Only the rows whose
  target was hidden go to web's own `_web_read_resolve_many2one`.
- **A named many2one to a comodel whose name Python computes** goes to web's
  resolver whole. Those calls used to fall back, so 16 more calls of the
  recorded traffic route.
- **`read(load=None)`** asks for every many2one raw.

Handing every many2one to web's resolver was correct and cost the speed:
routed `web_search_read` went from 0.56x to 1.06x Python. Keeping visible
labels brings it back to 0.56x.

`harness/web_many2one.py` ran the probe as a battery stage; on the previous
build it compared 1,778 calls and failed on `res.users` at uid 9 and
`res.country` at uid 4, and on this build it compared 1,966 with none
mismatching. It is now `harness/every_user.py`, the stage "every user": the
same users read every table-backed model through every routed method --
many2ones and x2manys through `web_search_read`, `search_read` and
`read(load=None)`, `search_count`, `display_name`, `name_search`,
`_read_group` and `web_read_group`. Each user reads in its default context,
with `active_test=False` and in every other installed language, and a user of
several companies reads with each allowed alone and all of them in both
orders, since the first allowed company is `env.company`. In about two and a
half minutes:

```
EVERY USER compared 19802 over 9 users in 31 contexts   0 mismatching
  search_count 2316, display_name 1678, name_search 1579,
  web_search_read many2one 2286, named 2286, x2many 972,
  search_read many2one 1983, x2many 984, read load=None 1146,
  _read_group 2286, web_read_group 2286
```

A runtime contract reads Belgium's currency, which a committed
rule hides, through `web_search_read` plain and named and `read(load=None)`.
The contract fails on the previous build.

## The Rust cursor sent `%%` inside a quoted literal as it was written

The same comparison over `/base`, 3,947 tests on `rustorm_o31`, had eight tests
failing only under routing. Two were `res.partner` addresses rendered as their
format string and one a CHECK constraint that let a row through:

```
_display_address()   'TestCity 12345'  ->  '%(city)s %(zip)s %(nothing)s'
add_constraint       CHECK (name !~ '%')  stored as  CHECK (name !~ '%%')
```

psycopg splits a query on `%` without reading SQL: when parameters are passed,
an empty tuple included, `%%` is a percent and `%s` a placeholder wherever they
stand, inside a string literal as much as outside. Odoo's SQL is written for
that, `'%%'` in literals. `translate_placeholders` skipped quoted literals,
quoted identifiers, dollar-quoted bodies and comments, so a `%%` there reached
the server doubled. Every `SQL` object reaches the cursor with its parameter
tuple, so this was any fork statement with a percent in a literal: a stored
format, a constraint definition, a `LIKE 'x%%'` pattern.

```
                                          psycopg          rust, before
SELECT '%%(city)s' AS a, %s AS b  ['x']   '%(city)s'       '%%(city)s'
SQL("SELECT '%%' AS b")                   '%'              '%%'
SELECT %(v)s, 'x %% y'  {'v': 1}          'x % y'          'x %% y'
SELECT '%%(city)s' AS a  (no params)      '%%(city)s'      '%%(city)s'
```

The translation now reads `%` as psycopg does. Four unit tests had pinned the
quote-aware reading; a query they protected, `LIKE '%save%'` with parameters,
is one psycopg itself rejects, so no working caller could have depended on it.
The type probe runs five percent queries through both cursors; the committed
cursor fails three.

Running `/base` again, on odoo trees that peers had just synced, turned up five
more things.

- **A closed Rust connection kept its backend.** `RustConn.close()` rolled back
  and set a flag; the socket closed only when the last `Arc<Client>` dropped,
  which a live Python reference could put off for the life of the process.
  `/base` held 88 idle backends on `rustorm_o31`, the shared cluster refused
  every new connection, and a peer's suite failed. `close()` now takes the
  client out of the connection, and tokio-postgres sends Terminate. A runtime
  contract closes a connection and requires its backend gone from
  `pg_stat_activity`; the previous build keeps it. `TestCursorBulkMethods`
  alone peaked at 16 connections, now 3, and the whole of `/base` at 11 where
  psycopg's run peaks at 10.
- **A refused connection failed at once.** psycopg_pool keeps reconnecting in
  the background, so `getconn` waits until its timeout and raises
  `PoolTimeout`, which the fork's pool turns into `PoolError`. The shim raised
  "too many clients already" as a RuntimeError on the first try. It now retries
  with backoff until the deadline, then raises `PoolTimeout`.
- **The fork's persistence protocol grew four members** --
  `supports_recursive_queries`, `columns`, `descendants` and
  `read_group_rows` -- and `RustBackend` lacked them: 646 errors, every import
  among them. The port now delegates them explicitly, and delegates any member
  it does not know through `__getattr__`, counted under "not in this port's
  protocol", so the next addition degrades to Python instead of raising. The
  conformance test still requires each one to be named.
- **A grouping in a timezone PostgreSQL does not know logged nothing.** Python
  resolves the context `tz` against `pg_timezone_names`, maps an alias to its
  zone, and groups in UTC with a warning when neither is known. The shim now
  resolves it with the fork's own `_resolve_sql_timezone_name` and falls back
  to Python when it cannot.
- **A test run never built a kernel.** 10bc8e6 made the shim decline while
  `registry.ready` was false, to stop an ERROR traceback when a read during
  module loading exported a half-loaded registry. Odoo runs `--test-tags`
  tests inside loading, before `ready`, so those runs routed nothing. The shim
  now tries, and when the export refuses as incomplete it logs at debug, keeps
  the process's one attempt, and tries again once the registry has more models.

- **Routed `web_search_read` stamped an envelope version Python no longer
  sends.** The fork moved `web_search_read` from `@versioned_envelope` to
  `@versioned`, and it now reads through `search_fetch` and `web_read`; only
  `web_read` stamps the JSON-RPC envelope, and only when records came back.
  The shim stamped every routed page, so byte parity read 146 divergences, each
  an empty page with a `version` key. It now stamps a non-empty page only, and
  falls back for a model that overrides `search_fetch`.

The source stamp no longer covers `engine-py/src/bin`: building the type probe
changed the checksum without rebuilding the extension, and the addon refused a
build that was current.

```
/base, 3951 tests    no engine: 4 failed, 19 errors    routing on: 8 failed, 19 errors, routed=907
failing only under routing: the cursor-parity residuals -- two pipeline-mode checks, the
binary COPY type probe, and a KeyboardInterrupt during connection construction
```

## The cursor parity gate reaches zero, and could not tell a fixed test from a missing one

Odoo's own cursor suite run against both cursors had three tests failing only
under the Rust cursor. Two of them were already passing, and the gate could not
say so. The suite recorded only failures, errors and skips, so a baseline test
that passed on both legs looked exactly like one the fixture never ran, and the
branch that reports a fixed entry could never fire. It printed "not exercised by
this fixture" for a run in which both pipeline tests had passed under both
cursors. Every test the suite loads is now recorded as ok before failures are
overlaid.

The pipeline tests pass because the fork changed what they assert. Its
382726182c1d made `description` and `rowcount` sync inside an entered pipeline,
so a block's results are answerable from the second statement on. The Rust
cursor runs every statement immediately and always answered. Pipeline mode is
still not built, and no test distinguishes that from psycopg any more.

The third was a real disagreement, and wider than the test showed. COPY
`set_types` asked the encoder whether it could encode a NULL of each type, and
every type accepts a NULL. Over every base type in `pg_catalog` the Rust copy
accepted 89 types for which psycopg has no dumper, identically in text and
binary format, so Odoo's `_can_dump_binary` guard disagreed with the cursor it
guards. The copy wrapper now asks psycopg's own dumper lookup for the copy's
format first, which is what psycopg's `Copy.set_types` does.

The Rust leg also broke on the shared conf. It pointed the shim at the test
database but kept the connection identity the conf had armed for another one,
so no pool was intercepted and the vacuity guard stopped the run. The battery
passes a Python-only conf and never saw it.

```
cursor parity   ran psycopg=382 rust=382   not-ok both=19   ONLY-RUST 3 -> 0
```

The suite used to exclude `test_a_baseexception_during_construction_returns_the_connection`
from both legs, because its `KeyboardInterrupt` escaped and ended the run. The
interrupt was escaping from psycopg_pool's worker thread. The test patched the
connection's `cursor` for the whole process and raised on the first call
anywhere, and after another test the first call was the worker resetting a
returned connection. The dead worker then starved every later borrow, which is
why Odoo's own runner showed six COPY tests timing out on the pool with no engine
at all. odoo a700313e3293 raises only when the caller is `Cursor.__init__`, and
patches the class of the connection the pool lends, so the test now runs and
passes under both cursors and the exclusion is gone.

```
/base cursor classes, --db_maxconn=8   no engine 1 failed, 6 errors of 25 -> 0 of 25
                                       rust cursor 1 failed of 25 -> 0 of 25
cursor parity                          383 tests, ONLY-RUST 0, nothing excluded
/base, 3955 tests                      no engine 5 failed, 18 errors; routing on the same
                                       5 and 18 by name, routed=916
```

## `web_read` routes, and labels visible many2one targets in the kernel

A form view loads its record through `web_read`, and only the `read` inside it
was routed: web's `_web_read_resolve_many2one` then ran Python access checks and
name reads for every many2one, and routed `web_read` stayed at 0.97x Python on
recorded traffic. The shim now routes `web_read` the way it routes
`web_search_read`: the kernel reads the ids with active_test off, plain
many2ones raw, named ones labelled where the target is visible and returned as
the bare id where it is not, and only those go to web's resolver. A record the
kernel does not return -- deleted, or hidden by a rule -- falls back, so Python
raises its own MissingError or AccessError. It is gated like `read`, so a model
overriding `read` or `_check_access` falls back, and it keeps
`@api.readonly` and `@versioned_envelope`, whose stamp the fork's
`web_search_read` now relies on.

```
recorded traffic       routing off    routing on
web_read               0.270 s        0.138 s    0.51x   (was 0.97x)
all 1822 calls         2.721 s        2.049 s    0.75x   (was 0.82x)
```

The every-user stage reads `web_read` plain, named and on x2manys at every user
and context: 2,995 of its 23,216 comparisons, none differing.

The same sync added `ancestors`, `read_m2m_groups`, `set_parent_paths`,
`move_parent_paths` and `records_with_parent_changed` to `StorageBackend`,
removed `read_m2m_pairs`, and dropped four support flags
(`supports_parent_store`, `supports_record_rules`, `supports_joined_m2m_read`,
`supports_translation_terms`). The port's pass-through delegated the new methods
from the first call; the conformance test named them, and the port now lists
exactly the fork's protocol again.

## The fork contract pins behaviour, not only names

`harness/fork_contract.py` checked that every fork name the engine imports or
patches exists. The fork changes that hurt on 2026-09-13 kept every name:
`web_search_read` began reading through `search_fetch` and counting through
`search_count`, and its envelope versioning moved to `web_read`. Each silently
changed what a routed answer must be, and one was caught by byte parity only
because its corpus happened to page past the end.

The contract now also pins six behaviours the shim relies on, read from the
fork's source with `ast`, needing no database: the decorators and calls of
`web_search_read`, `_format_web_search_read_results`, `web_read` and `_web_read`,
that `Many2one.convert_to_read_multi` asks `_filtered_display_name_access`, and
that `fetch` runs `check_access`. A change fails the first battery stage with
the assumption it breaks. Run against `web_read.py` from before a25b7418c7a4:

```
behaviour Base.web_search_read: no longer calls ['search_fetch'] -- the shim
assumes routed web_search_read ... falls back for a model overriding search_fetch
```

## A domain through a Python search method is resolved before the kernel sees it

A non-stored field with a `search=` method -- `discuss.channel.is_member`,
`res.groups.all_user_ids` -- is decided by Python, and the kernel refused any
domain that named one: 283 sweep cases between those two alone. Python's own
`_search` runs `Domain.optimize_full`, which calls those methods and leaves a
domain over stored columns (`is_member = True` becomes `channel_member_ids any
[partner_id in [3]]`, `all_user_ids in [2]` an id list). When a domain reaches
such a field -- at the root, through a dotted path, or inside `any` --
`_request` flushes, applies `optimize_full`, and sends the result marked
`trusted_domain`, so an `any!` that a field's own bypass produced compiles as
the port's does; dispatch now reads that flag too. `display_name` and related
fields keep the kernel's own handling. An exception while resolving, an access
error included, refuses the call rather than counting against the breaker.

Across every user of `rustorm_o31`, six such domains through `search_count`
and `search_read`: 108 comparisons, 19 routed where 3 were, none differing.
`discuss.channel` itself still falls back -- its record rules name `is_member`
and the kernel compiles rules from the export -- and `res.groups` overrides
`_search`. The tours route 215 calls, share 0.34, where they routed 172 to
190.

The same pass found the fork's `web_search_read` computing its length through
`self.search_count`, so a model overriding `search_count` now falls back, and
the gate's verdict cache keys on the identity of the methods it compared: a
method patched at runtime is seen, as `TestWebSearchRead` patches
`res.currency.search_count`.

## Odoo's own ORM test modules, with routing on

Installing a fresh engine into the workspace venv put routing under every
server on `p314o19m.conf`. The battery's "upstream suites" stage runs two small
suites that reach the kernel three times, so it said little about that. Odoo's
own `test_orm`, `test_read_group`, `test_access_rights`, `test_search_panel` and
`test_inherits`, 1,444 tests, were run on a database of their own with no engine
and then with routing on:

```
no engine        2 failed, 3 errors
routing on      34 failed, 4 errors      20 tests failing only under routing
```

Three were the engine's.

- **The Rust cursor wrote a tuple inside JSON as its `str()`.** `py_to_json`
  had cases for dicts and lists and none for tuples, so a properties
  definition's `selection: [("draft", "Draft")]` was stored as
  `["('draft', 'Draft')"]`, and the next write of the record raised `Wrong
  options`. Dict keys went through `str()` too, and an integer past 64 bits
  became a float. JSON parameters are now the text `json.dumps` produces, which
  is what psycopg's `Json` adapter sends, encoded as JSONB's binary form so a
  binary COPY accepts them.
- **The Rust cursor read a 20-digit JSON integer as a float.** It decoded JSONB
  through `serde_json::Value`, where a number past `i64` is an `f64`. psycopg
  decodes with `json.loads`, which keeps it an int. The cursor now reads the
  text and calls `json.loads`.
- **A routed `_read_group` ordered by an `_order` changed at runtime.** The
  test sets `res.partner._order` on the class; the kernel orders from the
  export taken when it was built. The shim now snapshots every model's `_order`
  when the kernel is built, and refuses a call whose model, or a many2one
  comodel its order or groupby reaches, has a different one.

The other thirteen were `test_orm`'s uniform-update tests, which spy on
`PostgresBackend._update_rows_uniform` and `_update_rows_values` through
`env.backend` -- a `RustBackend` once the port is installed. They now patch the
test transaction's backend to `POSTGRES_BACKEND` (odoo cc4741849aff).

A read during module loading also made the shim build a kernel from a
half-loaded registry: an ERROR traceback, and the process's one build attempt
spent. `_ensure_kernel` now declines while the registry is not ready, and the
gate reports "registry still loading".

```
no engine        2 failed, 3 errors of 1444
routing on       2 failed, 3 errors of 1444, the same tests, routed=96
```

`harness/orm_tests.sh` runs both legs and fails when a test fails only under
routing, or when the routing leg did not route. It is the battery stage "orm
test modules", run when `RUSTORM_ORM_TEST_DB` names a database with those
modules installed. Read over the first routing-on log it lists 24 failing
tests, subtests included. The type probe gained wrapped JSON values: the
committed cursor fails three of them.

## A many2one to a model that decides access in Python was labelled from rules

The kernel labels a many2one target from the comodel's `_rec_name`, and hides
the ones record rules hide. Python labels through
`Many2one.convert_to_read_multi`, which asks `_filtered_display_name_access`,
which asks `_filtered_access("read")`, which runs the comodel's
`_check_access`. Five models in this database override that method --
`mail.message`, `mail.activity`, `mail.followers`, `mail.scheduled.message`
and `ir.attachment` -- and the kernel read none of them.

It stayed hidden because the gate also refused any read with a many2one to a
comodel whose name Python computes, and the reads that reached these targets
usually carried one. Letting Python label those columns, so those reads could
route, exposed it at once: the every-user stage found `mail.mail.mail_message_id`
read at the admin as `(8788, 'Security Update: Password Changed')` routed and
`False` from Python, and `{"id": 8788}` against a name through
`web_search_read`.

- **The export records `check_access_pure`**, whether `_check_access` is
  Odoo's own. The kernel refuses to label a target whose comodel's is not,
  for anyone but the superuser, whatever called it.
- **The shim labels those columns in Python.** `search_read` and `read` ask
  the kernel for them raw and pass them to `convert_to_read_multi`, the
  labelling `read()` uses. `web_search_read` sends them to web's own resolver
  whole. A comodel whose name Python computes goes the same way, so those
  reads route instead of falling back.
- **`read(ids)` on a model that overrides `_check_access` falls back.**
  Python's `fetch` runs `check_access("read")` on the ids it is given, which
  `search_fetch` does not, so only `read` needs it.

A runtime contract dispatches `mail.mail.search_read` with `mail_message_id`
at the admin and requires the kernel to refuse it, and to answer when the
column is asked for raw. It fails on the previous build with `KernelRefused
not raised`. The every-user stage now reads 20,095 routed calls with none
disagreeing, and recorded traffic routes 260 `search_read` calls where it
routed 252, at the same speed.

## A routed call no longer waits on its savepoint

A routed call ran the kernel inside `env.cr.savepoint(flush=False)`, so a
statement the kernel sent that failed would not abort the caller's
transaction. That is three round trips where Python's own call is one:
`SAVEPOINT`, the kernel's query, `RELEASE`, each waiting on the reply before
the next goes out. For `search_count`, whose query is as cheap as either of the
others, that was the whole of its slowness.

`RustKernel.dispatch` now opens the savepoint itself, and only enqueues
`SAVEPOINT` and, when the dispatch succeeds, `RELEASE`. tokio-postgres writes
requests in the order they are queued and pages past the replies of a request
whose future was dropped, so the kernel's first query leaves without waiting
on `SAVEPOINT`, and the caller's next statement queues behind `RELEASE`. A
dispatch that fails waits on `ROLLBACK TO SAVEPOINT; RELEASE SAVEPOINT` in one
round trip and drops the connection's prepared plans, as Python's own rollback
to a savepoint did.

Recorded traffic, the savepoint build against the one before, alternating
builds, best of three rounds each:

```
routed calls        before          kernel savepoint
web_search_read     0.55x  0.56x    0.41x  0.37x
search_read         0.53x  0.52x    0.39x  0.36x
name_search         0.72x  0.76x    0.52x  0.50x
search_count        1.08x  1.07x    0.70x  0.63x
web_read_group      0.98x  1.04x    0.99x  0.88x
```

`web_read_group` and `web_read` stay near parity, between 0.88x and 0.99x;
where their time goes has not been measured.

A runtime contract dispatches a call that answers, one the kernel refuses and
one that reads a row, and after each runs a statement and requires the
kernel's savepoint to be gone. Releasing a leaked one would succeed and take
the Python savepoint above it along, so only an error naming
`rust_kernel_dispatch` passes. A build that sends no `RELEASE` fails it: `a
kernel savepoint was left open after search_count`. The existing contract that
times a dispatch out on a locked table still requires the transaction usable
afterwards.

**The fork's persistence protocol gained a member.** `StorageBackend` now
declares `sequences`, the sequence store that became a port beside the row
port. The shim conformance test read it from the fork and failed; `RustBackend`
mirrors its delegate's store as it mirrors the support flags.

## A write to `res.users` taints a cursor only through Odoo's own invalidation fields

"Cursor wrote a security model" is the most frequent reason a browser-tour
read falls back to Python. Every write to `res.users` set it. With the taint's
cause logged at debug, one run of the eight tour tags read:

```
28  res.users (group_ids)             6  res.users (company_ids)
13  res.users (odoobot_state)         6  res.groups (implied_ids)
12  res.users (image_1920)            6  ir.default (create/unlink)
12  res.users (create/unlink)         3  res.users (tz)
 8  res.company (alias_domain_id)     ... and a tail of name, email, lang
```

Odoo names the fields whose change invalidates what it caches about a user --
`res.users._get_fields_invalidation()`: groups, active, lang, tz, companies,
and the session-token fields, extendable by addons -- and clears its caches on
exactly those. A write outside that set leaves the rule domains Python answers
from unchanged, so the kernel's snapshot of the user stays in step with
Python's. The shim now taints a `res.users` write only when it touches that
set; creates, unlinks and every other security model taint as before. A
runtime contract writes a signature and requires the next read to route with
Python's answer, then writes a group and requires the next read to fall back.

**The tours did not move** (routed 186, share 0.29, before and after), and the
reason is worth stating: a tour runs its requests inside one test transaction,
and the test's own setup writes groups and companies, so the cursor is tainted
by genuine security writes before the harmless ones arrive. In production a
request is its own transaction; what this changes there is a request that
writes a preference, an avatar or a presence state and then reads, which a tour
cannot show.

## `web_read_group` routes whole

Routed `web_read_group` read 0.95x Python on recorded traffic because only its
`_read_group` was routed: web then formatted the groups in Python, and a third
of the call went to deciding which group labels the user may see. The shim now
answers `formatted_read_group`, the step `web_read_group` builds on, in one
kernel dispatch: the groups, their aggregates and their many2one labels come
back together, and the shim assembles web's group dicts -- the value, the
`__extra_domain` web ANDs from each groupby, the aggregates -- without
browsing a record.

Python labels a group whose target the user cannot read with `""`, not with an
error, and names only the visible ones under sudo. The kernel's label step
refused any id it could not name; `groupby_hidden_labels_empty` asks it for
Python's answer instead, and it still refuses when the comodel decides its
read path in Python, where it cannot tell which ids are visible.

What still goes to Python, each for a reason the shim names: `having`; no
groupby; a model overriding any of web's five formatting hooks;
`fill_temporal`; a groupby with `group_expand`; date, datetime, many2many,
properties, dotted or granularity groupbys; a many2one whose comodel's labels
are decided in Python.

Recorded traffic, best of three rounds, on odoo `3fe945ee5eee`:

```
                          calls   python    routed
all traffic                1802   4.265 s   2.639 s   0.62x
web_read_group              282   0.899 s   0.535 s   0.60x
  routed                    246   0.632 s   0.247 s   0.39x   (0.95x before)
```

The shadow replay of the same capture compares 1,502 calls with 0 differences,
and "every user" compares 4,474 `web_read_group` calls across 6 users and 24
contexts with 0 mismatches.

**Real grouped views barely route yet, and the capture says why.** The
capture recorded only the methods the replay already covered, so it now also
records `formatted_read_group`, `formatted_read_grouping_sets` and
`read_progress_bar`. Twelve tour classes over project, helpdesk, planning,
account, hr and knowledge made 26 `web_read_group`, 20 `formatted_read_group`
and 32 `read_progress_bar` calls -- kanban columns with `auto_unfold`, a stage
groupby with `group_expand`, progress bars, a burndown graph by `date:week` --
and the replay of that capture routes 0.09 of its calls, with 0 differences.
The grouped calls fall back for two reasons in about equal measure:

```
project.task       web_read_group / formatted_read_group  group_expand; _read_group overridden
helpdesk.ticket    web_read_group / formatted_read_group  group_expand; _search overridden
knowledge.article  web_read_group                          group_expand
burndown report    formatted_read_group date:week          fill_temporal; _read_group overridden
```

`group_expand` and `fill_temporal` are grouping, and the next steps here. The
overrides are not access decisions but argument rewrites the gate cannot see
through: `project.task._read_group` renames a `triage_id` groupby to
`triage_ids`, and `helpdesk.ticket._search` turns an order by `ticket_ref`
into one by `id`. Each refuses every grouped call on its model, whatever it
groups by.

## `group_expand` routes

A kanban column list asks for its groupby with `read_group_expand` in the
context, and a field with `group_expand` then adds the columns no record is in
yet -- the empty stages. The routed `formatted_read_group` refused that shape.
It now takes the kernel's groups, rebuilds them as the records web's
`_web_read_group_expand` expects, and calls that hook unchanged: the
`group_expand` method is business logic and stays Python, under the same
`offset` and `limit` conditions web applies. The kernel's labels are kept for
the groups it returned; only the columns the expansion added are labelled by
web's own formatter, and since display-name visibility is decided per record,
labelling that subset gives the answer labelling all of them would. `__fold`
is read under sudo from the stage, as web does.

Every-user gained the shape -- `web_read_group` with `read_group_expand` over a
stored many2one or selection that has `group_expand` -- and compares 100 such
calls with 0 mismatches. The captured kanban traffic moves little yet:
`knowledge.article` routes, `project.task` and `helpdesk.ticket` stay behind
the argument-rewriting overrides above.

## An order term only Python can write reaches the kernel as its SQL

The project kanban reads `is_user_favorite DESC, sequence ASC, name ASC, id ASC`, and
`is_user_favorite` is ordered by `mixin.user.favorite._order_field_to_sql`, which the
export records as a hook on that one field. The kernel drops a hooked field, so every
kanban read refused over its first order term. For a `search_read`, the shim now asks
the model's own `_order_field_to_sql` for each non-stored, non-related term, on an empty
`Query` of the model's table and with no direction; a hook that adds a join, or a field
Python cannot order either, leaves the order as it was. The expression travels with the
request's nonce as `order_fragments`, the term becomes `$n` with its direction and nulls,
and the kernel places the fragment there -- on the root table only, never through a
traversal.

The export's purity check looked at the first class in the MRO defining a hook. Every
class defining it must be transparent, and their scopes add up: project.project's
`_order_field_to_sql` comes from the activity mixin first and the favorite mixin after,
so `is_user_favorite` was not recorded as hooked. It is non-stored, so nothing was ordered
wrongly; a deeper hook on a stored field would have been.

On the grouped-view tours the refused calls fall from 67 to 33 and routing reads 0.92x
Python; routed `web_search_read` goes from 0.96x to 0.81x.

## A many2many groupby joins its relation table, and a read of a vanished id is answered

The kernel had no many2many groupby. project.task's dependency counts group by
`successor_ids` and `predecessor_ids` with `state:array_agg`, so every form read of a
task paid a kernel dispatch, a refusal and the whole Python read again -- 7.5 ms more
per call than Python alone. `Compiler::many2many_group_join` now builds what
`_read_group_groupby_many2many` builds: a LEFT JOIN of the relation table on
`id = column1`, narrowed by `column2 IN` the comodel's ids under the field domain, the
active test and the comodel's rules unless `bypass_search_access`, when any of those
applies. The group value is `column2`, ordered as it is; aggregates run over the
joined rows. An `array_agg` over a selection decodes as the text array it is.

A read the kernel answered with fewer rows than ids was refused whole, because a row
missing from its answer may be one the rules hide -- Python raises -- or one that does
not exist. Python's read drops a record that does not exist as soon as it reads one
of its columns, and keeps it when it reads only relations or computes. The shim now
asks `exists()` about the missing ids when the read names a stored column; if none
exists, the kernel's rows are Python's answer.

`traffic_bench` times only the captured calls that succeed in Python on the database
it runs on: reads of records the recorded tours created and rolled back are not the
calls Odoo served. On the grouped-view tours 358 of 414 qualify, and routing reads
0.96x Python over them; refusals over both captures fall from 343 to 192.

## A subquery Python resolved reaches the kernel as a bound fragment

A search method may answer with SQL rather than ids: knowledge's
`_search_user_has_access` is one recursive query, so the record rules on
`knowledge.article` and every model that reads through it optimise to
`('id', 'any!', SQL(...))`. The shim had no wire form for an `SQL` or a `Query`,
so those rules stayed unresolved and the kernel refused them as a traversal of a
non-stored field -- and a domain carrying one was refused outright.

The shim now sends such a value as `{"$sql", "$params", "$nonce"}`, a `Query` as
its `subselect()`. The nonce is drawn per request and travels only in the
request, so a domain a client wrote cannot name a fragment. The kernel turns each
`%s` into a bound `$n` cast to its parameter's JSON type -- `int8`, `text`,
`float8`, `bool`, or their arrays, because the Postgres client encodes a typed
value and an `int8` is not an `int4` -- turns `%%` into `%`, drops comments, and
refuses a dollar sign, a second statement, or a parameter with no typed form. It
compiles the fragment where Odoo builds a subselect from one: `id [NOT] IN`, a
stored many2one with Python's `IS NULL OR` on the negative, and an x2many through
its comodel's `id` as the superuser. Anything else is still Python's.

Three refusals went with it. A blank `order` is no order, as Python's falsy test
reads it; the kernel ordered those reads by id alone, which knowledge's own
`search_fetch` refusal had hidden. A web read no longer refuses whole when one
x2many in its specification reads through a comodel that searches in Python or
carries a domain computed per record: that field goes to web_read's half. And
account's two field-scoped `_field_to_sql` hooks, like knowledge's
`is_user_favorite` ordering, refuse only the fields they touch.

On the captured grouped-view tours the routed share rises from 0.51 to 1.04, the
reads verified in shadow from 141 to 236, and the refusals over both captures
from 502 to 377.

## A rollback no longer throws the kernel's plans away

The prepared-statement cache follows psycopg: a `ROLLBACK` tag -- which
`ROLLBACK TO SAVEPOINT` also reports -- or a `DROP` clears it, because a plan
prepared against an object the rollback removed fails. The rust connection
applied that to every plan it held, the kernel's included, so every read request
that ended in a rollback, and every routed call a savepoint rolled back, sent its
kernel statements through `PREPARE` again. On the captured grouped-view tours
215 of 217 kernel statements per round were freshly prepared: 35 ms of preparing
against 82 ms of executing, a third of the SQL the kernel spent.

A plan can only be broken by a rollback that undoes DDL: without DDL, every
object it names exists before and after, and a plan PostgreSQL finds stale is
already dropped by the kernel on its error. The connection now remembers whether
its transaction ran DDL -- a statement led by `CREATE`, `ALTER`, `DROP`,
`TRUNCATE`, `COMMENT`, `DO` or `REFRESH`, or a multi-statement string -- and a
rollback drops the cursor's own plans, as psycopg does, but the kernel's only
after DDL. A `DROP` still clears both at once. The mark starts clear with each
transaction; the first version kept it from an earlier one ended by an executed
`COMMIT` string or run in autocommit, and the runtime contract caught it.

The same replay now prepares none of its 217 kernel statements after the first
round, and the kernel's SQL time falls from 117 ms to 52 ms. The tour replay reads
0.97-1.09x Python on a loaded machine, from 1.09-1.10x.

## A web read the kernel can only half answer is split, not refused

One computed field in a list view's specification -- `is_user_favorite`, a
rating count, `message_needaction` -- sent the whole `web_search_read` to
Python: the search, the order, the count and every stored field with it. On the
captured traffic those were the largest refusal class once access stopped being
one.

The specification planner now sorts fields instead of refusing them. What the
kernel reads -- stored and related fields, many2ones labelled or raw, x2many ids
-- goes to the kernel; a compute, a properties or reference field, a many2one
with sub-fields or a context, an x2many with a sub-specification goes to
Python. The kernel runs the search with its half; web's own `web_read` answers
the other half on exactly the records the kernel returned, and the two merge by
id in the specification's order. Only an unknown field or a malformed spec
still refuses the call.

The Python half is web's own method, so its access check, computes and
formatting are Python's. Two things had to be reproduced around it:

- `search_fetch` checks read access on every requested field before it
  searches, so a group-restricted compute raises even when nothing matches.
  every-user found the routed answer returning `[]` there where Python raised;
  the split now checks the Python half's fields before dispatching.
- the kernel's half warms the cache before the Python half runs, as
  `search_fetch` fills it before `web_read`. It writes no x2many, so it cannot
  change what `read()`'s access check sees.

every-user gains a shape reading the first three computed fields with a
many2one, and one reading `display_name` through `web_search_read`: a model
whose display name Python computes now sends that field, not the call, to the
Python half.

**On real traffic this is not yet a speed-up, and the resolved rules were why
routing lost ground.** Replaying the captured grouped-view tours, routing read
1.09-1.10x Python on the committed build. A profile of the routed leg against
the Python one put 0.36 s of 0.72 s of dispatch overhead in `_resolved_rules`:
every dispatch resolved the rules of its model and every comodel, and half of
the resolutions were `message_partner_ids`, which resolves to a subquery the
kernel cannot receive and compiles natively anyway, so the work was thrown
away. Whether a model's rules need Python is now remembered per rule-cache
generation, identity and model, and so is a resolution that turned out not to
be data; the overhead fell to 0.19 s, the knowledge rules that are resolved for
real. With the split, the same replay reads 1.05-1.11x: level with the build
before it while routing more (share 0.66 against 0.58), and still behind
Python. The recorded capture reads 0.63x, as before.

**A failed statement left the rust transaction reading as healthy.** The
milestone /web run failed `TestProdNodesDeclineNotCached.
test_a_failed_save_statement_leaves_the_transaction_usable` only under the
engine: the asset save closes its savepoint with `rollback=cr.in_failed_
transaction()`, which reads psycopg's INERROR, and the shim reported INTRANS for
any open transaction -- its own comment said no reader in the fork told them
apart, which stopped being true. The savepoint was released instead of rolled
back and PostgreSQL refused. `RustConn` now marks its transaction aborted when a
statement fails there with a server error, clears the mark on a successful
`ROLLBACK TO SAVEPOINT`, a rollback or a commit, and the shim reports INERROR
from it. A runtime contract fails a statement inside a savepoint and requires
INERROR, then INTRANS after the rollback to it, and no mark from a client-side
error; the build before this fails it on the missing attribute.

The fork's persistence protocol moved under the port twice meanwhile: it gained
`count_m2m_groups`, now delegated, and lost `supports_column_scan`, now gone from
`RustBackend` too.

## Record rules Python resolves to data are handed to the kernel

Knowledge articles restrict themselves with `('user_has_access', '=', True)`, and
their members, favorites, threads and stages with `article_id.user_has_access`.
The search method behind it reads member permissions inherited down the article
tree with a recursive query and returns `('id', 'in', [...])`: data, but data
computed by Python from the database, not a leaf the kernel could compile. The
captured knowledge views refused on it 54 times.

The shim now looks at the user's read rules for the request's model and for the
comodels of its requested fields and groupbys. Where a rule reaches a field with
a Python search method, it resolves the domain the way `_search` does --
`optimize_full` on the model under sudo -- and when the result is plain data it
sends it as `resolved_rules`. The kernel applies those domains over its cached
rule set for that request only: a knowledge permission can change inside a
transaction, and the per-identity cache would otherwise serve a stale list.
Where the resolution is not data -- `message_partner_ids` resolves to a
subquery -- nothing is sent and the kernel compiles the leaf itself or refuses.
A rule whose computation raises, as it does for a context naming companies the
user does not have, is a refusal rather than an error.

**A constant group rule did not absorb the rule beside it.** An administrator
holds both knowledge groups, so the grant `(1, '=', 1)` is ORed with
`user_has_access`; Python's `optimize` folds that to TRUE before any search
method runs, and the kernel compiled the OR as written and refused on the leaf.
Rule domains now fold their constants (`fold_constants`) before they compile;
request domains are left as they are, since folding there could answer a
domain Python rejects for an invalid leaf in a dead branch.

The captured grouped-view traffic now routes 0.58 of its calls, from 0.09 before
grouped views routed and 0.43 after the follower rules, with no difference in
shadow.

## Record rules through followers compile

With the overrides gone, project and helpdesk kanban views refused on their
record rules: `project.project` and `helpdesk.team` restrict follower-only
records with `('message_partner_ids', 'in', [user.partner_id.id])`, a
non-stored many2many whose search is `mail.thread._search_message_partner_ids`,
and the kernel refused every leaf on a field with a Python search method. Forty
active rules on the enterprise database go through that field, most of them
from a model to its project.

The search method is plain SQL over `mail.followers`: a sudo search on
`res_model = <model> AND partner_id <op> <operand>`, returned as `id IN (that
subselect)`, with negative operators left to the ORM, which negates the positive
form. The export now marks a field whose search method is that one, unchanged,
by its owner class (`KERNEL_SEARCHES`), and the kernel compiles `in` and `=`
over partner ids to the same subselect, `not in` and `!=` as its negation, and
an empty list to FALSE or TRUE. It does so only under sudo: Python evaluates
record rules on `model.sudo()`, while a request domain naming the field is
resolved by the shim before dispatch, and its portal check is Python's.
`child_of` -- the portal rules -- and any other operator still refuse.

Two differences with Python surfaced on the models this opened, and neither is
the rule.

**A many2many the record rules traverse is read from a sudo cache.** Reading
`project.workflow.step.project_ids` as a user returned all eleven linked
projects in Python and the six the user may see in the kernel. The step's own
rules go through `project_ids`, so `read` and `web_read` run `_check_access`,
which evaluates them with `sudo().filtered_domain` and caches the unfiltered
relation; the fetch that follows builds the rule-filtered query and then answers
from that cache. The shim now refuses a `read` or `web_read` of an x2many that
is the head of a condition in the user's read rules on the model; a search path
does not check access first and still routes.

**Rows tied on the order had no order.** Three milestones equal on every term
of `_order` came back 7, 8, 9 from the kernel and 7, 9, 8 from Python. odoo
2e8e8fc55a99 makes a search's ORDER BY end in the record id unless it names id,
and the kernel's search_read and x2many reads now do the same
(`parse_total_order`).

## A field can name the field it groups and orders through

Two overrides kept every grouped kanban call on their models in Python, and
neither decided anything: `project.task._read_group` renamed a `triage_id`
groupby to `triage_ids`, and `helpdesk.ticket._search` turned an order by
`ticket_ref` into one by `id`. The gate keys on method identity -- it cannot
see what an override touches -- so a view grouped by stage refused because a
different groupby would have been renamed.

The fork now says it declaratively. `Field.group_by_field` names the stored
field a groupby on this one resolves to, and `Field.order_by_field` the field an
order term on it sorts by; setup refuses a stand-in that does not exist or
groups other values. Core resolves them where the SQL is built --
`_read_group_groupby`, the grouping-sets row-duplication check, the group
ordering, `_order_field_to_sql` -- so group keys, order terms and web's
`__extra_domain` keep the name the caller used, and the override that renamed
both the groupby and the order is gone, as is `_read_grouping_sets`' missing
twin of it. The export carries both attributes; the kernel resolves a groupby
through its stand-in, orders a many2one stand-in by its comodel's `_order`, and
rewrites an order term to its stand-in with the direction and nulls intact.
A many2many stand-in still falls back, as a many2many groupby does.

On the captured traffic the override refusal is gone from `project.task` and
`helpdesk.ticket`. What refuses next is their record rules: `project.project`
and `helpdesk.team` rules go through `message_partner_ids`, whose custom search
the kernel does not compile -- the access layer again, now without an override
in front of it.

## A computed x2many was read as its inverse

Every-user on the enterprise database found routed `search_read` answering
`resource.calendar.attendance_ids_1st_week` with every attendance of the
calendar, where Python answers `[]` for a calendar that is not a two-week one.
The field is a non-stored one2many computed in Python, and it names
`calendar_id` as its inverse. The kernel's read planner accepted every x2many,
and `o2m_inverse()` refused a computed field only when it had no inverse to
name, so the name alone was enough to read it as the plain relation.
`m2m_columns()` had the same shape for a computed many2many naming a relation
table. Both accessors now refuse a non-stored field before looking at its
relation, so every path that reaches an x2many -- the read, the domain, the
access traversal -- refuses it.

The same run found `sign.template.web_search_read` failing in PostgreSQL:
`column sign_item.template_id does not exist`. `sign_item_ids` is a stored
one2many whose inverse, `sign.item.template_id`, is a non-stored related
field. `o2m_inverse_column` exists for exactly that case and the domain path
asked it, but the x2many read path asked `o2m_inverse()`. The read and the
access traversal now ask `o2m_inverse_column` too, and security.rs's own copy
of that check is gone.

## A float8 sum is defined only up to summation order

The grouped-view tours quarantined `planning.slot`: `_read_group` summing
`allocated_hours` per sale line answered 16.8 in Python and
16.799999999999997 in the kernel. It did not reproduce on a rerun, nor on
controlled data with the same domain, where both sides agree; Python's
statement is a plain LEFT JOIN. The difference is PostgreSQL's: `SUM` over
`float8` adds in the order rows reach the aggregate, SQL does not fix that
order, and float addition is not associative:

```
SELECT SUM(h) FROM t                                   0.6
SELECT SUM(h) FROM (SELECT h FROM t ORDER BY h ASC) s  0.6000000000000001
```

So Python's own answer is defined only up to the plan, and the shadow check's
`==` quarantined a model for a difference a changed plan makes in Python alone.
The routed `_read_group` and `formatted_read_group` now compare `sum` and
`avg` over a `float8` column within `rel_tol=1e-12, abs_tol=1e-9`. Everything
else stays exact: numeric columns, `min`, `max`, counts, group keys and
labels, so the earlier numeric-average defect would still be caught.

## A rust connection whose backend died read as open

The /web differential failed 17 `web_tour` HOOT tests only under the engine.
The server answered `/web/bundle/web_tour.recorder` with 500: an asset save
opens an autonomous cursor, the pool lent it an idle rust connection whose
backend was gone, and the pool's health check raised `connection closed` to
the request. Two defects lined up. `RustConn.closed` reported an explicit
`close()` and nothing else, so a tokio-postgres client that had died still
read as open and `putconn` kept it idle; it now asks the client too, as
psycopg's `closed` reflects a broken connection. And the shim's pool raised a
failed check, where `psycopg_pool` closes the connection and hands out another;
it now does the same and counts `failed_checks`.

A runtime contract lends a connection, terminates its backend from another
session, and requires the connection to read as closed and the next checkout
to be a different, working one. The build before this change fails it: `a rust
connection whose backend 1580705 was terminated still reads as open`. A shim
unit test covers the check path alone.

## The persistence port, and the first write the kernel owns

Everything above reaches Odoo the same way: `rust_orm_shim` replaces eight
methods on `BaseModel`. That works and it is how every measurement on this
page was taken, but it has a ceiling. Five of the eight it serves --
`search_read`, `search_count`, `read`, `_read_group`, `name_search` -- and
those are the calls a CLIENT names; the other three, `create`, `write` and
`unlink`, it replaces only to NOTICE that a write happened, because it has no
seam on the write itself. And for every call it does serve it has to decide,
in Python and per call, whether the kernel supports the shape it was handed.
The Known gaps entry about the routing shim no longer modelling what the
kernel supports is that ceiling written down.

The fork offers a better seam, and this engine was not on it.
`odoo/orm/runtime/backend.py` declares `StorageBackend`: twelve methods and
five capability flags through which **every row read and every row write in
the ORM** passes. It is not a convention. `test_backend_dispatch_surface.py`
enumerates the fifteen sites across nine files that choose between the SQL
path and the port, says for each what the in-memory branch does NOT do, and
asserts that the mixins hold no row I/O SQL of their own. Two implementors
ship: `PostgresBackend` and `InMemoryBackend`, the second being how the whole
ORM runs with no database at all.

`RustBackend` is a third. It wraps the backend the transaction would
otherwise have used and answers what it can, delegating the rest unchanged:

```
      BaseModel.write()  ->  cache flush  ->  env.backend.update_rows()
                                                     |
                                        RustBackend -+- armed?  -> kernel composes the UPDATE
                                                     |            caller's cursor executes it
                                                     +- else     -> PostgresBackend, unchanged
```

It is installed by wrapping `Transaction.__init__`, the one place a backend
is chosen (`environment.py:121` is its only caller). A transaction that chose
`InMemoryBackend` keeps it -- wrapping that one would put a kernel needing a
connection in front of the case defined by not having one -- and a
transaction for another database keeps `PostgresBackend`, so a process
serving more than one database is unaffected.

**Installing it changes no answer.** `NATIVE` is the arming switch and it is
deliberately separate from the implementations: a method absent from that set
is delegated without ever being asked, so an implementation with nothing
verifying it stays off. `rust_engine_port = off` leaves `env.backend` exactly
as the fork built it.

**The addon and the extension are versioned apart, and the first commit of the
port forgot it.** It called `engine_py.install_backend()` unguarded, after the
method shims were installed and before the registry hook was armed. The venv
carries an `engine_py.so` built before the port existed, and every `odoo-bin`
run without `PYTHONPATH` on a fresh build imports that one -- which includes
every session using the shared workspace conf. Those servers logged
`CRITICAL Couldn't load module rust_engine` and ran half armed. A peer session
reported the `AttributeError` and was told it was a rebuild in progress; it
was this. Two battery stages that generate their corpora through plain
`odoo-bin` failed on it later, which is what found it. An extension without
the port, or a port that raises while installing, now logs one warning and
the rest of the engine arms as before.

### `update_rows`

The first armed method is a write, and it is the bottom of every `write()` in
the ORM. The cache flush hands `update_rows` a column-group and a list of
rows; it renders one `UPDATE`. What it renders is decided entirely by field
METADATA -- the column's declared cast, whether the field is translated as a
whole value, whether it is company-dependent -- which is what the kernel's
registry already holds. So `kernel/src/write.rs` composes the statement and
the caller's own cursor executes it, with the same parameters Python would
have passed. Nothing new binds values and nothing new reaches PostgreSQL: the
statement goes through the same logging, metrics and savepoints as before.
What moved is the composition.

Two field members had to be exported, because neither is derivable from the
column:

| exported | why the column cannot say |
|---|---|
| `translate_whole` | `field.translate is True`, which is NOT `translated`. A field translated term by term (`Html(translate=html_translate)`) sits in the same jsonb column and reads the same way. Its WRITE differs: one merges the new value into the languages already stored, the other replaces the column. The wrong choice loses a language silently |
| `column_cast` | the field's own `column_type[1]` rather than `information_schema`'s spelling. The statement has to carry the SAME cast Python emits, not an equivalent one |

Neither is visible to the `ir_model` bootstrap, so a registry built that way
carries `None` and refuses every write it would have decided -- the same
posture as the rest of that bootstrap.

Refused and delegated rather than guessed: a **company-dependent** column,
whose assignment interpolates `ir.default`'s per-company fallbacks (resolving
those from the kernel's snapshot would write the wrong jsonb rather than
refuse); a column with no declared cast; a field this registry does not
carry. A group with ONE refused column refuses whole, because splitting it
would issue two statements where Python issues one, and the second would not
see the first's row locks in the order Python takes them.

### How a write is verified, given that a wrong one does not raise

`harness/write_sql_contract.json` holds the statement text, and two tests
derive it independently: `kernel/tests/pure.rs` asserts the kernel composes
it, and `harness/test_shims.py` drives the **fork's own** `PostgresBackend`
against a stub model and asserts it composes the same thing. One literal,
derived twice -- so neither derivation is checked against a copy of itself. A
fork that starts composing something else fails on the Python side and stays
failing on the Rust side until the kernel is taught it, which is the order
the two should move in; `write_sql_contract.py --update` is the
regeneration step for an intended change.

The contract is BYTE equality, not equivalence. An equivalent statement that
read differently in `--log-sql` would be a second dialect to keep in step.

What the text cannot see is the row. A statement that reads correctly and
binds its parameters in the wrong order writes the wrong value without
erroring. `harness/write_path.py` writes the same values twice on one
database -- once through the kernel's statement and once with the port
disarmed so the fork's composes -- and compares what PostgreSQL stored, read
back by an independent connection neither path touched. It also proves the
path RAN, per statement shape: a leg that delegated everything would compare
two identical Python writes and pass while exercising nothing, which is the
same hole the copy encoder's stream counter exists to close.

Both statements are exercised, and the translated pair took finding. A
whole-value translated column names its value THREE times in the merge
expression, so the uniform statement binds it three times -- and a group
holding one can never BE uniform, because its update value is a
`PsycopgJson` wrapper and `_UNIFORM_UPDATE_TYPES` does not list it. The one
path on which that three-parameter binding runs is a NULL on every row, which
the harness reaches by clearing a column. Built for one occurrence instead,
the id array lands inside the `CASE`.

### `create_rows`

The second armed method is the other half of the write path, and it is
narrower than `update_rows` on purpose. `PostgresBackend.create_rows` has two
strategies. `COPY_THRESHOLD` rows or more (50 since odoo e6fc39e30777), outside a pipeline, go as a binary `COPY`: the
cursor preallocates the ids, resolves each column's type OID and streams the
rows, and with the db shim installed that stream is already encoded by
`RustCopy` and verified by `harness/copy_path.py`. Everything else is one
`INSERT ... VALUES ... RETURNING "id"`, and that statement is what the kernel
now composes.

So the port delegates the `COPY` strategy and says so, with the reason
`COPY strategy: the cursor owns it`. The split is taken from the fork's own
`COPY_THRESHOLD`, `COPY_DISABLED` and `cr.in_pipeline`, so a change to the
threshold moves both sides together rather than leaving the kernel composing
creates Python sends as `COPY`. The one create above the threshold the kernel
does compose is the one inside a pipeline, where `COPY` cannot run.

The values are still converted by the fork: `_prepare_insert_rows` runs
`convert_to_column_insert`, which decides a translated or company-dependent
column's jsonb from the environment's language and company. What the kernel
decides is that every column named is one this registry knows the table to
have, so a column added by an upgrade it was not rebuilt for refuses instead
of failing mid-create. A converted value that is an `SQL` object or a tuple is
delegated too. `SQL` inlines the first as code and expands the second into a
list, so neither is the single parameter the statement binds. No converter
returns either today, and a check costs nothing against the day one does.

A record created with nothing stored is `INSERT INTO t ("id") VALUES
(DEFAULT), ...`, and the contract pins that form alongside the others.

Its verification is the same pair. `write_sql_contract.json`, renamed from
the update-only file it started as, carries insert cases the fork's
`create_rows` and the kernel each derive. `harness/write_path.py` gained
creates, and they check something the update legs never had to: **the id
pairing.** `create()` pairs the ids `RETURNING` hands back with the values it
sent, in order, and fills the cache from that pairing. An id list in the
wrong order would give every record its neighbour's values in the cache while
the table stayed right, and no read-back over an id range would see it. So
each created record is read back by its own id and compared with the case it
came from, on the columns a create stores verbatim.

The first run of that check reported fourteen mismatches, and every one was
the check. `comment` on `res.partner` is Html, so the sanitizer wraps it in
`<p>`; `partner_latitude` is a rounded numeric that reads back as a `Decimal`.
The `COPY` leg, which the kernel never touches, showed the identical
fourteen, and that is what said so: a difference present on the path the
change does not reach is not the change's. Those two columns are now compared
against the Python control, where both legs went through the same conversion.

```
create of 5                 INSERT   native
create of 12                COPY     delegated, reason reported
create of 12 in a pipeline  INSERT   native
```

### `search`: exact, and not armed

`search` is the method that would let the method-level shim retire, and it is
the first one on the port that is compiled natively, verified exactly, and
**deliberately left out of `NATIVE`**, because it is slower than the Python it
would replace.

It does not return rows. It returns a lazy `Query` the ORM keeps composing --
`_order_to_sql` joins onto it, `search_count` wraps it, a field's `search=`
method embeds it as a sub-select -- so a native one has to be that same object
with a different WHERE. `Orm::compile_where` compiles the domain and the
caller's record rules into a fragment in Odoo's own dialect (`%s` for every
parameter, a literal `%` doubled, the root table named by its table name,
which is the alias `Query` gives it), and the port attaches it to an ordinary
`Query`. Ordering, limit and offset stay the fork's. The domain that reaches
the port has already been through `optimize_full`, so two knobs were needed:
`root_active_test: false`, because `_search` already decided the root's
active filter -- including a `_search(active_test=False)` keyword the port
never sees -- while the context's `active_test` still governs every
sub-query; and `trusted_domain`, because an `any!` in an ORM-composed domain
is a field's own bypass declaration and not a caller asking for one.

Delegated: `bypass_access` without superuser, which is neither of the
kernel's two modes; a domain carrying a value with no wire form, such as the
`Query` a `search=` method leaves or a custom SQL node; and every refusal the
compiler already had.

**The flush was the hard part, and the sweep is what found it.** Odoo's WHERE
is an `SQL` whose `to_flush` names the fields the cursor flushes when the
query EXECUTES. A fragment missing one reads a stale row after a write in the
same transaction, and no read-only comparison would notice. So
`harness/search_path.py` requires the native fragment's set to cover Odoo's,
case by case. The fork's dependency collector gives the fields a domain names;
the first sweep failed 411 cases, and every one of the forty it printed was a
field no domain names: a sub-query's comodel `active` column, a relational
field's own `domain=`, and -- most of them -- the comodel's record rules. The
collection now expands every relational field it traverses with all three,
to a fixed point.

Over the sweep corpus at every identity it seeds, after that:

```
native     4,140   ids, unlimited count, as a sub-select, flush coverage: all equal
delegated    505   with reasons: bypass_access 49, non-stored traversal 29,
                   a comodel's Python _search 44, Python-computed field domains 34, ...
both raised 3,101  the access check `_search` makes before it reaches the port
```

**And it is slower.** `harness/search_bench.py` times `search()` and query
execution apart, over the 4,612 distinct searches the corpus holds that
Python answers:

```
                                   python        native
first version                      3.4-3.7 s     7.5-8.8 s
rule dependencies cached           3.1-3.8 s     4.7-5.7 s
query execution, either version    ~1.5 s        ~1.5 s
```

The first version optimized every traversed comodel's rule domain on every
call to collect its columns, and optimizing a rule can SEARCH -- the
`ir.attachment` and mail access checks do -- so that was two thirds of the
cost. `_get_domain_accessible_records` is an ormcache that returns the same
object until the rules change, so its identity is now the cache key. What
remains, profiled: the savepoint around the kernel call, two statements per
search and the largest single cost, then the kernel's own compile, which
includes a signalling round trip. The savepoint is what keeps a kernel-side
database error from aborting the caller's transaction, so removing it is not
a speed fix but a correctness trade. Those two are the backlog for arming it.

The benchmark's two measurements straddle a rebase of `odoo` by another
session. Both legs of each run are on the same tree, so each ratio holds; the
absolute times across the two rows are not comparable to the second decimal.

### `search`, taken from 1.5x slower to parity

The first native `search` compiled exactly and ran 1.4 to 1.6 times slower
than Python. Four changes took it to parity, each measured, and a fifth was a
correctness gap found on the way.

**The flush set comes from the kernel now.** The port used to predict the
fields Odoo's WHERE flushes with the fork's dependency collector, expanded
over comodel rules, field domains and `active` columns; optimizing a comodel's
rule domain can itself search. The compiler instead records every (model,
field) whose column its SQL reads -- through `ExprCtx::touch`, shared by every
copy of the context so rule compiles report too -- and `compile_where`
returns it. It is Odoo's `to_flush` by construction rather than by
prediction, and the sweep's coverage check agrees on every native case. The
collector and its rule cache are gone.

**The watermark is read once per transaction.** The caller's transaction is
REPEATABLE READ, so the signalling row it can see does not move inside it. The
connection records the snapshot a compile verified against its transaction
serial, and later compiles in that transaction reuse it. It is the snapshot
the check produced, not `registry.dynamic()`, which another connection may
have refreshed from a commit this transaction cannot see.

**Trusting Python's registry instead of the watermark was unsound, and is
gone.** A first version let the first compile of a transaction skip the
watermark whenever the environment's registry sequences equalled the kernel
snapshot's on the registry and security tables. The registry object is shared
by every thread of the process and advances while a transaction is open, so
an OLD transaction's registry can already read the NEW sequences, match a
refreshed kernel snapshot, and have the kernel apply rules its database
snapshot cannot see. The runtime contract "security refresh cannot make old
transactions use future rule metadata" failed on it when the same shortcut was
put on the method shim's dispatch. It shipped with the port's `search`, which
is not armed, and nothing routed through it. The same contract then showed a
second hole in the per-transaction reuse: a remembered snapshot must still be
COMPARED with the kernel's current metadata, because the kernel holds one
current security state and refuses a transaction whose snapshot precedes it.
Reuse now skips only the read; `Orm::snapshot` repeats the comparison in
memory, and the contract checks `dispatch` and `search_where`, offline and
online, against a refresh from another process.

What replaced the shortcut for the first compile of a transaction: the
signalling read is sent even offline. It takes no lock and fails only when the
connection has, which Python's own next statement would meet the same way, so
it costs a round trip and not a savepoint.

**No savepoint unless the kernel needs the database.** `Db` has an offline
mode that raises `NeedsRoundTrip` instead of sending a statement. The port
compiles offline first; only a compile that has to ask -- an identity or rule
set not yet cached, a watermark Python does not agree with -- runs online,
inside the savepoint. Opening the transaction is sent either way: the query
would send the same `BEGIN`. The first sweep with offline compiles delegated
936 cases instead of 507, because rule evaluation turned the round-trip error
into "rules could not be evaluated" and CACHED that for the identity; it
propagates the error now, as it does a database error.

**The correctness gap: a security write in the same transaction.** The
kernel's rules move with the watermark, and the watermark moves on commit. The
method shim has always refused a cursor that wrote `ir.rule`, a group, a user
or the like (`DIRTY_CRS`); the port did not check it. A search after an
in-transaction `ir.rule` would have applied the old rules. It delegates now,
and without the shim installed -- nothing then records such writes -- it
delegates everything. `search_path.py` ends by writing a rule and requiring the
delegation and Python's answer.

Measured over the sweep corpus's 4,624 searches, best of five interleaved
rounds, after the shortcut above was removed (load average about 4 on 22
cores, other sessions running; the noise is near ten percent):

```
a new transaction every      python      native     native / python
search                       4.19 s      4.90 s     1.17
8 searches                   3.62 s      3.32 s     0.92
round (steady state)         2.67 s      2.78 s     1.04
no compile needed a savepoint in any measured round (online = 0)
```

Parity over transactions of a few searches, and slower where each transaction
holds one: the first compile of a transaction still reads the watermark, the
one statement the kernel cannot avoid. So `search` stays out of `NATIVE`, by the rule this
port was built on. The cost that remains is upstream of the seam:
`optimize_full` runs in `_search` before the backend is asked, and it is the
same on both legs. At the seam alone native is clearly faster -- `backend.search`
took 1.12 to 1.24 s against Python's 1.69 to 1.75 s, because the kernel
compiles a sub-query itself where Python calls `_search` again -- which says
where the next gain has to come from.

### Counting every call cost a browser tour

The port counts what it answered and what it delegated, and the first version
did it under a `threading.Lock`. That put one process-wide mutex on
`env.backend.fetch` and `env.backend.search` -- the two hottest methods in the
ORM, taken on every field read and every query the ORM builds -- in a server
whose conf is `workers = 0`, which is threaded.

Nothing in the battery reported it as a slow number -- the soak's
throughput figure was unremarkable and no stage measures the port. It
surfaced as
`web/tests/test_login.py::TestUserSwitch.test_user_switch` failing with
`RPC_ERROR: Odoo Session Expired` at the step that clicks Log out, which is a
race between the logout destroying the session and an RPC already in flight.
Measured over the same eight tour tags on the same database:

```
with the lock       3 runs, 2 failed
without the lock    7 runs, 0 failed
port off            3 runs, 0 failed
```

The counters are plain `Counter` increments under the GIL now. Two threads
incrementing the same key can lose one of the two; nothing is corrupted, and a
lost unit of instrumentation is the right trade against serialising every row
read in the process. A figure read out of `stats()` is a lower bound under
concurrency, and says so.

The general point is worth more than the fix: **the port sits under the whole
ORM, so anything it does per call, it does on the hottest path there is.** The
method-level shim above it is entered only for the calls a client names, and
never had this exposure.

### What the port says about the rest

Every delegated call is counted with its reason, so the port reports its own
coverage. One `write_path` run, which is a handful of creates and writes:

```
before create_rows   native     update_rows 6
                     delegated  fetch 40   search 39   create_rows 4   as_query 1

after                native     update_rows 6   create_rows (every INSERT)
                     delegated  fetch 40   search 39   as_query 1
                                create_rows only on the COPY strategy
```

That is the map for what moves next, and it is a different map from the
routed-share table above: `fetch` and `search` are hot here because the port
sees every field read and every query the ORM builds, not only the ones an
RPC method names.

## The road to replacement (M2, then M3)

`M3-PLAN.md` is the plan of record for the end state: the Rust binary as the
server, with embedded CPython running addon logic on this engine. What stands
between here and there, in the order the evidence says to take it:

1. `web_search_read` + session auth → point the stock web client at it.
2. Replay-based shadow testing: capture real `call_kw` traffic, diff at scale.
3. The business-logic wall: PyO3-embedded Python for model overrides, with
   the Rust kernel as the data engine underneath. This is the step that turns
   the refusal backlog from a permanent boundary into a temporary one — every
   refusal above whose cause is "Python computes it" is answered by running
   that Python *on* the kernel instead of beside it.
4. The write path, flush and the dependency graph — the remaining half of the
   ORM, and the one that makes Python's copy removable rather than merely
   bypassed. **Started**: `update_rows`, the statement every `write()` ends
   in, and the `INSERT` strategy of `create_rows` are composed by the kernel and runs through the fork's own
   `StorageBackend` port rather than through a patched method. The section
   below says what that port is and why it is the seam the rest of this step
   should arrive on.
