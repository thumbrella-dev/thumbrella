# Running Thumbrella as a Windows service

Docs and support:  https://thumbrella.dev
Server reference:  https://thumbrella.dev/docs/server/

The Thumbrella executable is a console program, so it cannot be registered
with `sc.exe create` directly (the service manager would fail with
error 1053). A service wrapper bridges this: the wrapper registers itself
as the real service, then starts and monitors `thumbrella.exe` as an
ordinary child process. This example uses NSSM
(https://nssm.cc); WinSW (https://github.com/winsw/winsw) works the same
way.

Run each command in an administrator PowerShell. The server is configured
through environment variables, shown at the bottom.

```powershell
# 1. Download nssm.exe from https://nssm.cc/download and note its path,
#    then install the service (adjust both paths for your machine):

    nssm install Thumbrella C:\thumbrella\thumbrella.exe serve

# 2. Persistent cache (optional but recommended). This directory must
#    already exist and be writable by the service:

    nssm set Thumbrella AppEnvironmentExtra TBR_CACHE=sqlite:C:\thumbrella\cache.db

# 3. Plain-text logs, since there is no console attached to a service:

    nssm set Thumbrella AppEnvironmentExtra ++ NO_COLOR=1

# 4. Start it:

    nssm start Thumbrella
```

Useful commands after that:

```powershell
    nssm status Thumbrella     # is it running?
    nssm restart Thumbrella    # after changing settings
    nssm remove Thumbrella confirm   # uninstall
```

NSSM writes the server's stdout to the Windows Event Log by default. To
keep logs in a plain file instead:

```powershell
    nssm set Thumbrella AppStdout C:\thumbrella\log.txt
    nssm set Thumbrella AppStderr C:\thumbrella\log.txt
```

Common environment variables (set more with the `++` form shown above):

    TBR_PORT=3114              port the server listens on
    TBR_CACHE=sqlite:...       persistent cache, as shown above
    TBR_HANDSHAKE=...          require a secret on every request
    TBR_LOG=full               more detailed logging

The full reference is at https://thumbrella.dev/docs/server/
