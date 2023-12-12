from datetime import datetime, timedelta, timezone

import bytewax.operators as op
from bytewax.dataflow import Dataflow
from bytewax.operators import _BatchLogic, _BatchState
from bytewax.testing import TestingSink, TestingSource, run_main


def test_batch_logic_snapshot():
    timeout = timedelta(seconds=10)
    logic = _BatchLogic("test_step", timeout, 3, _BatchState())

    now = datetime(2023, 1, 1, tzinfo=timezone.utc)
    logic.on_item(now, 1)

    assert logic.snapshot() == _BatchState([1], now + timeout)


def test_batch():
    inp = list(range(10))
    out = []

    flow = Dataflow("test_df")
    s = op.input("inp", flow, TestingSource(inp))
    s = op.key_on("key", s, lambda _x: "ALL")
    # Use a long timeout to avoid triggering that.
    # We can't easily test system time based behavior.
    s = op.batch("batch", s, timedelta(seconds=10), 3)
    op.output("out", s, TestingSink(out))

    run_main(flow)
    assert out == [
        ("ALL", [0, 1, 2]),
        ("ALL", [3, 4, 5]),
        ("ALL", [6, 7, 8]),
        ("ALL", [9]),
    ]