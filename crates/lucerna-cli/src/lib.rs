#![warn(missing_docs)]
//! Esqueleto da linha de comando dos apps do Lucerna.
//!
//! **Protocolo:** fala o protocolo 1.0 (pelo `lucerna-ipc`), com `client` =
//! `cli` no aperto de mão.
//!
//! O app declara os subcomandos ([`CliCommand`]) e cada um vira um método do
//! protocolo. O [`CliApp`] acrescenta os comuns (`status`, `watch <tópico>`), os
//! ocultos `serve` e `tui <args…>` e a opção global `--json`, com a ajuda em
//! português.
//!
//! Códigos de saída: [`EXIT_OK`], [`EXIT_APP`], [`EXIT_NO_SERVICE`] e
//! [`EXIT_USAGE`]; `--help`/`--version` saem com `0`.

pub mod render;

/// O `clap` desta versão, para o app declarar os subcomandos sem depender dele
/// direto (e sem descasar a versão).
pub use clap;
/// O ambiente (pastas XDG) que [`CliApp::run`] usa para achar o socket.
pub use lucerna_paths::Env;

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, Command};
use lucerna_ipc::{Client, ClientError};
use lucerna_paths::AppPaths;
use lucerna_protocol::{RpcError, State, codes};
use serde_json::{Map, Value};

use crate::render::render_default;

/// Sucesso.
pub const EXIT_OK: i32 = 0;
/// O serviço respondeu com erro (ou com uma resposta que não dá para ler).
pub const EXIT_APP: i32 = 1;
/// Não deu para falar com o serviço.
pub const EXIT_NO_SERVICE: i32 = 2;
/// Uso errado da linha de comando (`EX_USAGE`; o `2` do clap colidiria com
/// "sem serviço").
pub const EXIT_USAGE: i32 = 64;

/// Prazo para conectar e fazer o aperto de mão.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Prazo de cada chamada.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Nomes que o [`CliApp`] declara sozinho.
const RESERVED: [&str; 4] = ["status", "watch", "serve", "tui"];
/// Ids e opções longas de argumento que o [`CliApp`] declara sozinho.
const RESERVED_ARGS: [&str; 3] = ["help", "json", "version"];

const TEMPLATE: &str = "{about}\n\nUso: {usage}\n\n{all-args}";

/// O método a chamar e os parâmetros.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    /// Nome do método.
    pub method: String,
    /// Parâmetros (`None` manda o pedido sem `params`).
    pub params: Option<Value>,
}

/// Um subcomando do app.
///
/// Nomes reservados ao [`CliApp`]: o subcomando (nome e aliases) não pode ser
/// `status`, `watch`, `serve`, `tui` nem repetir outro subcomando do app; os
/// argumentos não podem usar os ids nem as opções longas `help`, `json` e
/// `version`, nem a opção curta `-h`. [`CliApp::command`] entra em pânico com
/// uma mensagem clara em qualquer desses casos.
///
/// O uso (`Uso:` da ajuda) é gerado em português pelo [`CliApp`]
/// (`{bin} {sub} [OPÇÕES] <arg>…`) e substitui um `override_usage` do app.
pub struct CliCommand {
    /// O subcomando do clap: nome, ajuda em português e argumentos.
    pub command: Command,
    /// Transforma os argumentos lidos na chamada ao serviço.
    ///
    /// Um `Err` é uso errado da linha de comando: [`CliApp::run`] escreve a
    /// mensagem (em português, pronta para o usuário) e sai com [`EXIT_USAGE`],
    /// sem conectar ao serviço.
    pub to_call: fn(&ArgMatches) -> Result<Call, String>,
    /// Texto legível do resultado; `None` usa [`render_default`].
    pub render: Option<fn(&Value) -> String>,
}

