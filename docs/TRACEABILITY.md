# FS Manager: Requirement traceability

One row per requirement in [SPECIFICATION.md](SPECIFICATION.md). Tasks are in [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md). Update the row when a task closes.

- **Test:** what proves the requirement.
- **Verified in:** the version in which its acceptance criterion last passed. `(local)` means the same steps passed on the development machine and in a Linux container, but the CI workflow has not run yet (it needs the Gitea runners, H-02). `–` means not verified yet.

## Platform

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| PLAT-01 | M2-09, M2-13 | – | – |
| PLAT-02 | M1-11, M2-10 | Detection: `fsm-win` `winfsp::tests`, with the installed 2.2 in `the_installed_winfsp_is_ready` (`#[ignore]`). Installer: M2-10 | – |
| PLAT-03 | M0-1, M1-12c | Mounted unelevated by every test in `crates/fsm-winfsp/tests`, on this Windows 10 Home. M2-09, M2-13 for the rest | – |
| PLAT-04 | M1-16, M1-17 | – | – |

## Profiles

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-PROF-01 | M1-13, M1-14, M1-15, M1-17 | – | – |
| FR-PROF-02 | M1-13, M1-14, M1-15, M1-17 | – | – |
| FR-PROF-03 | M1-13, M1-14, M1-15, M1-17 | – | – |
| FR-PROF-04 | M1-13, M1-14, M1-15, M1-17 | – | – |
| FR-PROF-05 | M1-13, M1-14, M1-15, M1-17 | – | – |
| FR-PROF-06 | M1-13, M1-14, M1-15, M1-17 | – | – |
| FR-PROF-07 | M1-11, M1-13, M1-14, M1-15, M1-17 | So far only the removal: `fsm-win` `credentials::tests::delete_all_leaves_nothing_and_only_touches_that_profile`. Profile delete: M1-15 | – |
| FR-PROF-08 | M1-13, M1-14, M1-15, M1-17 | – | – |
| FR-PROF-09 | M1-13, M1-14, M1-15, M1-17 | – | – |

## SSH/SFTP backend

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-SSH-01 | M1-08b, M1-17 | Below the UI: `tests/auth.rs` (needs S-03), such as `a_stored_password_logs_in_without_asking`, `a_missing_password_is_asked_for_once_and_not_on_reconnect`, `a_remembered_password_is_handed_over_after_the_login`. Storing in Credential Manager: M1-11, M1-15. Dialog: M1-17 | – |
| FR-SSH-02 | M1-08b, M1-17 | Below the UI: `each_key_format_logs_in_without_asking`, `each_encrypted_key_format_asks_once_and_not_on_reconnect` (need S-03), and the key-loading unit tests in `connector/auth.rs`. Dialog: M1-17 | – |
| FR-SSH-03 | M1-08b, M1-17 | Refused at connect: `ppk_files_are_refused_whatever_their_name`, `a_ppk_file_is_a_format_error` (needs S-03). Hint and disabled Save: M1-13, M1-17 | – |
| FR-SSH-04 | M1-08b, M1-17 | `a_key_in_the_agent_logs_in_without_asking` and its companions (need S-03), with an in-process agent on a named pipe. Real `ssh-agent`, Unix socket: `a_key_in_real_ssh_agent_logs_in`. **Not run:** the Windows OpenSSH agent itself, and Pageant. M1-19 | – |
| FR-SSH-05 | M1-08b, M1-17 | Below the UI: `ask_with_a_password_stores_nothing_and_reconnects_without_asking` and its companions (need S-03). Dialog and no credential entries: M1-17 | – |
| FR-SSH-06 | M1-08b, M1-17 | `a_certificate_next_to_the_key_is_offered` (Ed25519 and RSA), `without_its_certificate_the_key_is_refused`, `a_certificate_that_cannot_be_used_leaves_the_plain_key` (need S-03) | – |
| FR-SSH-07 | M1-08b, M1-17 | Below the UI: `a_stored_password_answers_the_password_prompt_and_only_the_code_is_asked`, `a_key_can_be_the_first_factor_and_the_server_asks_the_rest` and companions (need S-03). Dialog: M1-17 | – |
| FR-SSH-08 | M1-07, M1-08a, M1-17 | The file: `crates/fsm-sftp/tests/known_hosts.rs`, and `ssh_and_this_module_agree_on_one_file` (`known_hosts_openssh.rs`, needs S-03). In a connection: `tests/connector.rs` (needs S-03), such as `a_revoked_key_is_refused_without_asking` and `a_key_type_on_file_is_asked_for_first`. Dialogs: M1-17 | – |
| FR-SSH-09 | M1-07, M1-08a, M1-17 | Below the UI: `trust_and_save_is_asked_once_and_real_ssh_agrees`, `trust_once_connects_and_saves_nothing`, `cancel_refuses_and_saves_nothing` (`tests/connector.rs`, need S-03). Dialog: M1-17 | – |
| FR-SSH-10 | M1-07, M1-08a, M1-17 | Refusal below the UI, with the line and both fingerprints: `a_changed_key_is_refused_without_asking` (needs S-03). Simulated by editing the file, not by replacing the server's key. Traffic after key exchange was not captured. Dialog: M1-17 | – |
| FR-SSH-11 | M1-09, M1-15, M1-17 | Below the UI: `crates/fsm-sftp/tests/jump.rs`, such as `each_host_key_is_asked_for_and_saved_for_its_own_host` (needs S-03), and `a_jump_host_can_log_in_with_the_agent` in `tests/auth.rs`. Storing `jump-password`: M1-15. Fields and dialogs: M1-17 | – |
| FR-SSH-12 | M1-08a, M1-10, M2-01 | So far the SSH layer: `unanswered_keep_alives_close_the_connection` and its two companions (`tests/connector.rs`, need S-03), now 3 intervals. State change to *Reconnecting*: M2-01 | – |
| FR-SSH-13 | M1-08a, M1-10 | `remote_paths_are_resolved_and_checked`, and `drive_letter_paths_work_where_the_server_uses_them` against a Linux server laid out like Windows (need S-03). **Real Windows OpenSSH has not run:** lab matrix, §10.4 | – |
| FR-SSH-14 | M1-08a, M1-10 | `the_contract_holds_on_openssh` and `the_contract_holds_on_a_server_without_the_extensions` (`crates/fsm-sftp/tests/sftpfs.rs`, need S-03) | – |
| FR-SSH-15 | M1-08a, M1-10 | The contract suite against OpenSSH 9.2 and ProFTPD 1.3.8. OpenSSH 7.4, Windows OpenSSH: lab matrix, §10.4 | – |

