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
| `tester` | password, or public key | password `tester`; keys `keys/id_ed25519`, `keys/id_ed25519_pass` |
| `keyonly` | public key only | `keys/id_ed25519`, or `keys/id_ed25519_pass` with passphrase `test-passphrase` |
| `certuser` | OpenSSH certificate only | `keys/id_cert` + `keys/id_cert-cert.pub`, signed by `keys/user_ca` for principal `certuser` |
| `totpuser` | keyboard-interactive: password, then TOTP | password `totp-pass`; TOTP secret `JBSWY3DPEHPK3PXP` (`oathtool --totp -b JBSWY3DPEHPK3PXP`) |

- `keys/` is created by `gen-keys.sh` and is git-ignored. Existing keys are kept, so the host key stays stable.
- The TOTP file has no `DISALLOW_REUSE`, so tests can log in more than once per 30 s window.
- **Performance tests must connect to the WSL VM's IP (`wsl hostname -I`, port 2222), not `localhost`.** The Windows port-forward relay stalls single writes of 16–64 KiB by about 44 ms each (see `docs/spikes/m0-2-sftp.md`).
