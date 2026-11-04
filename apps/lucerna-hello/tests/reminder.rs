//! O lembrete do `lucerna-hello`, em processo, com o agendador local e as
//! pastas numa pasta temporária.

mod common;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{Running, start, start_in};
use lucerna_cli::{Invocation, Spawn};
use lucerna_ipc::ClientError;
use lucerna_service::{AppPaths, Env};
use serde_json::{Value, json};

/// Agora, em segundos Unix.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `reminder.json` na pasta de dados do hello.
fn reminder_file(env: &Env) -> PathBuf {
    AppPaths::new("hello", env)
        .unwrap()
        .data_dir()
        .unwrap()
        .join("reminder.json")
}

/// A entrada de início automático do hello.
fn autostart_file(env: &Env) -> PathBuf {
    AppPaths::new("hello", env)
        .unwrap()
        .autostart_file()
        .unwrap()
}

/// Lê o `reminder.json`.
fn read_file(env: &Env) -> Value {
    serde_json::from_slice(&std::fs::read(reminder_file(env)).unwrap()).unwrap()
}

/// Espera, em até `secs` segundos, um estado do `reminder` que satisfaça `ok`.
async fn wait_state(c: &lucerna_ipc::Client, secs: u64, ok: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let s = c.next_state().await.expect("a conexão fechou");
            if s.topic == "reminder" && ok(&s.data) {
                return s.data;
            }
        }
    })
    .await
    .expect("o estado esperado não chegou a tempo")
}

/// Marca o lembrete e devolve o `at`.
async fn set(r: &Running, params: Value) -> u64 {
    let c = r.client().await;
    let res = c.call("reminder.set", Some(params)).await.unwrap();
    res["at"]
        .as_u64()
        .unwrap_or_else(|| panic!("sem at: {res}"))
}

#[tokio::test]
async fn reminder_fires_and_frees_the_service() {
    let r = start(None).await;
    let before = now();
    let at = set(&r, json!({"in": 3})).await;
    assert!(at >= before + 3 && at <= now() + 3, "at = {at}");
    let c = r.client().await;
    let states = c.subscribe(&["reminder"]).await.unwrap();
    assert_eq!(states[0].data["at"], at);
    assert_eq!(states[0].data["persistent"], false);
    let fired = wait_state(&c, 5, |d| d["last_fired"] == at).await;
    assert_eq!(
        fired,
        json!({"at": null, "persistent": false, "last_fired": at})
    );
    assert!(!reminder_file(&r.env).exists());
    assert!(!autostart_file(&r.env).exists());
    r.stop().await;
}

#[tokio::test]
async fn persistent_reminder_keeps_the_autostart_entry_until_it_fires() {
    let r = start(None).await;
    let c = r.client().await;
    c.subscribe(&["reminder"]).await.unwrap();
    let at = set(&r, json!({"in": 3, "persistent": true})).await;
    assert!(autostart_file(&r.env).exists());
    assert_eq!(read_file(&r.env), json!({"at": at, "persistent": true}));
    let fired = wait_state(&c, 5, |d| d["last_fired"] == at).await;
    assert_eq!(fired["at"], Value::Null);
    assert!(!reminder_file(&r.env).exists());
    assert!(!autostart_file(&r.env).exists());
    r.stop().await;
}

#[tokio::test]
async fn overdue_reminder_fires_on_start() {
    let dir = tempfile::tempdir().unwrap();
    let env = common::temp_env(dir.path());
    let at = now() - 10;
    let file = reminder_file(&env);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, json!({"at": at, "persistent": true}).to_string()).unwrap();
    let r = start_in(dir, None).await;
    let c = r.client().await;
    let states = c.subscribe(&["reminder"]).await.unwrap();
    assert_eq!(
        states[0].data,
        json!({"at": null, "persistent": false, "last_fired": at})
    );
    assert!(!reminder_file(&r.env).exists());
    r.stop().await;
}

