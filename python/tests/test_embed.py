"""The binding end to end: Python processes that embed the router elect one,
and when it is killed the other takes over and this process's node follows.

Each process gets its own XDG_RUNTIME_DIR / XDG_STATE_HOME, so the real router
of whoever runs the tests is never touched.
"""

import os
import subprocess
import sys
import time

import pytest

import zapd

CANDIDATE = """
import sys, zapd
zapd.embed()
me = zapd.Node(sys.argv[1])
sys.stdin.read()
"""


@pytest.fixture
def home(tmp_path, monkeypatch):
    (tmp_path / "run").mkdir()
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path / "run"))
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path / "state"))
    monkeypatch.setenv("ZAP_LOG", "zapd=info")
    return tmp_path


def spawn(name):
    return subprocess.Popen(
        [sys.executable, "-c", CANDIDATE, name],
        stdin=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def elected(procs, timeout=10):
    """The candidate whose stderr says it won."""
    deadline = time.monotonic() + timeout
    for p in procs:
        os.set_blocking(p.stderr.fileno(), False)
    while time.monotonic() < deadline:
        for p in procs:
            line = p.stderr.readline()
            if "elected router" in (line or ""):
                return p
        time.sleep(0.05)
    raise AssertionError("no candidate was elected")


def listed(me, want, timeout=10):
    deadline = time.monotonic() + timeout
    while True:
        ids = sorted(n["id"] for n in me.nodes())
        if ids == sorted(want):
            return
        assert time.monotonic() < deadline, f"listed {ids}, want {sorted(want)}"
        time.sleep(0.05)


def at(name):
    return f"agent/{zapd.host()}/{name}"


def test_router_moves_and_the_node_follows(home):
    a, b = spawn("agent/test-a"), spawn("agent/test-b")
    try:
        first = elected([a, b])
        me = zapd.Node("agent/test-me")
        listed(me, [at("test-a"), at("test-b"), at("test-me")])
        assert me.id == at("test-me")
        # The pairing code the router minted on first use is loopback-only.
        assert zapd.pair().startswith("ws://127.0.0.1:")

        first.kill()
        first.wait()
        survivor = b if first is a else a
        assert elected([survivor]) is survivor
        name = "test-b" if survivor is b else "test-a"
        listed(me, [at(name), at("test-me")])
    finally:
        for p in (a, b):
            p.kill()
            p.wait()


def test_no_router_is_a_timeout(home):
    me = zapd.Node("agent/test-alone")
    with pytest.raises(TimeoutError):
        me.nodes(timeout=0.5)


def test_role_is_checked():
    with pytest.raises(ValueError):
        zapd.Node("agent/test-x", role="router")
