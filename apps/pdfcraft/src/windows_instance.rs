//! Windows Explorer/Open With launches join the existing window using a document-only named
//! pipe. No TCP listener, control commands, PID state or new PDF-loading path.
//!
//! Interprocess owns the unsafe Win32 calls. An OS-held file lock elects the primary and permits
//! recreating a pipe whose client disconnected before accept. The lock file is never deleted:
//! its existence/content means nothing; Windows releases the lock on process exit/crash.

use crate::open_protocol::{self as protocol, IO_TIMEOUT, POLL_INTERVAL};
use interprocess::os::windows::{
    named_pipe::{PipeListener, PipeListenerOptions, PipeStream, pipe_mode},
    security_descriptor::SecurityDescriptor,
};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::OwnedHandle};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const QUEUE_LIMIT: usize = 16;
const MAX_CLIENTS: usize = 8;
const ACCEPTED: u8 = 1;
type Listener = PipeListener<pipe_mode::Bytes, pipe_mode::Bytes>;
type Stream = PipeStream<pipe_mode::Bytes, pipe_mode::Bytes>;
type Wake = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Inbox {
    batches: Mutex<VecDeque<Vec<String>>>,
    wake: Mutex<Option<Wake>>,
}

impl Inbox {
    fn push(&self, paths: Vec<String>) -> io::Result<()> {
        {
            let mut batches = self.batches.lock().map_err(|_| io::Error::other("document inbox unavailable"))?;
            if batches.len() >= QUEUE_LIMIT {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "document inbox full; retry after PdfCraft finishes opening files"));
            }
            batches.push_back(paths);
        }
        if let Some(wake) = self.wake.lock().ok().and_then(|w| w.clone()) {
            wake();
        }
        Ok(())
    }
}