#[tokio::test]
async fn pending_reminder_is_scheduled_again_on_start() {
    let dir = tempfile::tempdir().unwrap();
    let env = common::temp_env(dir.path());
    let at = now() + 3600;
    let file = reminder_file(&env);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, json!({"at": at, "persistent": false}).to_string()).unwrap();
    let r = start_in(dir, None).await;
    let c = r.client().await;
    let states = c.subscribe(&["reminder"]).await.unwrap();
    assert_eq!(
        states[0].data,
        json!({"at": at, "persistent": false, "last_fired": null})
    );
    let res = c.call("reminder.cancel", None).await.unwrap();
    assert_eq!(res, json!({}));
    assert!(!reminder_file(&r.env).exists());
    let s = wait_state(&c, 2, |d| d["at"].is_null()).await;
    assert_eq!(
        s,
        json!({"at": null, "persistent": false, "last_fired": null})
    );
    r.stop().await;
}

#[tokio::test]
async fn bad_reminders() {
    let r = start(None).await;
    let c = r.client().await;
    match c.call("reminder.set", Some(json!({"in": 1}))).await {
        Err(ClientError::Rpc(e)) => {
            assert_eq!(e.code, 2);
            assert_eq!(
                e.message,
                "O lembrete precisa ser para daqui a 2 s ou mais."
            );
        }
        other => panic!("esperava o erro 2: {other:?}"),
    }
    for params in [
        json!({}),
        json!({"in": "3"}),
        json!({"in": -3}),
        json!({"in": 1.5}),
    ] {
        match c.call("reminder.set", Some(params.clone())).await {
            Err(ClientError::Rpc(e)) => assert_eq!(e.code, -32602, "{params}"),
            other => panic!("esperava -32602 com {params}: {other:?}"),
        }
    }
    match c
        .call("reminder.set", Some(json!({"in": 3, "persistent": "sim"})))
        .await
    {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, -32602),
        other => panic!("esperava -32602: {other:?}"),
    }
    assert!(!reminder_file(&r.env).exists());
    r.stop().await;
}

/// Roda a linha de comando do hello em processo, sem subir serviço.
async fn cli(r: &Running, args: &[&str]) -> (i32, String, String) {
    let app = lucerna_hello::cli().spawn(Spawn::Never);
    let Ok(Invocation::Cli(run)) = app.parse_from(args) else {
        panic!("não é um subcomando da linha de comando: {args:?}");
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = app.run(run, &r.env, &mut out, &mut err).await;
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[tokio::test]
async fn remind_and_forget_from_the_command_line() {
    let r = start(None).await;
    let before = now();
    let (code, out, err) = cli(&r, &["lucerna-hello", "remind", "60", "--persistent"]).await;
    assert_eq!(code, 0, "stderr: {err}");
    let at: u64 = out
        .strip_prefix("Lembrete às ")
        .and_then(|s| s.strip_suffix(".\n"))
        .unwrap_or_else(|| panic!("saída: {out:?}"))
        .parse()
        .unwrap();
    assert!(at >= before + 60 && at <= now() + 60, "at = {at}");
    assert_eq!(read_file(&r.env), json!({"at": at, "persistent": true}));
    assert!(autostart_file(&r.env).exists());

    assert_eq!(
        cli(&r, &["lucerna-hello", "forget"]).await,
        (0, String::new(), String::new())
    );
    assert!(!reminder_file(&r.env).exists());
    assert!(!autostart_file(&r.env).exists());

    let (code, _, err) = cli(&r, &["lucerna-hello", "remind", "1"]).await;
    assert_eq!(
        (code, err.as_str()),
        (
            1,
            "erro: O lembrete precisa ser para daqui a 2 s ou mais.\n"
        )
    );
    r.stop().await;
}

#[test]
fn remind_help_is_in_portuguese() {
    let mut cmd = lucerna_hello::cli().clap();
    let remind = cmd.find_subcommand_mut("remind").unwrap();
    assert_eq!(
        remind.get_about().map(ToString::to_string).as_deref(),
        Some("Marca um lembrete para daqui a n segundos.")
    );
    let persistent = remind
        .get_arguments()
        .find(|a| a.get_id() == "persistent")
        .unwrap();
    assert_eq!(
        persistent.get_help().map(ToString::to_string).as_deref(),
        Some("Sobrevive a reiniciar o computador.")
    );
    let forget = cmd.find_subcommand_mut("forget").unwrap();
    assert_eq!(
        forget.get_about().map(ToString::to_string).as_deref(),
        Some("Tira o lembrete.")
    );
}
