//! One image upload per attachment, owned and cancelled with the terminal loop.
//! Clipboard access and file transfer run beside input and output, with a
//! bounded queue and a deadline. Failed transfers are reported, never retried.

use std::future::{Future, pending};
use std::time::Duration;

use crate::clipboard::{self, Image};
use anyhow::{Result, anyhow, bail};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug)]
pub(super) enum Uploaded {
    NoImage,
    ClipboardFailed(anyhow::Error),
    Image { bytes: usize },
}

pub(super) enum Update {
    Started {
        bytes: usize,
    },
    Finished {
        result: Result<Uploaded>,
        key: Vec<u8>,
    },
}

struct Request {
    name: String,
    key: Vec<u8>,
}

pub struct Uploads {
    requests: mpsc::Sender<Request>,
    results: mpsc::Receiver<Update>,
    worker: JoinHandle<()>,
    busy: bool,
}

impl Uploads {
    pub fn new<F, Fut>(upload: F) -> Self
    where
        F: FnMut(String, Image) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        Self::with_reader(clipboard::image, upload)
    }

    fn with_reader<R, Read, F, Fut>(mut read: R, mut upload: F) -> Self
    where
        R: FnMut() -> Read + Send + 'static,
        Read: Future<Output = Result<Option<Image>>> + Send,
        F: FnMut(String, Image) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        let (requests, mut wanted) = mpsc::channel::<Request>(1);
        let (completed, results) = mpsc::channel(2);
        let worker = tokio::spawn(async move {
            while let Some(request) = wanted.recv().await {
                let transfer = async {
                    let image = match read().await {
                        Ok(Some(image)) => image,
                        Ok(None) => return Ok(Uploaded::NoImage),
                        Err(e) => return Ok(Uploaded::ClipboardFailed(e)),
                    };
                    let bytes = image.data.len();
                    completed
                        .send(Update::Started { bytes })
                        .await
                        .map_err(|e| anyhow!("reporting image upload: {e}"))?;
                    upload(request.name, image).await?;
                    Ok(Uploaded::Image { bytes })
                };
                let result = match tokio::time::timeout(Duration::from_secs(300), transfer).await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!("image upload timed out")),
                };
                if completed
                    .send(Update::Finished {
                        result,
                        key: request.key,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            requests,
            results,
            worker,
            busy: false,
        }
    }

    pub(super) fn start(&mut self, name: &str, key: &[u8]) -> Result<()> {
        if self.busy {
            bail!("an image paste is already in progress");
        }
        self.requests
            .try_send(Request {
                name: name.to_string(),
                key: key.to_vec(),
            })
            .map_err(|e| anyhow!("starting image upload: {e}"))?;
        self.busy = true;
        Ok(())
    }

    pub(super) async fn next(&mut self) -> Update {
        if !self.busy {
            return pending().await;
        }
        let update = match self.results.recv().await {
            Some(update) => update,
            None => {
                let error = match (&mut self.worker).await {
                    Err(e) => anyhow!("image upload worker: {e}"),
                    Ok(()) => anyhow!("image upload worker stopped"),
                };
                Update::Finished {
                    result: Err(error),
                    key: Vec::new(),
                }
            }
        };
        if matches!(update, Update::Finished { .. }) {
            self.busy = false;
        }
        update
    }
}

impl Drop for Uploads {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test(start_paused = true)]
    async fn an_upload_that_stops_answering_expires_without_stopping_the_terminal() {
        let mut uploads = Uploads::with_reader(
            || async {
                Ok(Some(Image {
                    kind: "png",
                    data: vec![1],
                }))
            },
            |_, _| pending::<Result<()>>(),
        );
        uploads.start("build", b"\x16").unwrap();
        assert!(matches!(uploads.next().await, Update::Started { .. }));
        let Update::Finished { result, .. } = uploads.next().await else {
            panic!("the upload should time out");
        };
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(uploads.start("build", b"\x16").is_ok());
    }

    #[tokio::test]
    async fn a_text_clipboard_returns_the_original_key_without_uploading() {
        let mut uploads = Uploads::with_reader(
            || async { Ok(None) },
            |_, _| async { panic!("text is never uploaded") },
        );
        uploads.start("build", b"\x16").unwrap();
        let Update::Finished { result, key } = uploads.next().await else {
            panic!("text needs no transfer");
        };
        assert!(matches!(result.unwrap(), Uploaded::NoImage));
        assert_eq!(key, b"\x16");
    }

    #[tokio::test]
    async fn a_pending_upload_rejects_duplicates_and_is_cancelled_when_its_owner_leaves() {
        let (started, ready) = oneshot::channel();
        let (cancelled, dropped) = oneshot::channel::<()>();
        let mut started = Some(started);
        let mut cancelled = Some(cancelled);
        let mut uploads = Uploads::with_reader(
            || async {
                Ok(Some(Image {
                    kind: "png",
                    data: vec![1, 2, 3],
                }))
            },
            move |name, image| {
                assert_eq!(name, "build");
                assert_eq!(image.data.len(), 3);
                let started = started.take().unwrap();
                let cancelled = cancelled.take().unwrap();
                async move {
                    let _cancelled = cancelled;
                    started.send(()).unwrap();
                    pending().await
                }
            },
        );
        uploads.start("build", b"\x16").unwrap();
        ready.await.unwrap();
        assert!(matches!(uploads.next().await, Update::Started { bytes: 3 }));
        assert!(uploads.start("build", b"\x16").is_err());
        drop(uploads);
        assert!(
            dropped.await.is_err(),
            "the transfer future must be dropped"
        );
    }
}
