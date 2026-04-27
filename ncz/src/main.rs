use std::process::ExitCode;

fn main() -> ExitCode {
    match ncz::run() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => {
            eprintln!("ncz: {err}");
            ExitCode::from(err.exit_code().clamp(0, 255) as u8)
        }
    }
}
