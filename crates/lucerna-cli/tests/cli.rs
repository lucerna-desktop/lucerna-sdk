use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Arg, Command, value_parser};
use lucerna_cli::render::render_default;
use lucerna_cli::{Call, CliApp, CliCommand, Invocation, Spawn, usage_exit};
use lucerna_ipc::{ServerBuilder, Topic};
use lucerna_paths::Env;
use lucerna_protocol::RpcError;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

/// `Env` com o runtime em `<dir>/run` e a pasta `<dir>/run/lucerna` criada.
fn env_in(dir: &Path) -> Env {
    std::fs::create_dir_all(dir.join("run/lucerna")).unwrap();
    Env {
        xdg_runtime_dir: Some(dir.join("run")),
        home: Some(dir.to_path_buf()),
        ..Env::default()
    }
}

fn socket(dir: &Path) -> std::path::PathBuf {
    dir.join("run/lucerna/demo.sock")
}

/// Serviço `demo` com o tópico `counter` e os métodos `counter.add` e `app.fail`.
async fn service(dir: &Path) -> (Env, Topic, JoinHandle<()>) {
    let env = env_in(dir);
    let mut b = ServerBuilder::new("demo", "0.1.0");
    let topic = b.topic("counter", json!({"value": 0}));
    let t = topic.clone();
    b.method("counter.add", move |p| {
        let t = t.clone();
        async move {
            let n = p
                .as_ref()
                .and_then(|p| p["n"].as_i64())
                .ok_or_else(|| RpcError::invalid_params("n"))?;
            let v = t.current().data["value"].as_i64().unwrap_or(0) + n;
            t.publish(json!({"value": v}));
            Ok(json!({"value": v}))
        }
    });
    b.method("app.fail", |_| async { Err(RpcError::app(1, "Falhou.")) });
    let server = b.build();
    let listener = UnixListener::bind(socket(dir)).unwrap();
    let handle = tokio::spawn(async move {
        let _ = server.serve(listener).await;
    });
    (env, topic, handle)
}

fn app() -> CliApp {
    CliApp::new("demo", "demo", "0.1.0", "Demonstração.")
        .spawn(Spawn::Never)
        .command(CliCommand {
            command: Command::new("add").about("Soma ao contador.").arg(
                Arg::new("n")
                    .required(true)
                    .value_parser(value_parser!(i64))
                    .help("Quanto somar."),
            ),
            to_call: |m| {
                Ok(Call {
                    method: "counter.add".into(),
                    params: Some(json!({"n": m.get_one::<i64>("n").copied().unwrap()})),
                })
            },
            render: Some(|v| format!("Contador: {}", v["value"])),
        })
        .command(CliCommand {
            command: Command::new("fail").about("Falha de propósito."),
            to_call: |_| {
                Ok(Call {
                    method: "app.fail".into(),
                    params: None,
                })
            },
            render: None,
        })
}

/// Saída compartilhada: o teste lê enquanto o `run` escreve.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    async fn wait_for(&self, pred: impl Fn(&str) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pred(&self.text()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("saída esperada não veio: {:?}", self.text()));
    }
}

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn run(args: &[&str], env: &Env) -> (i32, String, String) {
    run_with(&app(), args, env).await
}

