# Windows

Every service and `regencyctl` build and run on Windows. Each service has an
MSI that installs:

- `C:\Program Files\regency\bin\regency-<service>.exe`;
- the Windows service `regency-<service>` (account `NetworkService`,
  manual start, restarted on failure);
- `C:\ProgramData\regency\<service>.toml` (kept on upgrade and uninstall).

```powershell
msiexec /i regency-runner-1.2.0-x86_64.msi /qn
notepad C:\ProgramData\regency\runner.toml
Set-Service regency-runner -StartupType Automatic
Start-Service regency-runner
```

Paths in the configuration use TOML literal strings:
`ca_file = 'C:\ProgramData\regency\tls\ca.pem'`.

A Windows service has no console: the shipped configuration writes the logs
(JSON lines) to `C:\ProgramData\regency\logs\<service>.log`
(`[log] file`). Point your log agent at that directory; rotate it with your
usual tooling. To watch a service interactively, stop it and run the binary
in a console with the same `--config` argument.

The `regencyctl` MSI adds `regencyctl` to `PATH`.

## Managing Windows hosts

Runners reach Windows hosts over SSH (Windows' OpenSSH server) or SSHOQ, and
regops runs on Windows as the service `regops`. regent-sdk's Windows support
covers services, users, local groups, hostname, commands, line-in-file
edits and templates; the host lock uses a lock file
(`C:\ProgramData\regent\regent.lock`).
