{
    "name": "Rust Read Engine",
    "version": "19.0.1.0.0",
    "category": "Technical",
    "summary": "Route selected ORM reads through the Rust engine or compare them with Python results",
    "description": "Provides a server-wide loader for the engine_py extension and its ORM read-routing hooks. Supports off, shadow and active modes, model filters and fallback controls. Shadow mode returns Python results while recording comparisons. Requires the built extension and server-wide loading configuration; routing defaults to off. This addon is marked non-installable and is loaded through its post-load hook.",
    "author": "AgroMarin",
    "license": "LGPL-3",
    "depends": ["base"],
    "installable": False,
    "auto_install": False,
    "post_load": "start",
}
