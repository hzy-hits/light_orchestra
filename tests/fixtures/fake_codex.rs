/// fake_codex — a test fixture that mimics the `codex exec --json` interface.
///
/// CLI interface (mirrors real codex):
///   fake_codex exec -C <cwd> -s <sandbox> -o <output_path> --json --skip-git-repo-check <prompt>
///
/// Behaviour driven by prompt content:
///   - "SUCCEED"      -> exit 0, emit token_count + task_complete JSONL, write output file
///   - "FAIL"         -> exit 1, emit nothing
///   - "SLOW_SUCCEED" -> sleep 200ms then behave like SUCCEED
use std::env;
use std::fs;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();

    // Parse -o <output_path> from args
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
            // Flags to skip: exec, -C <val>, -s <val>, --json, --skip-git-repo-check, -m <val>
            "exec" | "--json" | "--skip-git-repo-check" => {}
            "-C" | "-s" | "-m" => {
                // consume value
                i += 1;
            }
            arg => {
                // Anything not recognised as a flag/flag-value is the prompt
                // (it's the last positional argument)
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
    let fail = prompt.contains("FAIL");

    if slow {
        std::thread::sleep(Duration::from_millis(200));
    }

    if fail {
        std::process::exit(1);
    }

    if succeed {
        // Emit valid JSONL events to stdout.
        // Use print! + "\n" with escaped braces to avoid format-string interpretation.
        println!("{}", r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"output_tokens":50}}}}"#);
        println!("{}", r#"{"payload":{"type":"task_complete"}}"#);

        // Write output file
        if let Some(path) = output_path {
            // Create parent dirs if needed
            if let Some(parent) = std::path::Path::new(&path).parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(&path, "# Result\nDone.\n").expect("failed to write output file");
        }

        std::process::exit(0);
    }

    // Unknown prompt — exit 1
    eprintln!("fake_codex: unrecognised prompt (no SUCCEED/FAIL/SLOW_SUCCEED): {:?}", prompt);
    std::process::exit(1);
}
