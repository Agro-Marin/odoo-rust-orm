{
    "name": "Rust read engine",
    "version": "19.0.1.0.0",
    "category": "Technical",
    "summary": "Serve search_read / search_count / _read_group from the Rust kernel",
    "description": """
Loads the `engine_py` extension and routes the ORM's three read entry points
through it, falling back to Python for anything the kernel declines.

THIS IS A SERVER-WIDE MODULE, NOT AN INSTALLABLE ONE. It patches BaseModel and
the connection pool for the whole process and has no models, no views and no
data; there is nothing for `-i rust_engine` to install. Put it in
`server_wide_modules` so `post_load` runs in every worker:

    [options]
    server_wide_modules = base,web,rust_engine
    rust_engine_db = mydb              ; the one database to route
    rust_engine_mode = shadow          ; on | off | shadow  (default off)
    rust_engine_verify_sample = 0.01   ; in mode `on`, check 1 call in 100
    rust_engine_report_seconds = 60    ; report + kill-switch tick; 0 disables both

THE KILL SWITCH. `set_mode()` reaches one process and a prefork server is
several, so turning routing off across a running server used to mean
restarting it. Two `ir.config_parameter` keys override the file at runtime and
every worker picks them up within one tick, no restart and no shell:

    rust_engine.mode           on | off | shadow
    rust_engine.verify_sample  0.0 .. 1.0

`rust_engine_mode` defaults to OFF, so adding the module to a running server
changes nothing until someone says otherwise. `shadow` runs both engines and
logs divergences while returning Python's answer, which is how to qualify a
database before trusting it.
""",
    "author": "AgroMarin",
    "license": "LGPL-3",
    "depends": ["base"],
    "installable": False,
    "auto_install": False,
    "post_load": "start",
}
