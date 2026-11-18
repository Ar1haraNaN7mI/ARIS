use std::process::Command;

// Exercise the real main-thread stack: in-process parser tests run on Rust's
// larger test-thread stacks and missed a Windows-only command-builder overflow.
#[test]
fn help_completion_and_conflicts_work_in_the_real_entrypoint() {
    for (arguments, expected_status, expected_text) in [
        (vec!["--help"], 0, "Usage:"),
        (vec!["team", "--help"], 0, "handoff"),
        (vec!["models", "--help"], 0, "model"),
        (vec!["web", "--help"], 0, "--assets"),
        (vec!["computer", "--help"], 0, "session"),
        (vec!["completion", "bash"], 0, "complete"),
        (
            vec!["completion", "powershell"],
            0,
            "Register-ArgumentCompleter",
        ),
        (vec!["--startup"], 2, "unexpected argument"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_aris"))
            .args(&arguments)
            .output()
            .expect("launch CLI entrypoint");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(expected_status),
            "{arguments:?}: {stderr}"
        );
        let message = if expected_status == 0 {
            &stdout
        } else {
            &stderr
        };
        assert!(message.contains(expected_text), "{arguments:?}: {message}");
    }
}
