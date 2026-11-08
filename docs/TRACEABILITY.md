# DriveMount: Requirement traceability

One row per requirement in [SPECIFICATION.md](SPECIFICATION.md). Tasks are in [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md). Update the row when a task closes.

- **Test:** what proves the requirement.
- **Verified in:** the version in which its acceptance criterion last passed. `(local)` means the same steps passed on the development machine and in a Linux container, but the CI workflow has not run yet (it needs the Gitea runners, H-02). `–` means not verified yet.

## Platform

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| PLAT-01 | M2-09, M2-13 | – | – |
| PLAT-02 | M1-11, M2-10 | Detection: `drivemount-win` `winfsp::tests`, with the installed 2.2 in `the_installed_winfsp_is_ready` (`#[ignore]`). Installer: M2-10 | – |
| PLAT-03 | M0-1, M1-12c | Mounted unelevated by every test in `crates/drivemount-winfsp/tests`, on this Windows 10 Home. M2-09, M2-13 for the rest | – |
| PLAT-04 | M1-16, M1-17 | All sizes are logical pixels. The owner checked 100% and 150% (H-06). Two monitors with different scaling: not tried | – |

## Profiles

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-PROF-01 | M1-13, M1-14, M1-15, M1-17 | Surviving a restart and being gone after a delete, in the file: `each_kind_of_login_comes_back`, `any_profile_comes_back`, `a_deleted_profile_is_gone_and_the_others_keep_their_keys` (`crates/drivemount-core/src/config/tests.rs`). Through the commands: `a_new_profile_is_saved_and_can_be_mounted`, `delete_takes_the_profile_and_its_secrets_and_leaves_the_others` (`crates/drivemount-core/src/manager/tests.rs`). The dialog: `save_sends_the_profile_and_waits_for_the_answer`, `editing_shows_what_is_saved_and_keeps_the_id` (`apps/drivemount/src/ui/tests/dialog.rs`). All together, by hand: `examples/connect.rs` against S-03 | – |
| FR-PROF-02 | M1-13, M1-14, M1-15, M1-17 | So far the model: a new id in `Profile::new`, and no rule looks at it but to tell a profile from itself (`the_defaults_of_a_new_profile_are_valid_and_each_gets_its_own_id`, `a_profile_does_not_clash_with_itself`). In the file: `a_rename_keeps_the_id_and_the_unknown_keys`, `the_second_profile_with_an_id_is_unreadable`. The credentials following it: `a_rename_keeps_the_secrets`. Through the boxes: `every_kind_of_profile_comes_back_from_the_boxes_as_it_was`, `a_new_profile_keeps_one_id_through_every_check` (`apps/drivemount/src/ui/form.rs`) | – |
| FR-PROF-03 | M1-13, M1-14, M1-15, M1-17 | The rule: `a_name_is_unique_ignoring_case`, `a_second_profile_cannot_take_the_name_of_another` (`drivemount-core`). In the Save command: `a_second_profile_cannot_take_a_name_ignoring_case`, `two_saves_at_once_cannot_both_take_a_name`. Inline error: `a_name_in_use_is_marked_in_any_case_but_not_against_itself`, `each_rule_shows_under_its_box_and_save_comes_on_when_all_are_met` | – |
| FR-PROF-04 | M1-13, M1-14, M1-15, M1-17 | Each rule of §7.3 has unit tests in `crates/drivemount-core/src/profile/validate.rs`, and `each_bad_field_is_reported_against_its_own_box` for the marks. Save refusing them: `save_checks_the_fields_and_changes_nothing_if_they_are_wrong`. Marking fields and disabling Save: `each_box_that_makes_no_sense_is_marked_and_stops_saving`, `each_rule_shows_under_its_box_and_save_comes_on_when_all_are_met`, `a_refused_save_marks_the_box_and_keeps_what_was_typed` | – |
| FR-PROF-05 | M1-13, M1-14, M1-15, M1-17 | Splitting: `the_acceptance_case_fills_all_four_fields`, `a_url_fills_all_four_fields` and the rest of `crates/drivemount-core/src/profile/paste.rs`. Filling the boxes: `pasting_into_the_host_box_fills_the_others`, `typing_a_host_and_port_is_not_split_along_the_way`, `typing_is_not_pasting` | – |
| FR-PROF-06 | M1-13, M1-14, M1-15, M1-17 | Below the UI, wrong password and missing path included: `test_connection_reports_a_good_login_a_bad_one_and_a_missing_path` (`apps/drivemount/tests/real_drive.rs`, needs S-03). The command: `test_reports_the_round_trip_and_touches_nothing` and its companions. The dialog: `test_connection_reports_in_the_dialog`, `test_connection_says_what_went_wrong`, `a_test_can_be_cancelled_and_its_answer_no_longer_counts`. By hand against S-03: a wrong password, then a right one | – |
| FR-PROF-07 | M1-11, M1-13, M1-14, M1-15, M1-17 | The removal: `drivemount-win` `credentials::tests::delete_all_leaves_nothing_and_only_touches_that_profile`. Profile delete: `delete_takes_the_profile_and_its_secrets_and_leaves_the_others`, `deleting_a_mounted_profile_unmounts_it_first`, `a_secret_that_cannot_be_deleted_keeps_the_profile_so_it_can_be_tried_again`. Asking first: `delete_asks_first_and_escape_or_cancel_keep_the_profile`, `deleting_a_mounted_profile_says_it_unmounts_first` (`apps/drivemount/src/ui/tests.rs`) | – |
| FR-PROF-08 | M1-13, M1-14, M1-15, M1-17 | The rule, field by field: `connection_fields_are_locked_while_mounted_and_free_after`, `what_is_not_a_connection_field_can_change_while_mounted`. Read-only boxes and the hint: `while_mounted_the_host_is_locked_and_the_name_is_not`, `a_mount_starting_with_the_dialog_open_locks_it_and_puts_the_boxes_back`, `while_locked_only_the_name_and_flags_can_change` | – |

