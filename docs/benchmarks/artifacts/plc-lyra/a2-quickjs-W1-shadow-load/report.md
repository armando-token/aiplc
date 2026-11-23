# Benchmark Report: a2-quickjs-W1-shadow-load

- Host: `luckfox` (`arm`, kernel `6.1.99`, RT `SCHED_FIFO(prio=55)`)
- Engine: `a2-quickjs` | Workload: `W1` | Mode: `shadow-load`
- Scans: `350` | Deadline misses: `0`
- Eval duration (ns): p50=1053792, p95=1248917, p99=1741541, p99.9=2401583, max=2401583
- Cycle duration (ns): p50=1063417, p99=1759625, max=2409166
- Peak RSS: `3112` KB | Peak PSS: `1255` KB | Min MemAvailable: `363984` KB | Max SoC Temp: `51138` mC
