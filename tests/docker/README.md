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

ProFTPD 1.3.8c `mod_sftp` (Debian trixie; bookworm's 1.3.8 aborts with a heap error after `mkdir` of some name lengths), with every optional extension off (FR-SSH-14). User `tester`, password `tester`, host key `keys/ssh_host_rsa_key`. Its own log is `/var/log/proftpd/sftp.log` in the container, and says why it refused something.

## SMB server `samba`: the WSL VM's address, port 445

Samba 4.17 (Debian bookworm), no NetBIOS. **Connect to the WSL VM's IP (`wsl hostname -I`), not `localhost`.** Windows' own SMB server holds port 445 on the Windows side, and the Windows 10 SMB client cannot use another port. The IP changes when WSL restarts.

| User | Password |
|---|---|
| `smbuser1` | `smbpass1` |
| `smbuser2` | `smbpass2` |

Shares: `a` (writable, holds `hello.txt` and `sub/deeper/`), `b` (writable, empty), `ro` (read-only, holds `readme.txt`). Both users can use all three.

- **Error 1219:** map one share as `smbuser1`, then another as `smbuser2`.
- **A second name for the server:** `wsl -u root -- ip addr add <another address in the VM's /20>/20 dev eth0`. It is lost when WSL restarts.
- **Outages:** `docker stop`/`start`/`restart`. For dropped packets, use `iptables -I DOCKER-USER -p tcp --dport 445 -j DROP` (the `INPUT` chain does not see container traffic).

See `docs/spikes/m3-01-smb.md`.

## NFS server `nfs`: the WSL VM's address, ports 111, 2049, 20048

The WSL kernel's own knfsd, NFSv3 over TCP only, configured by a Debian bookworm container. It runs privileged and on the VM's own network (`network_mode: host`): knfsd is per network namespace, and Docker's port proxy would hide the client's source port, which the `secure` option checks. **Connect to the WSL VM's IP**, as for Samba. `servers.ps1 up` loads the `nfsd` module first (`modprobe nfsd`, once per WSL boot).

Fixed ports: portmapper 111, NFS 2049, MOUNT 20048. Exports, each on its own tmpfs (emptied when the container restarts):

| Export | Options | For |
|---|---|---|
| `/srv/nfs/rw` | `rw`, `secure`, `root_squash` | the normal case; `sub/` inside |
| `/srv/nfs/insecure` | `rw`, `insecure` | *Use reserved source port* off |
| `/srv/nfs/ro` | `ro`, `insecure` | read-only; holds `hello.txt` |
| `/srv/nfs/denied` | `rw` for `192.0.2.1` only | E-NFS-ACCESS (`MNT3ERR_ACCES`) |

- **Server restart, data kept:** `docker exec drivemount-test-nfs-1 rpc.nfsd 0`, then `rpc.nfsd --no-nfs-version 4 --no-udp 8`. `docker restart` empties the exports, so old handles go stale.
- **Dropped packets:** `tc` on the VM's `eth0` with a filter on source port 2049 (see `docs/spikes/m4-01-nfs.md` §1). `DOCKER-USER` does not see this container's traffic, as it is not bridged.

See `docs/spikes/m4-01-nfs.md`.

## Notes


- `keys/` is created by `gen-keys.sh` and is git-ignored. Existing keys are kept, so the host key stays stable.
- The TOTP file has no `DISALLOW_REUSE`, so tests can log in more than once per 30 s window.
- **Performance tests must connect to the WSL VM's IP (`wsl hostname -I`, port 2222), not `localhost`.** The Windows port-forward relay stalls single writes of 16–64 KiB by about 44 ms each (see `docs/spikes/m0-2-sftp.md`).
