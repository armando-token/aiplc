# Benchmark Report: a1-aot-W1-shadow

- Host: `luckfox` (`arm`, kernel `6.1.99`, RT `SCHED_FIFO(prio=55)`)
- Engine: `a1-aot` | Workload: `W1` | Mode: `shadow`
- Scans: `350` | Deadline misses: `0`
- Eval duration (ns): p50=806167, p95=999250, p99=1026667, p99.9=1162875, max=1162875
- Cycle duration (ns): p50=819001, p99=1041542, max=1176292
- Peak RSS: `2212` KB | Peak PSS: `785` KB | Min MemAvailable: `363952` KB | Max SoC Temp: `46980` mC