/// O que o binário deve fazer com os argumentos.
#[derive(Debug)]
pub enum Invocation {
    /// Abrir a TUI com estes argumentos: sem subcomando (lista vazia) ou com o
    /// subcomando oculto `tui <args…>`, que é como o shell abre a TUI
    /// (`tui.open`: `<app.tui> tui <args…>`). Depois do primeiro argumento,
    /// tudo vai para a lista, mesmo o que começa com `-`.
    Tui(Vec<OsString>),
    /// O subcomando oculto `serve`: rodar o serviço.
    Serve,
    /// Um subcomando da linha de comando: passe a [`CliApp::run`].
    Cli(CliRun),
}

/// Um subcomando da linha de comando já lido, pronto para [`CliApp::run`].
#[derive(Debug)]
pub struct CliRun {
    kind: RunKind,
    json: bool,
}

#[derive(Debug)]
enum RunKind {
    Call {
        call: Call,
        render: Option<fn(&Value) -> String>,
    },
    /// O `to_call` recusou os argumentos, com esta mensagem.
    Invalid(String),
    Status,
    Watch(String),
}

/// A linha de comando de um app.
pub struct CliApp {
    app: String,
    bin: String,
    version: String,
    about: String,
    commands: Vec<CliCommand>,
}

impl CliApp {
    /// Começa a linha de comando do app `app`, no binário `bin`. O app passa
    /// `env!("CARGO_PKG_VERSION")` em `version`; `about` abre a ajuda.
    pub fn new(app: &str, bin: &str, version: &str, about: &str) -> Self {
        CliApp {
            app: app.to_string(),
            bin: bin.to_string(),
            version: version.to_string(),
            about: about.to_string(),
            commands: Vec::new(),
        }
    }

    /// Acrescenta um subcomando do app.
    ///
    /// Entra em pânico com nome ou alias `status`, `watch`, `serve`, `tui` ou
    /// repetido, e com argumento `help`, `json`, `version` ou `-h` (veja
    /// [`CliCommand`]).
    pub fn command(mut self, c: CliCommand) -> Self {
        let name = c.command.get_name();
        let names = |c: &Command| -> Vec<String> {
            std::iter::once(c.get_name())
                .chain(c.get_all_aliases())
                .map(str::to_string)
                .collect()
        };
        for n in names(&c.command) {
            assert!(
                !RESERVED.contains(&n.as_str()),
                "o subcomando {name:?} usa o nome {n:?}, declarado pelo lucerna-cli"
            );
            assert!(
                self.commands
                    .iter()
                    .all(|o| !names(&o.command).contains(&n)),
                "subcomando repetido: {name:?} (nome ou alias {n:?})"
            );
        }
        for a in c.command.get_arguments() {
            let id = a.get_id().as_str();
            assert!(
                !RESERVED_ARGS.contains(&id)
                    && !a.get_long().is_some_and(|l| RESERVED_ARGS.contains(&l))
                    && a.get_short() != Some('h'),
                "o argumento {id:?} do subcomando {name:?} colide com help, json, version ou -h, \
                 declarados pelo lucerna-cli"
            );
        }
        self.commands.push(c);
        self
    }