async fn run_with(a: &CliApp, args: &[&str], env: &Env) -> (i32, String, String) {
    let Invocation::Cli(r) = a.parse_from(args).unwrap() else {
        panic!("não é um subcomando da linha de comando: {args:?}");
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = a.run(r, env, &mut out, &mut err).await;
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Roda numa tarefa, com a saída compartilhada.
fn spawn_run(args: &'static [&'static str], env: &Env) -> (JoinHandle<i32>, Shared, Shared) {
    let (out, err) = (Shared::default(), Shared::default());
    let env = env.clone();
    let (mut o, mut e) = (out.clone(), err.clone());
    let h = tokio::spawn(async move {
        let a = app();
        let Invocation::Cli(r) = a.parse_from(args).unwrap() else {
            panic!("não é um subcomando da linha de comando");
        };
        a.run(r, &env, &mut o, &mut e).await
    });
    (h, out, err)
}

#[test]
fn parse_dispatch() {
    let a = app();
    assert!(matches!(a.parse_from(["demo"]).unwrap(), Invocation::Tui(v) if v.is_empty()));
    assert!(matches!(
        a.parse_from(["demo", "serve"]).unwrap(),
        Invocation::Serve
    ));
    assert!(matches!(
        a.parse_from(["demo", "add", "3"]).unwrap(),
        Invocation::Cli(_)
    ));
    let e = a.parse_from(["demo", "add", "três"]).unwrap_err();
    assert_eq!(usage_exit(&e), 64);
    assert_eq!(
        usage_exit(&a.parse_from(["demo", "--help"]).unwrap_err()),
        0
    );
    assert_eq!(
        usage_exit(&a.parse_from(["demo", "--version"]).unwrap_err()),
        0
    );
    assert!(matches!(
        a.parse_from(["demo", "add", "-3"]).unwrap(),
        Invocation::Cli(_)
    ));
    let help = a.clap().render_help().to_string();
    assert!(!help.contains("serve"), "{help}");
    assert!(
        help.contains("Uso:") && help.contains("Comandos") && help.contains("Mostra esta ajuda."),
        "{help}"
    );
    assert!(
        !help.contains("Usage:") && !help.contains("Print help"),
        "{help}"
    );
}

#[test]
fn tui_subcommand_carries_the_arguments() {
    let a = app();
    let tui = |args: &[&str]| match a.parse_from(args).unwrap() {
        Invocation::Tui(v) => v,
        other => panic!("não é a TUI: {other:?}"),
    };
    assert_eq!(tui(&["demo", "tui", "t_42"]), vec![OsString::from("t_42")]);
    assert!(tui(&["demo", "tui"]).is_empty());
    // Depois do primeiro argumento, tudo é da TUI, mesmo o que parece opção.
    assert_eq!(
        tui(&["demo", "tui", "nota", "-x", "--linha", "3"]),
        ["nota", "-x", "--linha", "3"].map(OsString::from)
    );
    // Argumento que não é UTF-8 passa intacto.
    use std::os::unix::ffi::OsStringExt;
    let raw = OsString::from_vec(vec![b'n', 0xff]);
    let got = match a
        .parse_from([OsString::from("demo"), "tui".into(), raw.clone()])
        .unwrap()
    {
        Invocation::Tui(v) => v,
        other => panic!("não é a TUI: {other:?}"),
    };
    assert_eq!(got, vec![raw]);
    let help = a.clap().render_help().to_string();
    assert!(!help.contains("tui"), "o tui é oculto: {help}");
}

#[test]
#[should_panic(expected = "tui")]
fn tui_is_a_reserved_name() {
    let _ = app().command(CliCommand {
        command: Command::new("tui"),
        to_call: noop,
        render: None,
    });
}

#[tokio::test]
async fn to_call_error_exits_64_without_connecting() {
    let a = app().command(CliCommand {
        command: Command::new("par").arg(Arg::new("n").required(true)),
        to_call: |m| {
            let n = m.get_one::<String>("n").unwrap();
            if n.len() % 2 == 0 {
                Ok(Call {
                    method: "x".into(),
                    params: None,
                })
            } else {
                Err(format!("O valor {n} precisa ter um número par de letras."))
            }
        },
        render: None,
    });
    // Sem serviço: um 2 aqui diria que tentou conectar.
    let env = env_in(tempfile::tempdir().unwrap().path());
    for (json, want) in [
        (
            false,
            "erro: O valor abc precisa ter um número par de letras.\n\n\
             Para mais informações, use '--help'.\n"
                .to_string(),
        ),
        (
            true,
            json!({"error":{"message":"O valor abc precisa ter um número par de letras."}})
                .to_string()
                + "\n",
        ),
    ] {
        let args: &[&str] = if json {
            &["demo", "--json", "par", "abc"]
        } else {
            &["demo", "par", "abc"]
        };
        let Invocation::Cli(r) = a.parse_from(args).unwrap() else {
            panic!("não é um subcomando da linha de comando");
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(a.run(r, &env, &mut out, &mut err).await, 64);
        assert!(out.is_empty());
        assert_eq!(String::from_utf8(err).unwrap(), want);
    }
}

#[test]
fn subcommand_help_is_portuguese_too() {
    let mut c = app().clap();
    let help = c
        .find_subcommand_mut("add")
        .unwrap()
        .render_help()
        .to_string();
    assert!(
        help.contains("Uso:") && help.contains("Mostra esta ajuda."),
        "{help}"
    );
    assert!(
        !help.contains("Usage:") && !help.contains("Print help"),
        "{help}"
    );
    assert!(help.contains("Uso: demo add [OPÇÕES] <n>"), "{help}");
    assert!(!help.contains("[OPTIONS]"), "{help}");
    let e = app().parse_from(["demo", "add", "--help"]).unwrap_err();
    assert_eq!(usage_exit(&e), 0);
}

fn noop(_: &clap::ArgMatches) -> Result<Call, String> {
    Ok(Call {
        method: "x".into(),
        params: None,
    })
}

#[test]
#[should_panic(expected = "watch")]
fn reserved_alias_panics() {
    let _ = app().command(CliCommand {
        command: Command::new("seguir").alias("watch"),
        to_call: noop,
        render: None,
    });
}

#[test]
#[should_panic(expected = "repetido")]
fn alias_repeating_a_command_panics() {
    let _ = app().command(CliCommand {
        command: Command::new("somar").alias("add"),
        to_call: noop,
        render: None,
    });
}

#[test]
fn reserved_argument_ids_panic() {
    for arg in [
        Arg::new("json").long("saida"),
        Arg::new("saida").long("help"),
        Arg::new("version"),
        Arg::new("hora").short('h'),
    ] {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app().command(CliCommand {
                command: Command::new("outro").arg(arg),
                to_call: noop,
                render: None,
            })
        }));
        let msg = *r
            .err()
            .expect("devia entrar em pânico")
            .downcast::<String>()
            .unwrap();
        assert!(
            msg.contains("colide com help, json, version ou -h"),
            "{msg}"
        );
    }
}

