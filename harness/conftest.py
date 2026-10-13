import sys

from _env import odoo_root

if odoo_root() not in sys.path:
    sys.path.append(odoo_root())
