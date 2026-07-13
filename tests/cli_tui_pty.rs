//! 実 PTY 上で合成ルートの raw mode / 代替スクリーン lifetime を通す結合テスト。

#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 100×24 の PTY master/slave pair を開く。
fn open_pty() -> io::Result<(File, File)> {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: output pointers refer to writable local integers, `size` is initialized, and the
    // optional terminal-name / termios pointers are null. A successful call returns two owned fds.
    let result = unsafe {
        libc::openpty(
            &raw mut master_fd,
            &raw mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openpty` succeeded and transferred two distinct, valid descriptors to this caller.
    let pair = unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) };
    Ok(pair)
}

fn terminal_attributes(terminal: &File) -> io::Result<libc::termios> {
    let mut attributes = std::mem::MaybeUninit::uninit();
    // SAFETY: `attributes` points to writable storage for one termios value and `terminal` owns a
    // live PTY slave descriptor for the duration of the call.
    if unsafe { libc::tcgetattr(terminal.as_raw_fd(), attributes.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `tcgetattr` initialized every field of `attributes`.
    Ok(unsafe { attributes.assume_init() })
}

/// PTY の window size を更新して、foreground process に resize を通知する。
fn resize_pty(terminal: &File, columns: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `terminal` owns the PTY master and `size` points to a fully initialized winsize.
    if unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_pty(mut master: File) -> Vec<u8> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match master.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => output.extend_from_slice(&chunk[..read]),
            // Linux PTYs report EIO, while Darwin normally reports EOF, after the final slave
            // descriptor closes. Both mean the captured stream is complete.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("PTY outputの読み取りに失敗: {error}"),
        }
    }
    output
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY上のusagiが終了しなかった",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_hop(home: &std::path::Path, slave: &File) -> io::Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("hop")
        .env("USAGI_HOME", home)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?))
        .spawn()
}

fn send(master: &mut File, input: &[u8]) {
    master.write_all(input).unwrap();
    master.flush().unwrap();
}

fn short_home() -> tempfile::TempDir {
    // The daemon's generation socket is nested under USAGI_HOME. Keep this
    // real-PTY fixture within the Unix sockaddr path-length limit.
    tempfile::Builder::new()
        .prefix("usagi-")
        .tempdir_in("/tmp")
        .expect("short daemon data directory")
}

fn stop_daemon(home: &std::path::Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["daemon", "stop"])
        .env("USAGI_HOME", home)
        .output()
        .expect("usagi daemon stop を起動できる");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn real_pty_entry_resize_quit_and_reattach_restore_terminal() {
    let home = short_home();
    let roots = tempfile::tempdir().unwrap();
    let workspace = roots.path().join("pty-workspace");
    std::fs::create_dir(&workspace).unwrap();

    // 非対話 open も同じ本番合成ルートを通して Recent 用の registry entry を作る。
    let registered = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["open".as_ref(), workspace.as_os_str()])
        .env("USAGI_HOME", home.path())
        .output()
        .expect("workspaceを事前登録できる");
    assert!(registered.status.success());

    let (mut master, slave) = open_pty().unwrap();
    let attributes_before = terminal_attributes(&slave).unwrap();
    let reader_master = master.try_clone().unwrap();
    let reader = thread::spawn(move || read_pty(reader_master));

    let mut child = spawn_hop(home.path(), &slave).expect("PTY上でusagi hopを起動できる");

    // `1` は Welcome の予約 input で最初の Recent を開く。`x` は Workspace 上の
    // non-reserved input で、画面遷移や quit を起こさず次フレームだけを要求する。入力は
    // PTY の line discipline が raw mode へ切り替わる時間を確保してから送る。
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"1");
    thread::sleep(Duration::from_millis(150));
    // Resize while Home is visible. The runtime must invalidate the diff base and repaint the
    // new surface instead of leaving cells from the former 100-column frame behind.
    resize_pty(&master, 80, 20).unwrap();
    thread::sleep(Duration::from_millis(100));
    // The legacy workspace loop observes resize on the next frame boundary. `x` is a no-op key
    // which requests that boundary without changing the visible Home state.
    send(&mut master, b"x");
    thread::sleep(Duration::from_millis(100));
    send(&mut master, b"q");

    let status = match wait_with_timeout(&mut child, Duration::from_secs(5)) {
        Ok(status) => status,
        Err(error) => {
            drop(slave);
            drop(master);
            let captured = reader.join().unwrap();
            panic!(
                "{error}: {}",
                String::from_utf8_lossy(&captured).replace('\u{1b}', "<ESC>")
            );
        }
    };
    let attributes_after = terminal_attributes(&slave).unwrap();

    // One client can leave and immediately attach again to the same OS terminal.  A leaked raw
    // flag, alternate screen, mouse capture, or hidden cursor would make this second entry flaky.
    assert!(status.success());
    assert_eq!(attributes_after.c_iflag, attributes_before.c_iflag);
    assert_eq!(attributes_after.c_oflag, attributes_before.c_oflag);
    assert_eq!(attributes_after.c_cflag, attributes_before.c_cflag);
    assert_eq!(attributes_after.c_lflag, attributes_before.c_lflag);
    assert_eq!(attributes_after.c_cc, attributes_before.c_cc);

    let mut reattached =
        spawn_hop(home.path(), &slave).expect("同じPTYへ再接続してhopを起動できる");
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"q");
    let reattached_status = wait_with_timeout(&mut reattached, Duration::from_secs(5)).unwrap();
    let attributes_reattached = terminal_attributes(&slave).unwrap();

    // slave をすべて閉じると reader が EOF/EIO を受け取れる。
    drop(slave);
    drop(master);
    let captured = reader.join().unwrap();
    let output = String::from_utf8_lossy(&captured);

    assert!(status.success(), "PTY output: {output}");
    assert!(reattached_status.success(), "PTY output: {output}");
    assert!(output.contains("Recent"), "PTY output: {output}");
    assert!(output.contains("pty-workspace"), "PTY output: {output}");
    assert!(output.contains("Sessions"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?1049h"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?1049l"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?25l"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?25h"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?1000h"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?1000l"), "PTY output: {output}");
    assert!(
        output.matches("\u{1b}[?1049h").count() >= 2,
        "both entries must use the alternate screen: {output}"
    );
    assert!(
        output.matches("\u{1b}[?1049l").count() >= 2,
        "both exits must restore the primary screen: {output}"
    );
    assert!(
        output.matches("\u{1b}[2J").count() >= 2,
        "the initial and resized surfaces must both be cleared: {output}"
    );

    assert_eq!(attributes_reattached.c_iflag, attributes_before.c_iflag);
    assert_eq!(attributes_reattached.c_oflag, attributes_before.c_oflag);
    assert_eq!(attributes_reattached.c_cflag, attributes_before.c_cflag);
    assert_eq!(attributes_reattached.c_lflag, attributes_before.c_lflag);
    assert_eq!(attributes_reattached.c_cc, attributes_before.c_cc);
    stop_daemon(home.path());
}