## Mounting

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-MNT-01 | M1-11, M1-12a–c, M1-15, M2-07 | – | – |
| FR-MNT-02 | M1-11, M1-12a–c, M1-15, M2-07 | – | – |
| FR-MNT-03 | M1-11, M1-12a–c, M1-15, M2-07 | The definition of free: `fsm-win` `drives::tests`, with a `subst` drive and, `#[ignore]`d, `a_remembered_mapping_that_is_down_is_taken`, the acceptance case. Skipping A, B and C: M1-15 | – |
| FR-MNT-04 | M1-11, M1-12a–c, M1-15, M2-07 | – | – |
| FR-MNT-05 | M1-11, M1-12a–c, M1-15, M2-07 | The name in Explorer's own window: `a_running_explorer_shows_the_name_and_hides_it_within_a_second` and `a_name_that_needs_unicode_is_shown_as_it_is` (`crates/fsm-winfsp/tests/explorer.rs`, `#[ignore]`d, need a desktop). In the registry: `tests/lifecycle.rs`. **Not checked:** the icon, and *Network locations*: H-06 | – |
| FR-MNT-06 | M1-11, M1-12a–c, M1-15, M2-07 | Read from Explorer's own window through UI Automation: `tests/explorer.rs` (`#[ignore]`d). Showing about 850 ms; **hiding about 1.2 s, over the 1 s of the criterion**, see `docs/PROGRESS.md`. The test allows 3 s and prints what it measured. H-06 | – |
| FR-MNT-07 | M1-11, M1-12a–c, M1-15, M2-07 | Through a mounted drive, with `VolumeConfig::read_only`: `a_read_only_volume_refuses_changes_before_they_reach_the_server` (`crates/fsm-winfsp/tests/read_path.rs`), and `copying_into_a_read_only_drive_fails_cleanly` and `delete_on_close_does_nothing_on_a_read_only_drive` (`tests/write_path.rs`). The choice per profile: M1-15 | – |
| FR-MNT-08 | M1-11, M1-12a–c, M1-15, M2-07 | – | – |
| FR-MNT-09 | M1-11, M1-12a–c, M1-15, M2-07 | – | – |
| FR-MNT-10 | M1-11, M1-12a–c, M1-15, M2-07 | `files_left_open_do_not_hold_up_unmounting_and_are_closed`, `a_flush_that_fails_as_the_drive_stops_does_not_fail_the_unmount`, `a_server_that_stopped_answering_holds_up_unmounting_for_one_timeout_at_most` (`tests/lifecycle.rs`), through Win32 handles. Notepad itself: H-07 | – |
| FR-MNT-11 | M1-11, M1-12a–c, M1-15, M2-07 | Below the UI: `dropping_a_mount_in_progress_cancels_it_at_once` and `a_mount_dropped_just_as_the_drive_starts_leaves_no_drive`. The row's button: M1-15, M1-16 | – |
| FR-MNT-12 | M1-11, M1-12a–c, M1-15, M2-07 | `fsm-win` `explorer_opens_at_the_root_of_the_drive` (`#[ignore]`): a real window at a `subst` drive's root. A mounted drive: M1-19 | – |