    /// O `clap::Command` completo: os subcomandos do app, `status`, `watch`, os
    /// ocultos `serve` e `tui` e `--json`, com a ajuda em português.
    pub fn clap(&self) -> Command {
        let mut root = Command::new(self.bin.clone())
            .version(self.version.clone())
            .about(self.about.clone());
        for c in &self.commands {
            root = root.subcommand(c.command.clone());
        }
        let root = root
            .subcommand(Command::new("status").about("Mostra o estado de todos os tópicos."))
            .subcommand(
                Command::new("watch")
                    .about("Acompanha as mudanças de um tópico.")
                    .arg(
                        Arg::new("topic")
                            .value_name("tópico")
                            .required(true)
                            .help("O tópico a acompanhar."),
                    ),
            )
            .subcommand(Command::new("serve").about("Roda o serviço.").hide(true))
            .subcommand(
                Command::new("tui").about("Abre a TUI.").hide(true).arg(
                    Arg::new("args")
                        .value_name("args")
                        .num_args(0..)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true)
                        .value_parser(clap::value_parser!(OsString))
                        .help("Argumentos da TUI (a nota, a tarefa)."),
                ),
            );
        localize(root, &self.bin)
            .arg(
                Arg::new("version")
                    .short('V')
                    .long("version")
                    .action(ArgAction::Version)
                    .help("Mostra a versão."),
            )
            .arg(
                Arg::new("json")
                    .long("json")
                    .global(true)
                    .action(ArgAction::SetTrue)
                    .help_heading("Opções")
                    .help("Imprime o resultado em JSON, para scripts."),
            )
    }

    /// Lê os argumentos (o primeiro é o nome do binário).
    ///
    /// O erro inclui `--help` e `--version`: passe a [`usage_exit`].
    pub fn parse_from<I, T>(&self, args: I) -> Result<Invocation, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let m = self.clap().try_get_matches_from(args)?;
        let Some((name, sub)) = m.subcommand() else {
            return Ok(Invocation::Tui(Vec::new()));
        };
        let kind = match name {
            "serve" => return Ok(Invocation::Serve),
            "tui" => {
                let args = sub
                    .get_many::<OsString>("args")
                    .map(|v| v.cloned().collect())
                    .unwrap_or_default();
                return Ok(Invocation::Tui(args));
            }
            "status" => RunKind::Status,
            "watch" => RunKind::Watch(sub.get_one::<String>("topic").cloned().unwrap_or_default()),
            _ => {
                // O clap só aceita subcomandos declarados: este existe.
                let c = self
                    .commands
                    .iter()
                    .find(|c| c.command.get_name() == name)
                    .expect("subcomando declarado");
                match (c.to_call)(sub) {
                    Ok(call) => RunKind::Call {
                        call,
                        render: c.render,
                    },
                    Err(msg) => RunKind::Invalid(msg),
                }
            }
        };
        let json = m.get_flag("json") || sub.get_flag("json");
        Ok(Invocation::Cli(CliRun { kind, json }))
    }

    /// Conecta ao serviço (`client` = `cli`), roda o subcomando e devolve o
    /// código de saída. O resultado vai para `out`, as mensagens de erro para `err`.
    ///
    /// Com `--json`, todo erro sai em `err` como uma linha JSON e `out` fica vazio:
    /// erro do app (código `1`) como `{"error":{"code","message","data"?}}`; sem
    /// serviço (código `2`) como `{"error":{"message":"Sem conexão com o serviço …"}}`,
    /// sem `code` (não há erro JSON-RPC; o código de saída distingue); argumentos
    /// recusados pelo `to_call` (código `64`) também só com `message`.
    pub async fn run(
        &self,
        run: CliRun,
        env: &Env,
        out: &mut (dyn Write + Send),
        err: &mut (dyn Write + Send),
    ) -> i32 {
        if let RunKind::Invalid(msg) = &run.kind {
            return usage_error(err, msg, run.json);
        }
        let path = match AppPaths::new(&self.app, env).and_then(|p| p.socket()) {
            Ok(p) => p,
            Err(e) => return self.no_service(err, &e.to_string(), run.json),
        };
        let client = match Client::connect_timeout(&path, "cli", &self.app, CONNECT_TIMEOUT).await {
            Ok(c) => c,
            Err(e) => return self.fail(e, Stage::Connect(&path), run.json, err),
        };
        match run.kind {
            RunKind::Call { call, render } => {
                match client
                    .call_timeout(&call.method, call.params, CALL_TIMEOUT)
                    .await
                {
                    Ok(v) => {
                        let text = if run.json {
                            v.to_string()
                        } else {
                            render.unwrap_or(render_default)(&v)
                        };
                        if !text.is_empty() {
                            let _ = writeln!(out, "{text}");
                        }
                        let _ = out.flush();
                        EXIT_OK
                    }
                    Err(e) => self.fail(e, Stage::Call, run.json, err),
                }
            }
            RunKind::Invalid(_) => unreachable!("tratado antes de conectar"),
            RunKind::Status => self.status(&client, run.json, out, err).await,
            RunKind::Watch(topic) => self.watch(client, &topic, run.json, out, err).await,
        }
    }

    /// Como [`CliApp::run`], com o ambiente do processo ([`Env::from_process`]),
    /// a saída no stdout e os erros no stderr: o que o `main` de um app usa.
    pub async fn run_from_env(&self, run: CliRun) -> i32 {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        self.run(run, &Env::from_process(), &mut out, &mut err)
            .await
    }

    async fn status(
        &self,
        client: &Client,
        json: bool,
        out: &mut (dyn Write + Send),
        err: &mut (dyn Write + Send),
    ) -> i32 {
        let topics = client.hello().topics.clone();
        let states = if topics.is_empty() {
            Vec::new()
        } else {
            let refs: Vec<&str> = topics.iter().map(String::as_str).collect();
            match subscribe(client, &refs).await {
                Ok(s) => s,
                Err(e) => return self.fail(e, Stage::Call, json, err),
            }
        };
        if json {
            let map: Map<String, Value> = states.into_iter().map(|s| (s.topic, s.data)).collect();
            let _ = writeln!(out, "{}", Value::Object(map));
        } else {
            for s in &states {
                let _ = write!(out, "{}:\n{}", s.topic, indented(&s.data));
            }
        }
        let _ = out.flush();
        EXIT_OK
    }

    async fn watch(
        &self,
        client: Client,
        topic: &str,
        json: bool,
        out: &mut (dyn Write + Send),
        err: &mut (dyn Write + Send),
    ) -> i32 {
        let first = match subscribe(&client, &[topic]).await {
            Ok(s) => s,
            Err(e) => return self.fail(e, Stage::Call, json, err),
        };
        let print = |out: &mut (dyn Write + Send), s: &State| {
            if json {
                let line = serde_json::to_string(s).unwrap_or_default();
                writeln!(out, "{line}")?;
            } else {
                write!(out, "{} #{}\n{}", s.topic, s.seq, indented(&s.data))?;
            }
            out.flush()
        };
        for s in &first {
            if print(out, s).is_err() {
                // Quem lia a saída foi embora (ex.: `| head`): nada a fazer.
                return EXIT_OK;
            }
        }
        while let Some(s) = client.next_state().await {
            if print(out, &s).is_err() {
                return EXIT_OK;
            }
        }
        self.no_service(err, "o serviço fechou a conexão.", json)
    }

    /// Mensagem de "sem serviço" e o código `2`.
    fn no_service(&self, err: &mut (dyn Write + Send), detail: &str, json: bool) -> i32 {
        let message = format!("Sem conexão com o serviço {}: {detail}", self.app);
        if json {
            let v = serde_json::json!({ "error": { "message": message } });
            let _ = writeln!(err, "{v}");
        } else {
            let _ = writeln!(err, "{message}");
        }
        let _ = err.flush();
        EXIT_NO_SERVICE
    }

    /// Erro do app (`erro: …` ou `{"error":…}` com `--json`) e o código `1`.
    fn app_error(&self, err: &mut (dyn Write + Send), e: &RpcError, json: bool) -> i32 {
        if json {
            let v = serde_json::json!({ "error": e });
            let _ = writeln!(err, "{v}");
        } else {
            let _ = writeln!(err, "erro: {}", e.message);
        }
        let _ = err.flush();
        EXIT_APP
    }

    fn fail(
        &self,
        e: ClientError,
        stage: Stage<'_>,
        json: bool,
        err: &mut (dyn Write + Send),
    ) -> i32 {
        match e {
            ClientError::Connect(io) => {
                let detail = match (io.kind(), stage) {
                    (std::io::ErrorKind::NotFound, Stage::Connect(p)) => {
                        format!("o socket {} não existe.", p.display())
                    }
                    (std::io::ErrorKind::ConnectionRefused, Stage::Connect(p)) => {
                        format!("ninguém atende em {}.", p.display())
                    }
                    _ => format!("{io}."),
                };
                self.no_service(err, &detail, json)
            }
            ClientError::Io(io) => {
                self.no_service(err, &format!("erro de comunicação: {io}."), json)
            }
            ClientError::Closed => self.no_service(err, "o serviço fechou a conexão.", json),
            ClientError::Timeout => self.no_service(err, "tempo esgotado.", json),
            // Provisório: a Tarefa 3 dá a cada um a sua mensagem.
            ClientError::NotStarted { .. } | ClientError::Spawn { .. } => {
                self.no_service(err, &e.to_string(), json)
            }
            // No aperto de mão (versão incompatível, socket de outro app): não
            // dá para falar com este serviço.
            ClientError::Rpc(e) if matches!(stage, Stage::Connect(_)) => {
                self.no_service(err, &e.message, json)
            }
            ClientError::Rpc(e) => self.app_error(err, &e, json),
            // Um `hello` fora da forma do protocolo: o que atende o socket não é
            // um serviço do Lucerna com que dê para falar.
            ClientError::Decode(d) if matches!(stage, Stage::Connect(_)) => {
                self.no_service(err, &format!("resposta inválida do serviço: {d}."), json)
            }
            // Numa chamada, o serviço falou e respondeu fora da forma esperada:
            // erro do app, não "sem serviço".
            ClientError::Decode(d) => self.app_error(
                err,
                &RpcError {
                    code: codes::INTERNAL_ERROR,
                    message: format!("Resposta inválida do serviço: {d}."),
                    data: None,
                },
                json,
            ),
        }
    }
}

