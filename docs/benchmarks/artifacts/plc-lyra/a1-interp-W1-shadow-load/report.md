# Benchmark Report: a1-interp-W1-shadow-load

- Host: `luckfox` (`arm`, kernel `6.1.99`, RT `SCHED_FIFO(prio=55)`)
- Engine: `a1-interp` | Workload: `W1` | Mode: `shadow-load`
- Scans: `350` | Deadline misses: `0`
- Eval duration (ns): p50=651875, p95=924292, p99=1224126, p99.9=1978375, max=1978375
- Cycle duration (ns): p50=661209, p99=1238417, max=1986250
- Peak RSS: `764` KB | Peak PSS: `776` KB | Min MemAvailable: `363736` KB | Max SoC Temp: `49950` mC
