//! O binário `lucerna-hello`: `serve` roda o serviço; os outros subcomandos
//! falam com ele. Sem argumentos (ou com `tui …`, que abriria a TUI), mostra a
//! ajuda: o hello não tem TUI.

use std::fmt::Display;
use std::process::ExitCode;

use lucerna_cli::{Invocation, usage_exit};
use lucerna_hello::{cli, hello};
use lucerna_service::Env;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli();
    let code = match cli.parse_from(std::env::args_os()) {
        // Sem TUI no hello: `lucerna-hello` e `lucerna-hello tui …` mostram a ajuda.
        Ok(Invocation::Tui(_)) => report(cli.clap().print_help()),
        Ok(Invocation::Serve) => {
            // O produtor do `ticks` vive até o serviço parar.
            let h = hello(&Env::from_process());
            let _producer = h.producer;
            report(h.service.run().await)
        }
        Ok(Invocation::Cli(run)) => cli.run_from_env(run).await,
        Err(e) => usage_exit(&e),
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// `0` no sucesso; no erro, `erro: …` no stderr e `1`.
fn report<E: Display>(r: Result<(), E>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("erro: {e}");
            1
        }
    }
}
