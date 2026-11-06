use super::{lock_runtime, now_ms_u64, AppError, RuntimeHandle, EXIT_CONFIG, EXIT_INTERNAL};
use dfor77::GatewayResumeSession;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const MAX_GATEWAY_SESSION_FILE_BYTES: usize = 64 * 1024;

pub(super) fn load_gateway_resume_session(
    path: &str,
) -> Result<Option<GatewayResumeSession>, AppError> {
    match dfor77::private_file::read_limited(Path::new(path), MAX_GATEWAY_SESSION_FILE_BYTES) {
        Ok(content) => {
            let content = std::str::from_utf8(&content).map_err(|_| {
                AppError::message(EXIT_CONFIG, "gateway session must be valid UTF-8")
            })?;
            GatewayResumeSession::from_json(content)
                .map(Some)
                .map_err(|why| {
                    AppError::message(EXIT_CONFIG, format!("gateway session invalid: {why}"))
                })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::message(
            EXIT_CONFIG,
            format!("gateway session read failed: {error}"),
        )),
    }
}

pub(super) fn save_gateway_resume_session(
    runtime: &RuntimeHandle,
    path: &str,
) -> Result<(), AppError> {
    let (session, invalidated) = {
        let runtime = lock_runtime(runtime)?;
        (
            runtime.gateway_resume_session(),
            runtime.gateway_resume_session_invalidated(),
        )
    };
    let path = Path::new(path);
    let result = match session {
        Some(session) => write_gateway_session_file(path, &session.to_json()),
        None if invalidated => match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        },
        None => Ok(()),
    };
    result.map_err(|error| {
        AppError::message(
            EXIT_INTERNAL,
            format!("gateway session write failed: {error}"),
        )
    })
}

pub(super) fn write_gateway_session_file(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = gateway_session_tmp_path(path);
    write_gateway_session_tmp(path, &tmp, content)
}

pub(super) fn write_gateway_session_tmp(path: &Path, tmp: &Path, content: &str) -> io::Result<()> {
    let mut created = false;
    let write_result = (|| -> io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(tmp)?;
        created = true;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        dfor77::private_file::restrict_permissions(&file)?;
        fs::rename(tmp, path)?;
        Ok(())
    })();
    if created && write_result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    write_result
}

pub(super) fn gateway_session_tmp_path(path: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "session".into());
    path.with_file_name(format!(
        "{}.tmp-{}-{}-{}",
        name,
        std::process::id(),
        now_ms_u64(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SharedRuntime;
    use dfor77::DforRuntime;

    fn session_root() -> PathBuf {
        let root = gateway_session_tmp_path(&std::env::temp_dir().join("dfor77-session-io"));
        fs::create_dir(&root).unwrap();
        root
    }

    fn ready_runtime() -> RuntimeHandle {
        let runtime = SharedRuntime::new(DforRuntime::new(false, false));
        lock_runtime(&runtime).unwrap().ingest_gateway_json(r#"{"op":0,"s":10,"t":"READY","d":{"session_id":"saved","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        runtime
    }

    #[test]
    fn cold_runtime_preserves_a_checkpoint_until_its_session_is_known() {
        let root = session_root();
        let path = root.join("session.json");
        let expected = GatewayResumeSession::new("saved", "wss://resume", 10, None)
            .unwrap()
            .to_json();
        fs::write(&path, &expected).unwrap();
        let runtime = SharedRuntime::new(DforRuntime::new(false, false));
        save_gateway_resume_session(&runtime, path.to_str().unwrap()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resumable_disconnects_preserve_the_checkpoint_and_cursor() {
        let root = session_root();
        let path = root.join("session.json");
        for frame in [r#"{"op":7,"d":null}"#, r#"{"op":9,"d":true}"#] {
            let runtime = ready_runtime();
            let expected = lock_runtime(&runtime)
                .unwrap()
                .gateway_resume_session()
                .unwrap();
            lock_runtime(&runtime)
                .unwrap()
                .ingest_gateway_json(frame, 1)
                .unwrap();
            save_gateway_resume_session(&runtime, path.to_str().unwrap()).unwrap();
            assert_eq!(
                load_gateway_resume_session(path.to_str().unwrap())
                    .unwrap()
                    .unwrap(),
                expected
            );
            assert!(!lock_runtime(&runtime)
                .unwrap()
                .gateway_resume_session_invalidated());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shutdown_removes_its_checkpoint_and_reports_unremovable_paths() {
        let root = session_root();
        let path = root.join("session.json");
        let runtime = ready_runtime();
        save_gateway_resume_session(&runtime, path.to_str().unwrap()).unwrap();
        lock_runtime(&runtime).unwrap().shutdown_gateway();
        save_gateway_resume_session(&runtime, path.to_str().unwrap()).unwrap();
        assert!(!path.exists());
        save_gateway_resume_session(&runtime, path.to_str().unwrap()).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            save_gateway_resume_session(&runtime, path.to_str().unwrap()),
            Err(AppError::Message(EXIT_INTERNAL, _))
        ));
        assert!(path.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_loading_accepts_the_exact_limit_and_rejects_invalid_encodings() {
        let root = session_root();
        let path = root.join("session.json");
        let path_text = path.to_str().unwrap();
        assert!(load_gateway_resume_session(path_text).unwrap().is_none());
        let expected = GatewayResumeSession::new("saved", "wss://resume", 10, None).unwrap();
        let mut text = expected.to_json();
        text.push_str(&" ".repeat(MAX_GATEWAY_SESSION_FILE_BYTES - text.len()));
        fs::write(&path, text).unwrap();
        assert_eq!(
            load_gateway_resume_session(path_text).unwrap(),
            Some(expected)
        );
        for bytes in [b"\xff".as_slice(), b"not JSON", b"{}"] {
            fs::write(&path, bytes).unwrap();
            assert!(matches!(
                load_gateway_resume_session(path_text),
                Err(AppError::Message(EXIT_CONFIG, _))
            ));
        }
        fs::remove_dir_all(root).unwrap();
    }
}
