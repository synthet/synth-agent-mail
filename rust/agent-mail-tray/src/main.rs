//! `agent-mail-tray`: the Windows tray icon and the loopback dashboard (ADR 0005).
//!
//! One instance per home (a named mutex); starting it again opens the dashboard. It also runs the
//! consumer that handles received mail (off until `dispatch.enabled`). The menu opens the
//! dashboard, shows whether `agent-maild` is running for this home and can start it, toggles the
//! dispatch kill switch, and quits. Elsewhere, or with `--no-tray`, only the dashboard runs.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;

/// agent-mail tray: dashboard, mailer status and kill switch for one agent-mail home.
#[derive(Parser, Clone)]
#[command(version)]
struct Args {
    /// agent-mail home (identity.json, policy.json, mail.db). Required until the TS cutover (T-10).
    #[arg(long)]
    home: PathBuf,
    /// Dashboard port on loopback. The TS tray also uses 8788; pick another while both run.
    #[arg(long, default_value_t = agent_mail_tray::dashboard::DEFAULT_PORT)]
    port: u16,
    /// agent-maild executable to start from the menu; defaults to the one next to this program.
    #[arg(long)]
    mailer: Option<PathBuf>,
    /// Serve the dashboard without a tray icon (until Ctrl+C).
    #[arg(long)]
    no_tray: bool,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            report(&error.to_string());
            std::process::ExitCode::from(1)
        }
    }
}

type Failure = Box<dyn std::error::Error>;

fn dashboard_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/dashboard")
}

fn run(args: Args) -> Result<(), Failure> {
    #[cfg(windows)]
    let _instance = match win::single_instance(&args.home) {
        Some(guard) => guard,
        None => {
            // Already running for this home: show it instead of starting a second one.
            win::open(&dashboard_url(args.port));
            return Ok(());
        }
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.port);
    let (port, _server) = runtime
        .block_on(agent_mail_tray::dashboard::start(args.home.clone(), listen))
        .map_err(|error| {
            format!(
                "dashboard on 127.0.0.1:{}: {error} (is another tray using the port?)",
                args.port
            )
        })?;
    // The consumer does nothing until policy.json turns dispatch on; the mutex above keeps it to
    // one per home.
    let _consumer = agent_mail_tray::consumer::spawn(
        args.home.clone(),
        agent_mail_tray::consumer::default_runners(),
    );
    #[cfg(windows)]
    if !args.no_tray {
        return win::tray(&args, port);
    }
    eprintln!("agent-mail-tray: dashboard at {}", dashboard_url(port));
    runtime.block_on(tokio::signal::ctrl_c())?;
    Ok(())
}

fn report(message: &str) {
    eprintln!("agent-mail-tray: {message}");
    #[cfg(windows)]
    win::message_box(message);
}

/// The default `agent-maild` path: next to this executable.
fn default_mailer() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "agent-maild.exe"
    } else {
        "agent-maild"
    };
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|dir| dir.join(name))
}