## SSH/SFTP backend

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-SSH-01 | M1-08b, M1-17 | Below the UI: `tests/auth.rs` (needs S-03), such as `a_stored_password_logs_in_without_asking`, `a_missing_password_is_asked_for_once_and_not_on_reconnect`, `a_remembered_password_is_handed_over_after_the_login`. Storing in Credential Manager: M1-11, and `a_password_ticked_in_the_prompt_is_stored_and_the_profile_marked`, `the_password_in_the_dialog_is_stored_if_remember_is_ticked`, `a_remembered_password_mounts_writes_and_asks_only_once` (`apps/drivemount/tests/real_drive.rs`, needs S-03). Dialog: M1-17 | – |
| FR-SSH-02 | M1-08b, M1-17 | Below the UI: `each_key_format_logs_in_without_asking`, `each_encrypted_key_format_asks_once_and_not_on_reconnect` (need S-03), and the key-loading unit tests in `connector/auth.rs`. Dialog: `the_secret_prompt_asks_with_the_titles_of_the_spec`, `a_secret_is_answered_with_what_was_typed_and_then_leaves_the_window`. By hand against S-03: `id_ed25519_pass`. Whole app path (M1-19): `an_rsa_pem_key_with_a_passphrase_mounts`, `a_key_with_a_passphrase_mounts_and_the_passphrase_stays_out_of_the_log` (`apps/drivemount/tests/mount_one.rs`, need S-03) | – |
| FR-SSH-03 | M1-08b, M1-13, M1-17 | Refused at connect: `ppk_files_are_refused_whatever_their_name`, `a_ppk_file_is_a_format_error` (needs S-03). Hint and no Save, below the UI: `a_ppk_file_gets_the_conversion_hint_and_blocks_saving` (`drivemount-core`). Dialog: `a_ppk_shows_the_hint_at_once_and_save_stays_off`, `each_kind_of_wrong_key_file_gets_its_own_text` | – |
| FR-SSH-04 | M1-08b, M1-17 | `a_key_in_the_agent_logs_in_without_asking` and its companions (need S-03), with an in-process agent on a named pipe. Real `ssh-agent`, Unix socket: `a_key_in_real_ssh_agent_logs_in`. The real Windows OpenSSH agent, through the app's manager, factory and WinFsp: `the_windows_agent_mounts` (`apps/drivemount/tests/mount_one.rs`, needs the service started by an administrator; M1-19, H-07). **Not run:** Pageant, which is not installed here | – |
| FR-SSH-05 | M1-08b, M1-17 | Below the UI: `ask_with_a_password_stores_nothing_and_reconnects_without_asking` and its companions (need S-03). No credential entries: `ask_each_time_never_stores_anything`, `the_password_in_the_dialog_is_not_stored_if_it_is_not`, `agent_and_ask_look_for_no_secret`. Dialog: `an_ask_each_time_profile_is_asked_for_a_password_or_a_key_file`, `where_nothing_may_be_stored_there_is_no_remember`. By hand against S-03, with a password | – |
| FR-SSH-06 | M1-08b, M1-17 | `a_certificate_next_to_the_key_is_offered` (Ed25519 and RSA), `without_its_certificate_the_key_is_refused`, `a_certificate_that_cannot_be_used_leaves_the_plain_key` (need S-03). Whole app path (M1-19): `a_certificate_mounts` (`apps/drivemount/tests/mount_one.rs`) | – |
| FR-SSH-07 | M1-08b, M1-17 | Below the UI: `a_stored_password_answers_the_password_prompt_and_only_the_code_is_asked`, `a_key_can_be_the_first_factor_and_the_server_asks_the_rest` and companions (need S-03). Dialog: `a_keyboard_interactive_round_asks_each_question_with_the_servers_text`. By hand against S-03: `totpuser` with a real code. **Differs from §7.4.1 as first written:** no *Remember* on a keyboard-interactive password, see there. Whole app path (M1-19): `keyboard_interactive_with_a_one_time_code_mounts`, `keyboard_interactive_then_a_password_mounts`, `a_key_then_a_password_and_a_code_mounts` (`apps/drivemount/tests/mount_one.rs`) | – |
| FR-SSH-08 | M1-07, M1-08a, M1-17 | The file: `crates/drivemount-sftp/tests/known_hosts.rs`, and `ssh_and_this_module_agree_on_one_file` (`known_hosts_openssh.rs`, needs S-03). In a connection: `tests/connector.rs` (needs S-03), such as `a_revoked_key_is_refused_without_asking` and `a_key_type_on_file_is_asked_for_first`. Dialogs: see FR-SSH-09 and FR-SSH-10 | – |
| FR-SSH-09 | M1-07, M1-08a, M1-17 | Below the UI: `trust_and_save_is_asked_once_and_real_ssh_agrees`, `trust_once_connects_and_saves_nothing`, `cancel_refuses_and_saves_nothing` (`tests/connector.rs`, need S-03). Dialog: `the_host_key_prompt_shows_the_key_and_takes_each_answer`. By hand against S-03: *Trust and save*, and no question on the next run. Whole app path (M1-19): `an_unknown_host_key_is_refused_unless_the_script_says_and_a_saved_one_satisfies_ssh` (`apps/drivemount/tests/mount_one.rs`): `ssh.exe -o BatchMode=yes` refuses the host before and accepts it after | – |
| FR-SSH-10 | M1-07, M1-08a, M1-17 | Refusal below the UI, with the line and both fingerprints: `a_changed_key_is_refused_without_asking` (needs S-03). Simulated by editing the file, not by replacing the server's key. Traffic after key exchange was not captured. Dialog: `a_changed_key_brings_up_its_dialog_with_no_way_to_trust_it`, `test_connection_brings_up_the_changed_key_dialog_too`. By hand against S-03, again by editing the file. *Open file location* was left to the owner's H-06, confirmed on 2026-11-06. Whole app path (M1-19): `a_changed_host_key_is_refused_and_not_asked_about` (`apps/drivemount/tests/mount_one.rs`) | – |
| FR-SSH-11 | M1-09, M1-13, M1-15, M1-17 | Below the UI: `crates/drivemount-sftp/tests/jump.rs`, such as `each_host_key_is_asked_for_and_saved_for_its_own_host` (needs S-03), and `a_jump_host_can_log_in_with_the_agent` in `tests/auth.rs`. Storing `jump-password`: `a_passphrase_and_a_jump_password_go_in_their_own_slots`, and reading it back: `a_jump_password_comes_from_its_own_slot`. Through the manager to a real bastion: not run. The fields and their rule: `a_jump_host_is_host_with_optional_user_and_port` and its companions (`drivemount-core`). Dialogs: M1-17. Whole app path (M1-19): `a_profile_behind_a_jump_host_mounts` (`apps/drivemount/tests/mount_one.rs`) | – |
| FR-SSH-12 | M1-08a, M1-10, M2-01 | So far the SSH layer: `unanswered_keep_alives_close_the_connection` and its two companions (`tests/connector.rs`, need S-03), now 3 intervals. State change to *Reconnecting*: M2-01 | – |
| FR-SSH-13 | M1-08a, M1-10 | `remote_paths_are_resolved_and_checked`, and `drive_letter_paths_work_where_the_server_uses_them` against a Linux server laid out like Windows (need S-03). **Real Windows OpenSSH has not run:** lab matrix, §10.4 | – |
| FR-SSH-14 | M1-08a, M1-10 | `the_contract_holds_on_openssh` and `the_contract_holds_on_a_server_without_the_extensions` (`crates/drivemount-sftp/tests/sftpfs.rs`, need S-03) | – |
| FR-SSH-15 | M1-08a, M1-10 | The contract suite against OpenSSH 9.2 and ProFTPD 1.3.8. OpenSSH 7.4, Windows OpenSSH: lab matrix, §10.4 | – |

