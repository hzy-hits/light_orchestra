/// fake_codex — a test fixture that mimics the `codex exec --json` interface.
///
/// CLI interface (mirrors real codex):
///   fake_codex exec -C <cwd> -s <sandbox> -o <output_path> --json --skip-git-repo-check <prompt>
///
/// Behaviour driven by prompt content:
///   - "SUCCEED"      -> exit 0, emit token_count + task_complete JSONL, write output file
///   - "FAIL"         -> exit 1, emit nothing
///   - "SLOW_SUCCEED" -> sleep 200ms then behave like SUCCEED
///   - "HANG"         -> sleep forever (until killed)
///   - "LARGE_OUTPUT" -> exit 0, write a ~4KB output file
///   - "STDERR_FAIL"  -> write to stderr then exit 1
use std::env;
use std::fs;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut output_path: Option<String> = None;
    let mut prompt: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                if i < args.len() {
                    output_path = Some(args[i].clone());
                }
            }
            "exec" | "--json" | "--skip-git-repo-check" => {}
            "-C" | "-s" | "-m" => {
                i += 1;
            }
            arg => {
                if !arg.starts_with('-') {
                    prompt = Some(arg.to_string());
                }
            }
        }
        i += 1;
    }

    let prompt = prompt.unwrap_or_default();

    let slow = prompt.contains("SLOW_SUCCEED");
    let succeed = prompt.contains("SUCCEED"); // covers both SUCCEED and SLOW_SUCCEED
    let fail = prompt.contains("FAIL") && !prompt.contains("STDERR_FAIL");
    let hang = prompt.contains("HANG");
    let large = prompt.contains("LARGE_OUTPUT");
    let stderr_fail = prompt.contains("STDERR_FAIL");

    if hang {
        // Sleep forever; the runner should kill us via SIGTERM/SIGKILL.
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    if stderr_fail {
        eprintln!("fake_codex: simulated stderr error from task");
        std::process::exit(1);
    }

    if fail {
        std::process::exit(1);
    }

    if slow {
        std::thread::sleep(Duration::from_millis(200));
    }

    if large {
        // Write a meaningfully large output (~4KB) so chunk tests can exercise offsets.
        let line = "The quick brown fox jumps over the lazy dog. ";
        let content = line.repeat(100); // ~4.5KB
        println!(
            "{}",
            r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":5000,"output_tokens":200}}}}"#
        );
        println!("{}", r#"{"payload":{"type":"task_complete"}}"#);
        if let Some(path) = output_path {
            if let Some(parent) = std::path::Path::new(&path).parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(&path, &content).expect("failed to write large output file");
        }
        std::process::exit(0);
    }

    if succeed {
        println!(
            "{}",
            r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"output_tokens":50}}}}"#
        );
        println!("{}", r#"{"payload":{"type":"task_complete"}}"#);
        if let Some(path) = output_path {
            if let Some(parent) = std::path::Path::new(&path).parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(&path, "# Result\nDone.\n").expect("failed to write output file");
        }
        std::process::exit(0);
    }

    eprintln!("fake_codex: unrecognised prompt (no SUCCEED/FAIL/SLOW_SUCCEED/HANG/LARGE_OUTPUT/STDERR_FAIL): {:?}", prompt);
    std::process::exit(1);
}
