use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use lucerna_ipc::raw::RawConn;
use lucerna_service::{Env, Scheduler, SchedulerKind, ServiceBuilder, TimerSpec};
use serde_json::json;

#[tokio::test]
async fn run_with_serves_then_exits_when_idle() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("s.sock");
    let mut b = ServiceBuilder::new("demo", "0.1.0");
    b.idle_timeout(Duration::from_millis(200));
    b.server().method("ping", |_| async { Ok(json!("pong")) });
    let l = tokio::net::UnixListener::bind(&p).unwrap();
    let h = tokio::spawn(b.run_with(l, None));
    let c = lucerna_ipc::Client::connect(&p, "test", "demo")
        .await
        .unwrap();
    assert_eq!(c.call("ping", None).await.unwrap(), json!("pong"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!h.is_finished(), "não sai com cliente conectado");
    drop(c);
    tokio::time::timeout(Duration::from_secs(2), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn pending_work_keeps_run_with_alive() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("s.sock");
    let mut b = ServiceBuilder::new("demo", "0.1.0");
    b.idle_timeout(Duration::from_millis(100));
    let work = b.work();
    let guard = work.hold();
    let l = tokio::net::UnixListener::bind(&p).unwrap();
    let h = tokio::spawn(b.run_with(l, None));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!h.is_finished(), "não sai com trabalho pendente");
    drop(guard);
    tokio::time::timeout(Duration::from_secs(2), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_client_accepted_at_the_deadline_is_served() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("s.sock");
    let mut b = ServiceBuilder::new("demo", "0.1.0");
    b.idle_timeout(Duration::from_secs(60));
    b.server().method("ping", |_| async { Ok(json!("pong")) });
    let l = tokio::net::UnixListener::bind(&p).unwrap();
    let mut run = Box::pin(b.run_with(l, None));

    // Um poll: o prazo da ociosidade começa a contar agora.
    let first = std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx).is_ready())).await;
    assert!(!first);
    // O prazo vence e, no mesmo instante, um cliente entra na fila do accept.
    tokio::time::advance(Duration::from_secs(60)).await;
    let std_c = std::os::unix::net::UnixStream::connect(&p).unwrap();
    // Deixa o driver de I/O ver o listener pronto antes de o serviço rodar.
    tokio::task::yield_now().await;

    let h = tokio::spawn(run);
    std_c.set_nonblocking(true).unwrap();
    let mut raw = RawConn::from_stream(tokio::net::UnixStream::from_std(std_c).unwrap());
    raw.hello("demo").await.unwrap();
    raw.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .await
        .unwrap();
    let resp = raw.recv().await.unwrap().unwrap();
    assert_eq!(resp["result"], json!("pong"));
    assert!(!h.is_finished());
    drop(raw);
    h.await.unwrap().unwrap();
}

// --- O agendador do serviço, o aviso de disparo e o gancho de subida ---

/// O relógio de parede, em segundos Unix.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Um `Env` só com pastas dentro de `dir`.
fn env_temp(dir: &Path) -> Env {
    Env {
        home: Some(dir.to_path_buf()),
        xdg_config_home: Some(dir.join("config")),
        xdg_data_home: Some(dir.join("data")),
        xdg_cache_home: Some(dir.join("cache")),
        xdg_state_home: Some(dir.join("state")),
        xdg_runtime_dir: Some(dir.join("run")),
    }
}

/// O serviço "demo" com o `Env` temporário, o agendador local forçado e
/// ociosidade de 100 ms.
fn builder(dir: &Path) -> ServiceBuilder {
    let mut b = ServiceBuilder::new("demo", "0.1.0");
    b.env(env_temp(dir));
    b.scheduler_kind(SchedulerKind::Local);
    b.idle_timeout(Duration::from_millis(100));
    b
}

fn listener(dir: &Path) -> tokio::net::UnixListener {
    tokio::net::UnixListener::bind(dir.join("s.sock")).unwrap()
}

#[tokio::test]
async fn local_timer_keeps_the_service_and_calls_on_timer() {
    let t = tempfile::tempdir().unwrap();
    let mut b = builder(t.path());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    b.on_timer(move |name| {
        let tx = tx.clone();
        async move {
            tx.send(name).unwrap();
        }
    });
    let timers = b.scheduler();
    assert_eq!(timers.kind(), None);
    let h = tokio::spawn(b.run_with(listener(t.path()), None));
    timers
        .schedule(&TimerSpec {
            name: "x".into(),
            at_unix: now() + 3,
            persistent: false,
        })
        .unwrap();
    assert_eq!(timers.kind(), Some(SchedulerKind::Local));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!h.is_finished(), "o timer segura a ociosidade");
    let name = tokio::time::timeout(Duration::from_secs(4), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(name, "x");
    tokio::time::timeout(Duration::from_secs(2), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn on_timer_holds_the_service_until_it_finishes() {
    let t = tempfile::tempdir().unwrap();
    let mut b = builder(t.path());
    b.idle_timeout(Duration::from_millis(50));
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    b.on_timer(move |_| {
        let d = d.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            d.store(true, Ordering::SeqCst);
        }
    });
    let timers = b.scheduler();
    let h = tokio::spawn(b.run_with(listener(t.path()), None));
    timers
        .schedule(&TimerSpec {
            name: "x".into(),
            at_unix: now() + 3,
            persistent: false,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(6), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        done.load(Ordering::SeqCst),
        "o serviço esperou o on_timer terminar"
    );
}

#[tokio::test]
async fn on_start_runs_before_serving() {
    let t = tempfile::tempdir().unwrap();
    let mut b = builder(t.path());
    let topic = b.server().topic("t", json!(0));
    b.on_start(move || async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        topic.publish(json!(1));
    });
    let p = t.path().join("s.sock");
    let h = tokio::spawn(b.run_with(listener(t.path()), None));
    let c = lucerna_ipc::Client::connect(&p, "test", "demo")
        .await
        .unwrap();
    let snap = c.subscribe(&["t"]).await.unwrap();
    assert_eq!(snap[0].data, json!(1));
    drop(c);
    tokio::time::timeout(Duration::from_secs(2), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn persistent_timer_autostart_follows_cancel() {
    let t = tempfile::tempdir().unwrap();
    let b = builder(t.path());
    let timers = b.scheduler();
    let h = tokio::spawn(b.run_with(listener(t.path()), None));
    timers
        .schedule(&TimerSpec {
            name: "p".into(),
            at_unix: now() + 3600,
            persistent: true,
        })
        .unwrap();
    let desktop = t.path().join("config/autostart/lucerna-demo.desktop");
    assert!(desktop.exists());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!h.is_finished(), "o timer persistente segura o serviço");
    timers.cancel("p").unwrap();
    assert!(!desktop.exists());
    tokio::time::timeout(Duration::from_secs(2), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