## Connection state and reconnect

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-REC-01 | M2-01, M2-02 | – | – |
| FR-REC-02 | M2-01, M2-02 | – | – |
| FR-REC-03 | M2-01, M2-02 | – | – |
| FR-REC-04 | M2-01, M2-02 | – | – |
| FR-REC-05 | M2-01, M2-02 | – | – |
| FR-REC-06 | M2-01, M2-02 | – | – |
| FR-REC-07 | M2-01, M2-02 | – | – |

## Tray

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-TRAY-01 | M0-3, M2-03 | – | – |
| FR-TRAY-02 | M0-3, M2-03 | – | – |
| FR-TRAY-03 | M0-3, M2-03 | – | – |
| FR-TRAY-04 | M0-3, M2-03 | – | – |

## Notifications

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-NOT-01 | M0-3, M2-04 | – | – |
| FR-NOT-02 | M0-3, M2-04 | – | – |
| FR-NOT-03 | M0-3, M2-04 | – | – |
| FR-NOT-04 | M0-3, M2-04 | – | – |

## Application lifecycle

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-APP-01 | M1-11, M2-02, M2-06 | – | – |
| FR-APP-02 | M1-11, M2-02, M2-06 | – | – |
| FR-APP-03 | M1-11, M2-02, M2-06 | – | – |
| FR-APP-04 | M1-11, M2-02, M2-06 | – | – |
| FR-APP-05 | M1-11, M2-02, M2-06 | – | – |
| FR-APP-06 | M1-11, M1-16, M2-02, M2-06 | Detection only: `fsm-win` `winfsp::tests`. Banner: M1-16 | – |
| FR-APP-07 | M1-11, M2-02, M2-06 | – | – |

## Settings

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-SET-01 | M2-05 | – | – |
| FR-SET-02 | M2-05 | – | – |
| FR-SET-03 | M2-05 | – | – |
| FR-SET-04 | M2-05 | – | – |
| FR-SET-05 | M2-05 | – | – |
| FR-SET-06 | M2-05 | – | – |
| FR-SET-07 | M2-05 | – | – |

## SMB backend

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-SMB-01 | M3-01 … M3-05 | – | – |
| FR-SMB-02 | M3-01 … M3-05 | – | – |
| FR-SMB-03 | M3-01 … M3-05 | – | – |
| FR-SMB-04 | M3-01 … M3-05 | – | – |
| FR-SMB-05 | M3-01 … M3-05 | – | – |
| FR-SMB-06 | M3-01 … M3-05 | – | – |
| FR-SMB-07 | M3-01 … M3-05 | – | – |
| FR-SMB-08 | M3-01 … M3-05 | – | – |

## NFS backend

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-NFS-01 | M4-01 … M4-06 | – | – |
| FR-NFS-02 | M4-01 … M4-06 | – | – |
| FR-NFS-03 | M4-01 … M4-06 | – | – |
| FR-NFS-04 | M4-01 … M4-06 | – | – |
| FR-NFS-05 | M4-01 … M4-06 | – | – |

## Performance

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-PERF-01 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-02 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-03 | M1-01, M2-08 | `scripts/check-exe-size.ps1` in CI. Measured against the full app: M2-08 | – |
| NFR-PERF-04 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-05 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-06 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-07 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-08 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-09 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-10 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |

