# Test servers

Run on Docker Engine inside WSL2 Ubuntu-22.04 (no Docker Desktop). From Windows:

```powershell
tests\docker\servers.ps1 up      # generate keys if missing, build, start
tests\docker\servers.ps1 check   # verify every auth method
tests\docker\servers.ps1 down
```

## SFTP server `sshd`: `localhost:2222`

All values are throw-away test credentials.

| User | Auth method | Secret |
|---|---|---|
| `tester` | password, or public key | password `tester`; any login key below |
| `keyonly` | public key only | any login key below |
| `certuser` | OpenSSH certificate only | `keys/id_cert` + `keys/id_cert-cert.pub`, or `keys/id_cert_rsa` + `keys/id_cert_rsa-cert.pub`, signed by `keys/user_ca` for principal `certuser` |
| `totpuser` | keyboard-interactive: password, then TOTP | password `totp-pass`; TOTP secret `JBSWY3DPEHPK3PXP` (`oathtool --totp -b JBSWY3DPEHPK3PXP`) |
| `keytotp` | public key, then keyboard-interactive | any login key below, then as `totpuser` |
| `kbdpass` | keyboard-interactive, then password | password `totp-pass` for both; no TOTP |

Login keys, all in `keys/`. Those with a passphrase use `test-passphrase`.

| File | Type | Format | Passphrase |
|---|---|---|---|
| `id_ed25519` | Ed25519 | OpenSSH | – |
| `id_ed25519_pass` | Ed25519 | OpenSSH | yes |
| `id_ecdsa` | ECDSA P-256 | OpenSSH | – |
| `id_rsa` | RSA 3072 | OpenSSH | – |
| `id_rsa_pem` | RSA 3072 | PEM (PKCS#1) | – |
| `id_rsa_pem_pass` | RSA 3072 | PEM (PKCS#1) | yes |
| `id_rsa_pkcs8_pass` | RSA 3072 | PEM (PKCS#8) | yes |
| `id_rsa_small` | RSA 1024, too small for FR-SSH-02 | OpenSSH | – |
| `id_wrongcert` | Ed25519, with a certificate for principal `nobody` | OpenSSH | – |

The same users and image also run with one difference each, for testing failures:

| Port | Service | Difference |
|---|---|---|
| 2223 | `sshd-nosftp` | No SFTP subsystem: the request is refused |
| 2224 | `sshd-twokeys` | Host keys: Ed25519 and RSA |
| 2225 | `sshd-legacy` | Only `ssh-rsa` (SHA-1) host key signatures |
| 2226 | `sshd-brokensftp` | The SFTP subsystem is accepted, then exits at once |
| 2227 | `sshd-hungsftp` | The SFTP subsystem is accepted, then never answers |
| 2228 | `sshd-bastion` | None: the jump host for `sshd-target` |
| none | `sshd-target` | Host key: RSA only. Reachable only through a jump host, as `sshd-target:22` |
| 2229 | `sshd-noforward` | `AllowTcpForwarding no`: a jump host that refuses the channel |
| 2231 | `sshd-rekey` | `RekeyLimit 1M`: rekeys all through a transfer |
| 2232 | `sshd-winpaths` | `tester` is locked into `/srv/winroot`, with the home directory `/C:/Users/tester` and a folder `/C:/data`, as Windows OpenSSH shows paths |

`sshd` also has `/srv/fixture`, owned by root and holding what `RemoteFs` cannot make: links to a file and a folder, a broken link, and a name that is not UTF-8. Its `/srv/small` is a 1 MiB `tmpfs` that any user can fill.

## SFTP server `proftpd`: `localhost:2230`

ProFTPD 1.3.8 `mod_sftp`, with every optional extension off (FR-SSH-14). User `tester`, password `tester`, host key `keys/ssh_host_rsa_key`. Its own log is `/var/log/proftpd/sftp.log` in the container, and says why it refused something.

## Notes


- `keys/` is created by `gen-keys.sh` and is git-ignored. Existing keys are kept, so the host key stays stable.
- The TOTP file has no `DISALLOW_REUSE`, so tests can log in more than once per 30 s window.
- **Performance tests must connect to the WSL VM's IP (`wsl hostname -I`, port 2222), not `localhost`.** The Windows port-forward relay stalls single writes of 16–64 KiB by about 44 ms each (see `docs/spikes/m0-2-sftp.md`).
