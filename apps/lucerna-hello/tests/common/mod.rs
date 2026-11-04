//! O serviço do `lucerna-hello` em processo, num socket temporário, com as
//! pastas (`HOME`, `XDG_*`) na mesma pasta temporária e o agendador local.

// Cada arquivo de teste usa uma parte das ferramentas.
#![allow(dead_code)]

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lucerna_hello::{APP, Producer, hello};
use lucerna_ipc::Client;
use lucerna_ipc::raw::RawConn;
use lucerna_service::{AppPaths, Env, SchedulerKind, Topic};
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

/// O serviço rodando numa tarefa.
pub struct Running {
    _dir: TempDir,
    pub path: PathBuf,
    /// O ambiente do serviço, todo dentro da pasta temporária.
    pub env: Env,
    ticks: Topic,
    _producer: Producer,
    task: JoinHandle<()>,
}

/// O ambiente de teste dentro de `dir`: `home`, `run`, `data`, `config` e `state`.
pub fn temp_env(dir: &Path) -> Env {
    Env {
        home: Some(dir.join("home")),
        xdg_runtime_dir: Some(dir.join("run")),
        xdg_data_home: Some(dir.join("data")),
        xdg_config_home: Some(dir.join("config")),
        xdg_state_home: Some(dir.join("state")),
        ..Env::default()
    }
}

/// Sobe o serviço num socket temporário; com `shell`, conecta ao `shell.sock` ali.
pub async fn start(shell: Option<PathBuf>) -> Running {
    start_in(tempfile::tempdir().unwrap(), shell).await
}

/// Como [`start`], mas em `dir`, que o teste já preparou (ex.: o
/// `reminder.json` de antes de subir; ver [`temp_env`]).
pub async fn start_in(dir: TempDir, shell: Option<PathBuf>) -> Running {
    let env = temp_env(dir.path());
    // No lugar onde a linha de comando procura: `<run>/lucerna/hello.sock`.
    let path = AppPaths::new(APP, &env).unwrap().socket().unwrap();
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path.parent().unwrap())
        .unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    let mut h = hello(&env);
    h.service.idle_timeout(Duration::from_secs(60));
    h.service.scheduler_kind(SchedulerKind::Local);
    let task = tokio::spawn(async move {
        h.service.run_with(listener, shell).await.unwrap();
    });
    Running {
        _dir: dir,
        path,
        env,
        ticks: h.ticks,
        _producer: h.producer,
        task,
    }
}

impl Running {
    /// Conexão crua, já com o aperto de mão feito.
    pub async fn raw(&self) -> RawConn {
        let mut c = RawConn::connect(&self.path).await.unwrap();
        let r = c.hello("hello").await.unwrap();
        assert!(r.get("result").is_some(), "aperto de mão falhou: {r}");
        c
    }

    /// Cliente com o aperto de mão feito.
    pub async fn client(&self) -> Client {
        Client::connect(&self.path, "test", "hello").await.unwrap()
    }

    /// O tópico contínuo `ticks`.
    pub fn ticks(&self) -> &Topic {
        &self.ticks
    }

    /// Para o serviço e espera a tarefa terminar.
    pub async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}
