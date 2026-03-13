use crate::tracking;
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

lazy_static! {
    // forge build noise lines to suppress
    static ref RE_BUILD_NOISE: Regex = Regex::new(
        r"(?x)
        ^Compiling\s+\d+\s+files? |
        ^Solc\s+[\d.]+ \s+finished |
        ^Compiler\s+run\s+successful |
        ^Nothing\s+to\s+compile |
        ^No\s+files\s+changed
        "
    ).unwrap();

    // forge test result lines — use .* so greedy backtracking handles nested
    // brackets like: [FAIL: ...; args=[hex, 617]] testName(...)
    static ref RE_TEST_RESULT: Regex = Regex::new(
        r"^\[(PASS|FAIL).*\]\s+\S+"
    ).unwrap();

    static ref RE_FAIL: Regex = Regex::new(
        r"^\[FAIL"
    ).unwrap();

    // Trace block start
    static ref RE_TRACES_HEADER: Regex = Regex::new(
        r"^Traces:"
    ).unwrap();

    // Suite result lines (keep for summary)
    static ref RE_SUITE_RESULT: Regex = Regex::new(
        r"^(Test result:|Ran \d+ test|Suite result:|Encountered)"
    ).unwrap();

    // Gas amount in brackets at start of trace line: [31493]
    static ref RE_GAS: Regex = Regex::new(
        r"\[\d+\]"
    ).unwrap();

    // Ethereum address: 0x followed by 40 hex chars
    static ref RE_ADDRESS: Regex = Regex::new(
        r"0x([0-9a-fA-F]{40})"
    ).unwrap();

    // Large integers (7+ digits) — simple match, we'll skip if inside a hex addr
    static ref RE_LARGE_INT: Regex = Regex::new(
        r"\b(\d{7,})\b"
    ).unwrap();

    // Box-drawing chars used by forge traces
    static ref RE_BOX_DRAWING: Regex = Regex::new(
        r"[├└│─]+"
    ).unwrap();

    // Leading whitespace before trace content (to measure depth)
    static ref RE_TRACE_INDENT: Regex = Regex::new(
        r"^(\s*)[├└│\s]*(.+)$"
    ).unwrap();
}