/// Argumentos recusados pelo `to_call`: a mensagem (`erro: …` e a dica do
/// `--help`, ou `{"error":{"message"}}` com `--json`) e o código [`EXIT_USAGE`].
fn usage_error(err: &mut (dyn Write + Send), msg: &str, json: bool) -> i32 {
    if json {
        let v = serde_json::json!({ "error": { "message": msg } });
        let _ = writeln!(err, "{v}");
    } else {
        let _ = write!(err, "erro: {msg}\n\nPara mais informações, use '--help'.\n");
    }
    let _ = err.flush();
    EXIT_USAGE
}

/// Em que ponto o erro aconteceu.
#[derive(Clone, Copy)]
enum Stage<'a> {
    /// Conectando ao socket em `path` ou no aperto de mão.
    Connect(&'a Path),
    /// Numa chamada, com a conexão aberta.
    Call,
}

async fn subscribe(client: &Client, topics: &[&str]) -> Result<Vec<State>, ClientError> {
    tokio::time::timeout(CALL_TIMEOUT, client.subscribe(topics))
        .await
        .map_err(|_| ClientError::Timeout)?
}

/// As linhas do [`render_default`] com dois espaços na frente, cada uma com `\n`.
fn indented(v: &Value) -> String {
    let text = render_default(v);
    text.lines().map(|l| format!("  {l}\n")).collect()
}

