//! The mailer's side of `mail_core::ipc`: answers `poke` and `status`. Creating the channel fails
//! when another mailer already owns it, which keeps two mailers off one home.

use std::path::PathBuf;
use std::sync::Arc;

use mail_core::store::Store;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

pub struct Status {
    pub home: PathBuf,
    pub fingerprint: String,
    pub port: u16,
    pub outbox: Arc<crate::outbox::Outbox>,
    /// How many new messages the receiver has stored since start; `wait` sleeps on it.
    pub received: tokio::sync::watch::Receiver<u64>,
}

async fn answer(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    notify: Arc<Notify>,
    status: Arc<Status>,
) {
    let (read, mut write) = tokio::io::split(stream);
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(256));
    if tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reader.read_line(&mut line),
    )
    .await
    .is_err()
    {
        return;
    }
    let reply = match line.trim() {
        "poke" => {
            notify.notify_one();
            "ok".to_owned()
        }
        "status" => {
            let home = status.home.clone();
            let counts = tokio::task::spawn_blocking(move || {
                Store::open(&home).and_then(|store| store.counts())
            })
            .await;
            match counts {
                Ok(Ok(counts)) => json!({ "fingerprint": status.fingerprint, "port": status.port, "pending": counts.pending,
                    "unread": counts.unread, "sent": counts.sent })
                .to_string(),
                _ => json!({ "fingerprint": status.fingerprint, "port": status.port }).to_string(),
            }
        }
        request if request.starts_with("wait ") => {
            wait_reply(&request["wait ".len()..], status.received.clone()).await
        }
        request if request.starts_with("envelope ") => {
            let fp = request["envelope ".len()..].trim();
            if !mail_core::js::is_fingerprint(fp) {
                "error".to_owned()
            } else {
                match status.outbox.envelope_for(fp).await {
                    Ok(mail_core::envelope::Envelope::V1_3) => {
                        mail_core::envelope::ENVELOPE_V1_3.to_owned()
                    }
                    Ok(mail_core::envelope::Envelope::V1_2) => {
                        mail_core::envelope::ENVELOPE_V1_2.to_owned()
                    }
                    Ok(mail_core::envelope::Envelope::V1_1) => {
                        mail_core::envelope::ENVELOPE_V1_1.to_owned()
                    }
                    Ok(mail_core::envelope::Envelope::V1_0) => "agent-mail/1".to_owned(),
                    Err(_) => "error".to_owned(),
                }
            }
        }
        _ => "error".to_owned(),
    };
    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
    let _ = write.flush().await;
}

/// `wait <seen|-> <ms>`: answers the current count at once unless it still equals `seen`, otherwise
/// when the next message is stored or `ms` (capped) passes.
async fn wait_reply(args: &str, mut received: tokio::sync::watch::Receiver<u64>) -> String {
    let mut parts = args.split_whitespace();
    let (Some(seen), Some(ms), None) = (parts.next(), parts.next(), parts.next()) else {
        return "error".to_owned();
    };
    let Ok(ms) = ms.parse::<u64>() else {
        return "error".to_owned();
    };
    let seen = match seen {
        "-" => None,
        count => match count.parse::<u64>() {
            Ok(count) => Some(count),
            Err(_) => return "error".to_owned(),
        },
    };
    if seen == Some(*received.borrow_and_update()) {
        let wait = std::time::Duration::from_millis(ms).min(mail_core::ipc::MAX_WAIT);
        let deadline = tokio::time::Instant::now() + wait;
        // A dropped sender makes changed() fail at once; sleep out the wait so no caller spins.
        if tokio::time::timeout_at(deadline, received.changed())
            .await
            .is_ok_and(|changed| changed.is_err())
        {
            tokio::time::sleep_until(deadline).await;
        }
    }
    let count = *received.borrow();
    count.to_string()
}

#[cfg(windows)]
pub fn serve(
    notify: Arc<Notify>,
    status: Arc<Status>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = mail_core::ipc::endpoint(&status.home);
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("another agent-maild already serves this home ({error})"),
            )
        })?;
    Ok(tokio::spawn(async move {
        loop {
            if server.connect().await.is_err() {
                continue;
            }
            let connected = server;
            server = match ServerOptions::new().create(&name) {
                Ok(next) => next,
                Err(error) => {
                    crate::log!("agent-maild: local channel stopped: {error}");
                    return;
                }
            };
            tokio::spawn(answer(connected, notify.clone(), status.clone()));
        }
    }))
}

#[cfg(unix)]
pub fn serve(
    notify: Arc<Notify>,
    status: Arc<Status>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let path = mail_core::ipc::endpoint(&status.home);
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "another agent-maild already serves this home",
        ));
    }
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    Ok(tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(answer(stream, notify.clone(), status.clone()));
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn a_wait_whose_counter_is_gone_sleeps_out_its_time_instead_of_answering_at_once() {
        let (sender, received) = tokio::sync::watch::channel(0u64);
        drop(sender);
        let started = Instant::now();
        assert_eq!(wait_reply("0 400", received).await, "0");
        assert!(
            started.elapsed() >= Duration::from_millis(380),
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_new_message_still_answers_at_once() {
        let (sender, received) = tokio::sync::watch::channel(0u64);
        let started = Instant::now();
        let waiter = tokio::spawn(wait_reply("0 5000", received));
        tokio::time::sleep(Duration::from_millis(50)).await;
        sender.send_modify(|count| *count += 1);
        assert_eq!(waiter.await.unwrap(), "1");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
