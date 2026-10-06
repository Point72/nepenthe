import inspect

import nepenthe


def test_public_api_surface():
    expected = {
        "activate",
        "build",
        "check",
        "create",
        "current_platform",
        "diff",
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
    }
    assert expected.issubset(set(nepenthe.__all__))
    for name in expected:
        assert callable(getattr(nepenthe, name))


def test_version_and_platform():
    assert isinstance(nepenthe.version(), str)
    assert nepenthe.version() == nepenthe.__version__
    platform = nepenthe.current_platform()
    assert isinstance(platform, str) and "-" in platform


def test_activation_script_api_defaults_on():
    functions = (
        (nepenthe.create, ("app", "file:///registry", "prefix")),
        (nepenthe.unpack, ("bundle.tar", "prefix")),
        (nepenthe.sync, ()),
    )
    for function, args in functions:
        parameter = inspect.signature(function).parameters["activation_script"]
        assert parameter.default is True
        inspect.signature(function).bind(*args, activation_script=False)


def test_status_of_missing_prefix(tmp_path):
    status = nepenthe.status(str(tmp_path / "absent"))
    assert status["exists"] is False
    assert status["packages"] == []
