use kernel::store::{Config, Store};
use std::time::{Duration, Instant};

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "writer-lock child did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn writer_lock_process_helper() {
    let Some(dir) = std::env::var_os("SEKEJAP_WRITER_LOCK_CHILD") else { return };
    let dir = std::path::PathBuf::from(dir);
    let _writer = Store::open(&dir, Config::default()).expect("child writer opens");
    std::fs::write(dir.join("writer-ready"), b"ready").unwrap();
    loop { std::thread::sleep(Duration::from_secs(1)); }
}

#[test]
fn one_writer_readers_allowed_and_process_death_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    drop(Store::create(dir.path(), Config::default()).unwrap());

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("writer_lock_process_helper")
        .arg("--nocapture")
        .env("SEKEJAP_WRITER_LOCK_CHILD", dir.path())
        .spawn()
        .unwrap();
    wait_for(&dir.path().join("writer-ready"));

    let refused = match Store::open(dir.path(), Config::default()) {
        Ok(_) => panic!("a second writer opened the same data file"),
        Err(error) => error.to_string(),
    };
    assert!(refused.contains("writer") && refused.contains("lock"),
        "the refusal must name the writer lock, got: {refused}");
    Store::open_snapshot(dir.path(), Config::default())
        .expect("a read-only snapshot remains available beside the writer");

    child.kill().unwrap();
    child.wait().unwrap();
    Store::open(dir.path(), Config::default())
        .expect("the OS releases the advisory lock when the writer process dies");
}

#[test]
fn dropping_a_writer_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::create(dir.path(), Config::default()).unwrap();
    #[cfg(not(feature = "test-support"))]
    assert!(Store::open(dir.path(), Config::default()).is_err(),
        "production builds must refuse a second writer in the same process too");
    drop(first);
    Store::open(dir.path(), Config::default()).expect("dropping the first writer releases its lock");
}