#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MessageBoxW, PostQuitMessage, SetTimer, TranslateMessage,
        MB_ICONERROR, MB_OK, MSG, SW_SHOWNORMAL, WM_TIMER,
    };

    use super::{dashboard_url, default_mailer, Args, Failure};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub struct Instance(HANDLE);

    impl Drop for Instance {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// A mutex per home, so two trays never serve one home. `None` when one already runs.
    pub fn single_instance(home: &Path) -> Option<Instance> {
        let name = wide(&format!(
            "Local\\AgentMailTray-{}",
            mail_core::ipc::home_key(home)
        ));
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
        if handle.is_null() {
            return None;
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Instance(handle))
    }

    pub fn open(url: &str) {
        let (verb, target) = (wide("open"), wide(url));
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    pub fn message_box(message: &str) {
        let (text, title) = (wide(message), wide("Agent Mail"));
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            )
        };
    }

    fn icon() -> Result<Icon, Failure> {
        let decoder = png::Decoder::new(std::io::Cursor::new(
            &include_bytes!("../assets/icon-256.png")[..],
        ));
        let mut reader = decoder.read_info()?;
        let mut pixels = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels)?;
        pixels.truncate(info.buffer_size());
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return Err("icon-256.png must be 8-bit RGBA".into());
        }
        Ok(Icon::from_rgba(pixels, info.width, info.height)?)
    }

    fn mailer_running(home: &Path) -> bool {
        mail_core::ipc::request(home, "status", Duration::from_secs(1))
            .is_ok_and(|reply| reply.starts_with('{'))
    }

    fn dispatch_enabled(home: &Path) -> Option<bool> {
        let policy = mail_core::policy::Policy::load(home).ok()?;
        Some(
            policy
                .dispatch
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        )
    }

    /// Starts `agent-maild` for this home with no console window, logging to `<home>/agent-maild.log`.
    fn start_mailer(mailer: &Path, home: &Path) -> Result<(), Failure> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new(mailer)
            .arg("--home")
            .arg(home)
            .arg("--log")
            .arg(home.join("agent-maild.log"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn()?;
        Ok(())
    }

    pub fn tray(args: &Args, port: u16) -> Result<(), Failure> {
        let home = args.home.clone();
        let mailer: Option<PathBuf> = args
            .mailer
            .clone()
            .or_else(default_mailer)
            .filter(|path| path.exists());
        let open_item = MenuItem::new("Open dashboard", true, None);
        let status_item = MenuItem::new("Mailer: checking...", false, None);
        let start_item = MenuItem::new("Start mailer", false, None);
        let dispatch_item = MenuItem::new("Dispatch: ...", false, None);
        let quit_item = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open_item,
            &PredefinedMenuItem::separator(),
            &status_item,
            &start_item,
            &dispatch_item,
            &PredefinedMenuItem::separator(),
            &quit_item,
        ])?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Agent Mail")
            .with_icon(icon()?)
            .build()?;
        let url = dashboard_url(port);

        let refresh = |first: bool| {
            let running = mailer_running(&home);
            status_item.set_text(if running {
                "Mailer: running"
            } else {
                "Mailer: not running"
            });
            start_item.set_enabled(!running && mailer.is_some());
            let dispatch = dispatch_enabled(&home);
            dispatch_item.set_text(match dispatch {
                Some(true) => "Dispatch: on (click to turn off)",
                Some(false) => "Dispatch: off (click to turn on)",
                None => "Dispatch: no valid policy.json",
            });
            dispatch_item.set_enabled(dispatch.is_some());
            let tip = if running {
                "Agent Mail: mailer running"
            } else {
                "Agent Mail: mailer not running (right-click to start)"
            };
            let _ = tray.set_tooltip(Some(tip));
            if first && !running {
                eprintln!("agent-mail-tray: agent-maild is not running for this home");
            }
        };
        refresh(true);
        unsafe { SetTimer(std::ptr::null_mut(), 0, 5000, None) };

        let mut message: MSG = unsafe { std::mem::zeroed() };
        loop {
            let got = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
            if got <= 0 {
                break;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            if message.message == WM_TIMER {
                refresh(false);
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } = event
                {
                    open(&url);
                }
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    refresh(false);
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == open_item.id() {
                    open(&url);
                } else if event.id == start_item.id() {
                    if let Some(mailer) = &mailer {
                        if let Err(error) = start_mailer(mailer, &home) {
                            message_box(&format!("Could not start agent-maild: {error}"));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                    refresh(false);
                } else if event.id == dispatch_item.id() {
                    if let Some(enabled) = dispatch_enabled(&home) {
                        if let Err(error) =
                            agent_mail_tray::dashboard::set_dispatch_enabled(&home, !enabled)
                        {
                            message_box(&format!("Kill switch: {error}"));
                        }
                    }
                    refresh(false);
                } else if event.id == quit_item.id() {
                    unsafe { PostQuitMessage(0) };
                }
            }
        }
        drop(tray);
        Ok(())
    }
}