## Mounting

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-MNT-01 | M1-11, M1-12a–c, M1-15, M2-07 | The commands: `a_profile_starts_disconnected_and_mounts_through_connecting`, `unmounting_goes_back_to_disconnected`. On a real drive: `a_remembered_password_mounts_writes_and_asks_only_once` (`apps/drivemount/tests/real_drive.rs`). The row's button and Enter: `the_button_follows_the_state_and_sends_the_right_command`, `the_arrows_move_the_selection_and_enter_acts_on_it` (`apps/drivemount/src/ui/tests.rs`). The ⋯ menu by real keys: looked at, in no test. Tray: M2-03 | – |
| FR-MNT-02 | M1-11, M1-12a–c, M1-15, M2-07 | `auto_takes_z_when_it_is_free`, `auto_goes_down_from_z_past_what_is_taken`, `a_free_letter_of_the_profile_is_used_under_either_policy` (`crates/drivemount-core/src/manager/letters.rs`) | – |
| FR-MNT-03 | M1-11, M1-12a–c, M1-15, M2-07 | The definition of free: `drivemount-win` `drives::tests`, with a `subst` drive and, `#[ignore]`d, `a_remembered_mapping_that_is_down_is_taken`, the acceptance case. Skipping A, B and C: `a_b_and_c_are_never_used_or_even_asked_about`, `a_profile_cannot_ask_for_a_b_or_c` | – |
| FR-MNT-04 | M1-11, M1-12a–c, M1-15, M2-07 | Both behaviours, below the UI: `a_taken_letter_under_auto_mounts_on_another_and_says_so`, `a_taken_letter_under_fail_stops_before_any_backend_is_made`, `a_profiles_letter_taken_during_the_mount_goes_by_the_setting`. The row note and toast: M2-07 | – |
| FR-MNT-05 | M1-11, M1-12a–c, M1-15, M2-07 | The name in Explorer's own window: `a_running_explorer_shows_the_name_and_hides_it_within_a_second` and `a_name_that_needs_unicode_is_shown_as_it_is` (`crates/drivemount-winfsp/tests/explorer.rs`, `#[ignore]`d, need a desktop). In the registry: `tests/lifecycle.rs`. The name, the network-drive icon and *Network locations*: confirmed by the owner, 2026-11-05, on a test drive, up only for moments. Told apart by their slugs on real drives: `two_names_that_look_alike_mount_together_on_their_own_letters` (`apps/drivemount/tests/real_drive.rs`). In the app: H-07 | – |
| FR-MNT-06 | M1-11, M1-12a–c, M1-15, M2-07 | Read from Explorer's own window through UI Automation: `tests/explorer.rs` (`#[ignore]`d). Showing about 850 ms, hiding about 1.2 s, as the criterion now says. The test allows 3 s, to catch Explorer not being told, and prints what it measured. Confirmed by the owner, 2026-11-05 | – |
| FR-MNT-07 | M1-11, M1-12a–c, M1-15, M2-07 | Through a mounted drive, with `VolumeConfig::read_only`: `a_read_only_volume_refuses_changes_before_they_reach_the_server` (`crates/drivemount-winfsp/tests/read_path.rs`), and `copying_into_a_read_only_drive_fails_cleanly` and `delete_on_close_does_nothing_on_a_read_only_drive` (`tests/write_path.rs`). The choice per profile: `a_read_only_profile_gets_a_read_only_drive` (`apps/drivemount/tests/real_drive.rs`) | – |
| FR-MNT-08 | M1-11, M1-12a–c, M1-15, M2-07 | – | – |
| FR-MNT-09 | M1-11, M1-12a–c, M1-15, M1-16, M2-07 | `mount_all_and_unmount_all_send_one_command_each`, `mount_all_takes_what_is_unmounted_and_unmount_all_what_is_up`. | – |
| FR-MNT-10 | M1-11, M1-12a–c, M1-15, M2-07 | `files_left_open_do_not_hold_up_unmounting_and_are_closed`, `a_flush_that_fails_as_the_drive_stops_does_not_fail_the_unmount`, `a_server_that_stopped_answering_holds_up_unmounting_for_one_timeout_at_most` (`tests/lifecycle.rs`), through Win32 handles. Notepad itself: H-07 | – |
| FR-MNT-11 | M1-11, M1-12a–c, M1-15, M2-07 | Below the UI: `dropping_a_mount_in_progress_cancels_it_at_once` and `a_mount_dropped_just_as_the_drive_starts_leaves_no_drive`. Through the manager: `cancel_while_connecting_returns_to_disconnected_at_once`, and `cancel_at_a_real_dialog_is_back_within_a_second` (`apps/drivemount/tests/real_drive.rs`). The row's button: M1-16 | – |
| FR-MNT-12 | M1-11, M1-12a–c, M1-15, M2-07 | `drivemount-win` `explorer_opens_at_the_root_of_the_drive` (`#[ignore]`): a real window at a `subst` drive's root. A mounted drive: M1-19 | – |