/// O uso em português de `c`, chamado como `path` (`{bin} {sub} …`).
fn usage(c: &Command, path: &str) -> String {
    let mut u = format!("{path} [OPÇÕES]");
    for a in c.get_positionals().filter(|a| !a.is_hide_set()) {
        let name = a
            .get_value_names()
            .and_then(|v| v.first())
            .map_or_else(|| a.get_id().to_string(), |n| n.to_string());
        let many = a.get_num_args().is_some_and(|r| r.max_values() > 1)
            || matches!(a.get_action(), ArgAction::Append);
        let dots = if many { "..." } else { "" };
        if a.is_required_set() {
            u.push_str(&format!(" <{name}>{dots}"));
        } else {
            u.push_str(&format!(" [{name}]{dots}"));
        }
    }
    if c.has_subcommands() {
        u.push_str(if c.is_subcommand_required_set() {
            " <COMANDO>"
        } else {
            " [COMANDO]"
        });
    }
    u
}

/// Ajuda em português num comando (chamado como `path`) e nos subcomandos dele.
fn localize(c: Command, path: &str) -> Command {
    let u = usage(&c, path);
    c.override_usage(u)
        .help_template(TEMPLATE)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        .allow_negative_numbers(true)
        .subcommand_help_heading("Comandos")
        .subcommand_value_name("COMANDO")
        .mut_args(|a| {
            if a.get_help_heading().is_some() {
                a
            } else if a.is_positional() {
                a.help_heading("Argumentos")
            } else {
                a.help_heading("Opções")
            }
        })
        .next_help_heading("Opções")
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help("Mostra esta ajuda."),
        )
        .mut_subcommands(|s| {
            let sub = format!("{path} {}", s.get_name());
            localize(s, &sub)
        })
}