#[test]
#[should_panic(expected = "status")]
fn reserved_command_names_panic() {
    let _ = app().command(CliCommand {
        command: Command::new("status"),
        to_call: |_| {
            Ok(Call {
                method: "x".into(),
                params: None,
            })
        },
        render: None,
    });
}

#[test]
#[should_panic(expected = "add")]
fn repeated_command_panics() {
    let _ = app().command(CliCommand {
        command: Command::new("add"),
        to_call: |_| {
            Ok(Call {
                method: "x".into(),
                params: None,
            })
        },
        render: None,
    });
}

#[tokio::test]
async fn call_readable_and_json() {
    let t = tempfile::tempdir().unwrap();
    let (env, _topic, _s) = service(t.path()).await;
    assert_eq!(
        run(&["demo", "add", "3"], &env).await,
        (0, "Contador: 3\n".into(), String::new())
    );
    let (code, out, _) = run(&["demo", "--json", "add", "2"], &env).await;
    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap(),
        json!({"value":5})
    );
    // `--json` depois do subcomando também vale (global); número negativo é valor.
    let (code, out, _) = run(&["demo", "add", "-1", "--json"], &env).await;
    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap(),
        json!({"value":4})
    );
}

#[tokio::test]
async fn app_error_exits_1() {
    let t = tempfile::tempdir().unwrap();
    let (env, _topic, _s) = service(t.path()).await;
    assert_eq!(
        run(&["demo", "fail"], &env).await,
        (1, String::new(), "erro: Falhou.\n".into())
    );
    let (code, out, err) = run(&["demo", "--json", "fail"], &env).await;
    assert_eq!(code, 1);
    assert_eq!(out, "");
    assert_eq!(
        serde_json::from_str::<Value>(&err).unwrap(),
        json!({"error":{"code":1,"message":"Falhou."}})
    );
}

