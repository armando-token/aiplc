# Benchmark Report: soak-1000

- Host: `luckfox` (`arm`, kernel `6.1.99`, RT `SOAK`)
- Engine: `wamr-aot` | Workload: `M1-M2-swap-rollback` | Mode: `soak`
- Scans: `1000` | Deadline misses: `0`
- Eval duration (ns): p50=17881044, p95=21462877, p99=25197669, p99.9=40305171, max=40934046
- Cycle duration (ns): p50=17881044, p99=25197669, max=40934046
- Peak RSS: `20064` KB | Peak PSS: `19213` KB | Min MemAvailable: `348752` KB | Max SoC Temp: `49356` mC
