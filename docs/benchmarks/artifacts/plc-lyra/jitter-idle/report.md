# Benchmark Report: jitter-idle

- Host: `luckfox` (`arm`, kernel `6.1.99`, RT `SCHED_FIFO(prio=60)`)
- Engine: `none` | Workload: `scheduler-jitter` | Mode: `idle`
- Scans: `500` | Deadline misses: `0`
- Eval duration (ns): p50=2625, p95=3209, p99=3500, p99.9=4083, max=4083
- Cycle duration (ns): p50=2625, p99=3500, max=4083
- Peak RSS: `3096` KB | Peak PSS: `2498` KB | Min MemAvailable: `364316` KB | Max SoC Temp: `48762` mC
