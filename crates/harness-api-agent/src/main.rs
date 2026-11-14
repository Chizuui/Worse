//! `harness-api-agent` binary (dev and gates); the product runs the same code as `harness-slint --api-agent` (§9.9).

fn main() -> std::process::ExitCode {
    harness_api_agent::run()
}