## Reliability

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-REL-01 | M1-03, M1-05, M1-12a–c, M2-01, M2-09 | Through a drive on `MemFs`: `what_is_written_reads_back_the_same_at_every_size`, `a_gibibyte_round_trips` (`#[ignore]`d, 1 GiB) and `tests/fsx.rs`, a Rust `fsx`, 10k operations and `FSX_OPS` for more. Over a real SFTP session to a server in the test process: the same tests with `FSM_TEST_SERVER=sftp`, and `what_is_written_over_sftp_reads_back_the_same_after_reopening`. On a real server: M2-09 | – |
| NFR-REL-02 | M1-05, M1-12a–c, M2-01, M2-09 | Below the drive: `nfr_rel_02_changes_show_in_the_very_next_listing` and a property test on `CachedFs` over `MemFs` (`crates/fsm-vfs/tests/cache.rs`). Through a drive on `MemFs`: `changes_show_in_the_very_next_listing` and `a_new_size_shows_in_the_very_next_listing` (`crates/fsm-winfsp/tests/write_path.rs`) | – |
| NFR-REL-03 | M1-05, M1-06, M1-12a–c, M2-01, M2-09 | Below the drive: `every_call_times_out_at_the_deadline_and_signals_suspect` (`crates/fsm-vfs/tests/deadline.rs`). Through the drive, for lookups, listings, opens, reads and volume queries: `a_hung_*_times_out` and `the_limit_is_for_the_callback_not_for_each_call` (`crates/fsm-winfsp/tests/read_path.rs`). Write callbacks: `a_hung_*_times_out`, `the_limit_is_for_the_whole_callback` and, for `Cleanup`, `closing_a_handle_is_bounded_when_the_server_hangs` (`tests/write_path.rs`) | – |
| NFR-REL-04 | M1-05, M1-12a–c, M2-01, M2-09 | `a_killed_process_leaves_no_drive` (`tests/lifecycle.rs`), with a test process. `fsm.exe` itself: M1-19 | – |
| NFR-REL-05 | M1-05, M1-12a–c, M2-01, M2-09 | – | – |
| NFR-REL-06 | M1-05, M1-12a–c, M2-01, M2-09 | – | – |

## Security

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-SEC-01 | M1-02, M1-07, M1-11, M2-11 | `fsm-win` `credentials::tests`: names, type and persistence as Windows reports them, round-trip, delete, limits | – |
| NFR-SEC-02 | M1-02, M1-07, M1-11, M2-11 | So far only the type: `fsm-vfs` `secret::tests` (`***`, zeroize on drop), `scripts/check-secret-format.sh` in CI. Config and logs: M1-14, M1-18 | – |
| NFR-SEC-03 | M1-02, M1-07, M1-11, M2-11 | So far only zeroing on drop: `fsm-vfs` `secret::tests::dropping_zeroizes_exactly_once`. Zeroed on unmount: M1-15 | – |
| NFR-SEC-04 | M1-02, M1-07, M1-11, M2-11 | – | – |
| NFR-SEC-05 | M1-02, M1-07, M1-11, M2-11 | – | – |
| NFR-SEC-06 | M1-02, M1-07, M1-11, M2-11 | – | – |
| NFR-SEC-07 | M1-01, M2-11 | CI `cargo deny check`, `cargo audit`. Signing: M2-11 | – |

## Usability

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-UX-01 | M1-16, M1-17 | – | – |
| NFR-UX-02 | M1-16, M1-17 | – | – |
| NFR-UX-03 | M1-16, M1-17 | – | – |
| NFR-UX-04 | M1-16, M1-17 | – | – |
| NFR-UX-05 | M1-16, M1-17 | – | – |

## Maintainability

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-MAINT-01 | M1-01 | `rust-toolchain.toml` pins 1.99.1; edition 2024 in the workspace manifest | 0.1.0 (local) |
| NFR-MAINT-02 | M1-01 | CI `lint`: `cargo fmt --check`, `cargo clippy -D warnings` | 0.1.0 (local) |
| NFR-MAINT-03 | M1-01 | CI builds and tests the workspace on Linux; holds again as each crate gains code | 0.1.0 (local) |
| NFR-MAINT-04 | M1-18 | – | – |

## Licensing

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-LIC-01 | M1-01 | `scripts/check-spdx.sh` in CI; negative test: fails on a missing header | 0.1.0 (local) |
| NFR-LIC-02 | M2-05, M2-12 | – | – |
| NFR-LIC-03 | M2-05 | – | – |
| NFR-LIC-04 | M2-10 | – | – |
| NFR-LIC-05 | M1-01 | CI `cargo deny check`; negative test: a `GPL-2.0-only` crate fails | 0.1.0 (local) |
| NFR-LIC-06 | M2-13 | – | – |