pub fn run_build(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("forge");
    cmd.arg("build");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: forge build {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run forge build. Is Foundry installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });

    let filtered = filter_forge_build(&raw);

    if !filtered.is_empty() {
        println!("{}", filtered);
    }

    timer.track(
        &format!("forge build {}", args.join(" ")),
        &format!("rtk forge build {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

pub fn run_test(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    // cli.verbose captures -v flags that clap eats before trailing_var_arg.
    // Also check args in case user spelled it out explicitly.
    let has_verbosity = verbose > 0
        || args
            .iter()
            .any(|a| matches!(a.as_str(), "-v" | "-vv" | "-vvv" | "-vvvv" | "--verbosity"));

    let mut cmd = Command::new("forge");
    cmd.arg("test");
    // Disable progress spinner — overrides show_progress=true in foundry.toml
    cmd.env("FOUNDRY_SHOW_PROGRESS", "false");
    // Forward RTK's verbose level back to forge as its own -v flags
    if verbose > 0 {
        cmd.arg(format!("-{}", "v".repeat(verbose as usize)));
    }
    for arg in args {
        cmd.arg(arg);
    }

    // Show immediate feedback so large suites don't appear frozen.
    eprint!("[forge] running...");

    // Stream stdout line-by-line so large test suites don't appear to hang.
    // stderr is inherited so compilation errors flow directly to the terminal.
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .context("Failed to run forge test. Is Foundry installed?")?;

    let (pass_count, fail_count, raw_len) = if let Some(stdout) = child.stdout.take() {
        stream_and_print_forge_test(BufReader::new(stdout), has_verbosity)
    } else {
        (0, 0, 0)
    };
    // Clear the "running..." line (stream_and_print already clears its own progress lines)
    eprint!("\r\x1b[2K");

    let status = child.wait().context("forge test process error")?;
    let exit_code = status
        .code()
        .unwrap_or(if status.success() { 0 } else { 1 });

    let summary = match (pass_count, fail_count) {
        (0, 0) => String::new(),
        (p, 0) => format!("{} passed", p),
        (0, f) => format!("{} failed", f),
        (p, f) => format!("{} passed, {} failed", p, f),
    };
    if !summary.is_empty() {
        println!("{}", summary);
    }

    let raw_placeholder = " ".repeat(raw_len);
    let filtered_placeholder = " ".repeat(summary.len());
    timer.track(
        &format!("forge test {}", args.join(" ")),
        &format!("rtk forge test {}", args.join(" ")),
        &raw_placeholder,
        &filtered_placeholder,
    );

    if !status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

pub fn run_other(args: &[OsString], verbose: u8) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("forge: no subcommand specified");
    }

    let timer = tracking::TimedExecution::start();
    let subcommand = args[0].to_string_lossy();
    let mut cmd = Command::new("forge");
    cmd.arg(&*subcommand);
    for arg in &args[1..] {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: forge {} ...", subcommand);
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run forge {}", subcommand))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    print!("{}", stdout);
    eprint!("{}", stderr);

    timer.track(
        &format!("forge {}", subcommand),
        &format!("rtk forge {}", subcommand),
        &raw,
        &raw,
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

/// Filter forge build output: suppress noise, keep errors and warnings.
fn filter_forge_build(output: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if RE_BUILD_NOISE.is_match(trimmed) {
            continue;
        }
        lines.push(line);
    }

    lines.join("\n")
}

/// Stream forge test output line-by-line, printing results live:
/// - [FAIL] lines and their traces print immediately as they arrive
/// - [PASS] lines are silently counted; a running tally prints to stderr
/// - forge's own summary/section lines are dropped
/// Returns (pass_count, fail_count, raw_byte_count); caller prints the final summary.
fn stream_and_print_forge_test<R: BufRead>(
    reader: R,
    has_verbosity: bool,
) -> (usize, usize, usize) {
    #[derive(PartialEq)]
    enum State {
        Scanning,
        InFailTrace,
        InPassTrace,
    }

    let mut state = State::Scanning;
    let mut pass_count: usize = 0;
    let mut fail_count: usize = 0;
    let mut seen_fails: HashSet<String> = HashSet::new();
    let mut raw_len: usize = 0;
    let mut last_reported = 0usize;

    for line in reader.lines().map_while(Result::ok) {
        raw_len += line.len() + 1;
        let trimmed = line.trim().to_string();

        let is_block_end = RE_TEST_RESULT.is_match(&trimmed)
            || RE_SUITE_RESULT.is_match(&trimmed)
            || trimmed == "Failing tests:";

        match state {
            State::InFailTrace | State::InPassTrace => {
                if is_block_end {
                    state = State::Scanning;
                    // fall through to handle this line
                } else {
                    if state == State::InFailTrace && !trimmed.is_empty() {
                        println!("{}", compress_trace_line(&line));
                    }
                    continue;
                }
            }
            State::Scanning => {}
        }

        if RE_SUITE_RESULT.is_match(&trimmed) || trimmed == "Failing tests:" {
            continue;
        }
        if RE_TRACES_HEADER.is_match(&trimmed) {
            continue;
        }

        if RE_TEST_RESULT.is_match(&trimmed) {
            if RE_FAIL.is_match(&trimmed) {
                if seen_fails.insert(trimmed.clone()) {
                    fail_count += 1;
                    println!("{}", trimmed);
                    if has_verbosity {
                        state = State::InFailTrace;
                    }
                }
            } else {
                pass_count += 1;
                if has_verbosity {
                    state = State::InPassTrace;
                }
            }

            // Print a live progress count to stderr every 10 tests so large
            // suites don't look frozen. Use \r to overwrite the same line.
            let total = pass_count + fail_count;
            if total >= last_reported + 10 {
                last_reported = total;
                eprint!("\r[forge] {} passed, {} failed...", pass_count, fail_count);
            }
        }
    }

    // Clear the progress line from stderr
    if last_reported > 0 {
        eprint!("\r\x1b[2K");
    }

    (pass_count, fail_count, raw_len)
}

/// Filter forge test output from a string (used in tests).
/// - Never show passing test traces
/// - Always show failing test lines (deduplicated) and their traces
/// - Compress traces (strip gas, compact addresses, tabs for indentation)
/// - Show "N passed, M failed" summary at the bottom
fn filter_forge_test(output: &str, has_verbosity: bool) -> String {
    #[derive(PartialEq)]
    enum State {
        Scanning,
        InFailTrace,
        InPassTrace,
    }

    let mut result: Vec<String> = Vec::new();
    let mut state = State::Scanning;
    let mut pass_count: usize = 0;
    let mut fail_count: usize = 0;
    // Forge prints failing tests twice: inline + "Failing tests:" section.
    // Track seen fail lines to deduplicate.
    let mut seen_fails: HashSet<String> = HashSet::new();

    for line in output.lines() {
        let trimmed = line.trim();

        // Section headers that terminate a trace block
        let is_block_end = RE_TEST_RESULT.is_match(trimmed)
            || RE_SUITE_RESULT.is_match(trimmed)
            || trimmed == "Failing tests:";

        match state {
            State::InFailTrace | State::InPassTrace => {
                if is_block_end {
                    state = State::Scanning;
                    // fall through to handle this line as Scanning
                } else {
                    if state == State::InFailTrace && !trimmed.is_empty() {
                        result.push(compress_trace_line(line));
                    }
                    continue;
                }
            }
            State::Scanning => {}
        }

        // Drop forge's own summary/section lines — we emit our own summary
        if RE_SUITE_RESULT.is_match(trimmed) || trimmed == "Failing tests:" {
            continue;
        }

        // "Traces:" header — just skip it, trace lines follow
        if RE_TRACES_HEADER.is_match(trimmed) {
            continue;
        }

        // Test result lines
        if RE_TEST_RESULT.is_match(trimmed) {
            if RE_FAIL.is_match(trimmed) {
                if seen_fails.insert(trimmed.to_string()) {
                    fail_count += 1;
                    result.push(trimmed.to_string());
                    if has_verbosity {
                        state = State::InFailTrace;
                    }
                }
                // duplicate [FAIL] line from "Failing tests:" section — skip
            } else {
                pass_count += 1;
                if has_verbosity {
                    state = State::InPassTrace;
                }
            }
        }
    }

    // Build final output
    let mut out = result.join("\n");

    let summary = match (pass_count, fail_count) {
        (0, 0) => String::new(),
        (p, 0) => format!("{} passed", p),
        (0, f) => format!("{} failed", f),
        (p, f) => format!("{} passed, {} failed", p, f),
    };

    if !summary.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&summary);
    }

    out
}

/// Compress a forge trace line:
/// - Strip gas amounts: [31493]
/// - Compact addresses: 0x<40hex> → 0x1234..abcd
/// - Convert large integers to scientific notation
/// - Replace box-drawing indent with tabs
fn compress_trace_line(line: &str) -> String {
    // Measure indent depth from box-drawing characters
    // Each level of nesting adds 2 spaces + a drawing char
    // Count leading whitespace groups of 2 as one tab level
    let leading_spaces = line.len() - line.trim_start().len();
    let depth = leading_spaces / 2;
    let tabs = "\t".repeat(depth);

    // Start with trimmed content
    let mut s = line.trim().to_string();

    // Strip box-drawing characters
    s = RE_BOX_DRAWING.replace_all(&s, "").to_string();
    s = s.trim().to_string();

    // Strip gas amounts in brackets
    s = RE_GAS.replace_all(&s, "").to_string();

    // Compact addresses
    s = RE_ADDRESS
        .replace_all(&s, |caps: &regex::Captures| {
            let hex = &caps[1];
            format!("0x{}..{}", &hex[..4], &hex[36..])
        })
        .to_string();

    // Scientific notation for large integers
    s = RE_LARGE_INT
        .replace_all(&s, |caps: &regex::Captures| {
            let n: u64 = caps[1].parse().unwrap_or(0);
            to_sci_notation(n)
        })
        .to_string();

    // Remove double spaces left by stripping
    while s.contains("  ") {
        s = s.replace("  ", " ");
    }
    s = s.trim().to_string();

    format!("{}{}", tabs, s)
}

/// Convert a large integer to scientific notation string (e.g. 1000000 → 1e6).
fn to_sci_notation(n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let exp = (n as f64).log10().floor() as u32;
    let mantissa = n as f64 / 10f64.powi(exp as i32);
    // Only use sci notation if it's actually shorter
    let raw = n.to_string();
    let sci = if (mantissa - mantissa.round()).abs() < 1e-9 {
        format!("{}e{}", mantissa.round() as u64, exp)
    } else {
        format!("{:.2}e{}", mantissa, exp)
    };
    if sci.len() < raw.len() {
        sci
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- forge build ---

    #[test]
    fn test_filter_forge_build_suppresses_noise() {
        let input = "\
Compiling 5 files with Solc 0.8.24
Solc 0.8.24 finished in 843.28ms
Compiler run successful!
";
        let out = filter_forge_build(input);
        assert!(out.is_empty(), "expected empty output, got: {}", out);
    }

    #[test]
    fn test_filter_forge_build_shows_errors() {
        let input = "\
Compiling 5 files with Solc 0.8.24
Error (7576): Member \"foo\" not found
 --> src/Counter.sol:45:9:
  |
45 |         Counter.foo();
Compiler run successful!
";
        let out = filter_forge_build(input);
        assert!(out.contains("Error (7576)"));
        assert!(out.contains("Counter.sol"));
        assert!(!out.contains("Compiling"));
        assert!(!out.contains("Compiler run successful"));
    }

    #[test]
    fn test_filter_forge_build_shows_warnings() {
        let input = "\
Compiling 2 files with Solc 0.8.24
Warning (2072): Unused local variable
 --> src/Token.sol:12:9
Compiler run successful!
";
        let out = filter_forge_build(input);
        assert!(out.contains("Warning"));
        assert!(out.contains("Token.sol"));
        assert!(!out.contains("Compiling"));
    }

    // --- forge test ---

    #[test]
    fn test_filter_forge_test_passes_only() {
        let input = "\
Ran 3 tests for test/Counter.t.sol:CounterTest
[PASS] testIncrement() (gas: 31493)
[PASS] testSetNumber(uint256) (runs: 256, μ: 6766, ~: 6766)
[PASS] testFuzz() (gas: 12000)
Test result: ok. 3 passed; 0 failed; finished in 1.23ms
Ran 1 test suite in 9.90ms (1.23ms CPU time): 3 tests passed, 0 failed, 0 skipped (3 total tests)
";
        let out = filter_forge_test(input, false);
        assert!(!out.contains("[PASS]"), "should not show passing tests");
        assert!(out.contains("3 passed"), "should mention pass count");
    }

    #[test]
    fn test_filter_forge_test_failures_shown() {
        let input = "\
Ran 3 tests for test/Counter.t.sol:CounterTest
[PASS] testIncrement() (gas: 31493)
[FAIL. Reason: assertion failed] testSetNumber(uint256) (runs: 1, μ: 5000, ~: 5000)
[PASS] testFuzz() (gas: 12000)
Test result: FAILED. 2 passed; 1 failed; finished in 1.23ms
";
        let out = filter_forge_test(input, false);
        assert!(!out.contains("[PASS]"), "should not show passing tests");
        assert!(out.contains("[FAIL"), "should show failing test");
        assert!(out.contains("2 passed, 1 failed"), "should show summary");
    }

    #[test]
    fn test_filter_forge_test_nested_brackets_in_fail() {
        // Fuzz counterexample: [FAIL: ...; args=[hex, 617]] testName(...)
        let input = "\
[PASS] testFoo() (gas: 1000)
[FAIL: assertion failed; counterexample: calldata=0xaabbccdd args=[0x000003d6, 617]] testBrutalizedUint248(bytes32,uint248) (runs: 0, μ: 0, ~: 0)

Failing tests:
Encountered 1 failing test in test/Brutalizer.t.sol:BrutalizerTest
[FAIL: assertion failed; counterexample: calldata=0xaabbccdd args=[0x000003d6, 617]] testBrutalizedUint248(bytes32,uint248) (runs: 0, μ: 0, ~: 0)

Encountered a total of 1 failing tests, 1 tests succeeded
";
        let out = filter_forge_test(input, false);
        assert!(out.contains("[FAIL"), "should detect nested-bracket fail");
        assert!(
            out.contains("testBrutalizedUint248"),
            "should show test name"
        );
        assert!(
            out.contains("1 passed, 1 failed"),
            "should show correct summary"
        );
        // dedup: the [FAIL] line appears twice in forge output, must only count once
        assert_eq!(
            out.matches("[FAIL").count(),
            1,
            "should deduplicate fail line"
        );
    }

    #[test]
    fn test_filter_forge_test_traces_only_for_failures() {
        let input = "\
[PASS] testIncrement() (gas: 31493)
Traces:
  [31493] CounterTest::testIncrement()
    ├─ [22508] Counter::setNumber(0)
    └─ ← [Stop]

[FAIL. Reason: assertion failed] testBroken()
Traces:
  [9999] CounterTest::testBroken()
    ├─ [1000] Counter::increment()
    └─ ← [Revert]

Test result: FAILED. 1 passed; 1 failed;
";
        let out = filter_forge_test(input, true);
        assert!(!out.contains("testIncrement"), "pass traces must be hidden");
        assert!(out.contains("testBroken"), "fail line must be shown");
        assert!(
            out.contains("Counter::increment"),
            "fail trace must be shown"
        );
        assert!(
            !out.contains("Counter::setNumber"),
            "pass trace must be hidden"
        );
    }

    // --- compress_trace_line ---

    #[test]
    fn test_compress_strips_gas() {
        let line = "  ├─ [31493] Counter::increment()";
        let out = compress_trace_line(line);
        assert!(!out.contains("31493"), "gas should be stripped");
        assert!(out.contains("Counter::increment"));
    }

    #[test]
    fn test_compress_compacts_address() {
        let line = "  ├─ [1000] 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266::call()";
        let out = compress_trace_line(line);
        assert!(!out.contains("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"));
        assert!(out.contains("0xf39F..2266"));
    }

    #[test]
    fn test_compress_sci_notation() {
        assert_eq!(to_sci_notation(1_000_000_000_000_000_000), "1e18");
        assert_eq!(to_sci_notation(1_000_000), "1e6");
        // 1500000 (7 chars) vs 1.50e6 (6 chars) → sci is shorter
        assert_eq!(to_sci_notation(1_500_000), "1.50e6");
        // small numbers stay as-is
        assert_eq!(to_sci_notation(42), "42");
    }

    #[test]
    fn test_compress_tabs_for_depth() {
        let line = "    ├─ Counter::call()"; // 4 spaces = depth 2
        let out = compress_trace_line(line);
        assert!(out.starts_with("\t\t"), "should use 2 tabs for depth 2");
    }

    // --- token savings ---

    #[test]
    fn test_build_token_savings() {
        let input = "\
Compiling 12 files with Solc 0.8.24
Solc 0.8.24 finished in 843.28ms
Compiler run successful!
";
        let out = filter_forge_build(input);
        let in_tokens = input.split_whitespace().count();
        let out_tokens = out.split_whitespace().count();
        assert_eq!(out_tokens, 0, "pure noise should produce zero output");
        let _ = in_tokens; // savings = 100%
    }

    #[test]
    fn test_test_token_savings() {
        let input = "\
[PASS] testA() (gas: 10000)
[PASS] testB() (gas: 20000)
[PASS] testC() (gas: 30000)
[PASS] testD() (gas: 40000)
[PASS] testE() (gas: 50000)
Test result: ok. 5 passed; 0 failed; finished in 1ms
";
        let out = filter_forge_test(input, false);
        let in_tokens = input.split_whitespace().count();
        let out_tokens = out.split_whitespace().count();
        // Expect at least 50% reduction on flat results (real savings are much higher with traces)
        assert!(
            out_tokens * 2 < in_tokens,
            "expected >50% reduction, got {}/{} tokens",
            out_tokens,
            in_tokens
        );
    }
}
