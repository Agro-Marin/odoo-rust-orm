"""A deterministic business workload, committed: one leg of the A/B database
differential. Run under `odoo-bin shell` on a clone of the demo snapshot,
once with the engine on and once without; `ab_compare.py` then diffs the
two databases table by table.

Everything it creates derives from a fixed seed and the demo records in id
order, so two runs from the same snapshot make the same rows in the same
order -- ids and sequence numbers included.
"""

SEED = 20260919


def first(rec, *names):
    for n in names:
        if hasattr(rec, n):
            return getattr(rec, n)
    raise AttributeError("%s has none of %s" % (rec._name, names))


def workload_partners(su, made):
    partners = su["res.partner"].create(
        [
            {
                "name": "AB Partner %02d" % i,
                "is_company": i % 3 == 0,
                "email": "ab%02d@example.com" % i,
                "street": "Street %d" % i,
                "zip": "%05d" % (1000 + i),
            }
            for i in range(20)
        ]
    )
    partners[::4].write({"comment": "written in the workload"})
    partners[1].write({"name": "AB Partner 01 renamed", "email": "renamed@example.com"})
    partners[19].unlink()
    made["partners"] = len(partners) - 1
    return partners


def workload_products(su):
    # no replenishment route: a demo product on an MTO route confirms into
    # "No rule has been found to replenish" and aborts the whole leg
    products = su["product.product"].search(
        [
            ("sale_ok", "=", True),
            ("purchase_ok", "=", True),
            ("route_ids", "=", False),
            ("is_storable", "=", True),
        ],
        order="id",
        limit=6,
    )
    # invoiced on order: no demo product is, and delivery-invoiced lines
    # have nothing to invoice until a picking is done
    products.product_tmpl_id.write({"invoice_policy": "ordered"})
    return products


def validate_pickings(env, su, orders, count):
    pickings = (
        orders.mapped("picking_ids")
        if hasattr(orders, "picking_ids")
        else su["stock.picking"].browse()
    )
    for pick in pickings.sorted("id")[:count]:
        for move in pick.move_ids:
            move.write({"quantity": move.product_uom_qty, "picked": True})
        pick.with_context(skip_backorder=True, skip_immediate=True).button_validate()
    env.flush_all()
    return len(pickings.sorted("id")[:count])


def create_sale_orders(env, su, products, customers):
    Sale = su["sale.order"]
    orders = Sale.browse()
    for i in range(10):
        lines = [
            (
                0,
                0,
                {
                    "product_id": products[(i + k) % len(products)].id,
                    "product_qty": 1 + (i + k) % 4,
                },
            )
            for k in range(1 + i % 3)
        ]
        orders |= Sale.create(
            {"partner_id": customers[i % len(customers)].id, "line_ids": lines}
        )
    env.flush_all()
    orders[0].line_ids[0].write({"product_qty": 7})
    for so in orders[:6]:
        first(so, "action_confirm")()
    env.flush_all()
    return orders


def invoice_sale_orders(env, su, orders):
    invoices = su["account.move"].browse()
    for so in orders[:4]:
        create = getattr(so, "_create_invoices", None)
        if create is None:
            break
        invoices |= create()
    env.flush_all()
    for inv in invoices:
        inv.action_post()
    env.flush_all()
    return invoices


def reverse_invoice(env, su, invoice):
    reversal = (
        su["account.move.reversal"]
        .with_context(active_model="account.move", active_ids=invoice.ids)
        .create(
            {
                "reason": "ab credit note",
                "journal_id": invoice.journal_id.id,
            }
        )
    )
    reversal.refund_moves()
    env.flush_all()


def pay_invoices(env, su, invoices):
    for inv in invoices:
        wizard = (
            su["account.payment.register"]
            .with_context(active_model="account.move", active_ids=inv.ids)
            .create({})
        )
        wizard.action_create_payments()
    env.flush_all()


def workload_sales(env, su, made, products, internal):
    customers = su["res.partner"].search(
        [("is_company", "=", True), ("id", "not in", internal)], order="id", limit=5
    )
    if not (products and customers and "sale.order" in env.registry):
        return
    orders = create_sale_orders(env, su, products, customers)
    made["sale_orders"] = len(orders)
    # the deliveries of the confirmed orders: stock moves, quants and
    # the valuation entries the port writes, half of them validated
    made["deliveries"] = validate_pickings(env, su, orders[:6], 3)
    invoices = invoice_sale_orders(env, su, orders)
    made["invoices"] = len(invoices)
    if invoices and "account.move.reversal" in env.registry:
        reverse_invoice(env, su, invoices[-1])
        made["credit_notes"] = 1
    if invoices and "account.payment.register" in env.registry:
        pay_invoices(env, su, invoices[:2])
        made["payments"] = 2