## Connection state and reconnect

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-REC-01 | M2-01, M2-02 | – | – |
| FR-REC-02 | M2-01, M2-02 | – | – |
| FR-REC-03 | M2-01, M2-02 | – | – |
| FR-REC-04 | M2-01, M2-02 | – | – |
| FR-REC-05 | M2-01, M2-02 | – | – |
| FR-REC-06 | M2-01, M2-02 | – | – |
| FR-REC-07 | M2-01, M2-02 | Below the triggers: `a_lost_connection_without_auto_reconnect_unmounts_and_is_an_error`, on a mock's `Health`. What makes `Health` say so, and the toast: M2-01, M2-04 | – |

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
| FR-APP-06 | M1-11, M1-16, M2-02, M2-06 | Detection: `drivemount-win` `winfsp::tests`. Banner and disabled buttons: `without_winfsp_the_banner_shows_and_mounting_is_off_and_editing_is_not`, `without_winfsp_only_starting_a_mount_is_off`. Opening the page: `only_a_secure_web_page_is_opened`, connected in `drivemount_app::app::engine` | – |
| FR-APP-07 | M1-11, M2-02, M2-06 | – | – |

## Settings

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| FR-SET-01 | M1-14, M2-05 | The value in the file: `settings_come_back`, `a_bad_setting_gets_its_default_and_the_file_stays_usable` (`drivemount-core`). The theme in the window: `the_theme_can_be_forced_either_way_and_handed_back`. The setting: M2-05 | – |
| FR-SET-02 | M1-14, M2-05 | The value in the file: `settings_come_back`, `a_bad_setting_gets_its_default_and_the_file_stays_usable` (`drivemount-core`). The setting: M2-05 | – |
| FR-SET-03 | M1-14, M2-05 | The value in the file: `settings_come_back`, `a_bad_setting_gets_its_default_and_the_file_stays_usable` (`drivemount-core`). The setting: M2-05 | – |
| FR-SET-04 | M1-14, M2-05 | The value in the file: `settings_come_back`, `a_bad_setting_gets_its_default_and_the_file_stays_usable` (`drivemount-core`). The setting: M2-05 | – |
| FR-SET-05 | M1-14, M2-05 | The value in the file: `settings_come_back`, `a_bad_setting_gets_its_default_and_the_file_stays_usable` (`drivemount-core`). The setting: M2-05 | – |
| FR-SET-06 | M1-14, M2-05 | The value in the file: `settings_come_back`, `a_bad_setting_gets_its_default_and_the_file_stays_usable` (`drivemount-core`). The setting: M2-05 | – |
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
| NFR-PERF-03 | M1-01, M2-08 | `scripts/check-exe-size.ps1` in CI: 14.88 MB with the engine, at M1-18. Measured against the full app: M2-08 | – |
| NFR-PERF-04 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-05 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-06 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-07 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-08 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-09 | M0-2, M0-3, M1-06, M1-10, M2-08 | – | – |
| NFR-PERF-10 | M0-2, M0-3, M1-06, M1-10, M1-17, M2-08 | The dialog's file system work is off the UI thread: `a_key_file_is_looked_at_off_the_ui_thread_and_only_then_can_be_saved`, and free letters and the file dialog are commands. Nothing measured | – |

