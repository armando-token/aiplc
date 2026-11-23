# Benchmark Report: jitter-load

- Host: `luckfox` (`arm`, kernel `6.1.99`, RT `SCHED_FIFO(prio=60)`)
- Engine: `none` | Workload: `scheduler-jitter` | Mode: `stress`
- Scans: `500` | Deadline misses: `0`
- Eval duration (ns): p50=1750, p95=2917, p99=3209, p99.9=3792, max=3792
- Cycle duration (ns): p50=1750, p99=3209, max=3792
- Peak RSS: `3420` KB | Peak PSS: `2598` KB | Min MemAvailable: `364164` KB | Max SoC Temp: `49950` mC
