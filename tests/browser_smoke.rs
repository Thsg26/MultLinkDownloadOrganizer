#![cfg(windows)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use mldo::browser::{BrowserCommand, BrowserEvent};

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        // Somente o processo criado por este teste, caso a asserção falhe.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(
        callback: Option<unsafe extern "system" fn(isize, isize) -> i32>,
        parameter: isize,
    ) -> i32;
    fn GetWindowThreadProcessId(window: isize, process: *mut u32) -> u32;
    fn GetWindowTextW(window: isize, text: *mut u16, max: i32) -> i32;
    fn IsIconic(window: isize) -> i32;
    fn GetWindow(window: isize, command: u32) -> isize;
    fn GetForegroundWindow() -> isize;
    fn SetForegroundWindow(window: isize) -> i32;
    fn AllowSetForegroundWindow(process: u32) -> i32;
    fn PostMessageW(window: isize, message: u32, wparam: usize, lparam: isize) -> i32;
}

fn window_for_process(process_id: u32) -> isize {
    struct Search {
        process_id: u32,
        window: isize,
    }
    unsafe extern "system" fn visit(window: isize, parameter: isize) -> i32 {
        // O ponteiro é válido durante a chamada síncrona de EnumWindows.
        let search = unsafe { &mut *(parameter as *mut Search) };
        let mut process_id = 0;
        unsafe {
            GetWindowThreadProcessId(window, &mut process_id);
        }
        if process_id == search.process_id {
            let mut text = [0u16; 256];
            let count = unsafe { GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32) };
            if count > 0 && String::from_utf16_lossy(&text[..count as usize]).starts_with("MLDO") {
                search.window = window;
                return 0;
            }
        }
        1
    }
    let mut search = Search {
        process_id,
        window: 0,
    };
    unsafe {
        EnumWindows(Some(visit), &mut search as *mut Search as isize);
    }
    search.window
}

