use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lucerna_paths::Env;
use lucerna_service::{
    LocalScheduler, ScheduleError, Scheduler, SchedulerKind, TimerSpec, WAKE_STEP, WorkTracker,
    choose_scheduler, render_autostart,
};
use tokio::sync::mpsc;

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Um relógio de parede que acompanha o tempo (pausado) do `tokio`, mais um
/// "salto" somado, para simular a suspensão (o relógio de parede anda, o do
/// `tokio` não).
fn paused_clock() -> (Clock, u64, Arc<AtomicU64>) {
    let base = 1_700_000_000;
    let start = tokio::time::Instant::now();
    let jump = Arc::new(AtomicU64::new(0));
    let j = jump.clone();
    let clock: Clock = Arc::new(move || {
        base + (tokio::time::Instant::now() - start).as_secs() + j.load(Ordering::SeqCst)
    });
    (clock, base, jump)
}

fn env_in(dir: &Path) -> Env {
    Env {
        home: Some(dir.into()),
        xdg_config_home: Some(dir.join("config")),
        xdg_runtime_dir: Some(dir.join("run")),
        ..Env::default()
    }
}

fn spec(name: &str, at_unix: u64, persistent: bool) -> TimerSpec {
    TimerSpec {
        name: name.into(),
        at_unix,
        persistent,
    }
}

fn desktop(dir: &Path) -> PathBuf {
    dir.join("config/autostart/lucerna-routine.desktop")
}

