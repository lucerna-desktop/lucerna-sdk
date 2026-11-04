# Serviço sem systemd — plano de implementação

> **Para agentes:** SUB-SKILL OBRIGATÓRIO: use superpowers:subagent-driven-development (o método já escolhido) para implementar este plano tarefa por tarefa. Os passos usam checkbox (`- [ ]`) para acompanhamento.

**Objetivo:** o serviço de qualquer app do Lucerna funciona sem systemd: o cliente (a linha de comando, e qualquer cliente que use o `lucerna-ipc`) sobe `lucerna-<app> serve` quando ninguém atende no socket, o próximo cliente o sobe de novo depois de uma queda, e o trabalho agendado vai por um agendador local dentro do serviço quando o systemd do usuário não está lá.

**Arquitetura:** de baixo para cima. `lucerna-paths` ganha a pasta de estado (o log do serviço subido pelo cliente) e o arquivo de início automático do XDG. `lucerna-ipc` ganha `ServeCommand` e `Client::connect_or_spawn` (roda `<programa> serve` solto, num grupo de processos novo, e tenta conectar por até 2 s). `lucerna-cli` passa a usar isso no `CliApp::run`. `lucerna-service` ganha o `LocalScheduler` (horários no próprio serviço, um `WorkGuard` por timer, a entrada `.desktop` enquanto houver timer persistente), a escolha entre ele e o `SystemdScheduler` (`choose_scheduler`) e, no `ServiceBuilder`, o agendador do serviço (`scheduler()`), o aviso de disparo (`on_timer`) e o gancho de subida (`on_start`). O `lucerna-hello` ganha um lembrete, que exercita tudo isso de ponta a ponta sem systemd.

**Tecnologia:** Rust 1.95, edição 2024; `tokio` 1 (o mesmo conjunto de features; nada de `process`), `std::process::Command` com `CommandExt::process_group(0)`; nenhuma dependência nova.

**Spec:** `docs/superpowers/specs/2026-10-19-protocolo-design.md`, seções "Serviço sob demanda", "Serviço sem systemd", "Linha de comando e binários" e "Testes" (leia o spec inteiro antes da primeira tarefa). Roteiro do trabalho: o corpo do PR #13 (`gh pr view 13 --repo lucerna-desktop/lucerna-sdk`). Contexto: `docs/ecossistema.md` ("Cada app funciona sozinho"). O plano anterior, `docs/superpowers/plans/2026-10-19-protocolo.md`, mostra como os crates foram feitos.

## Global Constraints

