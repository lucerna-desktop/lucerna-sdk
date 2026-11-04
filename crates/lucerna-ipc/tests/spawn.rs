//! Subida do serviço pelo cliente: `ServeCommand` e `Client::connect_or_spawn`.
//!
//! O "app" é um script `/bin/sh` numa pasta temporária, que sai sozinho. Para
//! "o serviço sobe", o script cria um arquivo-marca e uma tarefa do teste, ao
//! ver a marca, faz `bind` no socket e atende como o app `demo`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::demo;
use lucerna_ipc::{Client, ClientError, LOG_MAX, SPAWN_WAIT, ServeCommand};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

/// Escreve `body` num script `/bin/sh` em `dir/name`, com modo `0755`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Tarefa que espera a marca (com conteúdo: o script terminou de escrevê-la),
/// cria a pasta do socket, faz `bind` e atende como o app `demo`.
fn serve_when_marked(marca: &Path, sock: &Path) -> JoinHandle<()> {
    let marca = marca.to_path_buf();
    let sock = sock.to_path_buf();
    tokio::spawn(async move {
        while std::fs::read_to_string(&marca).map_or(true, |s| !s.ends_with('\n')) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&sock).unwrap();
        demo().build().serve(listener).await.unwrap();
    })
}

/// Espera (até 2 s) o arquivo `path` ter exatamente `want`.
fn wait_content(path: &Path, want: &str) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if std::fs::read_to_string(path).is_ok_and(|s| s == want) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), want);
}

#[test]
fn constants_have_the_fixed_values() {
    assert_eq!(SPAWN_WAIT, Duration::from_secs(2));
    assert_eq!(LOG_MAX, 1_048_576);
}

#[tokio::test]
async fn running_service_is_not_spawned_again() {
    let t = tempfile::tempdir().unwrap();
    let sock = t.path().join("demo.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let srv = tokio::spawn(async move { demo().build().serve(listener).await.unwrap() });
    let marca = t.path().join("marca");
    let prog = script(t.path(), "demo", &format!("touch '{}'\n", marca.display()));
    let serve = ServeCommand::new(&prog, t.path().join("serve.log"));
    let c = Client::connect_or_spawn(&sock, "test", "demo", &serve)
        .await
        .unwrap();
    assert_eq!(c.hello().app, "demo");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!marca.exists());
    assert!(!t.path().join("serve.log").exists());
    srv.abort();
}

#[tokio::test]
async fn missing_socket_spawns_and_connects() {
    let t = tempfile::tempdir().unwrap();
    let sock = t.path().join("run/lucerna/demo.sock"); // a pasta ainda não existe
    let marca = t.path().join("marca");
    let prog = script(
        t.path(),
        "demo",
        &format!(
            "echo \"$@\" > '{}'\necho oi\n[ \"$LUCERNA_X\" = 1 ] || echo sem-env\n",
            marca.display()
        ),
    );
    let log = t.path().join("state/lucerna/demo/serve.log");
    let serve = ServeCommand::new(&prog, &log).env("LUCERNA_X", "1");
    let srv = serve_when_marked(&marca, &sock);
    let c = Client::connect_or_spawn(&sock, "test", "demo", &serve)
        .await
        .unwrap();
    assert_eq!(c.hello().app, "demo");
    assert_eq!(std::fs::read_to_string(&marca).unwrap(), "serve\n");
    // A saída do programa vai para o log (e o `env` chegou ao filho).
    let log2 = log.clone();
    tokio::task::spawn_blocking(move || wait_content(&log2, "oi\n"))
        .await
        .unwrap();
    srv.abort();
}

#[tokio::test]
async fn stale_socket_spawns_too() {
    let t = tempfile::tempdir().unwrap();
    let sock = t.path().join("demo.sock");
    // Um socket velho: o arquivo fica, ninguém atende (`ConnectionRefused`).
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&sock)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::ConnectionRefused
    );
    let marca = t.path().join("marca");
    let prog = script(
        t.path(),
        "demo",
        &format!(
            "rm -f '{}'\necho ok > '{}'\n",
            sock.display(),
            marca.display()
        ),
    );
    let serve = ServeCommand::new(&prog, t.path().join("serve.log"));
    let srv = serve_when_marked(&marca, &sock);
    let c = Client::connect_or_spawn(&sock, "test", "demo", &serve)
        .await
        .unwrap();
    assert_eq!(c.hello().app, "demo");
    srv.abort();
}

