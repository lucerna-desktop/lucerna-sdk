//! O binário `lucerna-hello` de verdade: `serve` e os subcomandos em processos.
//!
//! Cada teste usa a própria pasta temporária como `HOME` e `XDG_RUNTIME_DIR`;
//! nenhum processo sobra, nem quando um teste entra em pânico.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// O binário com um ambiente limpo: só `HOME`, `XDG_RUNTIME_DIR` e `PATH`.
fn hello(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_lucerna-hello"));
    c.env_clear()
        .env("HOME", dir)
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    c
}

/// Um processo filho que é morto e esperado ao cair.
struct Serve(Child);

impl Serve {
    fn child(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

/// Sobe `lucerna-hello serve` com a ociosidade `idle_ms` e espera o socket.
fn serve(dir: &Path, idle_ms: u64) -> Serve {
    std::fs::create_dir_all(dir.join("run")).unwrap();
    let mut srv = Serve(
        hello(dir)
            .arg("serve")
            .env("LUCERNA_IDLE_TIMEOUT_MS", idle_ms.to_string())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let sock = dir.join("run/lucerna/hello.sock");
    let start = Instant::now();
    while !sock.exists() {
        assert!(
            srv.child().try_wait().unwrap().is_none(),
            "o serve saiu antes de criar o socket"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "o socket não apareceu"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    srv
}

/// Espera o processo sair, até `limite`.
fn wait_exit(child: &mut Child, limite: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s);
        }
        if start.elapsed() >= limite {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cli_json_and_exit_codes() {
    let t = tempfile::tempdir().unwrap();
    let out = hello(t.path())
        .env("LUCERNA_NO_SPAWN", "1")
        .args(["status"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2)); // ainda sem serviço
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("Sem conexão com o serviço hello"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _srv = serve(t.path(), 60_000);
    let out = hello(t.path()).args(["add", "3"]).output().unwrap();
    assert_eq!(
        (out.status.code(), String::from_utf8(out.stdout).unwrap()),
        (Some(0), "Contador: 3\n".into())
    );
    let out = hello(t.path()).args(["--json", "status"]).output().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!({"counter":{"value":3}})
    );
    // Chega ao máximo (3 + MAX - 3) e então passa dele.
    let out = hello(t.path())
        .args(["add", &(i64::MAX - 3).to_string()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let out = hello(t.path()).args(["add", "1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "erro: O contador passou do limite.\n"
    );
    assert_eq!(
        hello(t.path())
            .args(["add", "três"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(64)
    );
    // Sem TUI: a ajuda, com 0, também para o `tui …` que o shell usaria.
    for args in [&[][..], &["tui"], &["tui", "t_42", "--linha", "3"]] {
        let out = hello(t.path()).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("add"),
            "{args:?}"
        );
    }
    let out = hello(t.path()).arg("--version").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")));
}

/// `--json watch counter` com as linhas lidas numa thread (o `recv_timeout`
/// impede o teste de travar), já com o retrato inicial lido: a conexão está
/// aceita e contada pelo serviço.
fn watch_json(dir: &Path) -> (Serve, mpsc::Receiver<String>) {
    let mut watch = Serve(
        hello(dir)
            .args(["--json", "watch", "counter"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = watch.child().stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    assert_eq!(
        line(&rx),
        json!({"topic":"counter","seq":0,"data":{"value":0}})
    );
    (watch, rx)
}

/// A próxima linha JSON do `watch`, em até 5 s.
fn line(rx: &mpsc::Receiver<String>) -> Value {
    serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap()
}

#[test]
fn watch_prints_json_lines() {
    let t = tempfile::tempdir().unwrap();
    let _srv = serve(t.path(), 60_000);
    let (_watch, rx) = watch_json(t.path());
    let out = hello(t.path()).args(["add", "2"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        line(&rx),
        json!({"topic":"counter","seq":1,"data":{"value":2}})
    );
}

#[test]
fn idle_exit() {
    let t = tempfile::tempdir().unwrap();
    let mut srv = serve(t.path(), 200);
    let (watch, _rx) = watch_json(t.path());
    std::thread::sleep(Duration::from_millis(500)); // > 2× a ociosidade
    assert!(
        srv.child().try_wait().unwrap().is_none(),
        "com cliente conectado não sai"
    );
    drop(watch); // kill + wait
    assert!(
        wait_exit(srv.child(), Duration::from_secs(2))
            .expect("o serviço não saiu")
            .success(),
        "sai sozinho e com 0"
    );
    assert!(
        !t.path().join("run/lucerna/hello.sock").exists(),
        "o socket manual é apagado"
    );
}

#[test]
fn bar_item_holds_the_service_even_without_shell() {
    let t = tempfile::tempdir().unwrap();
    let mut srv = serve(t.path(), 200);
    // O `watch` segura o serviço até o `add` pôr o item na barra.
    let (watch, _rx) = watch_json(t.path());
    let out = hello(t.path()).args(["add", "1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    drop(watch);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        srv.child().try_wait().unwrap().is_none(),
        "o item na barra segura o serviço"
    );
    let out = hello(t.path()).args(["reset"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        wait_exit(srv.child(), Duration::from_secs(2))
            .expect("o serviço não saiu depois do reset")
            .success()
    );
}

#[test]
fn dnd_holds_the_service_even_without_shell() {
    let t = tempfile::tempdir().unwrap();
    let mut srv = serve(t.path(), 200);
    // O `watch` segura o serviço até o `quiet on` pedir o não perturbe.
    let (watch, _rx) = watch_json(t.path());
    let out = hello(t.path()).args(["quiet", "on"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    drop(watch);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        srv.child().try_wait().unwrap().is_none(),
        "o não perturbe segura o serviço"
    );
    let out = hello(t.path()).args(["quiet", "off"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        wait_exit(srv.child(), Duration::from_secs(2))
            .expect("o serviço não saiu depois do quiet off")
            .success()
    );
}

#[test]
fn sigterm_exits_clean() {
    let t = tempfile::tempdir().unwrap();
    let mut srv = serve(t.path(), 60_000);
    let pid = srv.child().id().to_string();
    let st = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(st.success());
    assert!(
        wait_exit(srv.child(), Duration::from_secs(2))
            .expect("o serviço não saiu com SIGTERM")
            .success(),
        "sai com 0"
    );
    assert!(
        !t.path().join("run/lucerna/hello.sock").exists(),
        "o socket manual é apagado"
    );
}

#[test]
fn second_serve_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let mut srv = serve(t.path(), 60_000);
    let out = hello(t.path()).arg("serve").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("Já há um serviço atendendo em"),
        "stderr: {err}"
    );
    assert!(
        srv.child().try_wait().unwrap().is_none(),
        "o primeiro continua"
    );
}
