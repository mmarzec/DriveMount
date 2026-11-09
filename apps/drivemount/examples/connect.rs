// SPDX-License-Identifier: GPL-3.0-or-later

//! `drivemount.exe` on scratch files, for trying the window against a real server (H-06) with nothing of the
//! user's read or changed: its `config.toml`, `known_hosts` and log are in a folder of their own in
//! the temp folder, and secrets stay in memory, so a *Remember* lasts until it ends. It mounts real
//! drives. Not shipped.
//!
//! ```text
//! cargo run -p drivemount --example connect -- [--fresh]
//! ```
//!
//! `--fresh` starts with no connections and no known hosts. With the S-03 server running
//! (`tests/docker/servers.ps1 up`), try `tester@localhost:2222/`.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::{env, fs};

    use drivemount_app::app::{Setup, run};
    use drivemount_vfs::MemorySecretStore;
    use drivemount_win::toast::AppId;

    let folder = env::temp_dir().join("drivemount-connect-preview");
    if env::args().any(|argument| argument == "--fresh") && folder.exists() {
        fs::remove_dir_all(&folder)?;
    }
    fs::create_dir_all(&folder)?;
    run(Setup {
        config: folder.join("config.toml"),
        known_hosts: folder.join("known_hosts"),
        window_state: Some(folder.join("ui-state.toml")),
        logs: Some(folder.join("logs")),
        secrets: Arc::new(MemorySecretStore::new()),
        // Its own, so that a click on its toasts after it has gone starts it, not the app.
        toasts: Some(AppId {
            aumid: "DriveMount.Preview".to_owned(),
            display_name: "DriveMount (preview)".to_owned(),
            activator: uuid::Uuid::from_u128(0x5d0e_3c1a_7b2f_4e8d_9a61_0c4b_2f7e_93a5),
        }),
    })
}