#[tokio::test]
async fn no_service_exits_2() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let (code, out, err) = run(&["demo", "status"], &env).await;
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert!(err.starts_with("Sem conexão com o serviço demo"), "{err}");

    // Com `--json`, o "sem serviço" também sai em JSON, para scripts.
    let (code, out, err) = run(&["demo", "--json", "status"], &env).await;
    assert_eq!((code, out.as_str()), (2, ""));
    let v: Value = serde_json::from_str(&err).unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.starts_with("Sem conexão com o serviço demo"), "{msg}");
    assert!(v["error"].get("code").is_none(), "{v}");

    // Review Focus 4: sem XDG_RUNTIME_DIR, erro claro (nunca /tmp).
    let env = Env {
        home: Some(t.path().to_path_buf()),
        ..Env::default()
    };
    let (code, _, err) = run(&["demo", "add", "1"], &env).await;
    assert_eq!(code, 2);
    assert!(err.contains("XDG_RUNTIME_DIR não está definido."), "{err}");
}

#[tokio::test]
async fn status_prints_every_common_topic() {
    let t = tempfile::tempdir().unwrap();
    let (env, _topic, _s) = service(t.path()).await;
    assert_eq!(
        run(&["demo", "status"], &env).await,
        (0, "counter:\n  value: 0\n".into(), String::new())
    );
    let (code, out, _) = run(&["demo", "--json", "status"], &env).await;
    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap(),
        json!({"counter":{"value":0}})
    );
}

#[tokio::test]
async fn watch_follows_until_the_service_goes_away() {
    let t = tempfile::tempdir().unwrap();
    let (env, topic, s) = service(t.path()).await;

    let (code, _, err) = run(&["demo", "watch", "x"], &env).await;
    assert_eq!((code, err.as_str()), (1, "erro: Tópico desconhecido: x.\n"));

    let (json_h, json_out, _) = spawn_run(&["demo", "--json", "watch", "counter"], &env);
    let (text_h, text_out, text_err) = spawn_run(&["demo", "watch", "counter"], &env);
    json_out.wait_for(|o| o.lines().count() == 1).await;
    text_out.wait_for(|o| o == "counter #0\n  value: 0\n").await;

    topic.publish(json!({"value": 1}));
    json_out.wait_for(|o| o.lines().count() == 2).await;
    text_out
        .wait_for(|o| o == "counter #0\n  value: 0\ncounter #1\n  value: 1\n")
        .await;

    // O serviço some no meio do `watch`: código 2.
    s.abort();
    let wait = |h: JoinHandle<i32>| async {
        tokio::time::timeout(Duration::from_secs(10), h)
            .await
            .expect("o watch não terminou")
            .unwrap()
    };
    assert_eq!(wait(json_h).await, 2);
    assert_eq!(wait(text_h).await, 2);
    assert!(
        text_err
            .text()
            .starts_with("Sem conexão com o serviço demo"),
        "{}",
        text_err.text()
    );
    let lines: Vec<Value> = json_out
        .text()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines,
        vec![
            json!({"topic":"counter","seq":0,"data":{"value":0}}),
            json!({"topic":"counter","seq":1,"data":{"value":1}}),
        ]
    );
}

/// Serviço falso que aceita conexões e não responde nada.
async fn silent_service(dir: &Path) -> Env {
    let env = env_in(dir);
    let listener = UnixListener::bind(socket(dir)).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    env
}

/// Serviço falso que responde o `hello` com `hello_result` e ignora o resto.
async fn fake_hello_service(dir: &Path, hello_result: Value) -> Env {
    fake_service(dir, hello_result, None).await
}

