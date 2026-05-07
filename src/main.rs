mod cli;
mod error;
mod generate;
mod geocode;
mod geofeed;
mod init_netbox;
mod netbox;
mod s3;

use std::process;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Load .env before anything else; real env vars and CLI flags win.
    // It's fine if the file doesn't exist.
    let _ = dotenvy::dotenv();

    let code = match cli::run().await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            error::exit_code_for(&e)
        }
    };

    process::exit(code);
}
