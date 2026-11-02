"""Host tests for tools/hits.py: python3 -I tools/test_hits.py"""
import importlib.util
import sys
from pathlib import Path

spec = importlib.util.spec_from_file_location("hits", Path(__file__).with_name("hits.py"))
hits = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hits)

HZ = hits.TICK_HZ


def test_rates_from_a_steady_and_a_bursty_site():
    text = "\n".join(
        [
            "1 plugin=memory-guard/0.1.0 diag=1 result=installed heap_allocations=0",
            f"{0 * HZ} diag hits a=0 b=0",
            f"{5 * HZ} diag hits a=500 b=0",
            f"{10 * HZ} diag hits a=1000 b=4000",
            f"{15 * HZ} diag hits a=1500 b=4000",
            f"{15 * HZ + 7} guard site=a verdict=repair value=0x1 to=0x0",
        ]
    )
    points = hits.samples(text)
    assert len(points) == 4
    r = hits.rates(points)
    total, mean, peak = r["a"]
    assert total == 1500 and abs(mean - 100) < 1e-9 and abs(peak - 100) < 1e-9
    total, mean, peak = r["b"]
    assert total == 4000 and abs(mean - 4000 / 15) < 1e-9 and abs(peak - 800) < 1e-9


def test_only_the_last_boot_counts():
    text = "\n".join(
        [
            "1 plugin=memory-guard/0.1.0 result=installed",
            f"{1 * HZ} diag hits a=999999",
            f"{2 * HZ} diag hits a=9999999",
            "2 plugin=memory-guard/0.1.0 result=installed",
            f"{10 * HZ} diag hits a=0",
            f"{20 * HZ} diag hits a=100",
        ]
    )
    total, mean, peak = hits.rates(hits.samples(text))["a"]
    assert (total, mean, peak) == (100, 10.0, 10.0)


def test_one_sample_gives_no_rate():
    assert hits.rates(hits.samples(f"{HZ} diag hits a=5")) == {}


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_"):
            fn()
            print("ok", name)
    sys.exit(0)
