//! Local transport. Windows pipes are private to the account, like Unix sockets.
#[cfg(unix)]
pub(crate) use tokio::net::UnixStream as Stream;
#[cfg(windows)]
pub(crate) use windows::Stream;

#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::Path;
    use std::time::Duration;
    use tokio::io::{ReadHalf, WriteHalf, split};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

    pub(crate) struct Stream(NamedPipeClient);

    impl Stream {
        pub(crate) async fn connect(path: &Path) -> io::Result<Self> {
            let name = crate::windows::pipe_name(path)?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                match ClientOptions::new().open(&name) {
                    Ok(pipe) => {
                        crate::windows::verify_server(&pipe)?;
                        return Ok(Self(pipe));
                    }
                    Err(e)
                        if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                            && tokio::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        pub(crate) fn into_split(self) -> (ReadHalf<NamedPipeClient>, WriteHalf<NamedPipeClient>) {
            split(self.0)
        }
    }
}
