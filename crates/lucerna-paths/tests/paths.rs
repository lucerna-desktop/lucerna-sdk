use lucerna_paths::*;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};

fn env(home: &str) -> Env {
    Env {
        home: Some(home.into()),
        xdg_runtime_dir: Some("/run/user/1000".into()),
        ..Env::default()
    }
}

#[test]
fn xdg_defaults_follow_ecossistema() {
    let p = AppPaths::new("routine", &env("/home/u")).unwrap();
    assert_eq!(
        p.data_dir().unwrap(),
        PathBuf::from("/home/u/.local/share/lucerna/routine")
    );
    assert_eq!(
        p.config_file().unwrap(),
        PathBuf::from("/home/u/.config/lucerna/routine.toml")
    );
    assert_eq!(
        p.cache_dir().unwrap(),
        PathBuf::from("/home/u/.cache/lucerna/routine")
    );
    assert_eq!(
        p.socket().unwrap(),
        PathBuf::from("/run/user/1000/lucerna/routine.sock")
    );
    assert_eq!(
        shell_socket(&env("/h")).unwrap(),
        PathBuf::from("/run/user/1000/lucerna/shell.sock")
    );
}

#[test]
fn xdg_variables_win_and_relative_ones_are_ignored() {
    let mut e = env("/home/u");
    e.xdg_data_home = Some("/dados".into());
    e.xdg_config_home = Some("relativo/config".into());
    let p = AppPaths::new("notes", &e).unwrap();
    assert_eq!(p.data_dir().unwrap(), PathBuf::from("/dados/lucerna/notes"));
    assert_eq!(
        p.config_file().unwrap(),
        PathBuf::from("/home/u/.config/lucerna/notes.toml")
    );
}

#[test]
fn missing_or_relative_runtime_dir_is_an_error() {
    let mut e = env("/home/u");
    e.xdg_runtime_dir = None;
    let err = AppPaths::new("a", &e).unwrap().socket().unwrap_err();
    assert!(matches!(err, PathsError::NoRuntimeDir));
    assert_eq!(err.to_string(), "XDG_RUNTIME_DIR não está definido.");
    e.xdg_runtime_dir = Some("run".into());
    assert!(matches!(
        AppPaths::new("a", &e).unwrap().socket(),
        Err(PathsError::NoRuntimeDir)
    ));
}