## Reliability

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-REL-01 | M1-03, M1-05, M1-12a–c, M2-01, M2-09 | Through a drive on `MemFs`: `what_is_written_reads_back_the_same_at_every_size`, `a_gibibyte_round_trips` (`#[ignore]`d, 1 GiB) and `tests/fsx.rs`, a Rust `fsx`, 10k operations and `FSX_OPS` for more. Over a real SFTP session to a server in the test process: the same tests with `DRIVEMOUNT_TEST_SERVER=sftp`, and `what_is_written_over_sftp_reads_back_the_same_after_reopening`. On a real server: M2-09. Real drive over real SFTP to OpenSSH (M1-19): `a_gibibyte_and_ten_thousand_small_files_go_in_and_out_unchanged` (`apps/drivemount/tests/real_drive.rs`), SHA-256 compared | – |
| NFR-REL-02 | M1-05, M1-12a–c, M2-01, M2-09 | Below the drive: `nfr_rel_02_changes_show_in_the_very_next_listing` and a property test on `CachedFs` over `MemFs` (`crates/drivemount-vfs/tests/cache.rs`). Through a drive on `MemFs`: `changes_show_in_the_very_next_listing` and `a_new_size_shows_in_the_very_next_listing` (`crates/drivemount-winfsp/tests/write_path.rs`). Real drive over OpenSSH (M1-19): the same test lists the GiB and the 10,000 files right after copying them | – |
| NFR-REL-03 | M1-05, M1-06, M1-12a–c, M2-01, M2-09 | Below the drive: `every_call_times_out_at_the_deadline_and_signals_suspect` (`crates/drivemount-vfs/tests/deadline.rs`). Through the drive, for lookups, listings, opens, reads and volume queries: `a_hung_*_times_out` and `the_limit_is_for_the_callback_not_for_each_call` (`crates/drivemount-winfsp/tests/read_path.rs`). Write callbacks: `a_hung_*_times_out`, `the_limit_is_for_the_whole_callback` and, for `Cleanup`, `closing_a_handle_is_bounded_when_the_server_hangs` (`tests/write_path.rs`) | – |
| NFR-REL-04 | M1-05, M1-12a–c, M2-01, M2-09 | `a_killed_process_leaves_no_drive` (`tests/lifecycle.rs`), with a test process. `drivemount.exe` itself, killed with `taskkill /F` while a drive is up: `killing_it_with_a_drive_up_leaves_no_drive` (`apps/drivemount/tests/drivemount_exe.rs`, M1-19) | – |
| NFR-REL-05 | M1-05, M1-12a–c, M1-14, M2-01, M2-09 | `crates/drivemount-core/src/config/tests.rs`. Atomic: `the_file_and_its_backup_are_replaced_not_written_into`, and `if_the_backup_cannot_be_written_the_file_is_left_alone`. The backup: `the_backup_is_the_previous_file`. Never overwritten: `a_damaged_file_means_read_only_and_is_never_written`, `a_file_damaged_while_running_is_not_overwritten_and_the_backup_is_kept`, `a_newer_file_means_read_only_with_the_words_in_the_spec`. Showing the error: M1-16 | – |
| NFR-REL-06 | M1-05, M1-12a–c, M2-01, M2-09 | – | – |

