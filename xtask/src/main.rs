use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("sentinel-baseline-guard") => sentinel_baseline_guard(&args[2..]),
        _ => {
            eprintln!("Usage: xtask <task>");
            eprintln!();
            eprintln!("Tasks:");
            eprintln!("  sentinel-baseline-guard [base_ref]  Check sentinel editing rules");
            std::process::exit(1);
        }
    }
}

fn sentinel_baseline_guard(extra_args: &[String]) {
    let base_ref = extra_args.first().map(|s| s.as_str()).unwrap_or("main");

    let checks_output = Command::new("git")
        .args([
            "diff",
            "--name-only",
            &format!("{}...HEAD", base_ref),
            "--",
            "autonoetic-gateway/src/sentinel/checks/",
        ])
        .output()
        .expect("failed to run git diff for checks/");

    let baseline_output = Command::new("git")
        .args([
            "diff",
            "--name-only",
            &format!("{}...HEAD", base_ref),
            "--",
            "autonoetic-gateway/src/sentinel/baseline/",
        ])
        .output()
        .expect("failed to run git diff for baseline/");

    let checks_files = String::from_utf8_lossy(&checks_output.stdout);
    let baseline_files = String::from_utf8_lossy(&baseline_output.stdout);

    let checks_touched: Vec<&str> = checks_files
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let baseline_touched: Vec<&str> = baseline_files
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();

    if checks_touched.is_empty() || baseline_touched.is_empty() {
        println!(
            "ok: only one of checks/ ({} files) or baseline/ ({} files) is modified.",
            checks_touched.len(),
            baseline_touched.len()
        );
        std::process::exit(0);
    }

    let log_output = Command::new("git")
        .args([
            "log",
            "--format=%s",
            &format!("{}...HEAD", base_ref),
        ])
        .output()
        .expect("failed to run git log");

    let commit_subjects = String::from_utf8_lossy(&log_output.stdout);

    let has_prefix = commit_subjects.lines().any(|line| {
        line.starts_with("[baseline-update]") || line.starts_with("[baseline-update ")
    });

    if has_prefix {
        println!("ok: [baseline-update] prefix found in commit history.");
        std::process::exit(0);
    }

    eprintln!("error: this branch modifies both sentinel/checks/ and sentinel/baseline/");
    eprintln!("       without a [baseline-update] prefix in any commit message.");
    eprintln!();
    eprintln!("The frozen-baseline contract requires that changes to baseline/ land in");
    eprintln!("a separate PR with a [baseline-update] prefix in the commit message.");
    eprintln!();
    eprintln!("Files in sentinel/checks/:");
    for f in &checks_touched {
        eprintln!("  {}", f);
    }
    eprintln!();
    eprintln!("Files in sentinel/baseline/:");
    for f in &baseline_touched {
        eprintln!("  {}", f);
    }
    eprintln!();
    eprintln!("If this is a deliberate baseline update, amend your commit to start");
    eprintln!("with '[baseline-update]' and re-push.");
    std::process::exit(1);
}
