// SPDX-License-Identifier: GPL-3.0-or-later

//! Mounts one saved profile with no window, checks the drive, unmounts it and says how it went, so
//! that a script can do what a user would. It goes through the real manager, factory, SFTP and WinFsp.
//! Secrets stay in memory, and the configuration is not changed. Not shipped.
//!
//! ```text
//! cargo run -p drivemount --example mount_one -- --config FILE [options]
//!
//!   --config FILE       config.toml to read the profile from
//!   --profile NAME      which one; not needed if it has only one
//!   --known-hosts FILE  default: known_hosts beside the configuration
//!   --logs FOLDER       default: logs beside the configuration; the level is the file's own
//!   --host-key ANSWER   to an unknown key: reject (default), once or save
//!   --secret-env VAR    the environment variable holding the password or passphrase
//!   --answers-env VAR   the one holding keyboard-interactive answers, separated by commas
//!   --login-key FILE    what an *Ask each time* profile logs in with, instead of the secret
//! ```
//!
//! Exit code 0 if all of it worked, 1 if not, and 2 if the arguments were wrong. Secrets are only read
//! from the environment, as a command line is visible to other programs.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    real::main()
}

#[cfg(windows)]
mod real {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow, bail, ensure};
    use drivemount_app::backend::{SftpWinFspFactory, SystemDriveLetters};
    use drivemount_app::logging;
    use drivemount_core::config::ConfigStore;
    use drivemount_core::manager::{ManagerParts, MountManager, MountState};
    use drivemount_core::profile::Profile;
    use drivemount_vfs::{
        DriveLetter, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest,
        MemorySecretStore, Prompter, Remember, Secret, SecretRequest, SecretStore, async_trait,
    };
    use drivemount_win::drives::is_letter_free;
    use tokio::sync::broadcast;
    use tokio::time::timeout;
    use uuid::Uuid;

    const PATIENCE: Duration = Duration::from_secs(60);
    /// `ERROR_WRITE_PROTECT`, which a read-only drive answers with.
    const WRITE_PROTECT: i32 = 19;

    struct Arguments {
        config: PathBuf,
        profile: Option<String>,
        known_hosts: Option<PathBuf>,
        logs: Option<PathBuf>,
        host_key: HostKeyDecision,
        secret_env: Option<String>,
        answers_env: Option<String>,
        login_key: Option<PathBuf>,
    }

    fn parse(mut given: impl Iterator<Item = String>) -> Result<Arguments> {
        let mut config = None;
        let mut arguments = Arguments {
            config: PathBuf::new(),
            profile: None,
            known_hosts: None,
            logs: None,
            host_key: HostKeyDecision::Reject,
            secret_env: None,
            answers_env: None,
            login_key: None,
        };
        while let Some(name) = given.next() {
            let value = given
                .next()
                .ok_or_else(|| anyhow!("{name} needs a value"))?;
            match name.as_str() {
                "--config" => config = Some(PathBuf::from(value)),
                "--profile" => arguments.profile = Some(value),
                "--known-hosts" => arguments.known_hosts = Some(value.into()),
                "--logs" => arguments.logs = Some(value.into()),
                "--host-key" => {
                    arguments.host_key = match value.as_str() {
                        "reject" => HostKeyDecision::Reject,
                        "once" => HostKeyDecision::TrustOnce,
                        "save" => HostKeyDecision::TrustSave,
                        other => bail!("--host-key {other}: use reject, once or save"),
                    };
                }
                "--secret-env" => arguments.secret_env = Some(value),
                "--answers-env" => arguments.answers_env = Some(value),
                "--login-key" => arguments.login_key = Some(value.into()),
                other => bail!("unknown option {other}"),
            }
        }
        arguments.config = config.context("--config is needed")?;
        Ok(arguments)
    }

    /// Answers what a user would have, from the arguments. What it was not given, it cancels.
    struct Script {
        secret: Option<String>,
        answers: Vec<String>,
        login_key: Option<PathBuf>,
        host_key: HostKeyDecision,
    }

    impl Script {
        fn secret(&self) -> Option<Secret<String>> {
            self.secret.clone().map(Secret::new)
        }
    }

    #[async_trait]
    impl Prompter for Script {
        async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
            Some((self.secret()?, Remember::No))
        }

        async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
            match &self.login_key {
                Some(path) => Some(LoginAnswer::KeyFile {
                    path: path.clone(),
                    passphrase: self.secret(),
                }),
                None => Some(LoginAnswer::Password(self.secret()?)),
            }
        }

        async fn keyboard_interactive(
            &self,
            request: KbdIntRequest,
        ) -> Option<Vec<Secret<String>>> {
            if request.prompts.len() > self.answers.len() {
                return None;
            }
            let answers = self.answers.iter().take(request.prompts.len());
            Some(answers.cloned().map(Secret::new).collect())
        }

        async fn host_key(&self, request: HostKeyRequest) -> HostKeyDecision {
            println!(
                "unknown host key {} {}: {:?}",
                request.key_type, request.fingerprint, self.host_key
            );
            self.host_key
        }
    }

    fn from_environment(variable: Option<&String>) -> Result<Option<String>> {
        variable
            .map(|name| env::var(name).with_context(|| format!("{name} is not set")))
            .transpose()
    }

    fn chosen(store: &ConfigStore, name: Option<&str>) -> Result<Profile> {
        let profiles = &store.config().profiles;
        let found = match name {
            Some(name) => profiles.iter().find(|profile| profile.name == name),
            None if profiles.len() == 1 => profiles.first(),
            None => bail!("the file has {} profiles: use --profile", profiles.len()),
        };
        found
            .cloned()
            .ok_or_else(|| anyhow!("there is no such profile in the file"))
    }

    /// What a user does with a new drive, as far as a script can tell it worked.
    fn check_drive(letter: DriveLetter, read_only: bool) -> Result<()> {
        let root = format!(r"{letter}\");
        let listed = fs::read_dir(&root).context("listing the root")?.count();
        println!("listed {root}: {listed} entries");

        let file = Path::new(&root).join(format!("mount_one-{}.txt", Uuid::new_v4().simple()));
        if read_only {
            let refused = fs::write(&file, "refused");
            match refused {
                Err(error) if error.raw_os_error() == Some(WRITE_PROTECT) => {
                    println!("writing was refused, as it should be");
                }
                other => bail!("writing to a read-only drive gave {other:?}"),
            }
            return Ok(());
        }
        let content = "written through the drive by mount_one";
        fs::write(&file, content).context("writing a file")?;
        let back = fs::read_to_string(&file).context("reading it back");
        let found = fs::read_dir(&root)?
            .filter_map(Result::ok)
            .any(|entry| entry.path().file_name() == file.file_name());
        let removed = fs::remove_file(&file).context("removing it");
        ensure!(back? == content, "the file came back different");
        ensure!(found, "the file was not in the next listing");
        removed?;
        println!("wrote, read back, found and removed {}", file.display());
        Ok(())
    }

    /// Waits for the state after *Connecting*, or after *Unmounting*.
    async fn settled(
        events: &mut broadcast::Receiver<drivemount_core::manager::MountEvent>,
    ) -> Result<drivemount_core::manager::MountEvent> {
        loop {
            let event = timeout(PATIENCE, events.recv())
                .await
                .context("nothing happened in time")??;
            println!("state: {:?}", event.state);
            if !matches!(event.state, MountState::Connecting | MountState::Unmounting) {
                return Ok(event);
            }
        }
    }

    async fn mount_check_unmount(manager: &MountManager, profile: &Profile) -> Result<()> {
        let mut events = manager.subscribe();
        manager
            .mount(profile.id)
            .await
            .context("the mount did not start")?;
        let mounted = settled(&mut events).await?;
        if let MountState::Error(error) = &mounted.state {
            bail!("the mount failed: {error}");
        }
        let detail = mounted.detail.context("a drive with no letter")?;
        println!("mounted on {}", detail.letter);

        let checked = check_drive(detail.letter, profile.read_only);
        manager.unmount(profile.id).await;
        let unmounted = settled(&mut events).await?;
        checked?;
        ensure!(
            unmounted.state == MountState::Disconnected,
            "the unmount ended in {:?}",
            unmounted.state
        );
        ensure!(
            is_letter_free(detail.letter)?,
            "{} is still in use",
            detail.letter
        );
        println!("unmounted, and {} is free again", detail.letter);
        Ok(())
    }

    fn run(arguments: Arguments, store: ConfigStore) -> Result<()> {
        let profile = chosen(&store, arguments.profile.as_deref())?;
        let folder = arguments.config.parent().unwrap_or(Path::new("."));
        let known_hosts = arguments
            .known_hosts
            .unwrap_or_else(|| folder.join("known_hosts"));
        let answers = from_environment(arguments.answers_env.as_ref())?;
        let prompter = Arc::new(Script {
            secret: from_environment(arguments.secret_env.as_ref())?,
            answers: answers.map_or_else(Vec::new, |answers| {
                answers.split(',').map(str::to_owned).collect()
            }),
            login_key: arguments.login_key,
            host_key: arguments.host_key,
        });

        let runtime = tokio::runtime::Runtime::new()?;
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let manager = {
            let _inside = runtime.enter();
            MountManager::new(ManagerParts {
                store,
                factory: Arc::new(SftpWinFspFactory::new(Arc::clone(&secrets), known_hosts)),
                secrets,
                letters: Arc::new(SystemDriveLetters),
                prompter,
            })
        };
        runtime.block_on(async {
            let result = mount_check_unmount(&manager, &profile).await;
            manager.shutdown().await;
            result
        })
    }

    pub fn main() -> ExitCode {
        let arguments = match parse(env::args().skip(1)) {
            Ok(arguments) => arguments,
            Err(error) => {
                eprintln!("{error:#}");
                return ExitCode::from(2);
            }
        };
        let store = ConfigStore::open(&arguments.config);
        let folder = arguments.config.parent().unwrap_or(Path::new("."));
        let logs = arguments
            .logs
            .clone()
            .unwrap_or_else(|| folder.join("logs"));
        let level = store.config().settings.log_level;
        let log = logging::start(&logs, level);
        let result = run(arguments, store);
        drop(log);
        match result {
            Ok(()) => {
                println!("all good");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error:#}");
                ExitCode::FAILURE
            }
        }
    }
}
