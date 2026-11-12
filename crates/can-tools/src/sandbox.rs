//! Sandbox für gestartete Programme: Linux-Namespaces oder ein Container pro Aufruf.

use std::path::Path;

use serde::Deserialize;

/// Wie ein Programm vom Rest des Rechners getrennt wird.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Keine Trennung: das Programm läuft mit den Rechten des Prozesses.
    #[default]
    None,
    /// Eigene Netzwerk-, Prozess-, IPC- und UTS-Namespaces über `unshare`, Grenzen über
    /// `prlimit`. Trennt nicht das Dateisystem.
    Namespaces,
    /// Ein Wegwerf-Container pro Aufruf: nur das Arbeitsverzeichnis ist eingehängt, das
    /// Wurzelverzeichnis ist schreibgeschützt, alle Capabilities sind entzogen.
    Docker,
}

/// Einstellungen der Sandbox. Ohne Angabe hat ein Programm in der Sandbox kein Netzwerk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    #[serde(default)]
    pub mode: SandboxMode,
    /// Darf das Programm ins Netzwerk?
    #[serde(default)]
    pub network: bool,
    /// Speichergrenze in MiB.
    pub memory_mb: Option<u64>,
    /// Grenze für verbrauchte CPU-Zeit in Sekunden (nur `namespaces`).
    pub cpu_secs: Option<u64>,
    /// Höchstzahl an Prozessen.
    pub max_processes: Option<u64>,
    /// Image für den Modus `docker`.
    pub image: Option<String>,
}

impl Sandbox {
    /// Sandbox über Namespaces, ohne Netzwerk.
    pub fn namespaces() -> Self {
        Self {
            mode: SandboxMode::Namespaces,
            ..Self::default()
        }
    }

    /// Sandbox über Container mit dem angegebenen Image, ohne Netzwerk.
    pub fn docker(image: impl Into<String>) -> Self {
        Self {
            mode: SandboxMode::Docker,
            image: Some(image.into()),
            ..Self::default()
        }
    }

    /// Das tatsächlich zu startende Programm samt Argumenten. Programm und Argumente des
    /// Aufrufers bleiben getrennte Argumente; nichts wird zu einer Shell-Zeile verklebt.
    pub fn command(&self, program: &str, args: &[String], workdir: &Path) -> (String, Vec<String>) {
        let mut wrapped: Vec<String> = Vec::new();
        let launcher = match self.mode {
            SandboxMode::None => return (program.to_owned(), args.to_vec()),
            SandboxMode::Namespaces => {
                if !self.network {
                    wrapped.push("--net".into());
                }
                wrapped.extend(
                    [
                        "--pid",
                        "--fork",
                        "--mount-proc",
                        "--ipc",
                        "--uts",
                        "--",
                        "prlimit",
                    ]
                    .map(String::from),
                );
                if let Some(memory) = self.memory_mb {
                    wrapped.push(format!("--as={}", memory.saturating_mul(1024 * 1024)));
                }
                if let Some(cpu) = self.cpu_secs {
                    wrapped.push(format!("--cpu={cpu}"));
                }
                if let Some(processes) = self.max_processes {
                    wrapped.push(format!("--nproc={processes}"));
                }
                wrapped.push("--".into());
                "unshare"
            }
            SandboxMode::Docker => {
                wrapped.extend(["run", "--rm", "--read-only", "--cap-drop=ALL"].map(String::from));
                wrapped.push("--security-opt=no-new-privileges".into());
                wrapped.push(format!(
                    "--network={}",
                    if self.network { "bridge" } else { "none" }
                ));
                if let Some(memory) = self.memory_mb {
                    wrapped.push(format!("--memory={memory}m"));
                }
                if let Some(processes) = self.max_processes {
                    wrapped.push(format!("--pids-limit={processes}"));
                }
                wrapped.push(format!("--volume={}:/work", workdir.display()));
                wrapped.push("--workdir=/work".into());
                wrapped.push(
                    self.image
                        .clone()
                        .unwrap_or_else(|| "debian:bookworm-slim".to_owned()),
                );
                "docker"
            }
        };
        wrapped.push(program.to_owned());
        wrapped.extend(args.iter().cloned());
        (launcher.to_owned(), wrapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        vec!["-c".to_owned(), "echo a; echo b".to_owned()]
    }

    #[test]
    fn no_sandbox_passes_the_command_through() {
        let (program, wrapped) = Sandbox::default().command("sh", &args(), Path::new("/w"));
        assert_eq!((program.as_str(), wrapped), ("sh", args()));
    }

    #[test]
    fn namespaces_isolate_network_and_apply_limits() {
        let sandbox = Sandbox {
            memory_mb: Some(256),
            cpu_secs: Some(10),
            max_processes: Some(32),
            ..Sandbox::namespaces()
        };
        let (program, wrapped) = sandbox.command("sh", &args(), Path::new("/w"));
        assert_eq!(program, "unshare");
        assert_eq!(
            wrapped,
            [
                "--net",
                "--pid",
                "--fork",
                "--mount-proc",
                "--ipc",
                "--uts",
                "--",
                "prlimit",
                "--as=268435456",
                "--cpu=10",
                "--nproc=32",
                "--",
                "sh",
                "-c",
                "echo a; echo b"
            ]
        );
        let open = Sandbox {
            network: true,
            ..Sandbox::namespaces()
        };
        assert!(
            !open
                .command("sh", &[], Path::new("/w"))
                .1
                .contains(&"--net".to_owned())
        );
    }

    #[test]
    fn docker_mounts_only_the_workdir_and_drops_privileges() {
        let sandbox = Sandbox {
            memory_mb: Some(512),
            max_processes: Some(64),
            ..Sandbox::docker("alpine:3")
        };
        let (program, wrapped) =
            sandbox.command("ls", &["-la".to_owned()], Path::new("/srv/arbeit"));
        assert_eq!(program, "docker");
        assert_eq!(
            wrapped,
            [
                "run",
                "--rm",
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--network=none",
                "--memory=512m",
                "--pids-limit=64",
                "--volume=/srv/arbeit:/work",
                "--workdir=/work",
                "alpine:3",
                "ls",
                "-la"
            ]
        );
    }

    #[test]
    fn config_parses_and_rejects_unknown_keys() {
        let sandbox: Sandbox =
            serde_json::from_value(serde_json::json!({"mode": "namespaces", "memory_mb": 128}))
                .unwrap();
        assert_eq!(
            (sandbox.mode, sandbox.network, sandbox.memory_mb),
            (SandboxMode::Namespaces, false, Some(128))
        );
        assert!(serde_json::from_value::<Sandbox>(serde_json::json!({"mode": "vm"})).is_err());
        assert!(serde_json::from_value::<Sandbox>(serde_json::json!({"netz": true})).is_err());
    }
}
