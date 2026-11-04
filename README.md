<p align="center"><img src="https://raw.githubusercontent.com/lucerna-desktop/lucerna-sdk/main/docs/assets/logo.svg" alt="Lucerna" height="120"></p>

# lucerna-sdk

Base comum dos apps do ecossistema Lucerna em Rust: o protocolo de conversa entre o serviço de cada app e os clientes (o painel no shell, a TUI e a linha de comando), o esqueleto do serviço sob demanda e da linha de comando, os caminhos e o manifesto.

**Situação:** a primeira versão (`0.1.0`) dos seis crates está pronta e testada, com o protocolo `1.0` e o app de exemplo [`lucerna-hello`](https://github.com/lucerna-desktop/lucerna-sdk/blob/main/apps/lucerna-hello). O lado do shell (o host de painéis e o `shell.sock`) é um projeto do `lucerna-shell`, ainda por fazer.

## Crates

| Crate | O que traz |
| --- | --- |
| [`lucerna-protocol`](https://crates.io/crates/lucerna-protocol) | Os tipos das mensagens (JSON-RPC 2.0, `hello`, `state`, erros), a versão do protocolo e os nomes reservados, sem I/O. |
| [`lucerna-ipc`](https://crates.io/crates/lucerna-ipc) | O cliente e o servidor do socket Unix: enquadramento por linha, aperto de mão, tópicos com retrato e `seq`, limite de tamanho e a subida do serviço pelo cliente (`connect_or_spawn`). |
| [`lucerna-service`](https://crates.io/crates/lucerna-service) | O esqueleto do serviço: socket do systemd, saída por ociosidade, trabalho pendente, timers agendados (do systemd ou o agendador local) e a conexão ao `shell.sock`. |
| [`lucerna-cli`](https://crates.io/crates/lucerna-cli) | O esqueleto da linha de comando: subcomandos como métodos, `status`, `watch`, `--json` e códigos de saída. |
| [`lucerna-paths`](https://crates.io/crates/lucerna-paths) | As pastas XDG (incluindo a pasta de estado) e o socket de cada app. |
| [`lucerna-manifest`](https://crates.io/crates/lucerna-manifest) | O manifesto JSON dos apps: leitura, validação e compatibilidade da `api` do painel. |

Um app depende só do `lucerna-service` e do `lucerna-cli`, que reexportam o que ele usa dos outros.

## Exemplo mínimo

Um binário só, `lucerna-<app>`, é o serviço (`serve`, o `ExecStart` da unidade do systemd), a linha de comando e a TUI. Um contador com um tópico (`counter`) e um método (`counter.add`), no molde do [`lucerna-hello`](https://github.com/lucerna-desktop/lucerna-sdk/blob/main/apps/lucerna-hello):

```toml
[dependencies]
lucerna-service = "0.1"
lucerna-cli = "0.1"
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use lucerna_cli::clap::{Arg, Command, value_parser};
use lucerna_cli::{Call, CliApp, CliCommand, Invocation, usage_exit};
use lucerna_service::{RpcError, ServiceBuilder, ServiceError};
use serde_json::json;

const APP: &str = "contador";

/// O serviço: o tópico `counter` e o método `counter.add {"n"}`.
async fn serve() -> i32 {
    let mut service = ServiceBuilder::new(APP, env!("CARGO_PKG_VERSION"));
    let server = service.server();
    let counter = server.topic("counter", json!({"value": 0}));
    let value = Arc::new(Mutex::new(0i64));
    server.method("counter.add", move |params| {
        let r = match params.as_ref().and_then(|p| p["n"].as_i64()) {
            Some(n) => {
                let mut v = value.lock().unwrap();
                *v += n;
                counter.publish(json!({"value": *v})); // os assinantes recebem
                Ok(json!({"value": *v}))
            }
            None => Err(RpcError::invalid_params("n precisa ser um número inteiro")),
        };
        async move { r }
    });
    match service.run().await {
        Ok(()) => 0, // ociosidade ou SIGTERM
        Err(ServiceError::AlreadyRunning(_)) => 0, // outro serviço já atende
        Err(e) => {
            eprintln!("erro: {e}");
            1
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // `lucerna-contador add 3` chama `counter.add {"n":3}`; `status`, `watch`
    // e `--json` vêm de graça.
    let cli = CliApp::new(APP, "lucerna-contador", env!("CARGO_PKG_VERSION"), "Um contador.")
        .command(CliCommand {
            command: Command::new("add")
                .about("Soma n ao contador.")
                .arg(Arg::new("n").required(true).value_parser(value_parser!(i64))),
            to_call: |m| {
                Ok(Call {
                    method: "counter.add".into(),
                    params: Some(json!({ "n": m.get_one::<i64>("n") })),
                })
            },
            render: None,
        });
    let code = match cli.parse_from(std::env::args_os()) {
        Ok(Invocation::Serve) => serve().await, // o ExecStart do systemd
        Ok(Invocation::Cli(run)) => cli.run_from_env(run).await,
        Ok(Invocation::Tui(_args)) => 0, // aqui o app abre a TUI
        Err(e) => usage_exit(&e),
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
```

Com o serviço rodando, `lucerna-contador add 3` imprime `value: 3`, e `lucerna-contador watch counter` acompanha o tópico. O [`lucerna-hello`](https://github.com/lucerna-desktop/lucerna-sdk/blob/main/apps/lucerna-hello) mostra o resto: um tópico contínuo, itens na barra do shell, não perturbe, as unidades do systemd, o manifesto e os testes.

## Sem systemd

O app funciona sem o systemd (e sem o shell):

- Quando ninguém atende o socket, a linha de comando sobe `lucerna-<app> serve` sozinha, solto da CLI, com o log em `$XDG_STATE_HOME/lucerna/<app>/serve.log`. Só depois de 2 s sem conseguir ela sai com o código `2`, dizendo o porquê.
- Se o serviço cair, o próximo cliente o sobe de novo.
- O trabalho agendado vai pelo agendador local, que mantém o serviço de pé até o horário (com o systemd, continua nos timers do systemd do usuário).
- Só enquanto houver um timer persistente marcado, o serviço mantém a entrada `$XDG_CONFIG_HOME/autostart/lucerna-<app>.desktop`, que o sobe no login do KDE, do GNOME e do XFCE para cumprir o que venceu com a sessão fechada; sem timer persistente, a entrada some e nada sobe no login. No Hyprland sem systemd, que não lê essa pasta, `exec-once = lucerna-<app> serve` na configuração faz esse papel (sem nada a fazer, o serviço sai por ociosidade).

## Ligação com o Lucerna

Faz parte do ecossistema do [Lucerna](https://github.com/lucerna-desktop/lucerna-shell), o shell de desktop para o Hyprland. O shell cuida só da interface; a lógica fica em Rust, neste e nos outros repositórios do ecossistema.

## Documentação

- [`docs/superpowers/specs/2026-10-19-protocolo-design.md`](https://github.com/lucerna-desktop/lucerna-sdk/blob/main/docs/superpowers/specs/2026-10-19-protocolo-design.md): o spec do protocolo, do manifesto, do serviço e da linha de comando;
- [`docs/ecossistema.md`](https://github.com/lucerna-desktop/lucerna-sdk/blob/main/docs/ecossistema.md): como os repositórios do ecossistema se encaixam;
- [`lucerna-sdk — Pendências.md`](https://github.com/lucerna-desktop/lucerna-sdk/blob/main/lucerna-sdk%20%E2%80%94%20Pend%C3%AAncias.md): o que falta fazer.

## Repositório

A branch `main` é protegida por um ruleset igual ao do `lucerna-shell`: PR obrigatório, conversas resolvidas, só merge commit, sem deleção nem force push. Os checks `fmt`, `clippy` e `test` são obrigatórios no ruleset.

## Desenvolvimento

O CI (`fmt`, `clippy` e `test`) roda estes comandos; rode-os antes de abrir o PR:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Os testes não tocam o systemd real: o `PATH` dos testes de processo não tem `systemctl`. `LUCERNA_NO_SPAWN=1` desliga a subida do serviço pela linha de comando.

Os testes de processo criam sockets Unix, então o `TMPDIR` precisa ser curto (o CI fixa `TMPDIR=/tmp`). O teste que usa o `systemctl --user` de verdade é `#[ignore]` e só roda numa sessão com systemd de usuário:

```bash
cargo test --workspace --locked -- --ignored
```

## Licença

[GPL-3.0](https://github.com/lucerna-desktop/lucerna-sdk/blob/main/LICENSE).
