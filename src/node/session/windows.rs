//! ConPTY ownership stays with the session, never with an attachment.
//! The reaper closes the console while the reader drains it, then joins both
//! I/O threads before publishing exit. Closing ConPTY can block on its output.
use super::*;
use anyhow::Context;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Error, Read, Write};
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::thread;
use tokio::runtime::Handle;
use windows_sys::Win32::System::Threading::TerminateProcess;

fn pty_size(size: Size) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl Session {
    pub fn spawn(
        spec: &SpawnSpec,
        name: String,
        events: broadcast::Sender<HostedEvent>,
        host_clients: Arc<AtomicUsize>,
    ) -> Result<Arc<Self>> {
        let size = spec.size.sane();
        let argv = if spec.command.is_empty() {
            vec![user::shell()]
        } else {
            spec.command.clone()
        };
        let label = spec.label.clone().unwrap_or_else(|| describe(&argv));
        let pair = native_pty_system()
            .openpty(pty_size(size))
            .context("opening ConPTY (Windows 10 1809 or newer required)")?;
        let mut command = CommandBuilder::new(&argv[0]);
        command.args(&argv[1..]);
        if let Some(user) = user::current() {
            command.cwd(&user.home);
        }
        if let Some(cwd) = &spec.cwd {
            command.cwd(cwd);
        }
        command.env("TERM", "xterm-256color");
        command.env("MM_SESSION", &name);
        let mut child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawning {}", argv[0]))?;
        let handle = child
            .as_raw_handle()
            .context("ConPTY child has no process handle")?;
        // SAFETY: child owns this handle throughout duplication. The session
        // retains a handle, not a reusable PID, for later termination.
        let process = unsafe { BorrowedHandle::borrow_raw(handle) }.try_clone_to_owned()?;
        let pid = child
            .process_id()
            .context("ConPTY child has no process id")?;
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(INPUT_BACKLOG);
        let (size_tx, mut sizes) = watch::channel(size);
        let (output_tx, _) = broadcast::channel(OUTPUT_BACKLOG);
        let (exit_tx, exit_rx) = watch::channel(None);
        let (stopped_tx, mut stopped) = watch::channel(false);
        let session = Arc::new(Self {
            process: Some(process),
            name: watch::channel(name).0,
            command: label,
            pid,
            started: SystemTime::now(),
            input_tx,
            size_tx,
            output_tx,
            state: Mutex::new(State::new(size)),
            exit_rx,
            events,
            host_clients,
            next_client: AtomicU64::new(0),
        });
        let s = Arc::clone(&session);
        let reading = thread::spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => s.ingest(&buffer[..n]),
                    Err(e) => {
                        debug!("ConPTY reader ended: {e}");
                        break;
                    }
                }
            }
        });
        let master = Arc::new(Mutex::new(Some(pair.master)));
        let resizing = Arc::clone(&master);
        let runtime = Handle::current();
        let writing = thread::spawn(move || {
            enum Work {
                Input(Vec<u8>),
                Resize(Size),
                Stop,
            }
            loop {
                let work = runtime.block_on(async {
                    tokio::select! {
                        biased;
                        _ = stopped.changed() => Work::Stop,
                        changed = sizes.changed() => {
                            if changed.is_err() { Work::Stop } else { Work::Resize(*sizes.borrow_and_update()) }
                        }
                        bytes = input_rx.recv() => bytes.map(Work::Input).unwrap_or(Work::Stop),
                    }
                });
                match work {
                    Work::Stop => break,
                    Work::Resize(size) => {
                        if let Some(master) = held(&resizing).as_ref()
                            && let Err(e) = master.resize(pty_size(size))
                        {
                            debug!("ConPTY resize failed: {e}");
                        }
                    }
                    Work::Input(bytes) => {
                        if let Err(e) = writer.write_all(&bytes) {
                            debug!("ConPTY input ended: {e}");
                            break;
                        }
                    }
                }
            }
            drop(writer);
        });
        let s = Arc::clone(&session);
        thread::spawn(move || {
            let code = match child.wait() {
                Ok(status) => i32::try_from(status.exit_code()).unwrap_or(-1),
                Err(e) => {
                    warn!("waiting on ConPTY child failed: {e}");
                    -1
                }
            };
            stopped_tx.send_replace(true);
            // Close the console before joining a writer that may be blocked
            // on a child that stopped reading input. The reader keeps draining.
            drop(pair.slave);
            let console = held(&master).take();
            drop(console);
            if writing.join().is_err() {
                warn!("ConPTY writer panicked");
            }
            if reading.join().is_err() {
                warn!("ConPTY reader panicked");
            }
            s.publish(EventKind::Exited(code));
            exit_tx.send_replace(Some(code));
        });
        Ok(session)
    }

    pub fn kill(&self) {
        if let Some(process) = &self.process {
            // SAFETY: the retained handle refers to this child even after exit.
            if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 && !self.has_exited() {
                warn!(
                    "terminating ConPTY child failed: {}",
                    Error::last_os_error()
                );
            }
        }
    }

    pub fn kill_now(&self) {
        self.kill();
    }
}