#[test]
fn native_edge_browser_has_one_page_foreground_owner_and_shared_session() {
    use std::os::windows::process::CommandExt;
    // Uma instância de teste real do programa, sem listas ou downloads externos.
    let mut main_window = Worker(
        Command::new(env!("CARGO_BIN_EXE_mldo"))
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let owner = loop {
        let window = window_for_process(main_window.0.id());
        if window != 0 {
            break window;
        }
        assert!(
            Instant::now() < deadline,
            "Janela de teste do MLDO não apareceu"
        );
        thread::sleep(Duration::from_millis(30));
    };
    unsafe {
        SetForegroundWindow(owner);
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_server = stop.clone();
    let page_request = Arc::new(Mutex::new(String::new()));
    let page_request_server = page_request.clone();
    let second_requests = Arc::new(AtomicUsize::new(0));
    let second_requests_server = second_requests.clone();
    let server = thread::spawn(move || {
        for stream in listener.incoming() {
            let mut socket = stream.unwrap();
            if stop_server.load(Ordering::SeqCst) {
                break;
            }
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 2048];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = socket.read(&mut chunk).unwrap_or(0);
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8_lossy(&request);
            let response = if request.starts_with("GET /page ")
                || request.starts_with("GET /second ")
            {
                if request.starts_with("GET /second ") {
                    second_requests_server.fetch_add(1, Ordering::SeqCst);
                }
                *page_request_server.lock().unwrap() = request.into_owned();
                let html = r#"<!doctype html><html><body><h1>MLDO — teste local</h1><p>Sem captcha real.</p><a id="go" href="/file.bin">Download</a><script>setTimeout(()=>document.getElementById('go').click(), 400)</script></body></html>"#;
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nSet-Cookie: mldo_session=local-test; Path=/; HttpOnly\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len())
            } else if request.starts_with("GET /file.bin ") {
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=sample.bin\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            };
            let _ = socket.write_all(response.as_bytes());
        }
    });
    let mut worker = Worker(
        Command::new(env!("CARGO_BIN_EXE_mldo"))
            .arg("--browser-worker")
            .arg("--browser-owner")
            .arg(owner.to_string())
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let foreground_granted = unsafe { AllowSetForegroundWindow(worker.0.id()) != 0 };
    let stdout = worker.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Ok(event) = serde_json::from_str::<BrowserEvent>(&line) {
                let _ = tx.send(event);
            }
        }
    });
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(30))
            .expect("Navegador não iniciou; WebView2 necessário"),
        BrowserEvent::Ready
    ));
    let stdin = worker.0.stdin.as_mut().unwrap();
    serde_json::to_writer(
        &mut *stdin,
        &BrowserCommand::Open {
            id: 7,
            url: format!("{base}/page"),
            label: "Teste local".into(),
            show: true,
        },
    )
    .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    serde_json::to_writer(
        &mut *stdin,
        &BrowserCommand::Open {
            id: 8,
            url: format!("{base}/second"),
            label: "Não abrir em paralelo".into(),
            show: true,
        },
    )
    .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    let mut refused_second = false;
    let access = loop {
        match rx
            .recv_timeout(Duration::from_secs(30))
            .expect("Download local não foi interceptado")
        {
            BrowserEvent::Captured { id, access } => {
                assert_eq!(id, 7);
                break access;
            }
            BrowserEvent::Error { message, .. } if message.contains("uma página") => {
                refused_second = true;
            }
            BrowserEvent::Error { message, .. } => panic!("Falha no navegador local: {message}"),
            _ => {}
        }
    };
    assert_eq!(access.url, format!("{base}/file.bin"));
    assert!(access.cookie.contains("mldo_session=local-test"));
    assert_eq!(access.filename.as_deref(), Some("sample.bin"));
    assert!(
        refused_second,
        "A segunda página deve aguardar a captura da primeira"
    );
    assert_eq!(second_requests.load(Ordering::SeqCst), 0);
    let browser_window = window_for_process(worker.0.id());
    assert_ne!(browser_window, 0);
    assert_eq!(
        unsafe { IsIconic(browser_window) },
        0,
        "A página de autenticação não pode ficar minimizada"
    );
    assert_eq!(
        unsafe { GetWindow(browser_window, 4) },
        owner,
        "Janela auxiliar deve pertencer ao MLDO e ficar acima dele"
    );
    if foreground_granted {
        assert_eq!(
            unsafe { GetForegroundWindow() },
            browser_window,
            "Janela deve receber foreground quando o Windows autoriza"
        );
    }
    let request = page_request.lock().unwrap();
    let actual_ua = request
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
                .map(|(_, value)| value.trim())
        })
        .unwrap();
    assert!(actual_ua.contains("Edg/") || actual_ua.contains("EdgWebView2/"));
    assert!(actual_ua.contains("Chrome/"));
    assert!(!actual_ua.contains("Macintosh"));
    assert!(!actual_ua.contains("Version/18.6"));
    assert_eq!(access.user_agent.as_deref(), Some(actual_ua));
    drop(request);
    let stdin = worker.0.stdin.as_mut().unwrap();
    serde_json::to_writer(
        &mut *stdin,
        &BrowserCommand::Open {
            id: 8,
            url: format!("{base}/second"),
            label: "Próximo link".into(),
            show: true,
        },
    )
    .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    loop {
        match rx.recv_timeout(Duration::from_secs(20)).unwrap() {
            BrowserEvent::Captured { id: 8, .. } => break,
            BrowserEvent::Error { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    assert_eq!(second_requests.load(Ordering::SeqCst), 1);
    let stdin = worker.0.stdin.as_mut().unwrap();
    serde_json::to_writer(&mut *stdin, &BrowserCommand::Shutdown).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    loop {
        if matches!(
            rx.recv_timeout(Duration::from_secs(15)).unwrap(),
            BrowserEvent::Closed
        ) {
            break;
        }
    }
    assert!(worker.0.wait().unwrap().success());
    unsafe {
        PostMessageW(owner, 0x0010, 0, 0);
    } // WM_CLOSE, somente a janela criada pelo teste
    assert!(main_window.0.wait().unwrap().success());
    stop.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(address);
    server.join().unwrap();
}