async fn sleep(secs: u64) {
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

#[tokio::test(start_paused = true)]
async fn fires_at_the_time_and_holds_work_until_handled() {
    let t = tempfile::tempdir().unwrap();
    let (work, (tx, mut rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let (clock, base, _jump) = paused_clock();
    let s = LocalScheduler::new("routine", &env_in(t.path()), work.clone(), tx)
        .unwrap()
        .with_clock(clock);
    s.schedule(&spec("lembrete", base + 5, false)).unwrap();
    assert_eq!(work.pending(), 1);
    sleep(4).await;
    assert!(rx.try_recv().is_err());
    sleep(2).await;
    let fired = rx.try_recv().unwrap();
    assert_eq!(fired.name, "lembrete");
    assert!(s.timers().is_empty());
    assert_eq!(work.pending(), 1, "o guarda vai com o disparo");
    drop(fired);
    assert_eq!(work.pending(), 0);
    assert!(
        s.cancel("lembrete").is_ok(),
        "cancelar depois do disparo não é erro"
    );
}

#[tokio::test(start_paused = true)]
async fn each_timer_holds_work_and_rescheduling_replaces() {
    let t = tempfile::tempdir().unwrap();
    let (work, (tx, mut rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let (clock, base, _jump) = paused_clock();
    let s = LocalScheduler::new("routine", &env_in(t.path()), work.clone(), tx)
        .unwrap()
        .with_clock(clock);
    s.schedule(&spec("a", base + 10, false)).unwrap();
    s.schedule(&spec("b", base + 20, false)).unwrap();
    assert_eq!(work.pending(), 2);
    s.schedule(&spec("a", base + 30, false)).unwrap();
    assert_eq!(work.pending(), 2, "trocar o horário não soma trabalho");
    assert_eq!(s.timers()["a"].at_unix, base + 30);
    s.cancel("b").unwrap();
    assert_eq!(work.pending(), 1);
    assert_eq!(s.timers().len(), 1);
    sleep(15).await;
    assert!(rx.try_recv().is_err(), "o \"a\" antigo não dispara");
    sleep(16).await;
    let fired = rx.try_recv().unwrap();
    assert_eq!(fired.name, "a");
    assert!(rx.try_recv().is_err(), "um disparo só");
    drop(fired);
    assert_eq!(work.pending(), 0);
}

#[tokio::test(start_paused = true)]
async fn refuses_past_times_and_bad_names() {
    let t = tempfile::tempdir().unwrap();
    let (work, (tx, _rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let (clock, base, _jump) = paused_clock();
    let s = LocalScheduler::new("routine", &env_in(t.path()), work.clone(), tx)
        .unwrap()
        .with_clock(clock);
    assert!(matches!(
        s.schedule(&spec("a", base + 1, true)),
        Err(ScheduleError::InPast)
    ));
    assert!(matches!(
        s.schedule(&spec("a", base, true)),
        Err(ScheduleError::InPast)
    ));
    s.schedule(&spec("b", base + 2, false)).unwrap();
    for bad in ["Lembrete", "a/b"] {
        assert!(matches!(
            s.schedule(&spec(bad, base + 100, true)),
            Err(ScheduleError::InvalidName(n)) if n == bad
        ));
        assert!(matches!(
            s.cancel(bad),
            Err(ScheduleError::InvalidName(n)) if n == bad
        ));
    }
    assert_eq!(s.timers().len(), 1);
    assert_eq!(work.pending(), 1);
    assert!(!desktop(t.path()).exists());
    assert!(matches!(
        LocalScheduler::new(
            "Routine",
            &env_in(t.path()),
            work.clone(),
            mpsc::unbounded_channel().0
        ),
        Err(ScheduleError::NoDir(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn autostart_only_while_a_persistent_timer_exists() {
    let t = tempfile::tempdir().unwrap();
    let entry = desktop(t.path());
    let (work, (tx, mut rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let (clock, base, _jump) = paused_clock();
    let s = LocalScheduler::new("routine", &env_in(t.path()), work.clone(), tx)
        .unwrap()
        .with_clock(clock);

    s.schedule(&spec("x", base + 3600, false)).unwrap();
    assert!(!entry.exists(), "não persistente não cria a entrada");

    s.schedule(&spec("p1", base + 3600, true)).unwrap();
    let text = render_autostart("routine");
    assert_eq!(std::fs::read_to_string(&entry).unwrap(), text);
    assert!(text.contains("Exec=lucerna-routine serve\n"));
    assert!(text.contains("NoDisplay=true\n"));
    assert!(!entry.with_extension("desktop.tmp").exists());

    s.schedule(&spec("p2", base + 3600, true)).unwrap();
    s.cancel("p1").unwrap();
    assert!(entry.exists(), "ainda há um persistente");
    s.cancel("p2").unwrap();
    assert!(!entry.exists(), "só sobrou o não persistente");

    s.schedule(&spec("p3", base + 5, true)).unwrap();
    assert!(entry.exists());
    sleep(6).await;
    let fired = rx.try_recv().unwrap();
    assert_eq!(fired.name, "p3");
    assert!(!entry.exists(), "o persistente disparou");
    assert_eq!(s.timers().keys().collect::<Vec<_>>(), ["x"]);
}

#[tokio::test(start_paused = true)]
async fn switching_persistence_updates_autostart() {
    let t = tempfile::tempdir().unwrap();
    let entry = desktop(t.path());
    let (work, (tx, _rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let (clock, base, _jump) = paused_clock();
    let s = LocalScheduler::new("routine", &env_in(t.path()), work.clone(), tx)
        .unwrap()
        .with_clock(clock);

    s.schedule(&spec("p", base + 3600, true)).unwrap();
    assert!(entry.exists());
    s.schedule(&spec("p", base + 3600, false)).unwrap();
    assert!(!entry.exists(), "o mesmo nome deixou de ser persistente");
    s.schedule(&spec("p", base + 3600, true)).unwrap();
    assert!(entry.exists());

    s.shutdown();
    assert!(entry.exists(), "o login precisa da entrada");
    assert!(s.timers().is_empty());
    assert_eq!(work.pending(), 0);

    // Sem timer persistente, o shutdown apaga uma entrada velha.
    let t2 = tempfile::tempdir().unwrap();
    let old = desktop(t2.path());
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, "velha").unwrap();
    let (tx2, _rx2) = mpsc::unbounded_channel();
    let (clock2, base2, _jump2) = paused_clock();
    let s2 = LocalScheduler::new("routine", &env_in(t2.path()), work.clone(), tx2)
        .unwrap()
        .with_clock(clock2);
    s2.schedule(&spec("x", base2 + 3600, false)).unwrap();
    s2.shutdown();
    assert!(!old.exists());
    assert_eq!(work.pending(), 0);
}

#[tokio::test(start_paused = true)]
async fn wall_clock_jump_fires_within_a_step() {
    let t = tempfile::tempdir().unwrap();
    let (work, (tx, mut rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let (clock, base, jump) = paused_clock();
    let s = LocalScheduler::new("routine", &env_in(t.path()), work.clone(), tx)
        .unwrap()
        .with_clock(clock);
    s.schedule(&spec("cedo", base + 3600, false)).unwrap();
    sleep(1).await;
    // A suspensão: o relógio de parede anda uma hora, o do tokio não.
    jump.store(3600, Ordering::SeqCst);
    tokio::time::sleep(WAKE_STEP + Duration::from_secs(1)).await;
    let fired = rx.try_recv().unwrap();
    assert_eq!(fired.name, "cedo");
    assert!(s.timers().is_empty());
}

#[tokio::test]
async fn no_config_dir_is_an_error_only_for_persistent() {
    let t = tempfile::tempdir().unwrap();
    let env = Env {
        xdg_runtime_dir: Some(t.path().join("run")),
        ..Env::default()
    };
    let (work, (tx, _rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let s = LocalScheduler::new("routine", &env, work.clone(), tx).unwrap();
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    assert!(matches!(
        s.schedule(&spec("p", at, true)),
        Err(ScheduleError::NoDir(_))
    ));
    assert!(s.timers().is_empty());
    assert_eq!(work.pending(), 0);
    s.schedule(&spec("x", at, false)).unwrap();
    s.cancel("x").unwrap();
    s.shutdown();
}

#[tokio::test(start_paused = true)]
async fn failed_autostart_write_undoes_the_timer() {
    let t = tempfile::tempdir().unwrap();
    // Um arquivo comum no lugar da pasta `autostart`.
    std::fs::create_dir_all(t.path().join("config")).unwrap();
    std::fs::write(t.path().join("config/autostart"), "").unwrap();
    let (work, (tx, _rx)) = (WorkTracker::new(), mpsc::unbounded_channel());
    let (clock, base, _jump) = paused_clock();
    let s = LocalScheduler::new("routine", &env_in(t.path()), work.clone(), tx)
        .unwrap()
        .with_clock(clock);
    assert!(matches!(
        s.schedule(&spec("p", base + 3600, true)),
        Err(ScheduleError::Io(_))
    ));
    assert!(s.timers().is_empty());
    assert_eq!(work.pending(), 0);
}

/// Um `systemctl` falso em `dir/<name>`: registra os argumentos em
/// `dir/<name>.log` e depois roda `body`.
fn fake_systemctl(dir: &Path, name: &str, body: &str) -> (PathBuf, PathBuf) {
    let log = dir.join(format!("{name}.log"));
    let bin = dir.join(name);
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n[ \"$1\" = __probe ] && exit 0\necho \"$@\" >> '{}'\n{body}\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    wait_executable(&bin);
    (bin, log)
}

/// Com os testes em paralelo, um `fork` de outra thread pode herdar por um
/// instante o fd de escrita do script, e o `exec` dá `ETXTBSY`.
fn wait_executable(bin: &Path) {
    for _ in 0..200 {
        match Command::new(bin).arg("__probe").status() {
            Ok(_) => return,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("o systemctl falso não roda: {e}"),
        }
    }
    panic!("o systemctl falso continua ocupado");
}

#[test]
fn choose_scheduler_falls_back_to_local() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let (loaded, log) = fake_systemctl(t.path(), "loaded", "echo loaded");
    assert_eq!(
        choose_scheduler("routine", &env, &loaded),
        SchedulerKind::Systemd
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "--user show -p LoadState --value lucerna-routine.service\n"
    );
    let (not_found, _) = fake_systemctl(t.path(), "nf", "echo not-found");
    assert_eq!(
        choose_scheduler("routine", &env, &not_found),
        SchedulerKind::Local
    );
    let (no_bus, _) = fake_systemctl(
        t.path(),
        "nobus",
        "echo 'Failed to connect to bus' >&2; exit 1",
    );
    assert_eq!(
        choose_scheduler("routine", &env, &no_bus),
        SchedulerKind::Local
    );
    assert_eq!(
        choose_scheduler("routine", &env, &t.path().join("nao-existe")),
        SchedulerKind::Local
    );
    assert_eq!(
        choose_scheduler("Routine", &env, &loaded),
        SchedulerKind::Local
    );
    assert!(!t.path().join("config").exists(), "não escreve nada");
}
