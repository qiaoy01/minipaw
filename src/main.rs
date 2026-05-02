use std::process;

fn main() {
    match minipaw::cli::run_from_env() {
        Ok(code) => process::exit(code),
        Err(err) => {
            eprintln!("minipaw failed: {err}");
            process::exit(1);
        }
    }
}
