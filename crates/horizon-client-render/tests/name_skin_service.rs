//! A separate process isolates the once-only hook registry from other renderer
//! tests, which may seal an empty registry before this provider is registered.
use horizon_client_render::skins::ElySkins;
use horizon_hooks::{Hooks, NameSkinService};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

struct Provider(NameSkinService);
impl Hooks for Provider {
    fn name_skin_service(&self) -> Option<NameSkinService> {
        Some(self.0)
    }
}

#[test]
fn an_offline_skin_worker_uses_the_registered_service() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "the injected service was never contacted"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        let body = r#"{"SKIN":{"url":"http://example.org/textures/skin.png","metadata":{"model":"slim"}},"CAPE":{"url":"http://other.example.org/cape.png"}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        String::from_utf8(request).unwrap()
    });
    let provider = Box::leak(Box::new(Provider(NameSkinService {
        textures: Box::leak(format!("http://{address}/textures/").into_boxed_str()),
        profiles: Box::leak(format!("http://{address}/profiles/").into_boxed_str()),
        upgrade_http_prefix: Some("http://example.org/textures/"),
    })));
    horizon_hooks::register(provider).unwrap();
    let mut skins = ElySkins::default();
    skins.ask(123, "Example");
    let deadline = Instant::now() + Duration::from_secs(5);
    let found = loop {
        let found = skins.take();
        if !found.is_empty() {
            break found;
        }
        assert!(
            Instant::now() < deadline,
            "the lookup did not produce textures"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let request = server.join().unwrap();
    assert!(
        request.starts_with("GET /textures/Example HTTP/1.1\r\n"),
        "{request}"
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, 123);
    assert_eq!(
        found[0].1.skin.as_deref(),
        Some("https://example.org/textures/skin.png")
    );
    assert_eq!(
        found[0].1.cape.as_deref(),
        Some("http://other.example.org/cape.png")
    );
    assert!(found[0].1.slim && found[0].1.signed);
}
