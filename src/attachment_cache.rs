use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;

#[derive(Debug)]
pub enum DownloadError<E> {
    Io(io::Error),
    Download(E),
}

impl<E: std::fmt::Display> std::fmt::Display for DownloadError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Io(e) => write!(f, "io error: {e}"),
            DownloadError::Download(e) => write!(f, "download error: {e}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for DownloadError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DownloadError::Io(e) => Some(e),
            DownloadError::Download(e) => Some(e),
        }
    }
}

pub async fn download_to_path<F, E>(
    final_path: &Path,
    download: F,
) -> Result<(), DownloadError<E>>
where
    F: for<'a> FnOnce(&'a mut File) -> Pin<Box<dyn std::future::Future<Output = Result<(), E>> + Send + 'a>>,
{
    // Build a temp path next to the final file so the rename is atomic.
    let tmp_name = final_path
        .file_name()
        .map(|n| {
            let mut s = n.to_os_string();
            s.push(".partial");
            s
        })
        .unwrap_or_else(|| "attachment.partial".into());
    let tmp_path = final_path.with_file_name(tmp_name);

    // Open the temp file (truncates any leftover from a prior crashed run).
    let mut file = File::create(&tmp_path).map_err(DownloadError::Io)?;

    // Run the download closure. On failure, clean up the temp and return.
    if let Err(e) = download(&mut file).await {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(DownloadError::Download(e));
    }

    // Flush and close before rename.
    let _ = file.flush();
    drop(file);

    // Atomic rename. On failure, remove the temp so we don't leak partial data.
    if let Err(e) = std::fs::rename(&tmp_path, final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(DownloadError::Io(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    #[tokio::test]
    async fn download_to_path_writes_final_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("attachment.bin");

        let data = b"hello world, this is the attachment content";
        let result = super::download_to_path(&final_path, |f| Box::pin(async move {
            f.write_all(data).unwrap();
            Ok::<_, &str>(())
        }))
        .await;

        assert!(result.is_ok(), "expected Ok(()), got {result:?}");
        assert!(
            final_path.exists(),
            "final file should exist after successful download"
        );
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            data,
            "final file content must match what the closure wrote"
        );

        // No temp file left behind — the only entry is the final file.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one entry (the final file) in tempdir after success"
        );
    }

    #[tokio::test]
    async fn download_to_path_preserves_existing_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("attachment.bin");

        // Pre-write an existing file at the final path.
        std::fs::write(&final_path, b"original data that must survive").unwrap();

        let result = super::download_to_path(&final_path, |_f| Box::pin(async { Err::<(), _>("network error") })).await;

        let err = match result {
            Err(super::DownloadError::Download(msg)) => msg,
            other => panic!("expected Err(DownloadError::Download(_)), got {other:?}"),
        };
        assert_eq!(
            err, "network error",
            "error should be the closure's error message"
        );

        // The pre-existing file is completely untouched.
        assert!(final_path.exists(), "pre-existing file must still exist");
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            b"original data that must survive",
            "pre-existing file content must be unchanged"
        );
    }

    #[tokio::test]
    async fn download_to_path_cleans_up_temp_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("attachment.bin");

        // final_path does not exist yet.
        assert!(
            !final_path.exists(),
            "precondition: final path does not exist yet"
        );

        let result = super::download_to_path(&final_path, |_f| Box::pin(async { Err::<(), _>("download failed") })).await;

        let err = match result {
            Err(super::DownloadError::Download(msg)) => msg,
            other => panic!("expected Err(DownloadError::Download(_)), got {other:?}"),
        };
        assert_eq!(
            err, "download failed",
            "error should be the closure's error message"
        );

        // No trace left in the tempdir — temp file must have been cleaned up.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            entries.is_empty(),
            "expected zero entries in tempdir after failed download, got {}",
            entries.len(),
        );
    }
}
