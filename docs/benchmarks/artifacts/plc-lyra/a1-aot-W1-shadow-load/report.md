# Benchmark Report: a1-aot-W1-shadow-load

- Host: `luckfox` (`arm`, kernel `6.1.99`, RT `SCHED_FIFO(prio=55)`)
- Engine: `a1-aot` | Workload: `W1` | Mode: `shadow-load`
- Scans: `350` | Deadline misses: `0`
- Eval duration (ns): p50=613666, p95=849041, p99=1039500, p99.9=2844625, max=2844625
- Cycle duration (ns): p50=623583, p99=1057000, max=2859500
- Peak RSS: `2276` KB | Peak PSS: `788` KB | Min MemAvailable: `364384` KB | Max SoC Temp: `50544` mC
