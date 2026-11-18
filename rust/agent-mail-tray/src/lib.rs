//! The agent-mail tray (ADR 0005): a loopback dashboard over `mail.db`, the mailer's status, and
//! the dispatch kill switch. The tray icon and menu live in `main.rs`; this library is what the
//! tests drive. The consumer that runs headless agents arrives with T-6.

pub mod dashboard;
