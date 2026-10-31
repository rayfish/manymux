//! Native Windows persistence through the same named pipe used by mm agent.
#![cfg(windows)]

use manymux::client::{SessionHalves, Stream, Update};
use manymux::proto::{Request, Response, Size, SpawnSpec};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const MM: &str = env!("CARGO_BIN_EXE_mm");

struct Node {
    socket: PathBuf,
    config: PathBuf,
}

impl Node {
    async fn start() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let config =
            std::env::temp_dir().join(format!("mm-windows-test-{}-{id}", std::process::id()));
        let socket = config.join("node");
        // This client exits before the test connects. Its daemon must survive.
        let output = Command::new(MM)
            .env("MM_CONFIG_DIR", &config)
            .args(["--socket"])
            .arg(&socket)
            .args(["new", "--detached", "--name", "shell", "cmd", "/Q"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self { socket, config }
    }

    async fn call(&self, request: Request) -> Response {
        Stream::local(&self.socket)
            .await
            .unwrap()
            .call(&request)
            .await
            .unwrap()
    }

    async fn attach(&self, name: &str, size: Size, read_only: bool) -> SessionHalves {
        Stream::local(&self.socket)
            .await
            .unwrap()
            .attach(name, size, 0, read_only)
            .await
            .unwrap()
            .split()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // Always stop the isolated node, including when an assertion fails.
        let output = Command::new(MM)
            .env("MM_CONFIG_DIR", &self.config)
            .arg("--socket")
            .arg(&self.socket)
            .args(["stop", "--force"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(&self.config).unwrap();
    }
}

async fn output_until(session: &mut SessionHalves, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut output = String::new();
        loop {
            match session.reader.next().await.unwrap() {
                Update::Output(bytes) | Update::Screen(bytes) => {
                    output.push_str(&String::from_utf8_lossy(&bytes))
                }
                Update::Ping => session.writer.pong().await.unwrap(),
                Update::Exited(code) => panic!("exited {code} before {needle:?}: {output:?}"),
                Update::Disconnected => panic!("disconnected before {needle:?}: {output:?}"),
                _ => {}
            }
            if output.contains(needle) {
                return output;
            }
        }
    })
    .await
    .expect("the session answers before the deadline")
}

async fn exit_code(session: &mut SessionHalves) -> i32 {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match session.reader.next().await.unwrap() {
                Update::Exited(code) => return code,
                Update::Ping => session.writer.pong().await.unwrap(),
                Update::Disconnected => panic!("disconnected without an exit status"),
                _ => {}
            }
        }
    })
    .await
    .expect("the child exits before the deadline")
}

