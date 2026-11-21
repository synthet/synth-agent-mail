//! `agent-mail service|tray install|uninstall|status`: per-user logon tasks (ADR 0005).
//!
//! `schtasks /SC ONLOGON` needs admin rights because it fires for any user. A task defined in XML
//! with a logon trigger for the current user only, running as that user at least privilege, does
//! not, so this writes that XML and registers it with `schtasks /Create /XML`. The task runs as the
//! owner, never as SYSTEM, so keys, policy and the agent harness logins stay the owner's.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

pub enum Program {
    /// `agent-maild`: always on, receives and delivers, on this TCP port.
    Mailer { port: u16 },
    /// `agent-mail-tray`: dashboard and kill switch.
    Tray { port: u16 },
}

impl Program {
    fn stem(&self) -> &'static str {
        match self {
            Program::Mailer { .. } => "agent-maild",
            Program::Tray { .. } => "agent-mail-tray",
        }
    }

    /// `AgentMail\<program>-<home key>`: one task per program and home.
    pub fn task_name(&self, home: &Path) -> String {
        format!(
            r"AgentMail\{}-{}",
            self.stem(),
            mail_core::ipc::home_key(home)
        )
    }

    /// The executable next to this one, unless `--exe` names another.
    pub fn default_exe(&self) -> Option<PathBuf> {
        let name = if cfg!(windows) {
            format!("{}.exe", self.stem())
        } else {
            self.stem().to_owned()
        };
        std::env::current_exe()
            .ok()?
            .parent()
            .map(|dir| dir.join(name))
    }

    pub fn arguments(&self, home: &Path) -> Result<String, String> {
        let quoted_home = quoted(home)?;
        Ok(match self {
            Program::Mailer { port } => format!(
                "--home {quoted_home} --port {port} --log {}",
                quoted(&home.join("agent-maild.log"))?
            ),
            Program::Tray { port } => format!("--home {quoted_home} --port {port}"),
        })
    }
}

fn quoted(path: &Path) -> Result<String, String> {
    let text = path.to_string_lossy();
    if text.contains('"') {
        return Err(format!("path contains a quote: {text}"));
    }
    Ok(format!("\"{text}\""))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `DOMAIN\user` of the person running this.
pub fn current_user() -> Result<String, String> {
    let user = std::env::var("USERNAME").map_err(|_| "USERNAME is not set")?;
    Ok(match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    })
}

/// The Task Scheduler XML: a logon trigger for `user`, run as `user` at least privilege, never
/// stopped for battery or time limits, restarted three times a minute apart if it fails.
///
/// `keep_alive` adds a trigger that fires every minute from registration on. With `IgnoreNew` it
/// does nothing while the program runs and starts it again within a minute after any exit. The
/// mailer needs it: it runs under `conhost --headless`, which reports exit code 0 whatever the
/// mailer returned, so restart-on-failure never fires for it.
pub fn task_xml(
    user: &str,
    exe: &Path,
    arguments: &str,
    description: &str,
    keep_alive: bool,
) -> String {
    let (user, exe, arguments, description) = (
        xml_escape(user),
        xml_escape(&exe.to_string_lossy()),
        xml_escape(arguments),
        xml_escape(description),
    );
    let keep_alive = if keep_alive {
        "\n    <TimeTrigger><Repetition><Interval>PT1M</Interval><StopAtDurationEnd>false</StopAtDurationEnd></Repetition><StartBoundary>2000-01-01T00:00:00</StartBoundary><Enabled>true</Enabled></TimeTrigger>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>{description}</Description></RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger>{keep_alive}
  </Triggers>
  <Principals>
    <Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>{exe}</Command><Arguments>{arguments}</Arguments></Exec>
  </Actions>
</Task>
"#
    )
}

