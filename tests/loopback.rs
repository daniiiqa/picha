//! A whole Quick Share transfer between picha's own sender and receiver, over real TCP.

use std::path::PathBuf;

use picha::error::Error;
use picha::receive::{self, Event};
use picha::send;
use picha::wire::{DeviceKind, Endpoint};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("picha-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs a receiver that answers every offer with `accept`; returns its address,
/// the PIN it saw and the final events.
async fn receiver(dir: PathBuf, accept: bool) -> (std::net::SocketAddr, tokio::task::JoinHandle<(Option<String>, Vec<PathBuf>, Result<(), Error>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let handler = tokio::spawn(async move { receive::handle(stream, &dir, tx).await });
        let (mut pin, mut files) = (None, Vec::new());
        while let Some(event) = rx.recv().await {
            match event {
                Event::Offer(offer, answer) => {
                    assert_eq!(offer.from.name, "S21 de prueba");
                    assert_eq!(offer.from.kind, DeviceKind::Phone);
                    pin = offer.pin;
                    answer.send(accept).unwrap();
                }
                Event::Files(paths) => files = paths,
                _ => {}
            }
        }
        (pin, files, handler.await.unwrap())
    });
    (addr, task)
}

#[tokio::test]
async fn sends_files_end_to_end() {
    let src = temp_dir("src");
    let big: Vec<u8> = (0..1_300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(src.join("video.mp4"), &big).unwrap();
    std::fs::write(src.join("nota.txt"), "qué onda").unwrap();
    std::fs::write(src.join("vacío"), "").unwrap();
    let out = temp_dir("out");

    let (addr, receiver) = receiver(out.clone(), true).await;
    let phone = Endpoint { name: "S21 de prueba".into(), kind: DeviceKind::Phone };
    let files = [src.join("video.mp4"), src.join("nota.txt"), src.join("vacío")];
    let mut sender_pin = String::new();
    send::send(tokio::net::TcpStream::connect(addr).await.unwrap(), &phone, &files, |p| sender_pin = p.to_string(), None).await.unwrap();

    let (pin, mut saved, result) = receiver.await.unwrap();
    result.unwrap();
    assert_eq!(pin.as_deref(), Some(sender_pin.as_str()));
    saved.sort();
    assert_eq!(saved, [out.join("nota.txt"), out.join("vacío"), out.join("video.mp4")]);
    assert_eq!(std::fs::read(out.join("video.mp4")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(out.join("nota.txt")).unwrap(), "qué onda");
}

#[tokio::test]
async fn rejecting_leaves_nothing_behind() {
    let src = temp_dir("src-reject");
    std::fs::write(src.join("foto.jpg"), "jpeg").unwrap();
    let out = temp_dir("out-reject");

    let (addr, receiver) = receiver(out.clone(), false).await;
    let phone = Endpoint { name: "S21 de prueba".into(), kind: DeviceKind::Phone };
    let sent = send::send(tokio::net::TcpStream::connect(addr).await.unwrap(), &phone, &[src.join("foto.jpg")], |_| {}, None).await;
    assert!(matches!(sent, Err(Error::Rejected)), "{sent:?}");

    let (_, saved, result) = receiver.await.unwrap();
    assert!(matches!(result, Err(Error::Rejected)));
    assert!(saved.is_empty());
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
}

#[test]
fn activity_file_lives_while_the_transfer_does() {
    let dir = std::env::temp_dir().join(format!("picha-activity-{}", std::process::id()));
    // SAFETY: this test process sets it before any thread reads it
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
    let mut a = picha::state::Activity::new(true, "Daniel's\nS21");
    a.progress(50, 200);
    let files: Vec<_> = std::fs::read_dir(dir.join("picha")).unwrap().flatten().collect();
    assert_eq!(files.len(), 1);
    assert_eq!(std::fs::read_to_string(files[0].path()).unwrap(), "in 25 Daniel's S21\n");
    drop(a);
    assert_eq!(std::fs::read_dir(dir.join("picha")).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}