#[tokio::test]
async fn service_that_never_comes_gives_not_started_with_the_log() {
    let t = tempfile::tempdir().unwrap();
    let sock = t.path().join("demo.sock");
    let prog = script(t.path(), "demo", "exit 0\n");
    let log = t.path().join("serve.log");
    let serve = ServeCommand::new(&prog, &log);
    let start = Instant::now();
    let e = Client::connect_or_spawn(&sock, "test", "demo", &serve)
        .await
        .err()
        .expect("não devia conectar");
    let elapsed = start.elapsed();
    let ClientError::NotStarted { log: got } = &e else {
        panic!("esperava NotStarted, veio {e:?}");
    };
    assert_eq!(got, &log);
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert_eq!(
        e.to_string(),
        format!(
            "O serviço não subiu em 2 s; veja o log em {}.",
            log.display()
        )
    );
}

#[tokio::test]
async fn missing_program_is_a_spawn_error() {
    let t = tempfile::tempdir().unwrap();
    let sock = t.path().join("demo.sock");
    let prog = t.path().join("nao-existe");
    let serve = ServeCommand::new(&prog, t.path().join("serve.log"));
    let start = Instant::now();
    let e = Client::connect_or_spawn(&sock, "test", "demo", &serve)
        .await
        .err()
        .expect("não devia conectar");
    assert!(start.elapsed() < Duration::from_millis(100));
    let ClientError::Spawn { program, source } = &e else {
        panic!("esperava Spawn, veio {e:?}");
    };
    assert_eq!(program, &prog);
    assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(
        e.to_string(),
        format!("Não foi possível rodar {} serve: {source}.", prog.display())
    );
}

#[tokio::test]
async fn other_connect_errors_do_not_spawn() {
    // Pasta sem permissão: o `connect` dá `PermissionDenied`, que não é "ninguém ouvindo".
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("fechada");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let sock = dir.join("demo.sock");
    let marca = t.path().join("marca");
    let prog = script(t.path(), "demo", &format!("touch '{}'\n", marca.display()));
    let serve = ServeCommand::new(&prog, t.path().join("serve.log"));
    let r = Client::connect_or_spawn(&sock, "test", "demo", &serve).await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    match r {
        Err(ClientError::Connect(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied)
        }
        Err(e) => panic!("esperava Connect, veio {e:?}"),
        Ok(_) => panic!("não devia conectar"),
    }
    assert!(!marca.exists());
}

#[test]
fn log_restarts_when_too_big() {
    let t = tempfile::tempdir().unwrap();
    let prog = script(t.path(), "demo", "echo novo\n");
    let log = t.path().join("serve.log");

    std::fs::write(&log, vec![b'x'; LOG_MAX as usize + 1]).unwrap();
    ServeCommand::new(&prog, &log).spawn().unwrap();
    wait_content(&log, "novo\n");

    std::fs::write(&log, "velho\n").unwrap();
    ServeCommand::new(&prog, &log).spawn().unwrap();
    wait_content(&log, "velho\nnovo\n");
}

#[test]
fn log_exactly_at_the_limit_is_kept() {
    let t = tempfile::tempdir().unwrap();
    let prog = script(t.path(), "demo", "echo novo\n");
    let log = t.path().join("serve.log");
    let mut want = vec![b'x'; LOG_MAX as usize];
    std::fs::write(&log, &want).unwrap();
    ServeCommand::new(&prog, &log).spawn().unwrap();
    want.extend_from_slice(b"novo\n");
    let start = Instant::now();
    while std::fs::read(&log).unwrap() != want && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(std::fs::read(&log).unwrap() == want);
}

#[test]
fn stdin_is_dev_null() {
    let t = tempfile::tempdir().unwrap();
    // `cat` com o stdin em `/dev/null` sai na hora, sem nada.
    let prog = script(t.path(), "demo", "cat\necho fim\n");
    let log = t.path().join("serve.log");
    ServeCommand::new(&prog, &log).spawn().unwrap();
    wait_content(&log, "fim\n");
}

#[test]
fn service_runs_in_the_root_folder() {
    // A pasta de trabalho do serviço é `/`, não a de quem o subiu.
    let t = tempfile::tempdir().unwrap();
    let prog = script(t.path(), "demo", "pwd\n");
    let log = t.path().join("serve.log");
    ServeCommand::new(&prog, &log).spawn().unwrap();
    wait_content(&log, "/\n");
}
