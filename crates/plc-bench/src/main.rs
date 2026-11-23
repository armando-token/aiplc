use std::path::PathBuf;
use plc_bench::runner::{run_engine_benchmark, run_hil_suite, run_jitter, run_soak};

fn arg_val(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find_map(|w| if w[0] == flag { Some(w[1].clone()) } else { None })
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str).unwrap_or("");
    match sub {
        "jitter" => {
            let out = PathBuf::from(arg_val(&args, "--out").unwrap_or_else(|| "/tmp/plc-jitter".into()));
            let scans = arg_val(&args, "--scans").and_then(|s| s.parse().ok()).unwrap_or(1000);
            let period_ms = arg_val(&args, "--period-ms").and_then(|s| s.parse().ok()).unwrap_or(20);
            let load = arg_val(&args, "--load").as_deref() == Some("stress");
            run_jitter(&out, scans, period_ms, load);
        }
        "run" => {
            let engine = arg_val(&args, "--engine").unwrap_or_else(|| "a1-aot".into());
            let workload = arg_val(&args, "--workload").unwrap_or_else(|| "W1".into());
            let mode = arg_val(&args, "--mode").unwrap_or_else(|| "active".into());
            let scans = arg_val(&args, "--scans").and_then(|s| s.parse().ok()).unwrap_or(1000);
            let profile = PathBuf::from(arg_val(&args, "--profile").expect("--profile required"));
            let pubkey = PathBuf::from(arg_val(&args, "--pubkey").expect("--pubkey required"));
            let pkg_dir = PathBuf::from(arg_val(&args, "--pkg-dir").expect("--pkg-dir required"));
            let worker_bin = PathBuf::from(arg_val(&args, "--worker-bin").expect("--worker-bin required"));
            let out = PathBuf::from(arg_val(&args, "--out").expect("--out required"));
            run_engine_benchmark(&engine, &workload, &mode, scans, &profile, &pubkey, &pkg_dir, &worker_bin, &out);
        }
        "soak" => {
            let swaps = arg_val(&args, "--swaps").and_then(|s| s.parse().ok()).unwrap_or(1000);
            let profile = PathBuf::from(arg_val(&args, "--profile").expect("--profile required"));
            let pubkey = PathBuf::from(arg_val(&args, "--pubkey").expect("--pubkey required"));
            let pkg_dir = PathBuf::from(arg_val(&args, "--pkg-dir").expect("--pkg-dir required"));
            let data_dir = PathBuf::from(arg_val(&args, "--data-dir").unwrap_or_else(|| "/tmp/plc-soak-data".into()));
            let out = PathBuf::from(arg_val(&args, "--out").expect("--out required"));
            run_soak(swaps, &profile, &pubkey, &pkg_dir, &data_dir, &out);
        }
        "hil" => {
            let profile = PathBuf::from(arg_val(&args, "--profile").expect("--profile required"));
            let pubkey = PathBuf::from(arg_val(&args, "--pubkey").expect("--pubkey required"));
            let pkg_dir = PathBuf::from(arg_val(&args, "--pkg-dir").expect("--pkg-dir required"));
            let worker_bin = PathBuf::from(arg_val(&args, "--worker-bin").expect("--worker-bin required"));
            let sup_bin = PathBuf::from(arg_val(&args, "--supervisor-bin").expect("--supervisor-bin required"));
            let data_dir = PathBuf::from(arg_val(&args, "--data-dir").unwrap_or_else(|| "/tmp/plc-hil-data".into()));
            let out = PathBuf::from(arg_val(&args, "--out").expect("--out required"));
            let hw = has_flag(&args, "--hardware-gpio");
            run_hil_suite(&profile, &pubkey, &pkg_dir, &worker_bin, &sup_bin, &data_dir, &out, hw);
        }
        _ => {
            eprintln!("Usage: plc-bench <jitter|run|soak|hil> [flags]");
            std::process::exit(2);
        }
    }
}