/// Lifetime guard: dropping this stops the receiver and releases the pipe name.
pub struct WindowsInstance {
    _ownership: File,
    inbox: Arc<Inbox>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

/// User profile separates endpoint names; the pipe ACL enforces access, not this hash.
/// Use a stable hash rather than DefaultHasher (which can change across Rust releases).
pub fn endpoint(profile: &Path) -> String {
    let mut digest = Sha256::new();
    for unit in profile.as_os_str().encode_wide() {
        digest.update(unit.to_le_bytes());
    }
    let hash: String = digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    format!(r"\\.\pipe\ai.storyteller.pdfcraft.open-v1-{hash}")
}

impl WindowsInstance {
    /// Some means we own the listener; None means the running process acknowledged the batch.
    /// Options are deliberately absent. Relative paths must be resolved by the launching process.
    pub fn start(name: &str, lock_path: &Path, paths: &[String]) -> io::Result<Option<Self>> {
        let bytes = protocol::encode(paths)?;
        // No DELETE sharing: another launch must not unlink/recreate the locked inode. Never
        // truncate or interpret its contents. The lock, not a stale on-disk marker, elects us.
        let ownership = OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(3).open(lock_path)?;
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            match ownership.try_lock() {
                Ok(()) => {
                    let listener = loop {
                        match listener(name) {
                            Ok(listener) => break listener,
                            // A crashed server's last client may briefly retain a pipe handle.
                            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                            Err(e) => return Err(e),
                        }
                    };
                    return Self::serve(listener, name.to_owned(), ownership).map(Some);
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
            match connect(name) {
                Ok(mut pipe) => {
                    // Never retry after sending: a lost acknowledgement may still mean the batch
                    // was queued. Retrying automatically could open the same documents twice.
                    let io_deadline = Instant::now() + IO_TIMEOUT;
                    protocol::write_frame(&mut pipe, &bytes, io_deadline)?;
                    let mut reply = [0];
                    protocol::read_exact(&mut pipe, &mut reply, io_deadline)?;
                    if reply != [ACCEPTED] {
                        return Err(io::Error::other("the existing PdfCraft window could not queue these files; retry when it responds"));
                    }
                    // Confirms the acknowledgement was consumed before the server closes its end.
                    // Failure here cannot undo acceptance; the server's bounded wait cleans up.
                    let _ = protocol::write_all(&mut pipe, &[ACCEPTED], io_deadline);
                    return Ok(None);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound || matches!(e.raw_os_error(), Some(231..=233)) => {
                    // Busy/startup race or crashed server: re-elect, never assume PID/file state.
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "the existing PdfCraft window is busy; retry after it responds"));
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn serve(listener: Listener, name: String, ownership: File) -> io::Result<Self> {
        let inbox = Arc::new(Inbox::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (queue, stopping) = (inbox.clone(), stop.clone());
        let worker = thread::Builder::new().name("pdfcraft-document-open".into()).spawn(move || {
            let mut current = Some(listener);
            let mut clients: Vec<JoinHandle<()>> = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                let mut index = 0;
                while index < clients.len() {
                    if clients.get(index).is_some_and(|client| client.is_finished()) {
                        if clients.swap_remove(index).join().is_err() {
                            eprintln!("pdfcraft: document client worker failed");
                        }
                    } else {
                        index += 1;
                    }
                }
                if clients.len() >= MAX_CLIENTS {
                    thread::sleep(POLL_INTERVAL);
                    continue;
                }
                if current.is_none() {
                    match self::listener(&name) {
                        Ok(replacement) => current = Some(replacement),
                        Err(_) => {
                            // Keep ownership while recovering. A transient failure must not leave
                            // a live GUI permanently owning a lock with no document listener.
                            thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                    }
                }
                let Some(listener) = current.as_ref() else { continue };
                match listener.accept() {
                    Ok(stream) => {
                        let inbox = queue.clone();
                        match thread::Builder::new().name("pdfcraft-open-client".into()).spawn(move || {
                            if let Err(e) = handle_client(stream, &inbox) {
                                eprintln!("pdfcraft: document forwarding: {e}");
                            }
                        }) {
                            Ok(client) => clients.push(client),
                            Err(e) => eprintln!("pdfcraft: could not receive document request: {e}"),
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(536) => thread::sleep(POLL_INTERVAL),
                    Err(e) => {
                        // Includes ERROR_NO_DATA: a client disconnected before accept. The
                        // library cannot reset that instance; safely recreate under our lock.
                        eprintln!("pdfcraft: restarting document listener: {e}");
                        current = None;
                        thread::sleep(POLL_INTERVAL);
                    }
                }
            }
            // Each client has a bounded deadline, so shutdown cannot wait forever.
            for client in clients {
                let _ = client.join();
            }
        })?;
        Ok(Self { _ownership: ownership, inbox, stop, worker: Some(worker) })
    }

    /// Attach the existing OS-event poller. Requests received during GUI startup stay queued.
    pub fn connect(&self, wake: impl Fn() + Send + Sync + 'static) -> Box<dyn FnMut() -> Vec<Vec<String>>> {
        if let Ok(mut slot) = self.inbox.wake.lock() {
            *slot = Some(Arc::new(wake));
        }
        let inbox = self.inbox.clone();
        Box::new(move || inbox.batches.lock().map(|mut batches| batches.drain(..).collect()).unwrap_or_default())
    }
}

impl Drop for WindowsInstance {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn listener(name: &str) -> io::Result<Listener> {
    // OW is OWNER RIGHTS: only the object's owner is granted access, no Everyone/anonymous ACE.
    // A protected DACL prevents inherited permissions; remote clients and inheritance are off.
    // https://learn.microsoft.com/windows/win32/secauthz/sid-strings
    let security = SecurityDescriptor::deserialize(widestring::u16cstr!("D:P(A;;GA;;;OW)"))?;
    PipeListenerOptions::new()
        .path(name)
        .nonblocking(true)
        .accept_remote(false)
        .inheritable(false)
        .security_descriptor(Some(security))
        .create_duplex::<pipe_mode::Bytes>()
}

/// Normalize Windows PIPE_NOWAIT into portable nonblocking I/O. Keep Win32 error codes out
/// of the shared framing/validation helper; a future Unix socket needs no Windows branches.
struct Pipe(File);

impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self.0.read(bytes) {
            // An empty PIPE_NOWAIT read can surface as zero bytes through std's Windows
            // file API, not only ERROR_NO_DATA. It must not end a frame or ACK exchange.
            // A real disconnect may also return zero: the protocol deadline bounds that
            // wait. Preserve Read's empty-buffer behavior.
            Ok(0) if !bytes.is_empty() => Err(io::ErrorKind::WouldBlock.into()),
            Err(error) if error.raw_os_error() == Some(232) => Err(io::ErrorKind::WouldBlock.into()),
            result => result,
        }
    }
}

impl Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    // Never call FlushFileBuffers: it can block indefinitely on a non-reading client.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn connect(name: &str) -> io::Result<Pipe> {
    // CreateFile returns immediately with PIPE_BUSY; unlike connect_by_path, this does not wait
    // indefinitely. SECURITY_ANONYMOUS (0) prevents a pipe server impersonating the launcher;
    // std adds SECURITY_SQOS_PRESENT. No unsafe raw-handle conversion is needed.
    let file = OpenOptions::new().read(true).write(true).security_qos_flags(0).open(name)?;
    let stream = Stream::try_from(OwnedHandle::from(file)).map_err(|e| io::Error::other(e.to_string()))?;
    stream.set_nonblocking(true)?;
    into_file(stream)
}

fn into_file(stream: Stream) -> io::Result<Pipe> {
    // Normalize std's nonblocking reads in Pipe, and avoid interprocess's automatic background
    // flush on drop: a non-reading client must not retain handles/threads forever.
    OwnedHandle::try_from(stream).map(|handle| Pipe(File::from(handle))).map_err(|_| io::Error::other("unexpected shared pipe handle"))
}

fn handle_client(stream: Stream, inbox: &Inbox) -> io::Result<()> {
    let mut pipe = into_file(stream)?;
    let deadline = Instant::now() + IO_TIMEOUT;
    let accepted = protocol::read_frame(&mut pipe, deadline).and_then(|bytes| protocol::decode(&bytes)).and_then(|paths| inbox.push(paths));
    let reply = if accepted.is_ok() { ACCEPTED } else { 0 };
    protocol::write_all(&mut pipe, &[reply], deadline)?;
    // Wait only until receipt/deadline, never FlushFileBuffers (which can block indefinitely).
    let mut receipt = [0];
    let _ = protocol::read_exact(&mut pipe, &mut receipt, deadline);
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn name() -> String {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        format!(r"\\.\pipe\pdfcraft-open-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn windows_pipe_idle_reads_retry_until_data_arrives() {
        let name = name();
        let listener = listener(&name).unwrap();
        let mut client = connect(&name).unwrap();
        let mut server = into_file(listener.accept().unwrap()).unwrap();
        let mut byte = [0];
        assert_eq!(client.read(&mut []).unwrap(), 0);
        assert_eq!(client.read(&mut byte).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(server.read(&mut byte).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        let deadline = Instant::now() + IO_TIMEOUT;
        protocol::write_all(&mut server, &[7], deadline).unwrap();
        protocol::read_exact(&mut client, &mut byte, deadline).unwrap();
        assert_eq!(byte, [7]);
    }

    #[test]
    fn windows_pipe_forwards_multiple_batches_and_releases_ownership() {
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let owner = WindowsInstance::start(&name, &lock_path, &[]).unwrap().unwrap();
        let paths = protocol::absolute_paths(&["a with spaces.pdf".into(), "日本語.pdf".into()]).unwrap();
        // Delivery before the GUI connects must not lose the launch.
        assert!(WindowsInstance::start(&name, &lock_path, &paths).unwrap().is_none());
        let mut poll = owner.connect(|| {});
        assert_eq!(poll(), vec![paths.clone()]);
        assert!(WindowsInstance::start(&name, &lock_path, &paths).unwrap().is_none());
        assert_eq!(poll(), vec![paths]);
        assert!(poll().is_empty());
        drop(owner);
        assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_some());
    }

    #[test]
    fn windows_pipe_rejects_bad_requests_without_poisoning_listener() {
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let owner = WindowsInstance::start(&name, &lock_path, &[]).unwrap().unwrap();
        let mut pipe = connect(&name).unwrap();
        let deadline = Instant::now() + IO_TIMEOUT;
        protocol::write_frame(&mut pipe, br#"{"version":1,"paths":[],"command":"quit"}"#, deadline).unwrap();
        let mut reply = [1];
        protocol::read_exact(&mut pipe, &mut reply, deadline).unwrap();
        assert_eq!(reply, [0]);
        drop(pipe);
        assert!(owner.connect(|| {})().is_empty());
        assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_none());
    }

    #[test]
    fn windows_pipe_simultaneous_launches_elect_one_owner() {
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let (send, receive) = std::sync::mpsc::channel();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (name, lock_path, barrier, send) = (name.clone(), lock_path.clone(), barrier.clone(), send.clone());
                thread::spawn(move || {
                    barrier.wait();
                    send.send(WindowsInstance::start(&name, &lock_path, &[])).unwrap();
                })
            })
            .collect();
        let results: Vec<_> = (0..4).map(|_| receive.recv_timeout(STARTUP_TIMEOUT + IO_TIMEOUT).unwrap().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_some()).count(), 1);
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn windows_pipe_rejects_oversized_frame_before_payload() {
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let owner = WindowsInstance::start(&name, &lock_path, &[]).unwrap().unwrap();
        let mut pipe = connect(&name).unwrap();
        let deadline = Instant::now() + IO_TIMEOUT;
        protocol::write_all(&mut pipe, &u32::MAX.to_le_bytes(), deadline).unwrap();
        let mut reply = [1];
        protocol::read_exact(&mut pipe, &mut reply, deadline).unwrap();
        assert_eq!(reply, [0]);
        drop(pipe);
        assert!(owner.connect(|| {})().is_empty());
        assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_none());
    }

    #[test]
    fn windows_pipe_accepted_batch_survives_missing_acknowledgement_receipt() {
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let owner = WindowsInstance::start(&name, &lock_path, &[]).unwrap().unwrap();
        let mut poll = owner.connect(|| {});
        let paths = protocol::absolute_paths(&["accepted.pdf".into()]).unwrap();
        for consume_ack in [false, true] {
            let mut pipe = connect(&name).unwrap();
            let deadline = Instant::now() + IO_TIMEOUT;
            protocol::write_frame(&mut pipe, &protocol::encode(&paths).unwrap(), deadline).unwrap();
            // Observe acceptance before losing either the ACK or its receipt. A disconnect
            // after enqueue cannot roll back the batch, and must not enqueue it twice.
            while owner.inbox.batches.lock().unwrap().is_empty() && Instant::now() < deadline {
                thread::sleep(POLL_INTERVAL);
            }
            if consume_ack {
                let mut reply = [0];
                protocol::read_exact(&mut pipe, &mut reply, deadline).unwrap();
                assert_eq!(reply, [ACCEPTED]);
            }
            drop(pipe);
            assert_eq!(poll(), vec![paths.clone()]);
            assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_none());
            assert_eq!(poll(), vec![Vec::<String>::new()]);
            assert!(poll().is_empty());
        }
    }

    #[test]
    fn windows_pipe_shutdown_with_stalled_client_is_bounded() {
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let owner = WindowsInstance::start(&name, &lock_path, &[]).unwrap().unwrap();
        let stalled = connect(&name).unwrap();
        // A healthy request proves the worker is accepting clients before shutdown.
        assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_none());
        let started = Instant::now();
        drop(owner);
        assert!(started.elapsed() < IO_TIMEOUT + Duration::from_secs(3));
        drop(stalled);
        assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_some());
    }

    #[test]
    fn windows_inbox_is_bounded() {
        let inbox = Inbox::default();
        for _ in 0..QUEUE_LIMIT {
            inbox.push(vec![]).unwrap();
        }
        assert_eq!(inbox.push(vec![]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn windows_pipe_recovers_a_client_that_disconnected_before_accept() {
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let ownership = OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(3).open(&lock_path).unwrap();
        ownership.try_lock().unwrap();
        let pending = listener(&name).unwrap();
        drop(connect(&name).unwrap());
        let owner = WindowsInstance::serve(pending, name.clone(), ownership).unwrap();
        assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_none());
        assert_eq!(owner.connect(|| {})().len(), 1);
    }

    #[test]
    fn windows_pipe_times_out_stalled_clients_and_accepts_the_next_launch() {
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let owner = WindowsInstance::start(&name, &lock_path, &[]).unwrap().unwrap();
        let stalled = connect(&name).unwrap();
        // An incomplete sender must not serialize all other launches behind its timeout.
        assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_none());
        thread::sleep(IO_TIMEOUT + Duration::from_millis(200));
        assert_eq!(owner.connect(|| {})().len(), 1);
        drop(stalled);
    }

    // Invoked in a child test process by the crash-recovery test, not a product command/option.
    #[test]
    fn windows_crash_child() {
        let Ok(name) = std::env::var("PDFCRAFT_TEST_PIPE") else { return };
        let lock_path = std::path::PathBuf::from(std::env::var_os("PDFCRAFT_TEST_LOCK").unwrap());
        let ready = std::env::var_os("PDFCRAFT_TEST_READY").unwrap();
        let _owner = WindowsInstance::start(&name, &lock_path, &[]).unwrap().unwrap();
        std::fs::write(ready, "ready").unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn windows_pipe_recovers_after_primary_process_is_killed() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let name = name();
        let lock_path = std::env::temp_dir().join(format!("{}.lock", name.rsplit('\\').next().unwrap()));
        let ready = lock_path.with_extension("ready");
        let _ = std::fs::remove_file(&ready);
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "windows_instance::tests::windows_crash_child", "--nocapture"])
                .env("PDFCRAFT_TEST_PIPE", &name)
                .env("PDFCRAFT_TEST_LOCK", &lock_path)
                .env("PDFCRAFT_TEST_READY", &ready)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }
        assert!(ready.exists(), "child did not create its listener");
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(lock_path.exists(), "stale lock file remains after abrupt exit");
        assert!(WindowsInstance::start(&name, &lock_path, &[]).unwrap().is_some());
        let _ = std::fs::remove_file(ready);
    }
}