/// Imprime o erro do clap (ajuda, versão ou uso errado) e devolve o código de
/// saída: `0` para `--help`/`--version`, [`EXIT_USAGE`] para o resto.
///
/// Os erros mais comuns (valor inválido, argumento faltando, subcomando ou
/// argumento desconhecido) saem em português; os outros ficam como o clap
/// os escreve.
pub fn usage_exit(e: &clap::Error) -> i32 {
    match e.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            let _ = e.print();
            EXIT_OK
        }
        _ => {
            match translate(e) {
                Some(text) => eprint!("{text}"),
                None => {
                    let _ = e.print();
                }
            }
            EXIT_USAGE
        }
    }
}

/// O erro do clap em português, com o uso e a dica do `--help`; `None` para os
/// tipos que não traduzimos.
fn translate(e: &clap::Error) -> Option<String> {
    use clap::error::ContextKind as K;
    let get = |k| e.get(k).map(|v| v.to_string()).unwrap_or_default();
    let msg = match e.kind() {
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            let mut m = format!(
                "valor inválido '{}' para '{}'",
                get(K::InvalidValue),
                get(K::InvalidArg)
            );
            // O motivo vem do `value_parser` (em inglês nos tipos padrão).
            if let Some(src) = std::error::Error::source(e) {
                m.push_str(&format!(": {src}"));
            }
            m
        }
        ErrorKind::MissingRequiredArgument => {
            format!("faltam argumentos obrigatórios: {}", get(K::InvalidArg))
        }
        ErrorKind::InvalidSubcommand => {
            format!("subcomando desconhecido '{}'", get(K::InvalidSubcommand))
        }
        ErrorKind::UnknownArgument => format!("argumento inesperado '{}'", get(K::InvalidArg)),
        _ => return None,
    };
    let mut text = format!("erro: {msg}\n\n");
    if let Some(u) = e.get(K::Usage) {
        text.push_str(&u.to_string().replacen("Usage:", "Uso:", 1));
        text.push_str("\n\n");
    }
    text.push_str("Para mais informações, use '--help'.\n");
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> CliApp {
        CliApp::new("demo", "demo", "0.1.0", "Demonstração.").command(CliCommand {
            command: Command::new("add").arg(
                Arg::new("n")
                    .required(true)
                    .value_parser(clap::value_parser!(i64)),
            ),
            to_call: |_| {
                Ok(Call {
                    method: "counter.add".into(),
                    params: None,
                })
            },
            render: None,
        })
    }

    fn err_text(args: &[&str]) -> String {
        translate(&app().parse_from(args).unwrap_err()).expect("erro traduzido")
    }

    #[test]
    fn common_clap_errors_are_portuguese() {
        let t = err_text(&["demo", "add", "três"]);
        assert!(
            t.starts_with("erro: valor inválido 'três' para '<n>'"),
            "{t}"
        );
        assert!(t.ends_with("Para mais informações, use '--help'.\n"), "{t}");
        let t = err_text(&["demo", "add"]);
        assert!(
            t.starts_with("erro: faltam argumentos obrigatórios: <n>"),
            "{t}"
        );
        // O clap só anexa o uso a alguns erros; quando anexa, sai em português.
        assert!(t.contains("Uso: demo add [OPÇÕES] <n>"), "{t}");
        let t = err_text(&["demo", "xyz"]);
        assert!(t.starts_with("erro: subcomando desconhecido 'xyz'"), "{t}");
        let t = err_text(&["demo", "add", "1", "--nada"]);
        assert!(t.starts_with("erro: argumento inesperado '--nada'"), "{t}");
        for t in [
            err_text(&["demo", "add"]),
            err_text(&["demo", "xyz"]),
            err_text(&["demo", "add", "1", "--nada"]),
        ] {
            assert!(!t.contains("Usage:") && !t.contains("For more"), "{t}");
        }
    }
}
