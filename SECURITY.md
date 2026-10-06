# Security

Ferry moves files between devices, so security reports get priority.

## Reporting a vulnerability

Please report vulnerabilities privately through
[GitHub security advisories](https://github.com/F7rx/Ferry/security/advisories/new),
not as a public issue. Include what an attacker can do, the steps to reproduce,
and the Ferry version and platform.

You can expect an acknowledgement within a few days. Once a fix is released,
the advisory is published with credit to you, unless you prefer otherwise.

## Scope

In scope: the desktop app, the browser app, the `ferry` CLI, the signaling
server (`ferry-signal`) and the protocol extensions described in
[docs/05-protocol.md](docs/05-protocol.md). The
[threat model](docs/04-threat-model.md) lists the attackers Ferry defends
against and the risks it accepts.

Issues in the upstream LocalSend protocol itself are best reported to the
[LocalSend project](https://github.com/localsend/localsend) as well.
