use std::process::ExitCode;

fn main() -> ExitCode {
    let invocation = manuvra_cli::invoke(std::env::args_os());
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if serde_json::to_writer(&mut output, &invocation.output).is_err()
        || std::io::Write::write_all(&mut output, b"\n").is_err()
    {
        return ExitCode::from(70);
    }
    ExitCode::from(invocation.exit_code)
}