- Branch: `feat/servico-sem-systemd` (já criada a partir da `main` com o PR #13). Commits em português, no estilo `Caminhos: a pasta de estado e o log do serviço`. Conta do `gh`: `EduardoSA8006`; remote `origin` = `github-pessoal:lucerna-desktop/lucerna-sdk.git`. Merge só por merge commit, pelo usuário.
- Toolchain: `cargo` em `~/develop/cargo/bin`; `rust-toolchain.toml` (`1.95`, `rustfmt`, `clippy`). Verificação de toda tarefa, os mesmos comandos do CI (`.github/workflows/ci.yml`), com `TMPDIR=/tmp` (sockets Unix têm limite de ~108 bytes no caminho):
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked` — sem avisos e sem falhas.
- `unsafe_code = "deny"` no workspace: nada de `setsid` no `pre_exec`; o "grupo de sessão novo" do spec é `CommandExt::process_group(0)` (grupo de processos novo: o Ctrl+C do terminal não chega ao serviço).
- **Nenhum teste toca o systemd, o D-Bus nem a sessão real.** O `SystemdScheduler` só é exercitado com um `systemctl` falso (script). Os testes de processo do `lucerna-hello` rodam com `env_clear()` e `PATH` = uma pasta temporária `bin/` (vazia, ou com um `systemctl` falso): sem `systemctl` no `PATH`, o serviço escolhe o agendador local. O teste `#[ignore]` que já existe (`real_systemd_round_trip`) continua o único a usar o systemd de verdade.
- **Nenhum teste escreve nas pastas reais do usuário** (`~/.config/autostart`, `~/.local/state`, `$XDG_RUNTIME_DIR` real): todo teste em processo passa um `Env` de uma pasta do `tempfile` (no `ServiceBuilder::env`, no `LocalScheduler::new`, no `CliApp::run`); todo teste de processo define `HOME`, `XDG_RUNTIME_DIR` e as variáveis XDG no filho.
- **Nenhum teste em processo do `lucerna-cli` usa `Spawn::CurrentExe`**: o executável atual de um teste é o binário de testes, e `<binário de teste> serve` rodaria os testes cujo nome contém `serve`. Os testes usam `Spawn::Never` ou `Spawn::Program(<script>)`.
- Valores fixos: espera da subida `SPAWN_WAIT = 2 s`; espera crescente entre tentativas `10 ms × 2^n`, no máximo `200 ms`; log do serviço em `$XDG_STATE_HOME/lucerna/<app>/serve.log` (`$XDG_STATE_HOME` absoluto ou `$HOME/.local/state`), recomeçado do zero quando passa de `LOG_MAX = 1 MiB` (`1_048_576` bytes) na hora de subir; passo máximo do relógio do agendador local `WAKE_STEP = 30 s`; entrada de início automático em `$XDG_CONFIG_HOME/autostart/lucerna-<app>.desktop`; variável `LUCERNA_NO_SPAWN` (definida e não vazia) desliga a subida pela linha de comando.
- Códigos de saída da linha de comando não mudam: `0`, `1`, `2` (sem serviço, agora só depois de tentar subir), `64`.
- Princípio do ecossistema: sem systemd ou sem shell é silêncio, nunca erro; o serviço não escreve "erro" no log por perder a corrida do lock (`AlreadyRunning` sai `0` sem nada no stderr).
- O protocolo não muda (versão `1.0`). Os crates ainda não foram publicados no crates.io, então mudanças de API (campo novo no `Env`, variantes novas no `ClientError`, `hello(env)`) não pedem troca de versão neste plano.

## Review Focus

1. **Linha de comando com a saída num pipe** (`lucerna-hello add 1 | cat`, `x=$(lucerna-hello status)`): o serviço subido pela CLI não pode herdar o stdout/stderr dela, senão o pipe só fecha quando o serviço sair (60 s depois). O esperado é a CLI terminar na hora. Teste na Tarefa 7 (`spawned_service_does_not_hold_the_pipes`).
2. **Máquina suspensa com um lembrete marcado**: o relógio monotônico do `tokio` não anda durante a suspensão; um lembrete das 7 h marcado às 23 h, com a máquina suspensa a noite toda, dispararia horas depois. O esperado é disparar até `WAKE_STEP` depois de o horário de parede passar. Teste na Tarefa 4 (`wall_clock_jump_fires_within_a_step`).
3. **Serviço que morre ao subir** (configuração quebrada, um arquivo comum no lugar do socket): o esperado é a CLI desistir em ~2 s com o código `2` e o caminho do log, e o log ter o porquê. Teste na Tarefa 7 (`service_that_dies_exits_2_with_the_log`).
4. **Queda com coisas para trás** (o serviço morto com `kill -9`: o socket velho fica, e a entrada `.desktop` de um lembrete persistente pode ficar sem lembrete nenhum): o esperado é o próximo cliente subir o serviço de novo, e o serviço apagar a entrada velha ao sair quando não há mais timer persistente. Testes na Tarefa 7 (`next_client_respawns_after_a_kill`, `stale_autostart_is_removed_on_exit`).
5. **Trocar a persistência de um timer com o mesmo nome** (persistente → não persistente, ou cancelar o último persistente com outro não persistente marcado): o esperado é a entrada `.desktop` existir só enquanto houver ao menos um persistente. Teste na Tarefa 4 (`switching_persistence_updates_autostart`).

---

## Decisões deste plano (o que o spec e o PR #13 deixavam ao construtor)

| Tema | Decisão | Por quê |
| --- | --- | --- |
| Quem monta o comando de subida | `lucerna_ipc::ServeCommand { program, log, envs }`; o `lucerna-ipc` não depende do `lucerna-paths`, quem resolve o log é o chamador (a CLI) | o `lucerna-ipc` continua sem regras de pasta; o shell (QML) e outros clientes montam o mesmo comando |
| Ambiente do serviço subido | a CLI passa ao filho as variáveis do `Env` que usou (`Env::vars()`: `HOME` e as `XDG_*` definidas) | com um `Env` explícito (testes, ou um app que muda o `Env`), o serviço cria o socket onde a CLI procura |
| Filhos que viram zumbi | uma thread por subida faz `child.wait()` | num cliente de vida longa (a TUI, o shell) o processo que saiu é recolhido; sem `unsafe` e sem `tokio::process` |
| Erros que sobem o serviço | só `NotFound` e `ConnectionRefused` no `connect`; o resto (permissão, um serviço que não responde) volta como antes | é "ninguém ouvindo"; os outros casos não se resolvem subindo outro serviço |
| Espelho síncrono do cliente | nenhum | não existe cliente síncrono no SDK (`lucerna-ipc/src/sync.rs` é só a trava interna) nem TUI ainda; quando a TUI vier, ela usa o `Client` assíncrono |
| A CLI e a subida | `CliApp::spawn(Spawn)` com `Spawn::{CurrentExe, Program(PathBuf), Never}`, padrão `CurrentExe`; `LUCERNA_NO_SPAWN` vence e vira `Never` | os testes em processo precisam injetar o programa (ver Global Constraints); a variável serve aos testes de processo e a quem depura |
| Escolha do agendador | `choose_scheduler(app, env, systemctl) -> SchedulerKind`: `Systemd` se `systemctl --user show -p LoadState --value lucerna-<app>.service` responde `loaded`; qualquer falha (sem `systemctl`, sem barramento, unidade não instalada) → `Local` | o PR #13 citava só `Io` e `ServiceNotInstalled`; sem barramento de sessão o `systemctl` sai com erro (`Systemctl {..}`), e isso também é "sem systemd" |
| Quando escolher | no `ServiceBuilder::run()`, logo depois de pegar o socket (o spec: "ao subir"); no `run_with` (testes, desenvolvimento), no primeiro uso do agendador. `ServiceBuilder::scheduler_kind(k)` força um dos dois | um `serve` que perde a corrida do lock não roda `systemctl` nem mexe em arquivo; os testes em processo forçam `Local` |
| Agendador do serviço | `ServiceBuilder::scheduler() -> Timers` (`Clone`, `impl Scheduler`), que resolve para `SystemdScheduler` ou `LocalScheduler` | o app não sabe qual está em uso (o spec), e pode pegar a alça antes do `run` |
| Disparo | `Fired { name, guard: WorkGuard }` num `mpsc` sem limite; o `ServiceBuilder` chama o `on_timer(name)` do app, um de cada vez, e só larga o guarda quando o future termina | o serviço não sai por ociosidade no meio do trabalho do lembrete; sem `on_timer`, o disparo é descartado |
| Timer depois de disparar | sai do agendador local sozinho (`cancel` depois é `Ok`, sem efeito) | é o que um `OnCalendar` de data fixa faz; o app chama `cancel` igual nos dois agendadores |
| Com o systemd escolhido | `on_timer` nunca é chamado: o timer do systemd sobe o serviço, e o app confere o que venceu no `on_start` | o contrato do spec ("ao subir, o serviço confere o que venceu") cobre os dois |
| Gancho de subida | `ServiceBuilder::on_start(f)`: roda depois de o serviço pegar o socket e antes de atender | sem ele, o `serve` que perde a corrida do lock também faria o trabalho vencido (lembrete em dobro) |
| Relógio do agendador local | horário de parede (`SystemTime`), conferido a cada no máximo `WAKE_STEP = 30 s`; relógio injetável (`with_clock`, oculto da doc) para os testes com o tempo do `tokio` pausado | o `tokio` usa o relógio monotônico, que para na suspensão (Review Focus 2) |
| Entrada de início automático | `Exec=lucerna-<app> serve` (o nome, como no spec, não o caminho do executável), escrita por arquivo temporário + `rename`; apagada quando não sobra timer persistente e também na saída do serviço (`run()`) | o binário instalado está no `PATH` da sessão; o `rename` não deixa entrada pela metade; a limpeza na saída cobre a entrada velha de uma queda |
| Systemd escolhido com uma entrada `.desktop` velha | apagada ao escolher | o systemd cobre o login; a entrada subiria o serviço à toa |
| `AlreadyRunning` no `lucerna-hello serve` | sai `0` sem escrever nada | é o caso normal de dois clientes subindo juntos (o PR #13); o teste `second_serve_is_refused` vira `second_serve_exits_quietly` |
| Lembrete do `lucerna-hello` | método `reminder.set {"in", "persistent"}`, `reminder.cancel`, tópico `reminder`, arquivo `reminder.json` na pasta de dados, subcomandos `remind <segundos> [--persistent]` e `forget` | o spec pede o teste "o trabalho vencido com o serviço parado sai ao subir", que só existe com um app que guarde o que agendou; o hello é o molde que os apps copiam |
| `hello()` | passa a ser `hello(env: &Env)` | o lembrete lê a pasta de dados; os testes em processo passam um `Env` temporário |

## Mapa de arquivos

| Arquivo | O que muda |
| --- | --- |
| `crates/lucerna-paths/src/lib.rs`, `tests/paths.rs` | `Env::xdg_state_home`, `state_home`, `vars`; `AppPaths::state_dir`, `serve_log`, `autostart_file` |
| `crates/lucerna-ipc/src/spawn.rs` (novo), `client.rs`, `lib.rs`, `tests/spawn.rs` (novo) | `ServeCommand`, `SPAWN_WAIT`, `LOG_MAX`, `Client::connect_or_spawn`, `ClientError::{NotStarted, Spawn}` |
| `crates/lucerna-cli/src/lib.rs`, `tests/cli.rs` | `Spawn`, `CliApp::spawn`, `LUCERNA_NO_SPAWN`, `run` com `connect_or_spawn` |
| `crates/lucerna-service/src/local.rs` (novo), `schedule.rs`, `tests/local.rs` (novo) | `LocalScheduler`, `Fired`, `render_autostart`, `SchedulerKind`, `choose_scheduler` |
| `crates/lucerna-service/src/timers.rs` (novo), `service.rs`, `lib.rs`, `activation.rs`, `tests/service.rs` | `Timers`, `ServiceBuilder::{scheduler, scheduler_kind, env, on_timer, on_start}`, reexports, docs |
| `apps/lucerna-hello/src/{lib,main}.rs`, `tests/common/mod.rs`, `tests/reminder.rs` (novo), `tests/process.rs` | lembrete, `hello(env)`, `AlreadyRunning` → `0`, testes sem systemd |
| `README.md`, `apps/lucerna-hello/README.md`, o spec, `docs/ecossistema.md` | documentação |

---

### Tarefa 1: `lucerna-paths` — pasta de estado, log do serviço e início automático

**Files:**
- Modify: `crates/lucerna-paths/src/lib.rs`
- Test: `crates/lucerna-paths/tests/paths.rs`

**Interfaces:**
- Consumes: nada.
- Produces:
  - `Env` ganha `pub xdg_state_home: Option<PathBuf>` (lido de `XDG_STATE_HOME` no `from_lookup`, com a mesma regra de vazio = ausente).
  - `pub fn Env::state_home(&self) -> Result<PathBuf, PathsError>` — `$XDG_STATE_HOME` se absoluto, senão `$HOME/.local/state` (mesma regra do `base`).
  - `pub fn Env::vars(&self) -> Vec<(&'static str, PathBuf)>` — os pares definidos, na ordem `HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR` (os ausentes ficam de fora; os valores como estão, mesmo relativos).
  - `pub fn AppPaths::state_dir(&self) -> Result<PathBuf, PathsError>` = `state_home/lucerna/<app>`.
  - `pub fn AppPaths::serve_log(&self) -> Result<PathBuf, PathsError>` = `state_dir/serve.log`.
  - `pub fn AppPaths::autostart_file(&self) -> Result<PathBuf, PathsError>` = `config_home/autostart/lucerna-<app>.desktop`.
  - A doc do crate passa a citar a pasta de estado ("o log do serviço subido pelo cliente").

- [ ] **Passo 1: testes que falham** em `tests/paths.rs`:

```rust
#[test]
fn state_and_autostart_paths() {
    let p = AppPaths::new("routine", &env("/home/u")).unwrap();
    assert_eq!(p.state_dir().unwrap(), PathBuf::from("/home/u/.local/state/lucerna/routine"));
    assert_eq!(p.serve_log().unwrap(), PathBuf::from("/home/u/.local/state/lucerna/routine/serve.log"));
    assert_eq!(p.autostart_file().unwrap(), PathBuf::from("/home/u/.config/autostart/lucerna-routine.desktop"));
    let mut e = env("/home/u");
    e.xdg_state_home = Some("/estado".into());
    e.xdg_config_home = Some("/cfg".into());
    let p = AppPaths::new("routine", &e).unwrap();
    assert_eq!(p.serve_log().unwrap(), PathBuf::from("/estado/lucerna/routine/serve.log"));
    assert_eq!(p.autostart_file().unwrap(), PathBuf::from("/cfg/autostart/lucerna-routine.desktop"));
    e.xdg_state_home = Some("relativo".into());
    assert_eq!(AppPaths::new("a", &e).unwrap().state_dir().unwrap(), PathBuf::from("/home/u/.local/state/lucerna/a"));
    let sem_home = Env::default();
    assert!(matches!(AppPaths::new("a", &sem_home).unwrap().serve_log(), Err(PathsError::NoHome)));
}

#[test]
fn state_home_is_read_from_the_environment() {
    // from_lookup com XDG_STATE_HOME="/s" → xdg_state_home == Some("/s"); com "" → None.
}

#[test]
fn vars_lists_only_what_is_set() {
    let e = Env { home: Some("/h".into()), xdg_runtime_dir: Some("/run/user/1".into()), xdg_state_home: Some("/s".into()), ..Env::default() };
    assert_eq!(e.vars(), vec![("HOME", "/h".into()), ("XDG_STATE_HOME", "/s".into()), ("XDG_RUNTIME_DIR", "/run/user/1".into())]);
}
```

- [ ] **Passo 2: rodar e ver falhar.** `cargo test -p lucerna-paths` → erro de compilação (`state_dir`, `xdg_state_home` não existem).
- [ ] **Passo 3: implementar** em `crates/lucerna-paths/src/lib.rs`, no molde de `config_home`/`cache_home`.
- [ ] **Passo 4: verificar.** `cargo test -p lucerna-paths` passa; os três comandos do CI passam (os `Env { .. }` de outros crates usam `..Env::default()` e continuam compilando; o que não usar, ajuste).
- [ ] **Passo 5: commit.** `git commit -m "Caminhos: a pasta de estado, o log do serviço e o início automático"`

---

### Tarefa 2: `lucerna-ipc` — subir o serviço pelo cliente

**Files:**
- Create: `crates/lucerna-ipc/src/spawn.rs`
- Modify: `crates/lucerna-ipc/src/client.rs` (variantes do `ClientError`, `connect_or_spawn`), `crates/lucerna-ipc/src/lib.rs` (`pub mod spawn; pub use spawn::{ServeCommand, SPAWN_WAIT, LOG_MAX};`)
- Test: `crates/lucerna-ipc/tests/spawn.rs`

**Interfaces:**
- Consumes: `Client::connect` (já existe).
- Produces:
  - `pub const SPAWN_WAIT: Duration` = 2 s; `pub const LOG_MAX: u64` = `1_048_576`.
  - `pub struct ServeCommand { pub program: PathBuf, pub log: PathBuf, pub envs: Vec<(OsString, OsString)> }` (`Clone, Debug`); `pub fn new(program: impl Into<PathBuf>, log: impl Into<PathBuf>) -> Self`; `pub fn env(self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self`.
  - `pub fn ServeCommand::spawn(&self) -> io::Result<()>` — cria a pasta do log (`create_dir_all`), abre o log em modo de acréscimo (se o arquivo passa de `LOG_MAX`, abre truncando), roda `program serve` com `stdin` = `/dev/null`, `stdout` e `stderr` = o log (`try_clone` do arquivo), `process_group(0)` e `envs`; uma thread (`std::thread::spawn`) faz `child.wait()`. Não espera o serviço.
  - `ClientError` ganha:
    - `NotStarted { log: PathBuf }` — `#[error("O serviço não subiu em 2 s; veja o log em {}.", log.display())]`;
    - `Spawn { program: PathBuf, source: io::Error }` — `#[error("Não foi possível rodar {} serve: {source}.", program.display())]`.
  - `pub async fn Client::connect_or_spawn(path: &Path, client: &str, app: &str, serve: &ServeCommand) -> Result<Client, ClientError>` — `Client::connect`; com `Err(Connect(e))` de tipo `NotFound`/`ConnectionRefused`, `serve.spawn()` (`Err` → `Spawn`) e novas tentativas de `Client::connect` com espera `min(10 ms × 2^n, 200 ms)` até `SPAWN_WAIT` desde a subida; cada tentativa que ainda dá `NotFound`/`ConnectionRefused` continua; qualquer outro resultado volta como está; o prazo passou → `NotStarted { log }`. Sem prazo para o aperto de mão (quem chama põe o seu, como a CLI faz).
  - Doc do módulo `spawn`: o molde do spec ("Serviço sem systemd", passo 2); dois clientes juntos sobem dois `serve`, e o lock do `bind_manual` faz o segundo sair; por que `process_group(0)` e não `setsid`.

Os testes usam scripts `/bin/sh` numa pasta temporária no lugar do binário do app (ajudante `script(dir, name, body) -> PathBuf` com modo `0755`). Para "o serviço sobe", o script cria um arquivo-marca e a tarefa do teste, ao ver a marca, faz `UnixListener::bind` no socket e atende com um `ServerBuilder` (como `crates/lucerna-ipc/tests/common/mod.rs` já faz).

- [ ] **Passo 1: testes que falham** em `tests/spawn.rs`:

```rust
#[tokio::test]
async fn running_service_is_not_spawned_again() {
    // serviço já atendendo em sock; programa = script que cria `marca`.
    // connect_or_spawn → Ok, hello().app == "demo"; `marca` não existe.
}

#[tokio::test]
async fn missing_socket_spawns_and_connects() {
    let t = tempfile::tempdir().unwrap();
    let sock = t.path().join("run/lucerna/demo.sock"); // a pasta ainda não existe
    let marca = t.path().join("marca");
    let prog = script(t.path(), "demo", &format!("echo \"$@\" > '{}'\necho oi\n", marca.display()));
    let log = t.path().join("state/lucerna/demo/serve.log");
    let serve = ServeCommand::new(&prog, &log).env("LUCERNA_X", "1");
    let srv = serve_when_marked(&marca, &sock); // tarefa: espera a marca, cria a pasta, bind, atende "demo"
    let c = Client::connect_or_spawn(&sock, "test", "demo", &serve).await.unwrap();
    assert_eq!(c.hello().app, "demo");
    assert_eq!(std::fs::read_to_string(&marca).unwrap(), "serve\n");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "oi\n"); // a saída do programa vai para o log
    srv.abort();
}

#[tokio::test]
async fn stale_socket_spawns_too() {
    // um socket velho: UnixListener::bind e drop do listener (o arquivo fica, connect → ConnectionRefused);
    // o script apaga o arquivo antes de marcar; connect_or_spawn → Ok.
}

#[tokio::test]
async fn service_that_never_comes_gives_not_started_with_the_log() {
    // script que só sai (`exit 0`); medir o tempo:
    // Err(ClientError::NotStarted { log }) com log == o caminho passado;
    // 2 s <= elapsed < 3 s; e.to_string() == format!("O serviço não subiu em 2 s; veja o log em {}.", log.display()).
}

#[tokio::test]
async fn missing_program_is_a_spawn_error() {
    // program = t/nao-existe → Err(ClientError::Spawn { program, .. }) com program == o caminho, em < 100 ms.
}

#[test]
fn log_restarts_when_too_big() {
    // log pré-existente com LOG_MAX + 1 bytes; script `echo novo`; ServeCommand::spawn(); esperar o
    // script sair (poll até 2 s pelo conteúdo) → o log é exatamente "novo\n".
    // log pré-existente com "velho\n" → depois "velho\nnovo\n".
}
```

- [ ] **Passo 2: rodar e ver falhar.** `cargo test -p lucerna-ipc --test spawn` → erro de compilação.
- [ ] **Passo 3: implementar** `spawn.rs` e `connect_or_spawn`, com as assinaturas acima.
- [ ] **Passo 4: verificar.** `cargo test -p lucerna-ipc` → tudo passa (o `match` exaustivo do `ClientError` no `lucerna-cli` ainda não cobre as variantes novas: acrescente nesta tarefa, em `CliApp::fail`, um braço provisório `ClientError::NotStarted { .. } | ClientError::Spawn { .. } => self.no_service(err, &e.to_string(), json)` para o workspace compilar; a Tarefa 3 o troca). Os três comandos do CI passam.
- [ ] **Passo 5: commit.** `git commit -m "IPC: o cliente sobe o serviço quando ninguém atende no socket"`

---

### Tarefa 3: `lucerna-cli` — a linha de comando sobe o serviço

**Files:**
- Modify: `crates/lucerna-cli/src/lib.rs` (`CliApp::run`, `fail`, doc do crate)
- Test: `crates/lucerna-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `ServeCommand`, `Client::connect_or_spawn`, `ClientError::{NotStarted, Spawn}` (Tarefa 2); `AppPaths::serve_log`, `Env::vars` (Tarefa 1).
- Produces:
  - `pub enum Spawn { CurrentExe, Program(PathBuf), Never }` (`Clone, Debug`), reexportado na raiz do crate.
  - `pub fn CliApp::spawn(self, s: Spawn) -> Self` (padrão `Spawn::CurrentExe`).
  - `pub const NO_SPAWN_ENV: &str = "LUCERNA_NO_SPAWN"`.
  - `CliApp::run`: com `Spawn::Never`, ou `LUCERNA_NO_SPAWN` definida e não vazia no processo, faz o que faz hoje (`Client::connect_timeout`). Senão, monta `ServeCommand::new(programa, AppPaths::serve_log())` com cada par de `env.vars()` e chama `tokio::time::timeout(CONNECT_TIMEOUT, Client::connect_or_spawn(&path, "cli", &self.app, &serve))` (`Elapsed` → `ClientError::Timeout`). Programa: `Program(p)` → `p`; `CurrentExe` → `std::env::current_exe()` (erro → "sem serviço" com `não deu para achar o executável: {e}.`). Sem caminho de log (`serve_log` falha), não sobe: "sem serviço" com a mensagem do `PathsError`.
  - `fail`: `NotStarted { log }` → `no_service` com `o serviço não subiu em 2 s; veja o log em {log}.`; `Spawn { program, source }` → `no_service` com `não deu para rodar {program} serve: {source}.`. Código `2` nos dois, e no `--json` só `message`, como os outros "sem serviço".
  - A doc do `run` e a do crate dizem que o código `2` vem depois da tentativa de subir.

- [ ] **Passo 1: ajustar o ajudante** `app()` dos testes para `.spawn(Spawn::Never)`: todos os testes de hoje continuam iguais (o `no_service_exits_2` confere o caminho sem subida).
- [ ] **Passo 2: testes que falham:**

```rust
#[tokio::test]
async fn spawns_the_program_and_reaches_the_service() {
    // script `demo` que grava "$@" e $XDG_RUNTIME_DIR na `marca`; o teste, ao ver a marca,
    // sobe o `service(dir)` de sempre no socket. app().spawn(Spawn::Program(script)).
    // run(["demo", "add", "2"]) → (0, "Contador: 2\n", "");
    // marca == format!("serve {}\n", dir.join("run").display()) — o filho recebeu o Env da CLI.
}

#[tokio::test]
async fn program_that_never_serves_exits_2_with_the_log() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let prog = script(t.path(), "demo", "exit 0\n");
    let a = app().spawn(Spawn::Program(prog));
    let (code, out, err) = run_with(&a, &["demo", "status"], &env).await;
    let log = t.path().join(".local/state/lucerna/demo/serve.log");
    assert_eq!((code, out.as_str()), (2, ""));
    assert_eq!(err, format!("Sem conexão com o serviço demo: o serviço não subiu em 2 s; veja o log em {}.\n", log.display()));
}

#[tokio::test]
async fn missing_program_exits_2_at_once() {
    // Spawn::Program(t/nao-existe), "--json status" → código 2 em < 1 s; stderr é JSON com
    // error.message começando por "Sem conexão com o serviço demo: não deu para rodar" e sem error.code.
}
```

(`run_with(&CliApp, args, env)` é o `run` de hoje recebendo o `CliApp`; o `run` antigo passa a chamá-lo com `app()`.)

- [ ] **Passo 3: rodar e ver falhar.** `cargo test -p lucerna-cli --test cli` → erro de compilação (`Spawn`).
- [ ] **Passo 4: implementar** (troca o braço provisório da Tarefa 2 pelos dois braços definitivos).
- [ ] **Passo 5: verificar.** `cargo test -p lucerna-cli` passa; o `LUCERNA_NO_SPAWN` é conferido na Tarefa 7, em processo de verdade (definir a variável dentro do processo de teste exige `unsafe` na edição 2024). Os três comandos do CI passam — inclusive `cargo test -p lucerna-hello`: o `cli_json_and_exit_codes` de hoje começa esperando `2` sem serviço e passaria a subir o serviço; nesta tarefa ponha `.env("LUCERNA_NO_SPAWN", "1")` só nessa primeira chamada (a Tarefa 7 reescreve o arquivo).
- [ ] **Passo 6: commit.** `git commit -m "Linha de comando: sobe o serviço antes de desistir com o código 2"`

---

### Tarefa 4: `lucerna-service` — agendador local e a escolha do agendador

**Files:**
- Create: `crates/lucerna-service/src/local.rs`
- Modify: `crates/lucerna-service/src/schedule.rs` (`SchedulerKind`, `choose_scheduler`, `validate_name`/`now_unix`/checagem de horário compartilhadas com `local.rs` como `pub(crate)`), `crates/lucerna-service/src/lib.rs` (`pub mod local;` e reexports)
- Test: `crates/lucerna-service/tests/local.rs`

**Interfaces:**
- Consumes: `AppPaths::autostart_file` (Tarefa 1); `WorkTracker`, `WorkGuard`, `Scheduler`, `TimerSpec`, `ScheduleError`, `SystemdScheduler` (existem).
- Produces:
  - `pub enum SchedulerKind { Systemd, Local }` (`Clone, Copy, Debug, PartialEq, Eq`).
  - `pub fn choose_scheduler(app: &str, env: &Env, systemctl: &Path) -> SchedulerKind` — `Systemd` se `SystemdScheduler::new(app, env)?.with_systemctl(systemctl)` passa no `check_service` (que hoje é privado: vira `pub(crate)`); qualquer `Err` → `Local`. Não escreve nada.
  - `pub struct Fired { pub name: String, pub guard: WorkGuard }` (`Debug`).
  - `pub const WAKE_STEP: Duration` = 30 s.
  - `pub struct LocalScheduler` (`Clone`; estado num `Arc`):
    - `pub fn new(app: &str, env: &Env, work: WorkTracker, fired: mpsc::UnboundedSender<Fired>) -> Result<LocalScheduler, ScheduleError>` — valida o id; guarda `tokio::runtime::Handle::current()` (chame dentro do runtime; a doc diz que entra em pânico fora dele, como `tokio::spawn`).
    - `#[doc(hidden)] pub fn with_clock(self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self` — o relógio de parede em segundos Unix (padrão: `SystemTime::now()`).
    - `pub fn timers(&self) -> BTreeMap<String, TimerSpec>` — os marcados agora.
    - `pub fn shutdown(&self)` — aborta as tarefas e solta os guardas; a entrada `.desktop` fica se havia timer persistente (o login precisa dela) e some se não havia.
  - `impl Scheduler for LocalScheduler`:
    - `schedule`: mesmas regras de nome e horário do `SystemdScheduler` (`InvalidName`, `InPast` para `at_unix <= agora + 1`, com o `clock`); troca o timer de mesmo nome (aborta a tarefa antiga, solta o guarda antigo); um `work.hold()` por timer; a tarefa dorme `min(at - clock(), WAKE_STEP)` em laço até `clock() >= at`, então tira o timer do mapa, sincroniza a entrada `.desktop` e manda `Fired { name, guard }` (o guarda do timer vai junto; sem receptor, cai). Depois, sincroniza a entrada; se a escrita falhar, desfaz o timer e devolve `Io`.
    - `cancel`: valida o nome; tira o timer (não existir não é erro); sincroniza a entrada.
    - Sincronizar = com ao menos um timer persistente, a entrada existe com o conteúdo de `render_autostart` (escrita em `<arquivo>.tmp` + `rename`, pasta criada); sem nenhum, o arquivo é apagado se existir. Sem `HOME`/`XDG_CONFIG_HOME`: `NoDir` só ao agendar um persistente.
  - `#[doc(hidden)] pub fn render_autostart(app: &str) -> String` — exatamente:

```ini
[Desktop Entry]
Type=Application
Name=Lucerna: <app>
Comment=Serviço do <app>, para o trabalho agendado
Exec=lucerna-<app> serve
Terminal=false
NoDisplay=true
```

  - A doc do módulo `schedule` deixa de dizer que o serviço precisa estar instalado como regra geral: o `SystemdScheduler` continua conferindo e recusando, mas o serviço escolhe o `LocalScheduler` quando o systemd não serve (ver `choose_scheduler` e `Timers`, Tarefa 5).

Os testes usam `#[tokio::test(start_paused = true)]` e um relógio de parede que acompanha o tempo do `tokio`: `base + (tokio::time::Instant::now() - inicio).as_secs()`, mais um `AtomicU64` de "salto" para simular a suspensão. Ajudante `env_in(dir)` com `home`, `xdg_config_home = dir/config` e `xdg_runtime_dir = dir/run`.

- [ ] **Passo 1: testes que falham** em `tests/local.rs`:

```rust
#[tokio::test(start_paused = true)]
async fn fires_at_the_time_and_holds_work_until_handled() {
    let t = tempfile::tempdir().unwrap();
    let (work, (tx, mut rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let (clock, base, _jump) = paused_clock();
    let s = LocalScheduler::new("routine", &env_in(t.path()), work.clone(), tx).unwrap().with_clock(clock);
    s.schedule(&TimerSpec { name: "lembrete".into(), at_unix: base + 5, persistent: false }).unwrap();
    assert_eq!(work.pending(), 1);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(rx.try_recv().is_err());
    tokio::time::sleep(Duration::from_secs(2)).await;
    let fired = rx.try_recv().unwrap();
    assert_eq!(fired.name, "lembrete");
    assert!(s.timers().is_empty());
    assert_eq!(work.pending(), 1, "o guarda vai com o disparo");
    drop(fired);
    assert_eq!(work.pending(), 0);
    assert!(s.cancel("lembrete").is_ok(), "cancelar depois do disparo não é erro");
}

#[tokio::test(start_paused = true)]
async fn each_timer_holds_work_and_rescheduling_replaces() {
    // "a" em +10 e "b" em +20 → pending 2; "a" de novo em +30 → pending 2, timers()["a"].at_unix == base+30;
    // cancel("b") → pending 1; avançar 15 s → nada disparou ("a" antigo não dispara); avançar até +31 → um disparo "a".
}

#[tokio::test(start_paused = true)]
async fn refuses_past_times_and_bad_names() {
    // at_unix base+1 → Err(InPast); base+2 → Ok; nome "Lembrete" e "a/b" → Err(InvalidName) em schedule e cancel;
    // nada marcado pelos que falharam (timers().len() == 1), nenhuma entrada .desktop.
}

#[tokio::test(start_paused = true)]
async fn autostart_only_while_a_persistent_timer_exists() {
    let t = tempfile::tempdir().unwrap();
    let desktop = t.path().join("config/autostart/lucerna-routine.desktop");
    // não persistente "x" → !desktop.exists()
    // persistente "p1" → read_to_string(desktop) == render_autostart("routine"),
    //   e render_autostart("routine") contém "Exec=lucerna-routine serve\n" e "NoDisplay=true\n"
    // persistente "p2"; cancel("p1") → existe; cancel("p2") → !exists (com "x" ainda marcado)
    // persistente "p3" disparando (avançar o tempo) → !exists depois do disparo
}

#[tokio::test(start_paused = true)]
async fn switching_persistence_updates_autostart() {
    // Review Focus 5: "p" persistente → existe; "p" de novo, não persistente → !exists;
    // "p" persistente de novo → existe; shutdown() → existe (login); timers vazios e pending 0 depois do shutdown.
    // Sem timer persistente, shutdown() apaga uma entrada que estiver lá (escreva uma à mão antes).
}

#[tokio::test(start_paused = true)]
async fn wall_clock_jump_fires_within_a_step() {
    // Review Focus 2: marcado para base+3600; jump.store(3600) (a suspensão: o relógio de parede anda,
    // o do tokio não); avançar o tokio WAKE_STEP + 1 s → disparou.
}

#[test]
fn choose_scheduler_falls_back_to_local() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let (loaded, log) = fake_systemctl(t.path(), "loaded", "echo loaded");
    assert_eq!(choose_scheduler("routine", &env, &loaded), SchedulerKind::Systemd);
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "--user show -p LoadState --value lucerna-routine.service\n");
    let (not_found, _) = fake_systemctl(t.path(), "nf", "echo not-found");
    assert_eq!(choose_scheduler("routine", &env, &not_found), SchedulerKind::Local);
    let (no_bus, _) = fake_systemctl(t.path(), "nobus", "echo 'Failed to connect to bus' >&2; exit 1");
    assert_eq!(choose_scheduler("routine", &env, &no_bus), SchedulerKind::Local);
    assert_eq!(choose_scheduler("routine", &env, &t.path().join("nao-existe")), SchedulerKind::Local);
}
```

(`fake_systemctl(dir, nome, corpo) -> (PathBuf, PathBuf)` é o ajudante de `tests/schedule.rs` com o corpo variável: grava `"$@"` no log e roda o corpo.)

- [ ] **Passo 2: rodar e ver falhar.** `cargo test -p lucerna-service --test local` → erro de compilação.
- [ ] **Passo 3: implementar** `local.rs` e `choose_scheduler`.
- [ ] **Passo 4: verificar.** `cargo test -p lucerna-service` → tudo passa (o `tests/schedule.rs` de hoje sem mudança; `real_systemd_round_trip` continua `ignored`). Os três comandos do CI passam.
- [ ] **Passo 5: commit.** `git commit -m "Serviço: o agendador local e a escolha entre ele e o systemd"`

---

### Tarefa 5: `lucerna-service` — o agendador no `ServiceBuilder`, `on_timer` e `on_start`

**Files:**
- Create: `crates/lucerna-service/src/timers.rs`
- Modify: `crates/lucerna-service/src/service.rs`, `crates/lucerna-service/src/lib.rs` (reexports e doc do crate), `crates/lucerna-service/src/activation.rs` (só doc), `apps/lucerna-hello/tests/process.rs` (só o `PATH` do ajudante, Passo 4)
- Test: `crates/lucerna-service/tests/service.rs`

**Interfaces:**
- Consumes: `LocalScheduler`, `Fired`, `SchedulerKind`, `choose_scheduler` (Tarefa 4); `SystemdScheduler`; `AppPaths::autostart_file`.
- Produces:
  - `pub struct Timers` (`Clone`; `impl Scheduler`) — a alça do agendador do serviço. Resolve uma vez (`OnceLock`), no primeiro `schedule`/`cancel` ou quando o `run()` pede: tipo forçado ou `choose_scheduler(app, env, Path::new("systemctl"))`; `Systemd` → `SystemdScheduler::new(app, env)` e apaga uma entrada `.desktop` velha do app (melhor esforço); `Local` → `LocalScheduler::new(app, env, work, fired_tx)`. Erro ao resolver volta do `schedule`/`cancel` e não fica guardado. `pub fn kind(&self) -> Option<SchedulerKind>` (`None` antes de resolver).
  - `ServiceBuilder` ganha:
    - `pub fn scheduler(&self) -> Timers`;
    - `pub fn scheduler_kind(&mut self, kind: SchedulerKind)` — força a escolha (testes; um app que quer sempre o local);
    - `pub fn env(&mut self, env: Env)` — o `Env` do serviço e do agendador (padrão: `Env::from_process()` na hora de usar). O `run()` passa a usar este `Env` para o socket também;
    - `pub fn on_timer<F, Fut>(&mut self, f: F) where F: Fn(String) -> Fut + Send + Sync + 'static, Fut: Future<Output = ()> + Send + 'static` — chamado a cada `Fired`, um de cada vez, segurando o guarda até o future terminar;
    - `pub fn on_start<F, Fut>(&mut self, f: F) where F: FnOnce() -> Fut + Send + 'static, Fut: Future<Output = ()> + Send + 'static` — esperado depois de o serviço ter o socket (no `run`, depois do `listener_from_env`; no `run_with`, logo no começo) e antes de atender.
  - `run()`: depois do `listener_from_env`, resolve o `Timers` (o spec: "escolhe ao subir"); um `serve` que recebe `AlreadyRunning` não chega aqui. Em `serve_until`: roda o `on_start`, sobe a tarefa que lê os `Fired` e chama o `on_timer` (sem `on_timer`, só larga), e ao sair (ociosidade, sinal) chama `shutdown()` do `LocalScheduler` se ele foi o escolhido.
  - Reexports em `lib.rs`: `pub use local::{Fired, LocalScheduler, WAKE_STEP}; pub use schedule::{SchedulerKind, choose_scheduler}; pub use timers::Timers; pub use lucerna_paths::{AppPaths, Env};` e `#[doc(hidden)] pub use local::render_autostart;`.
  - Docs: a do crate e a do `service` dizem que o serviço funciona com ou sem systemd (quem sobe: o systemd ou o cliente; quem acorda: o timer do systemd ou o agendador local; o app confere o que venceu no `on_start`); a do `activation` diz que o socket criado à mão é também o caminho do serviço subido pelo cliente, e que o lock resolve dois `serve` juntos.

- [ ] **Passo 1: testes que falham** em `tests/service.rs` (todos com `b.env(env_temp)` e `b.scheduler_kind(SchedulerKind::Local)`):

```rust
#[tokio::test]
async fn local_timer_keeps_the_service_and_calls_on_timer() {
    let t = tempfile::tempdir().unwrap();
    let mut b = builder(t.path()); // "demo", env temporário, Local, idle 100 ms
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    b.on_timer(move |name| { let tx = tx.clone(); async move { tx.send(name).unwrap(); } });
    let timers = b.scheduler();
    assert_eq!(timers.kind(), None);
    let h = tokio::spawn(b.run_with(listener(t.path()), None));
    timers.schedule(&TimerSpec { name: "x".into(), at_unix: now() + 2, persistent: false }).unwrap();
    assert_eq!(timers.kind(), Some(SchedulerKind::Local));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!h.is_finished(), "o timer segura a ociosidade");
    assert_eq!(tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap(), "x");
    tokio::time::timeout(Duration::from_secs(2), h).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn on_timer_holds_the_service_until_it_finishes() {
    // on_timer dorme 500 ms e depois marca um AtomicBool; idle 50 ms; timer em now()+2.
    // Quando o serviço termina, o AtomicBool já está marcado (o guarda segurou durante o handler).
}

#[tokio::test]
async fn on_start_runs_before_serving() {
    // on_start dorme 200 ms e publica no tópico "t" o valor 1 (tópico criado com 0).
    // Um Client que conecta logo após o spawn e assina "t" recebe o retrato com data == 1.
}

#[tokio::test]
async fn persistent_timer_autostart_follows_cancel() {
    // idle 100 ms; persistente "p" em now()+3600 → config/autostart/lucerna-demo.desktop existe
    // e o serviço segue de pé depois de 300 ms; cancel("p") → o arquivo some e o serviço sai em até 2 s.
}
```

- [ ] **Passo 2: rodar e ver falhar.** `cargo test -p lucerna-service --test service` → erro de compilação.
- [ ] **Passo 3: implementar** `timers.rs` e as mudanças no `ServiceBuilder`.
- [ ] **Passo 4: `PATH` dos testes de processo.** A partir desta tarefa o `run()` roda `systemctl` ao subir: em `apps/lucerna-hello/tests/process.rs`, o ajudante `hello(dir)` passa a pôr `PATH` = `dir/bin` (pasta criada, vazia) no lugar do `PATH` real, para nenhum `serve` de teste chamar o `systemctl` da máquina.
- [ ] **Passo 5: verificar.** `cargo test -p lucerna-service` passa; os dois testes de hoje (`run_with_serves_then_exits_when_idle`, `pending_work_keeps_run_with_alive`) continuam iguais e sem tocar o agendador (resolução preguiçosa). `cargo test -p lucerna-hello` passa. Os três comandos do CI passam.
- [ ] **Passo 6: commit.** `git commit -m "Serviço: o agendador do serviço, o aviso de disparo e o gancho de subida"`

---

### Tarefa 6: `lucerna-hello` — o lembrete

**Files:**
- Modify: `apps/lucerna-hello/src/lib.rs`, `apps/lucerna-hello/src/main.rs` (só a chamada `hello(&Env::from_process())`), `apps/lucerna-hello/tests/common/mod.rs`
- Create: `apps/lucerna-hello/tests/reminder.rs`

**Interfaces:**
- Consumes: `ServiceBuilder::{env, scheduler, scheduler_kind, on_timer, on_start}`, `Timers`, `SchedulerKind`, `Env`, `AppPaths`, `ScheduleError` (Tarefa 5, pelo `lucerna_service`).
- Produces:
  - `pub fn hello(env: &Env) -> Hello` — chama `service.env(env.clone())`; `Hello` ganha `pub reminder: Topic`.
  - Tópico `reminder`: `{"at": u64|null, "persistent": bool, "last_fired": u64|null}`, inicial `{"at": null, "persistent": false, "last_fired": null}`.
  - Arquivo `AppPaths::data_dir()/reminder.json` = `{"at": u64, "persistent": bool}`; ausente = sem lembrete. Timer `"lembrete"`.
  - Método `reminder.set {"in": u64, "persistent"?: bool}` → `at = agora + in`; agenda, grava o arquivo, publica; resultado `{"at": at}`. `ScheduleError::InPast` → `RpcError::app(2, "O lembrete precisa ser para daqui a 2 s ou mais.")`; outro `ScheduleError` → `RpcError::app(3, e.to_string())`; `in` ausente ou não inteiro → `-32602`. Chamar o agendador por `tokio::task::spawn_blocking` (o `SystemdScheduler` roda `systemctl`).
  - Método `reminder.cancel` → cancela, apaga o arquivo, publica `at: null`; resultado `{}`.
  - Disparo (`on_timer("lembrete")`) e subida (`on_start`: lê o arquivo; vencido (`at <= agora + 1`) → dispara já; senão agenda de novo): "disparar" = `cancel("lembrete")`, apaga o arquivo, publica `{"at": null, "persistent": false, "last_fired": at}`. Um app de verdade avisaria pelas notificações do sistema; o hello só publica.
  - Linha de comando: `remind <segundos> [--persistent]` (`reminder.set`; ajuda "Marca um lembrete para daqui a n segundos." e, no `--persistent`, "Sobrevive a reiniciar o computador."; saída `Lembrete às <at>.`) e `forget` (`reminder.cancel`; "Tira o lembrete."; saída vazia).
  - `tests/common/mod.rs`: `start` cria `Env` na pasta temporária (`home`, `xdg_runtime_dir`, `xdg_data_home`, `xdg_config_home`, `xdg_state_home`), chama `hello(&env)` e `scheduler_kind(SchedulerKind::Local)`; `Running` ganha `pub env: Env`. Ajudante novo `start_in(dir, shell)` para os testes que preparam arquivos antes de subir.

- [ ] **Passo 1: testes que falham** em `tests/reminder.rs`:

```rust
#[tokio::test]
async fn reminder_fires_and_frees_the_service() {
    // start(None); client.call("reminder.set", {"in": 2}) → {"at": at}; assinar "reminder":
    // retrato com at == at; em até 4 s chega um estado com last_fired == at e at == null;
    // reminder.json não existe; nenhuma entrada .desktop.
}

#[tokio::test]
async fn persistent_reminder_keeps_the_autostart_entry_until_it_fires() {
    // {"in": 2, "persistent": true} → config/autostart/lucerna-hello.desktop existe e reminder.json
    // == {"at": at, "persistent": true}; depois do disparo, os dois somem.
}

#[tokio::test]
async fn overdue_reminder_fires_on_start() {
    // antes de subir: data/lucerna/hello/reminder.json = {"at": now()-10, "persistent": true};
    // start_in(dir) → o primeiro retrato do "reminder" já tem last_fired == now()-10 e at == null; o arquivo sumiu.
}

#[tokio::test]
async fn pending_reminder_is_scheduled_again_on_start() {
    // reminder.json = {"at": now()+3600, "persistent": false} → retrato com at == now()+3600;
    // reminder.cancel → {} e o arquivo some.
}

#[tokio::test]
async fn bad_reminders() {
    // {"in": 1} → erro code 2, message "O lembrete precisa ser para daqui a 2 s ou mais."; {} → -32602.
}
```

- [ ] **Passo 2: rodar e ver falhar.** `cargo test -p lucerna-hello --test reminder` → erro de compilação (`hello` pede o `Env`).
- [ ] **Passo 3: implementar.** Os testes de hoje (`protocol.rs`, `shell.rs`, `files.rs`) só mudam pelo `common::start`.
- [ ] **Passo 4: verificar.** `cargo test -p lucerna-hello` passa (os de processo já rodam sem `systemctl` no `PATH` desde a Tarefa 5). Os três comandos do CI passam.
- [ ] **Passo 5: commit.** `git commit -m "Hello: um lembrete, o molde do trabalho agendado"`

---

### Tarefa 7: `lucerna-hello` — o binário e os testes de processo sem systemd

**Files:**
- Modify: `apps/lucerna-hello/src/main.rs`, `apps/lucerna-hello/tests/process.rs`

**Interfaces:**
- Consumes: tudo das tarefas anteriores, pelo binário.
- Produces:
  - `main`: `Invocation::Serve` com `Err(ServiceError::AlreadyRunning(_))` → `0` sem escrever nada; os outros erros como hoje.
  - `tests/process.rs`: o ajudante `hello(dir)` (que já tem `PATH` = `dir/bin` desde a Tarefa 5) passa a pôr `XDG_STATE_HOME` = `dir/state`, `XDG_CONFIG_HOME` = `dir/config` e `XDG_DATA_HOME` = `dir/data`; ajudantes novos `fake_systemctl_in(dir, corpo) -> PathBuf` (o script em `dir/bin/systemctl`, gravando `"$@"` em `dir/systemctl.log`), `serve_pid(sock) -> i32` (conecta com um runtime `current_thread` do `tokio` e lê `UnixStream::peer_cred().pid()`), `wait_gone(path, limite)`.

- [ ] **Passo 1: ajustar o arquivo** com os ajudantes acima. `cli_json_and_exit_codes` passa a começar com `hello(dir).env("LUCERNA_NO_SPAWN", "1").arg("status")` → `2` (o caminho sem subida, conferido em processo). `second_serve_is_refused` vira:

```rust
#[test]
fn second_serve_exits_quietly() {
    let t = tempfile::tempdir().unwrap();
    let mut srv = serve(t.path(), 60_000);
    let out = hello(t.path()).arg("serve").output().unwrap();
    assert_eq!((out.status.code(), out.stderr.as_slice()), (Some(0), &b""[..]));
    assert!(srv.child().try_wait().unwrap().is_none(), "o primeiro continua");
}
```

- [ ] **Passo 2: testes que falham** (todos sem `serve` prévio, com `LUCERNA_IDLE_TIMEOUT_MS` no ambiente da CLI, que o serviço herda; cada teste termina esperando o socket sumir, para nenhum serviço sobrar):

```rust
#[test]
fn cli_spawns_the_service_when_there_is_no_socket() {
    // add 3 com idle 500 ms → (0, "Contador: 3\n"); state/lucerna/hello/serve.log existe;
    // o socket some em até 3 s (o serviço sai sozinho).
}

#[test]
fn spawned_service_does_not_hold_the_pipes() {
    // Review Focus 1: idle 60_000; `.output()` de "add 1" volta em < 3 s (o serviço continua:
    // o socket existe); no fim, kill do serve_pid e wait_gone.
}

#[test]
fn two_clients_at_once_end_in_one_service() {
    // duas CLIs "add 1" lançadas juntas (spawn + wait) → as duas 0; "--json status" → counter.value == 2;
    // o serve.log não tem "erro" (o segundo serve saiu em silêncio).
}

#[test]
fn service_that_dies_exits_2_with_the_log() {
    // Review Focus 3: run/lucerna/hello.sock é um arquivo comum (connect → recusado; o serve não o apaga e sai com erro).
    // "status" → código 2 em < 4 s; stderr == "Sem conexão com o serviço hello: o serviço não subiu em 2 s; veja o log em <dir>/state/lucerna/hello/serve.log.\n";
    // o log contém "não é um socket".
}

#[test]
fn next_client_respawns_after_a_kill() {
    // Review Focus 4: "add 5" sobe o serviço; kill -KILL serve_pid (o socket velho fica);
    // "--json status" → 0 e counter.value == 0 (um serviço novo); kill/wait_gone no fim.
}

#[test]
fn local_reminder_fires_without_systemd() {
    // idle 300 ms; "remind 2 --persistent" → 0; config/autostart/lucerna-hello.desktop existe;
    // 1 s depois o serviço continua (o socket existe: o timer segura a ociosidade);
    // em até 5 s a entrada some e o socket some (disparou e saiu por ociosidade).
}

#[test]
fn overdue_reminder_fires_when_the_next_client_starts_the_service() {
    // "remind 2 --persistent"; SIGTERM no serve_pid antes do horário (`kill -TERM`): o serviço sai e a entrada
    // .desktop fica (o login precisa dela); dormir 3 s; "--json status" → 0, reminder.last_fired é um número e
    // reminder.at é null; a entrada some.
}

#[test]
fn stale_autostart_is_removed_on_exit() {
    // Review Focus 4: escrever config/autostart/lucerna-hello.desktop à mão; serve(dir, 300) e esperar sair;
    // a entrada sumiu.
}

#[test]
fn systemd_is_used_when_the_unit_loads() {
    // fake_systemctl_in(dir, "case \"$2\" in show) echo loaded;; esac"); "remind 60" → 0;
    // systemctl.log contém "--user show -p LoadState --value lucerna-hello.service\n" e
    // "--user restart lucerna-hello_lembrete.timer\n"; run/systemd/user/lucerna-hello_lembrete.timer existe;
    // nenhuma entrada .desktop. (O systemd real nunca é chamado: o PATH só tem o script.) Kill/wait_gone no fim.
}
```

- [ ] **Passo 3: rodar e ver falhar.** `cargo test -p lucerna-hello --test process` → `second_serve_exits_quietly` falha (código 1); os outros dependem das tarefas anteriores e passam ou falham conforme o que faltar.
- [ ] **Passo 4: implementar** o `main`.
- [ ] **Passo 5: verificar.** `cargo test -p lucerna-hello --test process` passa três vezes seguidas (flutuação de tempo); `ps -eo pid,args | grep 'lucerna-hello serve'` vazio depois. Os três comandos do CI passam.
- [ ] **Passo 6: commit.** `git commit -m "Hello: serve em silêncio quando outro já atende, e os testes sem systemd"`

---

### Tarefa 8: documentação, `ecossistema.md` e PR

**Files:**
- Modify: `README.md`, `apps/lucerna-hello/README.md`, `docs/superpowers/specs/2026-10-19-protocolo-design.md` (só a linha "Atualização"), `docs/ecossistema.md`

**Interfaces:**
- Consumes: os nomes públicos das tarefas anteriores.
- Produces: documentação; nenhum código.

- [ ] **Passo 1: `README.md`.** Na tabela, `lucerna-ipc` cita "a subida do serviço pelo cliente (`connect_or_spawn`)", `lucerna-service` cita "timers agendados (do systemd ou o agendador local)" e `lucerna-paths` cita "a pasta de estado". Seção nova "Sem systemd", curta: a CLI sobe `lucerna-<app> serve` quando ninguém atende (log em `$XDG_STATE_HOME/lucerna/<app>/serve.log`, código `2` só depois de 2 s), o próximo cliente o sobe de novo depois de uma queda, o trabalho agendado vai pelo agendador local, e a entrada `$XDG_CONFIG_HOME/autostart/lucerna-<app>.desktop` cobre o login no KDE, no GNOME e no XFCE; no Hyprland sem systemd, `exec-once = lucerna-<app> serve`. No exemplo mínimo, o braço do `serve` trata `ServiceError::AlreadyRunning` como `0`. Em "Desenvolvimento", uma linha: os testes não tocam o systemd real (o `PATH` dos testes de processo não tem `systemctl`), e `LUCERNA_NO_SPAWN=1` desliga a subida.
- [ ] **Passo 2: `apps/lucerna-hello/README.md`.** O lembrete (métodos, tópico, `remind`/`forget`), `tests/reminder.rs` e os testes de processo sem systemd, e "Sem instalar nada": `lucerna-hello add 3` já sobe o serviço; a linha `exec-once` para o Hyprland sem systemd.
- [ ] **Passo 3: spec.** A linha "Atualização (2026-10-27)" troca "O código ainda não faz isso" por "Implementado conforme `docs/superpowers/plans/2026-11-01-servico-sem-systemd.md`".
- [ ] **Passo 4: verificar e commit.** Os três comandos do CI passam (nada de código mudou). `git commit -m "Docs: o serviço sem systemd nos READMEs e no spec"`
- [ ] **Passo 5: `ecossistema.md`, num commit só dele** (o "commit pequeno" do PR #13):
  - em "Em aberto", tirar o item "Como o cliente sobe o serviço sem systemd (no `lucerna-ipc`) e como o trabalho agendado acontece sem os timers do systemd.";
  - em "Serviço sob demanda", a frase "Então o serviço registra um timer do systemd do usuário para o próximo horário (`lucerna-<app>_<nome>.timer`, persistente ou não). O timer sobe o serviço, que avisa pelas notificações do shell, cancela o timer e volta a sair." vira "Então o serviço marca o próximo horário pelo timer do systemd do usuário (`lucerna-<app>_<nome>.timer`, persistente ou não) ou, sem ele, pelo agendador local, que mantém o serviço de pé até o horário. No horário, o serviço avisa pelas notificações do sistema, cancela o timer e volta a sair."
  `git commit -m "Ecossistema: o serviço sem systemd saiu de Em aberto"`
- [ ] **Passo 6: push e PR.** `gh auth switch --user EduardoSA8006`; `git push -u origin feat/servico-sem-systemd`; `gh pr create --repo lucerna-desktop/lucerna-sdk --title "Serviço sem systemd" --body-file <(…o modelo .github/pull_request_template.md preenchido…)`, com "Trabalho a seguir" citando o item 7 do PR #13 (abaixo). `gh pr checks <n> --repo lucerna-desktop/lucerna-sdk --watch` → `fmt`, `clippy` e `test` em `pass`. O merge é do usuário (merge commit).

---

## Trabalho seguinte (fora deste plano)

- **Host de painéis do `lucerna-shell`** (item 7 do PR #13): com a conexão ao `<app>.sock` recusada, o host roda `<app.tui do manifesto> serve` pelo mesmo molde (grupo de processos novo, saída no `serve.log`, até 2 s com espera crescente) e reconecta. É do repositório `lucerna-shell`, junto com o cliente QML do protocolo.
- O `lucernad` adota o `connect_or_spawn`/agendador quando migrar para o SDK (spec próprio dele).
- A TUI, quando existir, usa `Client::connect_or_spawn` e mostra a causa do `NotStarted` em vez de sair.
- **Problema conhecido, a corrigir em outro plano:** com o systemd escolhido, um timer que dispara com o serviço já rodando não avisa o app (o `on_timer` só é chamado pelo agendador local, e o `on_start` só roda na subida), então o trabalho vencido espera até a próxima subida.

## Autorrevisão

- **Cobertura do spec ("Serviço sem systemd" e o que ele toca):** subida pelo cliente — socket ausente ou recusado, `serve` solto, grupo novo, entrada vazia, log em `$XDG_STATE_HOME`, até 2 s com espera crescente (T1, T2, T3, T7); dois clientes juntos → um serviço, o segundo `serve` sai (T7, `two_clients_at_once…`, `second_serve_exits_quietly`); 2 s sem conseguir → código `2` com o porquê e o log (T3, T7); socket criado à mão, ociosidade, socket apagado ao sair (já existia; T7 confere o socket sumindo); retomada depois de uma queda (T7, `next_client_respawns…`); painel — fora (Trabalho seguinte); escolha do agendador ao subir pelo `systemctl --user` e pela unidade (T4, T5, T7 `systemd_is_used…`); mesma interface `Scheduler` (T4, T5 `Timers`); aviso no horário e `cancel` (T4, T5 `on_timer`, T6); timer como trabalho pendente (T4, T5, T7); trabalho vencido conferido ao subir (T5 `on_start`, T6, T7); entrada `.desktop` só para persistentes, apagada sem nenhum (T4, T7); Hyprland sem systemd — documentado (T8); serviço subido no login sem nada sai por ociosidade (já é assim; T7 `stale_autostart…`). "Testes": os casos sem systemd estão em T7 e rodam no CI sem systemd de usuário. Tabelas dos crates e `ecossistema.md`: T8.
- **Roteiro do PR #13:** itens 1 a 6 em T1–T8; item 4 (`activation.rs` só doc) em T5; "espelho síncrono" — não há cliente síncrono, decisão registrada; item 7 em "Trabalho seguinte".
- **Tipos consistentes:** `ServeCommand`, `SPAWN_WAIT`, `LOG_MAX`, `ClientError::{NotStarted, Spawn}` (T2 → T3); `Spawn`, `CliApp::spawn`, `NO_SPAWN_ENV` (T3 → T7); `SchedulerKind`, `choose_scheduler`, `LocalScheduler::{new, with_clock, timers, shutdown}`, `Fired`, `WAKE_STEP`, `render_autostart` (T4 → T5); `Timers`, `ServiceBuilder::{scheduler, scheduler_kind, env, on_timer, on_start}` (T5 → T6); `hello(env)`, `Hello::reminder` (T6 → T7); `Env::{xdg_state_home, state_home, vars}`, `AppPaths::{state_dir, serve_log, autostart_file}` (T1 → T3, T4, T6).
- **Restrições de teste:** nenhum teste chama o `systemctl` real (scripts falsos e `PATH` temporário); nenhum escreve fora do `tempfile`; nenhum teste em processo da CLI sobe o binário de testes (`Spawn::Never`/`Program`); todo serviço subido pela CLI nos testes de processo sai sozinho (ociosidade curta) ou é morto pelo pid e esperado.
- **Review Focus:** os cinco itens têm teste na tarefa dona (T7 ×3, T4 ×2).
- **Proporção:** o plano tem cerca de três vezes o tamanho da seção do spec que implementa; o grosso são testes (pedidos como código ou como entrada e asserções em comentário) e assinaturas; os corpos ficam com o implementador.
