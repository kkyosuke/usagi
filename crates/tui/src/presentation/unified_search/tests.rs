use super::*;
use crate::usecase::application::unified_search::UnavailableUnifiedSearch;
use std::sync::mpsc;

struct Port {
    entered: Sender<()>,
    release: mpsc::Receiver<()>,
}
impl UnifiedSearchPort for Port {
    fn search(&mut self, query: &Query, _: Arc<AtomicBool>) -> Results {
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
        assert_ne!(query.text, "panic");
        Results::default()
    }
    fn read(&mut self, _: &str) -> Result<Vec<String>, String> {
        panic!("unavailable");
    }
}
#[test]
fn unified_search_worker_returns_port_and_contains_read_and_search_panics() {
    let (entered, calls) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (sender, receiver) = mpsc::channel();
    let mut port: Box<dyn UnifiedSearchPort> = Box::new(Port {
        entered,
        release: gate,
    });
    for query in ["normal", "panic"] {
        let job = spawn(
            port,
            (1, 2),
            Request::Search(query_value(query)),
            sender.clone(),
        );
        calls
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(receiver.try_recv().is_err());
        release.send(()).unwrap();
        let completion = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(job);
        assert_eq!(completion.token, (1, 2));
        let Response::Search(result) = completion.response else {
            panic!()
        };
        assert_eq!(result.errors.is_empty(), query == "normal");
        port = completion.port;
    }
    let job = spawn(port, (2, 0), Request::Read("path".into()), sender);
    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    drop(job);
    assert!(matches!(completion.response, Response::Read(Err(_))));
    let mut fallback = UnavailableUnifiedSearch;
    assert_eq!(
        fallback
            .search(&query_value(""), Arc::default())
            .errors
            .len(),
        1
    );
    assert!(fallback.read("missing").is_err());
}

struct CancellablePort {
    finished: Arc<AtomicBool>,
}
impl UnifiedSearchPort for CancellablePort {
    fn search(&mut self, _: &Query, cancelled: Arc<AtomicBool>) -> Results {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !cancelled.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.finished
            .store(cancelled.load(Ordering::Acquire), Ordering::Release);
        Results::default()
    }
    fn read(&mut self, _: &str) -> Result<Vec<String>, String> {
        unreachable!("this fixture only searches")
    }
}

#[test]
fn unified_search_worker_cancels_stale_queries_close_and_drop_then_joins() {
    for target in [Some(Some((1, 1))), Some(None), None] {
        let finished = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let job = spawn(
            Box::new(CancellablePort {
                finished: Arc::clone(&finished),
            }),
            (1, 0),
            Request::Search(query_value("query")),
            sender,
        );
        job.cancel_unless_current(Some((1, 0)));
        assert!(!job.cancelled.load(Ordering::Acquire));
        if let Some(target) = target {
            job.cancel_unless_current(target);
            assert!(job.cancelled.load(Ordering::Acquire));
        }
        drop(job);
        assert!(
            finished.load(Ordering::Acquire),
            "drop must cancel and join the worker"
        );
        assert_eq!(receiver.try_recv().unwrap().token, (1, 0));
    }
}

fn query_value(text: &str) -> Query {
    Query {
        text: text.into(),
        scope: crate::usecase::application::unified_search::Scope::All,
        projects: vec![],
        current: None,
    }
}