## Security

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-SEC-01 | M1-02, M1-07, M1-11, M2-11 | `drivemount-win` `credentials::tests`: names, type and persistence as Windows reports them, round-trip, delete, limits. Behind `SecretStore`: `the_store_goes_through_to_credential_manager` | – |
| NFR-SEC-02 | M1-02, M1-07, M1-11, M2-11 | So far only the type: `drivemount-vfs` `secret::tests` (`***`, zeroize on drop), `scripts/check-secret-format.sh` in CI. Config: `a_remembered_secret_is_not_in_the_file_or_its_backup` (`drivemount-core`), and the profile model has no field for a secret. Logs: `a_secret_in_a_field_comes_out_as_stars`, `other_crates_stay_at_warn_whatever_the_setting` (`drivemount_app::logging`), and at Debug through real mounts in `tests/mount_one.rs`, for a passphrase and a wrong password | – |
| NFR-SEC-03 | M1-02, M1-07, M1-11, M2-11 | Zeroing on drop: `drivemount-vfs` `secret::tests::dropping_zeroizes_exactly_once`. Held only while mounted: `the_backend_is_gone_by_the_time_disconnected_is_announced`, and for cancelling, failing and losing the connection in `crates/drivemount-core/src/manager/tests.rs`. That each `Secret` in `russh` is zeroed is not shown (M1-08b's note) | – |
| NFR-SEC-04 | M1-02, M1-07, M1-11, M2-11 | – | – |
| NFR-SEC-05 | M1-02, M1-07, M1-11, M2-11 | – | – |
| NFR-SEC-06 | M1-02, M1-07, M1-11, M2-11 | – | – |
| NFR-SEC-07 | M1-01, M2-11 | CI `cargo deny check`, `cargo audit`. Signing: M2-11 | – |

## Usability

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-UX-01 | M1-16, M1-17 | One click from the window: `the_button_follows_the_state_and_sends_the_right_command`. The tray: M2-03 | – |
| NFR-UX-02 | M1-16, M1-17 | By construction: the dialog and prompts are in the main window, and only Windows' own file dialog is another | – |
| NFR-UX-03 | M1-16, M1-17 | `each_error_has_the_text_and_code_of_the_catalogue`, `details_meant_for_the_log_are_not_shown`, `a_banner_says_what_to_do_and_takes_one_line_of_a_long_detail` (`apps/drivemount/src/ui/tests.rs`) | – |
| NFR-UX-04 | M1-16, M1-17 | Keys: `the_shortcuts_of_the_table_work`, `the_arrows_move_the_selection_and_enter_acts_on_it`, `the_selection_is_kept_in_view`. Names: `every_button_has_a_name_for_screen_readers`, `a_row_reads_out_its_state_and_address`. In the dialog: `nothing_behind_the_dialog_can_be_reached_by_keys`, `enter_in_the_box_answers_once_something_is_typed`, `the_next_prompt_has_the_keys_whatever_kind_came_before`. The walkthrough: the owner (H-06), confirmed for the dialog on 2026-11-06. A real screen reader: not tried | – |
| NFR-UX-05 | M1-16, M1-17 | By construction: the window's text is in `@tr` in `apps/drivemount/ui/*.slint`, except the messages of §7.3, which are `drivemount_core::profile::Problem`'s `Display`, in English. No test looks for a string left out, and no translation has been tried | – |

## Maintainability

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-MAINT-01 | M1-01 | `rust-toolchain.toml` pins 1.99.1; edition 2024 in the workspace manifest | 0.1.0 (local) |
| NFR-MAINT-02 | M1-01 | CI `lint`: `cargo fmt --check`, `cargo clippy -D warnings` | 0.1.0 (local) |
| NFR-MAINT-03 | M1-01 | CI builds and tests the workspace on Linux; holds again as each crate gains code | 0.1.0 (local) |
| NFR-MAINT-04 | M1-18 | `drivemount-core` `tests/mount_span.rs`; through a real mount, `a_password_mounts_checks_and_unmounts_in_a_span_of_its_own`. The log's format: `a_line_has_the_time_the_level_the_span_and_the_message` | – |

## Licensing

| Requirement | Tasks | Test | Verified in |
|---|---|---|---|
| NFR-LIC-01 | M1-01 | `scripts/check-spdx.sh` in CI; negative test: fails on a missing header | 0.1.0 (local) |
| NFR-LIC-02 | M2-05, M2-12 | – | – |
| NFR-LIC-03 | M2-05 | – | – |
| NFR-LIC-04 | M2-10 | – | – |
| NFR-LIC-05 | M1-01 | CI `cargo deny check`; negative test: a `GPL-2.0-only` crate fails | 0.1.0 (local) |
| NFR-LIC-06 | M2-13 | – | – |