/// Serviço falso: responde o `hello` com `hello_result` e os pedidos seguintes
/// com `later` (ou não responde, com `None`).
async fn fake_service(dir: &Path, hello_result: Value, later: Option<Value>) -> Env {
    let env = env_in(dir);
    let listener = UnixListener::bind(socket(dir)).unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            let result = hello_result.clone();
            let later = later.clone();
            tokio::spawn(async move {
                let (rd, mut wr) = s.into_split();
                let mut lines = BufReader::new(rd).lines();
                let mut first = true;
                while let Ok(Some(line)) = lines.next_line().await {
                    let answer = if first { Some(&result) } else { later.as_ref() };
                    first = false;
                    if let Some(answer) = answer {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let resp = json!({"jsonrpc":"2.0","id":req["id"],"result":answer});
                        let mut bytes = resp.to_string().into_bytes();
                        bytes.push(b'\n');
                        wr.write_all(&bytes).await.unwrap();
                    }
                }
            });
        }
    });
    env
}

fn good_hello() -> Value {
    json!({"protocol":"1.0","app":"demo","version":"0.1.0","topics":["counter"],"streams":[],"methods":["counter.add"]})
}

#[tokio::test(start_paused = true)]
async fn silent_service_times_out_with_2() {
    let t = tempfile::tempdir().unwrap();
    let env = silent_service(t.path()).await;
    let start = tokio::time::Instant::now();
    let (code, out, err) = run(&["demo", "add", "1"], &env).await;
    // Tempo virtual: o prazo do connect (5 s), não o da chamada (30 s).
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(30),
        "{elapsed:?}"
    );
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert_eq!(err, "Sem conexão com o serviço demo: tempo esgotado.\n");
}

#[tokio::test(start_paused = true)]
async fn unanswered_call_times_out_with_2() {
    let t = tempfile::tempdir().unwrap();
    let env = fake_hello_service(t.path(), good_hello()).await;
    for args in [&["demo", "add", "1"][..], &["demo", "status"][..]] {
        let start = tokio::time::Instant::now();
        let (code, _, err) = run(args, &env).await;
        // Tempo virtual: foi o prazo da chamada (30 s) que estourou.
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_secs(30), "{args:?}: {elapsed:?}");
        assert_eq!(code, 2, "{args:?}");
        assert_eq!(err, "Sem conexão com o serviço demo: tempo esgotado.\n");
    }
}

#[tokio::test]
async fn malformed_hello_is_no_service() {
    let t = tempfile::tempdir().unwrap();
    let env = fake_hello_service(t.path(), json!("oops")).await;
    let (code, out, err) = run(&["demo", "add", "1"], &env).await;
    assert_eq!(code, 2, "{err}");
    assert_eq!(out, "");
    assert!(
        err.starts_with("Sem conexão com o serviço demo: resposta inválida do serviço"),
        "{err}"
    );
}

#[tokio::test]
async fn malformed_reply_to_a_call_exits_1() {
    let t = tempfile::tempdir().unwrap();
    // O `hello` é bom; o `lucerna.subscribe` do `status` volta fora da forma.
    let env = fake_service(t.path(), good_hello(), Some(json!("oops"))).await;
    let (code, out, err) = run(&["demo", "status"], &env).await;
    assert_eq!(code, 1, "{err}");
    assert_eq!(out, "");
    assert!(
        err.starts_with("erro: Resposta inválida do serviço"),
        "{err}"
    );
    let (code, _, err) = run(&["demo", "--json", "status"], &env).await;
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(&err).unwrap();
    assert_eq!(v["error"]["code"], -32603);
}

#[test]
fn render_default_rules() {
    assert_eq!(render_default(&json!("oi")), "oi");
    assert_eq!(render_default(&json!(null)), "");
    assert_eq!(
        render_default(&json!({"b":1,"a":{"x":[1]}})),
        "a: {\"x\":[1]}\nb: 1"
    );
    assert_eq!(render_default(&json!(["um", "dois"])), "um\ndois");
    assert_eq!(render_default(&json!({"nome":"Ana"})), "nome: Ana");
    assert_eq!(render_default(&json!(3)), "3");
}

