use oracle_process_prototype::harness::{HarnessResult, run};
use serde_json::json;
use std::path::PathBuf;

#[tokio::main(worker_threads = 2)]
async fn main() {
    if let Err(error) = execute().await {
        eprintln!("P1 failed: {error}");
        std::process::exit(1);
    }
}
async fn execute() -> HarnessResult<()> {
    let mut alpha = None;
    let mut beta = None;
    let mut output = None;
    let mut cycles = 30;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--help" {
            println!("p1 --alpha PATH --beta PATH --cycles N --output PATH (N >= 30)");
            return Ok(());
        }
        let value = args.next().ok_or("flag requires a value")?;
        match flag.as_str() {
            "--alpha" => alpha = Some(PathBuf::from(value)),
            "--beta" => beta = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            "--cycles" => cycles = value.parse()?,
            _ => return Err(format!("unknown argument {flag}").into()),
        }
    }
    let alpha = alpha.ok_or("--alpha is required")?;
    let beta = beta.ok_or("--beta is required")?;
    let output = output.ok_or("--output is required")?;
    let result = run(&alpha, &beta, cycles).await;
    let report = match &result {
        Ok(report) => report.clone(),
        Err(error) => json!({"schema": 1, "status": "failed", "error": error.to_string()}),
    };
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    result?;
    println!("P1 passed; report: {}", output.display());
    Ok(())
}
