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


def main(env):
    su = env["res.partner"].sudo().env
    Partner = su["res.partner"]
    made = {}

    partners = Partner.create(
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
    # not the companies' own partners nor a warehouse's: a delivery to one of
    # those goes to inter-warehouse transit and needs a rule the demo lacks
    internal = (
        env.companies.partner_id.ids + su["stock.warehouse"].search([]).partner_id.ids
    )
    customers = su["res.partner"].search(
        [("is_company", "=", True), ("id", "not in", internal)], order="id", limit=5
    )
    if products and customers and "sale.order" in env.registry:
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
        made["sale_orders"] = len(orders)
        # the deliveries of the confirmed orders: stock moves, quants and
        # the valuation entries the port writes, half of them validated
        deliveries = (
            orders[:6].mapped("picking_ids")
            if hasattr(orders, "picking_ids")
            else su["stock.picking"].browse()
        )
        for pick in deliveries.sorted("id")[:3]:
            for move in pick.move_ids:
                move.write({"quantity": move.product_uom_qty, "picked": True})
            pick.with_context(
                skip_backorder=True, skip_immediate=True
            ).button_validate()
        env.flush_all()
        made["deliveries"] = len(deliveries.sorted("id")[:3])
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
        made["invoices"] = len(invoices)
        if invoices and "account.payment.register" in env.registry:
            for inv in invoices[:2]:
                wizard = (
                    su["account.payment.register"]
                    .with_context(active_model="account.move", active_ids=inv.ids)
                    .create({})
                )
                wizard.action_create_payments()
            env.flush_all()
            made["payments"] = 2

    vendors = su["res.partner"].search(
        [("is_company", "=", True), ("id", "not in", internal)],
        order="id desc",
        limit=3,
    )
    if products and vendors and "purchase.order" in env.registry:
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
        made["purchase_orders"] = len(pos)
        pickings = (
            pos[:3].mapped("picking_ids")
            if hasattr(pos, "picking_ids")
            else su["stock.picking"].browse()
        )
        for pick in pickings.sorted("id")[:2]:
            for move in pick.move_ids:
                move.write({"quantity": move.product_uom_qty, "picked": True})
            pick.with_context(
                skip_backorder=True, skip_immediate=True
            ).button_validate()
        env.flush_all()
        made["receipts"] = len(pickings.sorted("id")[:2])

    if "crm.lead" in env.registry:
        Lead = su["crm.lead"]
        stages = su["crm.stage"].search([], order="sequence, id")
        leads = Lead.create(
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

    if "project.task" in env.registry:
        project = su["project.project"].search([], order="id", limit=1)
        if project:
            Task = su["project.task"]
            tasks = Task.create(
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

    env.cr.commit()
    print("AB WORKLOAD done: %r seed=%d" % (made, SEED))


main(env)  # noqa: F821  injected by odoo-bin shell