#[tokio::test]
async fn a_session_keeps_its_process_and_environment_after_every_client_leaves() {
    let node = Node::start().await;
    let mut first = node.attach("shell", Size::new(90, 30), false).await;
    first
        .writer
        .send_input(b"set MM_TEST_VALUE=retained\recho initialized\r")
        .await
        .unwrap();
    output_until(&mut first, "initialized").await;
    let Response::Sessions(before) = node.call(Request::List).await else {
        panic!("a listing")
    };
    drop(first);
    let mut second = node.attach("shell", Size::new(100, 40), false).await;
    second
        .writer
        .send_input(b"echo value:%MM_TEST_VALUE%\r")
        .await
        .unwrap();
    output_until(&mut second, "value:retained").await;
    let Response::Sessions(after) = node.call(Request::List).await else {
        panic!("a listing")
    };
    assert_eq!(before[0].pid, after[0].pid);
    let listed = Command::new(MM)
        .env("MM_CONFIG_DIR", &node.config)
        .arg("--socket")
        .arg(&node.socket)
        .args(["ls", "local"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains("shell"));
    second.writer.send_input(b"exit 7\r").await.unwrap();
    assert_eq!(exit_code(&mut second).await, 7);
}

#[tokio::test]
async fn a_viewer_cannot_type_or_resize_a_windows_session() {
    let node = Node::start().await;
    let mut owner = node.attach("shell", Size::new(100, 40), false).await;
    let viewer = Stream::local(&node.socket)
        .await
        .unwrap()
        .attach("shell", Size::new(20, 5), 0, true)
        .await
        .unwrap();
    assert_eq!(viewer.size, Size::new(100, 40));
    let mut viewer = viewer.split();
    viewer.writer.send_input(b"exit 99\r").await.unwrap();
    owner.writer.resize(Size::new(120, 50)).await.unwrap();
    owner.writer.send_input(b"echo alive\r").await.unwrap();
    output_until(&mut owner, "alive").await;
    drop(viewer);
    owner.writer.send_input(b"exit 0\r").await.unwrap();
    assert_eq!(exit_code(&mut owner).await, 0);
}

#[tokio::test]
async fn windows_commands_preserve_arguments_and_the_requested_directory() {
    let node = Node::start().await;
    let marker = node.config.join("folder with spaces");
    std::fs::create_dir_all(&marker).unwrap();
    let script = marker.join("test.ps1");
    std::fs::write(&script, "param([string]$value)\n[Console]::WriteLine('argument:' + $value)\n[Console]::WriteLine('directory:' + ((Get-Location).Path -eq $PSScriptRoot))\n$null = [Console]::ReadLine()\nexit 13\n").unwrap();
    let Response::Spawned { name } = node
        .call(Request::Spawn(SpawnSpec {
            name: Some("arguments".into()),
            command: vec![
                "powershell.exe".into(),
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                script.to_string_lossy().into_owned(),
                "a b & c".into(),
            ],
            cwd: Some(marker.to_string_lossy().into_owned()),
            size: Size::new(200, 40),
            ..SpawnSpec::default()
        }))
        .await
    else {
        panic!("a spawned session")
    };
    let mut session = node.attach(&name, Size::new(200, 40), false).await;
    let output = output_until(&mut session, "directory:True").await;
    assert!(output.contains("argument:a b & c"), "{output:?}");
    session.writer.send_input(b"\r").await.unwrap();
    assert_eq!(exit_code(&mut session).await, 13);
}

#[tokio::test]
async fn a_second_daemon_cannot_replace_the_first_nodes_pipe() {
    let node = Node::start().await;
    let output = Command::new(MM)
        .env("MM_CONFIG_DIR", &node.config)
        .arg("--socket")
        .arg(&node.socket)
        .args(["daemon", "--no-notify"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let Response::Sessions(sessions) = node.call(Request::List).await else {
        panic!("a listing")
    };
    assert_eq!(sessions.len(), 1);
}

#[tokio::test]
async fn resizing_reaches_the_console_and_killing_publishes_exit() {
    let node = Node::start().await;
    let Response::Spawned { name } = node.call(Request::Spawn(SpawnSpec {
        name: Some("sized".into()),
        command: vec!["powershell.exe".into(), "-NoProfile".into(), "-Command".into(),
            "[Console]::WriteLine('ready'); $null=[Console]::ReadLine(); [Console]::WriteLine('width:'+[Console]::WindowWidth); $null=[Console]::ReadLine()".into()],
        ..SpawnSpec::default()
    })).await else { panic!("a spawned session") };
    let mut session = node.attach(&name, Size::new(80, 24), false).await;
    output_until(&mut session, "ready").await;
    session.writer.resize(Size::new(120, 35)).await.unwrap();
    session.writer.send_input(b"\r").await.unwrap();
    output_until(&mut session, "width:120").await;
    assert!(matches!(
        node.call(Request::Kill { name }).await,
        Response::Ok
    ));
    assert_eq!(exit_code(&mut session).await, 1);
}

#[tokio::test]
async fn closing_the_launchers_console_does_not_stop_the_node() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    let config = std::env::temp_dir().join(format!("mm-windows-console-{}", std::process::id()));
    let socket = config.join("node");
    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let mut command = CommandBuilder::new(MM);
    command.env("MM_CONFIG_DIR", &config);
    command.arg("--socket");
    command.arg(&socket);
    command.args([
        "new",
        "--detached",
        "--name",
        "shell",
        "local",
        "cmd.exe",
        "/Q",
    ]);
    let mut launcher = pair.slave.spawn_command(command).unwrap();
    let mut output = pair.master.try_clone_reader().unwrap();
    let mut input = pair.master.take_writer().unwrap();
    let reading = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut query = 0;
        let mut answered = false;
        let mut buffer = [0; 4096];
        loop {
            let count = output.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if !answered {
                for &byte in &buffer[..count] {
                    const QUERY: &[u8] = b"\x1b[6n";
                    query = if byte == QUERY[query] {
                        query + 1
                    } else {
                        usize::from(byte == QUERY[0])
                    };
                    if query == QUERY.len() {
                        input.write_all(b"\x1b[1;1R").unwrap();
                        answered = true;
                        break;
                    }
                }
            }
        }
        bytes
    });
    let status = launcher.wait().unwrap();
    drop(pair);
    let bytes = reading.join().unwrap();
    assert!(status.success(), "{}", String::from_utf8_lossy(&bytes));
    let node = Node { socket, config };
    let mut session = node.attach("shell", Size::default(), false).await;
    session.writer.send_input(b"echo survived\r").await.unwrap();
    output_until(&mut session, "survived").await;
}

#[tokio::test]
async fn listing_an_absent_node_does_not_start_one() {
    let config = std::env::temp_dir().join(format!("mm-windows-absent-{}", std::process::id()));
    let socket = config.join("node");
    let listed = Command::new(MM)
        .env("MM_CONFIG_DIR", &config)
        .arg("--socket")
        .arg(&socket)
        .args(["ls", "local"])
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(Stream::local(&socket).await.is_err());
    if config.exists() {
        std::fs::remove_dir_all(config).unwrap();
    }
}

#[tokio::test]
async fn the_powershell_launcher_preserves_binary_protocol_and_finds_the_user_install() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use std::process::Stdio;
    use tokio::process::Command as AsyncCommand;
    let node = Node::start().await;
    let install = node.config.join("manymux");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::copy(MM, install.join("mm.exe")).unwrap();
    let directory = node.config.to_string_lossy().replace('\'', "''");
    let socket = node.socket.to_string_lossy().replace('\'', "''");
    let script = include_str!("../src/ssh/windows-agent.ps1")
        .replace(
            "([Environment]::GetFolderPath('LocalApplicationData'))",
            &format!("'{directory}'"),
        )
        .replace(
            "$start.Arguments = 'agent'",
            &format!("$start.Arguments = '--socket \"{socket}\" agent'"),
        );
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut child = AsyncCommand::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &STANDARD.encode(bytes),
        ])
        .env("MM_CONFIG_DIR", &node.config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stream = Stream::from_halves(
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stdin.take().unwrap()),
    );
    let reply = tokio::time::timeout(Duration::from_secs(20), stream.call(&Request::Version))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(reply, Response::Version { .. }));
    drop(stream);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
