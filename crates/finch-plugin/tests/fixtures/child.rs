// Compiled by the integration tests; intentionally no third-party dependencies.
use std::io::{self, BufRead, Write};
use std::time::Duration;

fn main() {
    let executable = std::env::current_exe().unwrap();
    let name = executable.file_name().unwrap().to_string_lossy();
    if name.contains("no-read") {
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let offset = line.find("\"id\":").unwrap() + 5;
        let id: u64 = line[offset..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap();
        if line.contains("\"method\":\"initialize\"") {
            let version = if name.contains("bad-version") { 2 } else { 1 };
            println!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"api_version\":{version}}}}}"
            );
            io::stdout().flush().unwrap();
            if name.contains("blocked-writer") {
                std::thread::sleep(Duration::from_secs(30));
            }
            continue;
        }
        if line.contains("\"command\":\"crash\"") {
            std::process::exit(23);
        }
        if line.contains("\"command\":\"hang\"") {
            std::thread::sleep(Duration::from_secs(30));
        }
        if line.contains("\"command\":\"malformed\"") {
            println!("not-json");
        } else if line.contains("\"command\":\"oversized\"") {
            println!("{}", "x".repeat(1024 * 1024 + 1));
        } else if line.contains("\"command\":\"mismatch\"") {
            println!("{{\"jsonrpc\":\"2.0\",\"id\":999999,\"result\":{{}}}}");
        } else if line.contains("\"command\":\"remote-error\"") {
            println!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32602,\"message\":\"invalid command\"}}}}"
            );
        } else {
            if line.contains("\"command\":\"stderr\"") {
                eprintln!("{}", "diagnostic\n".repeat(32_000));
            }
            let pid = std::process::id();
            println!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"text\":\"{pid}\",\"navigate_to_page\":3}}}}"
            );
        }
        io::stdout().flush().unwrap();
    }
}
