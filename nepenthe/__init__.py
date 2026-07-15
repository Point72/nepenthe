"""nepenthe — forget your environment sorrows.

The high-level API mirrors the ``nepenthe`` CLI. Producer:

>>> import nepenthe
>>> nepenthe.build("environment.yaml", "app", output_dir="locks")  # doctest: +SKIP

Consumer (no conda required):

>>> nepenthe.create("app", "file:///srv/nepenthe", "./envs/app")   # doctest: +SKIP
>>> print(nepenthe.activate("./envs/app"))                          # doctest: +SKIP

All functions are implemented in Rust (the ``nepenthe.nepenthe`` extension).
"""

from .nepenthe import (
    __version__,
    activate,
    build,
    check,
    create,
    current_platform,
    diff,
    fsspec_publish,
    fsspec_pull,
    list_releases,
    manifest,
    pack,
    publish,
    pull,
    remove,
    show,
    status,
    sync,
    try_solve,
    unpack,
    version,
)

__all__ = [
    "__version__",
    "activate",
    "build",
    "check",
    "create",
    "current_platform",
    "diff",
    "fsspec_publish",
    "fsspec_pull",
    "list_releases",
    "manifest",
    "pack",
    "publish",
    "pull",
    "remove",
    "show",
    "status",
    "sync",
    "try_solve",
    "unpack",
    "version",
]