fn schtasks(args: &[&str]) -> Result<String, String> {
    let output = Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|error| format!("schtasks: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned()
        + String::from_utf8_lossy(&output.stderr).trim();
    if output.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// The one-time firewall rules the mailer needs, for an elevated prompt. Printed, never run.
pub fn firewall_commands(exe: &Path, port: u16) -> Vec<String> {
    let program = exe.to_string_lossy();
    vec![
        format!(
            r#"netsh advfirewall firewall add rule name="agent-maild (TCP {port})" dir=in action=allow program="{program}" protocol=TCP localport={port} profile=private"#
        ),
        format!(
            r#"netsh advfirewall firewall add rule name="agent-maild (UDP 47831)" dir=in action=allow program="{program}" protocol=UDP localport=47831 profile=private"#
        ),
    ]
}

pub fn install(
    program: &Program,
    home: &Path,
    exe: Option<PathBuf>,
    start: bool,
) -> Result<Value, String> {
    if !cfg!(windows) {
        return Err(
            "logon tasks are Windows-only; use your init system (systemd user unit, launchd agent)"
                .into(),
        );
    }
    let home = std::path::absolute(home).map_err(|error| error.to_string())?;
    if !home.join("identity.json").exists() {
        return Err(format!(
            "{} has no identity.json; run `agent-mail --home ... init` first",
            home.display()
        ));
    }
    let exe = exe
        .or_else(|| program.default_exe())
        .ok_or("can't find the executable; pass --exe")?;
    let exe = std::path::absolute(&exe).map_err(|error| error.to_string())?;
    if !exe.exists() {
        return Err(format!(
            "{} does not exist; build it or pass --exe",
            exe.display()
        ));
    }
    let name = program.task_name(&home);
    let arguments = program.arguments(&home)?;
    // The mailer is a console program. `conhost --headless` runs it without a window, as the TS
    // AgentMailListener task does; the tray is a GUI program and starts directly.
    let (command, command_arguments) = match program {
        Program::Mailer { .. } => (
            conhost(),
            format!("--headless {} {arguments}", quoted(&exe)?),
        ),
        Program::Tray { .. } => (exe.clone(), arguments.clone()),
    };
    let xml = task_xml(
        &current_user()?,
        &command,
        &command_arguments,
        &format!("agent-mail {} for {}", program.stem(), home.display()),
        matches!(program, Program::Mailer { .. }),
    );
    let file = std::env::temp_dir().join(format!("agent-mail-task-{}.xml", std::process::id()));
    // Task Scheduler reads UTF-16 with a byte-order mark.
    let bytes: Vec<u8> = std::iter::once(0xFEFFu16)
        .chain(xml.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    std::fs::write(&file, bytes).map_err(|error| error.to_string())?;
    let created = schtasks(&[
        "/Create",
        "/TN",
        &name,
        "/XML",
        &file.to_string_lossy(),
        "/F",
    ]);
    let _ = std::fs::remove_file(&file);
    created?;
    if start {
        schtasks(&["/Run", "/TN", &name])?;
    }
    let mut result =
        json!({ "installed": name, "exe": exe, "arguments": arguments, "started": start });
    if let Program::Mailer { port } = program {
        result["firewall"] = json!({
            "note": "Run these once in an elevated prompt so other PCs can reach this mailer; they are not run for you.",
            "commands": firewall_commands(&exe, *port),
        });
    }
    Ok(result)
}

/// `%SystemRoot%\System32\conhost.exe`.
fn conhost() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("conhost.exe")
}

pub fn uninstall(program: &Program, home: &Path) -> Result<Value, String> {
    let name = program.task_name(&std::path::absolute(home).map_err(|error| error.to_string())?);
    schtasks(&["/Delete", "/TN", &name, "/F"])?;
    Ok(json!({ "removed": name }))
}

pub fn status(program: &Program, home: &Path) -> Result<Value, String> {
    let name = program.task_name(&std::path::absolute(home).map_err(|error| error.to_string())?);
    Ok(match schtasks(&["/Query", "/TN", &name, "/FO", "LIST"]) {
        Ok(text) => json!({ "task": name, "installed": true, "details": text }),
        Err(_) => json!({ "task": name, "installed": false }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_names_the_user_and_escapes_paths() {
        let xml = task_xml(
            r"PC\o'neil",
            Path::new(r"C:\Tools & Bits\agent-maild.exe"),
            r#"--home "C:\h""#,
            "agent-mail <test>",
            false,
        );
        assert!(xml.contains(r"<UserId>PC\o&apos;neil</UserId>"));
        assert!(xml.contains(r"<Command>C:\Tools &amp; Bits\agent-maild.exe</Command>"));
        assert!(xml.contains("<Arguments>--home &quot;C:\\h&quot;</Arguments>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(!xml.contains("SYSTEM"));
    }

    #[test]
    fn only_keep_alive_tasks_repeat_every_minute() {
        let xml = |keep_alive| task_xml("PC\\me", Path::new(r"C:\x.exe"), "", "d", keep_alive);
        let mailer = xml(true);
        assert!(mailer.contains("<TimeTrigger><Repetition><Interval>PT1M</Interval>"));
        assert!(mailer.contains("<LogonTrigger>"));
        assert!(mailer.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(!xml(false).contains("TimeTrigger"));
    }

    #[test]
    fn task_names_are_per_home_and_program() {
        let (a, b) = (Path::new("/tmp/home-a"), Path::new("/tmp/home-b"));
        assert_ne!(
            Program::Mailer { port: 47832 }.task_name(a),
            Program::Mailer { port: 47832 }.task_name(b)
        );
        assert_ne!(
            Program::Mailer { port: 47832 }.task_name(a),
            Program::Tray { port: 1 }.task_name(a)
        );
        assert!(Program::Mailer { port: 47832 }
            .task_name(a)
            .starts_with(r"AgentMail\agent-maild-"));
    }

    #[test]
    fn mailer_logs_to_the_home_and_tray_gets_its_port() {
        let home = Path::new(r"C:\mail home");
        let mailer = Program::Mailer { port: 47842 }.arguments(home).unwrap();
        assert!(
            mailer.starts_with(r#"--home "C:\mail home" --port 47842 --log ""#),
            "{mailer}"
        );
        assert!(mailer.ends_with(r#"agent-maild.log""#), "{mailer}");
        assert_eq!(
            Program::Tray { port: 8799 }.arguments(home).unwrap(),
            r#"--home "C:\mail home" --port 8799"#
        );
        assert!(Program::Mailer { port: 47832 }
            .arguments(Path::new("C:\\bad\"path"))
            .is_err());
    }
}
