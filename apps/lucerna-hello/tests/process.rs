//! O binário `lucerna-hello` de verdade: `serve` e os subcomandos em processos.
//!
//! Cada teste usa a própria pasta temporária como `HOME`, `XDG_RUNTIME_DIR` e
//! as outras pastas XDG; nenhum processo sobra, nem quando um teste entra em
//! pânico (os `serve` subidos pela linha de comando não são filhos do teste:
//! o [`Reaper`] os acha pelo `/proc` e os mata ao cair).

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// O binário com um ambiente limpo: só `HOME`, as pastas XDG (`run`,
/// `state`, `config` e `data` dentro de `dir`) e `PATH`.
///
/// O `PATH` é a pasta `dir/bin`, vazia (ou só com o `systemctl` falso do
/// [`fake_systemctl_in`]): sem `systemctl`, o `serve` escolhe o agendador
/// local e nunca chama o systemd da máquina.
fn hello(dir: &Path) -> Command {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_lucerna-hello"));
    c.env_clear()
        .env("HOME", dir)
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("PATH", bin);
    c
}

/// Um `systemctl` falso em `dir/bin/systemctl`: grava os argumentos (`"$@"`)
/// numa linha de `dir/systemctl.log` e roda `body`.
fn fake_systemctl_in(dir: &Path, body: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("systemctl");
    let log = dir.join("systemctl.log");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n{body}\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// O pid de quem atende em `sock` (`SO_PEERCRED`).
fn serve_pid(sock: &Path) -> i32 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let s = tokio::net::UnixStream::connect(sock).await.unwrap();
        s.peer_cred().unwrap().pid().expect("sem pid do serviço")
    })
}

/// `kill -<sig> <pid>`.
fn kill(sig: &str, pid: i32) {
    let st = Command::new("kill")
        .args([sig, &pid.to_string()])
        .status()
        .unwrap();
    assert!(st.success(), "kill {sig} {pid}");
}