#[test]
fn app_ids() {
    for ok in ["hello", "routine", "phone-android", "a1"] {
        assert!(validate_app_id(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "Hello",
        "1app",
        "-a",
        "a_b",
        "a/b",
        "..",
        "a.sock",
        &"a".repeat(33),
    ] {
        assert!(validate_app_id(bad).is_err(), "{bad}");
    }
    assert!(AppPaths::new("../x", &env("/h")).is_err());
}

#[test]
fn manifest_dirs_user_first() {
    assert_eq!(
        manifest_dirs(&env("/home/u")),
        vec![
            PathBuf::from("/home/u/.local/share/lucerna/apps"),
            PathBuf::from("/usr/share/lucerna/apps")
        ]
    );
}

#[test]
fn private_dir_is_0700_even_if_it_existed() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("lucerna");
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    create_private_dir(&d).unwrap();
    assert_eq!(
        std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn private_dir_rejects_symlink_and_leaves_target_alone() {
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("alvo");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = t.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(create_private_dir(&link).is_err());
    assert_eq!(mode(&target), 0o755);
}

#[test]
fn private_dir_rejects_regular_file() {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("arq");
    std::fs::write(&f, "x").unwrap();
    assert!(create_private_dir(&f).is_err());
}

#[test]
fn private_dir_creates_nested_with_private_last_level() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("a/b/lucerna");
    create_private_dir(&d).unwrap();
    assert_eq!(mode(&d), 0o700);
    assert!(d.parent().unwrap().is_dir());
}

#[test]
fn private_dir_already_0700_is_ok() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("p");
    create_private_dir(&d).unwrap();
    create_private_dir(&d).unwrap();
    assert_eq!(mode(&d), 0o700);
}

#[test]
fn shell_id_is_reserved() {
    assert!(matches!(
        validate_app_id("shell"),
        Err(PathsError::InvalidAppId(_))
    ));
    assert!(validate_app_id("shell2").is_ok());
}

#[test]
fn missing_or_relative_home_is_no_home() {
    for home in [None, Some(PathBuf::from("rel"))] {
        let e = Env {
            home,
            ..Env::default()
        };
        let p = AppPaths::new("a", &e).unwrap();
        let err = p.data_dir().unwrap_err();
        assert!(matches!(err, PathsError::NoHome));
        assert_eq!(err.to_string(), "HOME não está definido.");
        assert!(p.config_file().is_err() && p.cache_dir().is_err());
    }
}

#[test]
fn from_lookup_treats_empty_as_absent() {
    let e = Env::from_lookup(|n| match n {
        "HOME" => Some("/home/u".into()),
        "XDG_DATA_HOME" | "XDG_RUNTIME_DIR" => Some("".into()),
        _ => None,
    });
    assert_eq!(e.home, Some(PathBuf::from("/home/u")));
    assert_eq!(e.xdg_data_home, None);
    assert_eq!(e.xdg_runtime_dir, None);
}

#[test]
fn manifest_dirs_without_home() {
    assert_eq!(
        manifest_dirs(&Env::default()),
        vec![PathBuf::from("/usr/share/lucerna/apps")]
    );
}

#[test]
fn runtime_dir_is_lucerna_subdir() {
    let p = AppPaths::new("a", &env("/h")).unwrap();
    assert_eq!(
        p.runtime_dir().unwrap(),
        PathBuf::from("/run/user/1000/lucerna")
    );
    assert_eq!(p.app(), "a");
}

#[test]
fn app_id_length_limits_and_message() {
    assert!(validate_app_id(&"a".repeat(32)).is_ok());
    assert!(validate_app_id(&"a".repeat(33)).is_err());
    assert!(validate_app_id("a-").is_ok());
    assert_eq!(
        validate_app_id("Bad").unwrap_err().to_string(),
        "Nome de app inválido: \"Bad\" (use a-z, 0-9 e \"-\", começando por letra, até 32)."
    );
}

#[test]
fn env_config_home_and_runtime_dir() {
    let e = env("/home/u");
    assert_eq!(e.config_home().unwrap(), PathBuf::from("/home/u/.config"));
    assert_eq!(e.runtime_dir().unwrap(), PathBuf::from("/run/user/1000"));
    let e = Env {
        xdg_config_home: Some("/cfg".into()),
        ..env("/home/u")
    };
    assert_eq!(e.config_home().unwrap(), PathBuf::from("/cfg"));
    // Relativo não vale: cai no HOME (config) ou é erro (runtime).
    let e = Env {
        home: Some("/home/u".into()),
        xdg_config_home: Some("cfg".into()),
        xdg_runtime_dir: Some("run".into()),
        ..Env::default()
    };
    assert_eq!(e.config_home().unwrap(), PathBuf::from("/home/u/.config"));
    assert!(matches!(e.runtime_dir(), Err(PathsError::NoRuntimeDir)));
    assert!(matches!(
        Env::default().config_home(),
        Err(PathsError::NoHome)
    ));
    assert!(matches!(
        Env::default().runtime_dir(),
        Err(PathsError::NoRuntimeDir)
    ));
}

#[test]
fn socket_path_longer_than_sun_path_is_an_error() {
    // `/` + n × `x` + `/lucerna/a.sock` (15 bytes): n = 91 dá 107 bytes, o limite.
    let mut e = env("/home/u");
    e.xdg_runtime_dir = Some(format!("/{}", "x".repeat(91)).into());
    let ok = AppPaths::new("a", &e).unwrap().socket().unwrap();
    assert_eq!(ok.as_os_str().len(), 107);

    e.xdg_runtime_dir = Some(format!("/{}", "x".repeat(92)).into());
    let err = AppPaths::new("a", &e).unwrap().socket().unwrap_err();
    let path = format!("/{}/lucerna/a.sock", "x".repeat(92));
    assert!(
        matches!(&err, PathsError::SocketPathTooLong { len: 108, path: p } if p.as_os_str() == path.as_str())
    );
    assert_eq!(
        err.to_string(),
        format!("O caminho do socket é longo demais (108 bytes; o limite é 107): {path}.")
    );
    // `shell.sock` tem 4 bytes a mais que `a.sock`.
    e.xdg_runtime_dir = Some(format!("/{}", "x".repeat(88)).into());
    assert!(matches!(
        shell_socket(&e),
        Err(PathsError::SocketPathTooLong { len: 108, .. })
    ));
}

#[test]
fn state_and_autostart_paths() {
    let p = AppPaths::new("routine", &env("/home/u")).unwrap();
    assert_eq!(
        p.state_dir().unwrap(),
        PathBuf::from("/home/u/.local/state/lucerna/routine")
    );
    assert_eq!(
        p.serve_log().unwrap(),
        PathBuf::from("/home/u/.local/state/lucerna/routine/serve.log")
    );
    assert_eq!(
        p.autostart_file().unwrap(),
        PathBuf::from("/home/u/.config/autostart/lucerna-routine.desktop")
    );
    let mut e = env("/home/u");
    e.xdg_state_home = Some("/estado".into());
    e.xdg_config_home = Some("/cfg".into());
    let p = AppPaths::new("routine", &e).unwrap();
    assert_eq!(
        p.serve_log().unwrap(),
        PathBuf::from("/estado/lucerna/routine/serve.log")
    );
    assert_eq!(
        p.autostart_file().unwrap(),
        PathBuf::from("/cfg/autostart/lucerna-routine.desktop")
    );
    e.xdg_state_home = Some("relativo".into());
    assert_eq!(
        AppPaths::new("a", &e).unwrap().state_dir().unwrap(),
        PathBuf::from("/home/u/.local/state/lucerna/a")
    );
    let sem_home = Env::default();
    assert!(matches!(
        AppPaths::new("a", &sem_home).unwrap().serve_log(),
        Err(PathsError::NoHome)
    ));
}

#[test]
fn state_home_is_read_from_the_environment() {
    let e = Env::from_lookup(|n| (n == "XDG_STATE_HOME").then(|| "/s".into()));
    assert_eq!(e.xdg_state_home, Some(PathBuf::from("/s")));
    let e = Env::from_lookup(|n| (n == "XDG_STATE_HOME").then(|| "".into()));
    assert_eq!(e.xdg_state_home, None);
}

#[test]
fn vars_lists_only_what_is_set() {
    let e = Env {
        home: Some("/h".into()),
        xdg_runtime_dir: Some("/run/user/1".into()),
        xdg_state_home: Some("/s".into()),
        ..Env::default()
    };
    assert_eq!(
        e.vars(),
        vec![
            ("HOME", PathBuf::from("/h")),
            ("XDG_STATE_HOME", PathBuf::from("/s")),
            ("XDG_RUNTIME_DIR", PathBuf::from("/run/user/1"))
        ]
    );
}