def create_purchase_orders(env, su, products, vendors):
    Purchase = su["purchase.order"]
    pos = Purchase.browse()
    for i in range(5):
        pos |= Purchase.create(
            {
                "partner_id": vendors[i % len(vendors)].id,
                "line_ids": [
                    (
                        0,
                        0,
                        {
                            "product_id": products[i % len(products)].id,
                            "product_qty": 2 + i,
                            "price_unit": 10.0 + i,
                        },
                    )
                ],
            }
        )
    env.flush_all()
    for po in pos[:3]:
        first(po, "action_confirm", "button_confirm")()
    env.flush_all()
    return pos


def bill_purchase_orders(env, su, pos):
    bills = su["account.move"].browse()
    for po in pos[:2]:
        create = getattr(po, "_create_invoices", None)
        if create is None:
            break
        try:
            with env.cr.savepoint():
                bills |= create()
        except Exception:
            break
    for bill in bills:
        bill.write({"invoice_date": bill.date or bill.create_date.date()})
        bill.action_post()
    env.flush_all()
    return bills


def workload_purchases(env, su, made, products, internal):
    vendors = su["res.partner"].search(
        [("is_company", "=", True), ("id", "not in", internal)],
        order="id desc",
        limit=3,
    )
    if not (products and vendors and "purchase.order" in env.registry):
        return
    pos = create_purchase_orders(env, su, products, vendors)
    made["purchase_orders"] = len(pos)
    made["vendor_bills"] = len(bill_purchase_orders(env, su, pos))
    made["receipts"] = validate_pickings(env, su, pos[:3], 2)


def workload_leads(env, su, made, partners):
    if "crm.lead" not in env.registry:
        return
    stages = su["crm.stage"].search([], order="sequence, id")
    leads = su["crm.lead"].create(
        [
            {
                "name": "AB Lead %02d" % i,
                "partner_id": partners[i % 19].id if i % 2 else False,
                "expected_revenue": 100.0 * (i + 1),
                "probability": [10, 30, 50, 80][i % 4],
            }
            for i in range(10)
        ]
    )
    env.flush_all()
    if stages:
        for i, lead in enumerate(leads):
            lead.write({"stage_id": stages[(i + 1) % len(stages)].id})
    leads[3].write({"description": "ab lead written", "expected_revenue": 999.5})
    leads[8].unlink()
    env.flush_all()
    made["leads"] = 9


def workload_tasks(env, su, made):
    if "project.task" not in env.registry:
        return
    project = su["project.project"].search([], order="id", limit=1)
    if not project:
        return
    tasks = su["project.task"].create(
        [
            {
                "name": "AB Task %02d" % i,
                "project_id": project.id,
                "priority": "1" if i % 2 else "0",
            }
            for i in range(8)
        ]
    )
    env.flush_all()
    steps = (
        su["project.workflow.step"].search([], order="id")
        if "project.workflow.step" in env.registry
        else None
    )
    if steps and hasattr(tasks, "step_id"):
        for i, t in enumerate(tasks):
            t.write({"step_id": steps[i % len(steps)].id})
    tasks[0].write({"description": "<p>ab</p>"})
    tasks[7].unlink()
    env.flush_all()
    made["tasks"] = 7


def main(env):
    su = env["res.partner"].sudo().env
    made = {}
    partners = workload_partners(su, made)
    products = workload_products(su)
    # not the companies' own partners nor a warehouse's: a delivery to one of
    # those goes to inter-warehouse transit and needs a rule the demo lacks
    internal = (
        env.companies.partner_id.ids + su["stock.warehouse"].search([]).partner_id.ids
    )
    workload_sales(env, su, made, products, internal)
    workload_purchases(env, su, made, products, internal)
    workload_leads(env, su, made, partners)
    workload_tasks(env, su, made)
    env.cr.commit()
    print("AB WORKLOAD done: %r seed=%d" % (made, SEED))


main(env)  # noqa: F821  injected by odoo-bin shell