/// Espera `path` sumir, até `limite`; `false` se continua lá.
fn wait_gone(path: &Path, limite: Duration) -> bool {
    let start = Instant::now();
    while path.exists() {
        if start.elapsed() >= limite {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Os pids dos `lucerna-hello serve` vivos com o `XDG_RUNTIME_DIR` de `dir`.
fn serves_of(dir: &Path) -> Vec<u32> {
    let want = format!("XDG_RUNTIME_DIR={}", dir.join("run").display()).into_bytes();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    procs
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            let args: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
            let is_serve =
                args.len() >= 2 && args[0].ends_with(b"lucerna-hello") && args[1] == b"serve";
            // Um zumbi não tem `environ`: já não conta.
            is_serve
                && std::fs::read(format!("/proc/{pid}/environ"))
                    .is_ok_and(|env| env.split(|b| *b == 0).any(|v| v == want.as_slice()))
        })
        .collect()
}

/// Ao cair, mata (`SIGKILL`) todo `serve` desta pasta e espera sumirem: os
/// subidos pela linha de comando não são filhos do teste e não têm `Serve`.
struct Reaper(PathBuf);

impl Drop for Reaper {
    fn drop(&mut self) {
        let start = Instant::now();
        loop {
            let pids = serves_of(&self.0);
            if pids.is_empty() || start.elapsed() > Duration::from_secs(5) {
                break;
            }
            for pid in pids {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// O log do serviço subido pela linha de comando.
fn serve_log(dir: &Path) -> PathBuf {
    dir.join("state/lucerna/hello/serve.log")
}

/// O socket do serviço.
fn sock(dir: &Path) -> PathBuf {
    dir.join("run/lucerna/hello.sock")
}

/// A entrada de início automático do hello.
fn autostart(dir: &Path) -> PathBuf {
    dir.join("config/autostart/lucerna-hello.desktop")
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
        json!({
            "counter": {"value": 3},
            "reminder": {"at": null, "persistent": false, "last_fired": null},
        })
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
fn second_serve_exits_quietly() {
    let t = tempfile::tempdir().unwrap();
    let mut srv = serve(t.path(), 60_000);
    let out = hello(t.path()).arg("serve").output().unwrap();
    assert_eq!(
        (out.status.code(), out.stderr.as_slice()),
        (Some(0), &b""[..])
    );
    assert!(
        srv.child().try_wait().unwrap().is_none(),
        "o primeiro continua"
    );
}

#[test]
fn cli_spawns_the_service_when_there_is_no_socket() {
    let t = tempfile::tempdir().unwrap();
    let _reaper = Reaper(t.path().into());
    let out = hello(t.path())
        .env("LUCERNA_IDLE_TIMEOUT_MS", "500")
        .args(["add", "3"])
        .output()
        .unwrap();
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into()
        ),
        (Some(0), String::from("Contador: 3\n")),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(serve_log(t.path()).exists(), "o log do serviço");
    // O item `counter` na barra segura o serviço: o `reset` o tira.
    let out = hello(t.path()).arg("reset").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        wait_gone(&sock(t.path()), Duration::from_secs(3)),
        "o serviço sai sozinho"
    );
}

#[test]
fn spawned_service_does_not_hold_the_pipes() {
    let t = tempfile::tempdir().unwrap();
    let _reaper = Reaper(t.path().into());
    let start = Instant::now();
    let out = hello(t.path())
        .env("LUCERNA_IDLE_TIMEOUT_MS", "60000")
        .args(["add", "1"])
        .output()
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "a CLI esperou o pipe: {:?}",
        start.elapsed()
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(sock(t.path()).exists(), "o serviço continua");
    kill("-KILL", serve_pid(&sock(t.path())));
    drop(_reaper);
    assert!(serves_of(t.path()).is_empty());
}

#[test]
fn two_clients_at_once_end_in_one_service() {
    let t = tempfile::tempdir().unwrap();
    let _reaper = Reaper(t.path().into());
    let spawn = || {
        Serve(
            hello(t.path())
                .env("LUCERNA_IDLE_TIMEOUT_MS", "500")
                .args(["add", "1"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let (mut a, mut b) = (spawn(), spawn());
    assert_eq!(a.child().wait().unwrap().code(), Some(0));
    assert_eq!(b.child().wait().unwrap().code(), Some(0));
    let out = hello(t.path())
        .env("LUCERNA_IDLE_TIMEOUT_MS", "500")
        .args(["--json", "status"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counter"]["value"], 2);
    // O item `counter` na barra segura o serviço: o `reset` o tira.
    let out = hello(t.path()).arg("reset").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        wait_gone(&sock(t.path()), Duration::from_secs(3)),
        "o serviço sai sozinho"
    );
    let log = std::fs::read_to_string(serve_log(t.path())).unwrap();
    assert!(!log.contains("erro"), "log: {log}");
}

#[test]
fn service_that_dies_exits_2_with_the_log() {
    let t = tempfile::tempdir().unwrap();
    let _reaper = Reaper(t.path().into());
    let dir = t.path().join("run/lucerna");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(sock(t.path()), b"").unwrap();
    let start = Instant::now();
    let out = hello(t.path()).arg("status").output().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "Sem conexão com o serviço hello: o serviço não subiu em 2 s; veja o log em {}.\n",
            serve_log(t.path()).display()
        )
    );
    let log = std::fs::read_to_string(serve_log(t.path())).unwrap();
    assert!(log.contains("não é um socket"), "log: {log}");
    assert!(sock(t.path()).is_file(), "o arquivo comum fica");
}

#[test]
fn next_client_respawns_after_a_kill() {
    let t = tempfile::tempdir().unwrap();
    let _reaper = Reaper(t.path().into());
    let cli = || {
        let mut c = hello(t.path());
        c.env("LUCERNA_IDLE_TIMEOUT_MS", "60000");
        c
    };
    assert_eq!(
        cli().args(["add", "5"]).output().unwrap().status.code(),
        Some(0)
    );
    let pid = serve_pid(&sock(t.path()));
    kill("-KILL", pid);
    let start = Instant::now();
    while serves_of(t.path()).contains(&(pid as u32)) {
        assert!(start.elapsed() < Duration::from_secs(5), "o kill não matou");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sock(t.path()).exists(), "o socket velho fica");
    let out = cli().args(["--json", "status"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counter"]["value"], 0, "um serviço novo");
    let new = serve_pid(&sock(t.path()));
    assert_ne!(new, pid);
    kill("-KILL", new);
    drop(_reaper);
    assert!(serves_of(t.path()).is_empty());
}

#[test]
fn local_reminder_fires_without_systemd() {
    let t = tempfile::tempdir().unwrap();
    let _reaper = Reaper(t.path().into());
    let out = hello(t.path())
        .env("LUCERNA_IDLE_TIMEOUT_MS", "300")
        .args(["remind", "3", "--persistent"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(autostart(t.path()).exists(), "a entrada .desktop");
    std::thread::sleep(Duration::from_secs(1));
    assert!(sock(t.path()).exists(), "o timer segura a ociosidade");
    assert!(
        wait_gone(&autostart(t.path()), Duration::from_secs(6)),
        "a entrada some depois do disparo"
    );
    assert!(
        wait_gone(&sock(t.path()), Duration::from_secs(6)),
        "o serviço sai por ociosidade"
    );
}

#[test]
fn overdue_reminder_fires_when_the_next_client_starts_the_service() {
    let t = tempfile::tempdir().unwrap();
    let _reaper = Reaper(t.path().into());
    let cli = || {
        let mut c = hello(t.path());
        c.env("LUCERNA_IDLE_TIMEOUT_MS", "300");
        c
    };
    let out = cli()
        .args(["remind", "3", "--persistent"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    kill("-TERM", serve_pid(&sock(t.path())));
    assert!(
        wait_gone(&sock(t.path()), Duration::from_secs(2)),
        "o serviço sai com SIGTERM"
    );
    assert!(autostart(t.path()).exists(), "a entrada fica para o login");
    std::thread::sleep(Duration::from_secs(4));
    let out = cli().args(["--json", "status"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["reminder"]["last_fired"].is_u64(), "{v}");
    assert!(v["reminder"]["at"].is_null(), "{v}");
    assert!(
        wait_gone(&autostart(t.path()), Duration::from_secs(3)),
        "a entrada some"
    );
    assert!(
        wait_gone(&sock(t.path()), Duration::from_secs(3)),
        "o serviço sai por ociosidade"
    );
}

#[test]
fn stale_autostart_is_removed_on_exit() {
    let t = tempfile::tempdir().unwrap();
    let entry = autostart(t.path());
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::write(&entry, "[Desktop Entry]\nType=Application\n").unwrap();
    let mut srv = serve(t.path(), 300);
    assert!(
        wait_exit(srv.child(), Duration::from_secs(3))
            .expect("o serviço não saiu")
            .success()
    );
    assert!(!entry.exists(), "a entrada velha some");
}

#[test]
fn systemd_is_used_when_the_unit_loads() {
    let t = tempfile::tempdir().unwrap();
    let _reaper = Reaper(t.path().into());
    fake_systemctl_in(t.path(), "case \"$2\" in show) echo loaded;; esac");
    let out = hello(t.path())
        .env("LUCERNA_IDLE_TIMEOUT_MS", "60000")
        .args(["remind", "60"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(t.path().join("systemctl.log")).unwrap();
    assert!(
        log.contains("--user show -p LoadState --value lucerna-hello.service\n"),
        "{log}"
    );
    assert!(
        log.contains("--user restart lucerna-hello_reminder.timer\n"),
        "{log}"
    );
    assert!(
        t.path()
            .join("run/systemd/user/lucerna-hello_reminder.timer")
            .exists()
    );
    assert!(!autostart(t.path()).exists(), "nenhuma entrada .desktop");
    kill("-KILL", serve_pid(&sock(t.path())));
    drop(_reaper);
    assert!(serves_of(t.path()).is_empty());
}
