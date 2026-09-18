use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

fn tocsin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tocsin"));
    command.env_remove("TYPESAFE_API_KEY");
    command
}

#[test]
fn triage_prints_routed_json_lines() {
    let mut child = tocsin()
        .args(["triage", "--offline", "--no-cache", "--only", "page,ticket"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"GET /health 200\n{\"level\":\"error\",\"msg\":\"payment 81 failed\"}\nOut of memory: killed process 912\nGET /health 200\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    let lines: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["route"], "ticket");
    assert_eq!(lines[0]["template"], "error payment <NUM> failed");
    assert_eq!(lines[1]["route"], "page");
    assert_eq!(lines[1]["line"], "Out of memory: killed process 912");
    assert!(String::from_utf8_lossy(&output.stderr).contains("4 lines → 3 templates"));
}

#[test]
fn requires_an_api_key_unless_offline() {
    let output = tocsin().args(["triage", "/dev/null"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TYPESAFE_API_KEY"));
}

#[test]
fn eval_reports_against_labels() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("labeled.tsv");
    let out = dir.path().join("report.json");
    std::fs::write(&data, "1\tkernel panic on node 4\n0\tjob 17 finished\n0\tjob 18 finished\n1\tkernel panic on node 9\n").unwrap();

    let output = tocsin()
        .args(["eval", "--offline", "--no-cache"])
        .arg(&data)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(report["lines"], 4);
    assert_eq!(report["positives"], 2);
    assert_eq!(report["templates"], 2);
    assert_eq!(report["scorers"][0]["at_threshold"]["tp"], 2);
    assert_eq!(report["scorers"][0]["at_threshold"]["fp"], 0);
}

#[test]
fn eval_rejects_unknown_labels() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("labeled.tsv");
    std::fs::write(&data, "0\tjob started\ntrue\tkernel panic\n").unwrap();
    let output = tocsin()
        .args(["eval", "--offline", "--no-cache"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("line 2: label must be 0 or 1"));
}

#[test]
fn triage_fails_on_unreadable_input() {
    let output = tocsin()
        .args(["triage", "--offline", "--no-cache"])
        .arg(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reading input"));
}

#[test]
fn triage_exits_cleanly_when_the_reader_goes_away() {
    let mut child = tocsin()
        .args(["triage", "--offline", "--no-cache", "--only", "log"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let lines: String = (0..20_000)
        .map(|i| format!("GET /health 200 took {i} ms\n"))
        .collect();
    let _ = child.stdin.take().unwrap().write_all(lines.as_bytes());
    assert!(child.wait().unwrap().success());
}