/// Escreve `body` num script `/bin/sh` em `dir/name`, com modo `0755`, e
/// espera ele poder rodar (veja [`wait_executable`]). Com o argumento
/// `__probe`, o script sai sem rodar `body`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\n[ \"$1\" = __probe ] && exit 0\n{body}"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    wait_executable(&path);
    path
}

/// Com os testes em paralelo, um `fork` de outra thread pode herdar por um
/// instante o fd de escrita do script (nem um `rename` resolve: o fd herdado
/// é do mesmo arquivo), e o `exec` dá `ETXTBSY`. Roda o script com
/// `__probe` (ele sai na hora) até o `exec` passar.
fn wait_executable(bin: &Path) {
    for _ in 0..200 {
        match std::process::Command::new(bin).arg("__probe").status() {
            Ok(_) => return,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => panic!("o script {} não roda: {e}", bin.display()),
        }
    }
    panic!("o script {} continua ocupado", bin.display());
}

#[tokio::test]
async fn spawns_the_program_and_reaches_the_service() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let marca = t.path().join("marca");
    let prog = script(
        t.path(),
        "demo",
        &format!("echo \"$@ $XDG_RUNTIME_DIR\" > '{}'\n", marca.display()),
    );
    // Ao ver a marca, o teste sobe o serviço de sempre no socket.
    let dir = t.path().to_path_buf();
    let m = marca.clone();
    let srv = tokio::spawn(async move {
        while std::fs::read_to_string(&m).map_or(true, |s| !s.ends_with('\n')) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let (_env, topic, handle) = service(&dir).await;
        (topic, handle)
    });
    let a = app().spawn(Spawn::Program(prog));
    let (code, out, err) = run_with(&a, &["demo", "add", "2"], &env).await;
    assert_eq!((code, out.as_str(), err.as_str()), (0, "Contador: 2\n", ""));
    assert_eq!(
        std::fs::read_to_string(&marca).unwrap(),
        format!("serve {}\n", t.path().join("run").display())
    );
    let (_topic, handle) = srv.await.unwrap();
    handle.abort();
}

#[tokio::test]
async fn spawned_service_sees_exactly_the_env() {
    // Sem `HOME` nem `XDG_CONFIG_HOME` no `Env`, o filho não os herda do
    // processo do teste (que tem `HOME`).
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("run/lucerna")).unwrap();
    let env = Env {
        xdg_runtime_dir: Some(t.path().join("run")),
        xdg_state_home: Some(t.path().join("state")),
        ..Env::default()
    };
    let marca = t.path().join("marca");
    let prog = script(
        t.path(),
        "demo",
        &format!(
            "echo \"${{HOME-sem}} ${{XDG_CONFIG_HOME-sem}} $XDG_STATE_HOME\" > '{}'\n",
            marca.display()
        ),
    );
    let dir = t.path().to_path_buf();
    let m = marca.clone();
    let srv = tokio::spawn(async move {
        while std::fs::read_to_string(&m).map_or(true, |s| !s.ends_with('\n')) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let (_env, topic, handle) = service(&dir).await;
        (topic, handle)
    });
    let a = app().spawn(Spawn::Program(prog));
    let (code, _, err) = run_with(&a, &["demo", "add", "1"], &env).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        std::fs::read_to_string(&marca).unwrap(),
        format!("sem sem {}\n", t.path().join("state").display())
    );
    let (_topic, handle) = srv.await.unwrap();
    handle.abort();
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
    assert_eq!(
        err,
        format!(
            "Sem conexão com o serviço demo: o serviço não subiu em 2 s; veja o log em {}.\n",
            log.display()
        )
    );
}

#[tokio::test]
async fn missing_program_exits_2_at_once() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let a = app().spawn(Spawn::Program(t.path().join("nao-existe")));
    let start = std::time::Instant::now();
    let (code, out, err) = run_with(&a, &["demo", "--json", "status"], &env).await;
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!((code, out.as_str()), (2, ""));
    let v: Value = serde_json::from_str(&err).unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.starts_with("Sem conexão com o serviço demo: não deu para rodar"),
        "{msg}"
    );
    assert!(v["error"].get("code").is_none(), "{v}");
}
