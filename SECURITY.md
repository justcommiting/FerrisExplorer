# Security Policy

## Supported Versions

Only the latest release of FerrisExplorer is supported with security updates.

| Version | Supported |
| ------- | --------- |
| Latest  | Yes       |
| Older   | No        |

## Reporting a Vulnerability

Please report vulnerabilities privately using GitHub's **Report a vulnerability** feature under the repository's **Security** tab (private advisories).

Do not disclose security vulnerabilities through public issues or discussions. You can expect an initial response within 7 days.

## Security Model

FerrisExplorer enforces multiple layers of protection to keep your system safe:

- **Webview Isolation**: Frontend code runs in an isolated webview under a strict Content Security Policy (`'self'`). The UI does not have direct filesystem access; all operations are handled through strictly scoped Rust commands.
- **Filesystem Protections**: Destructive actions like renames, moves, and undo operations refuse silent overwrites. Critical system paths, including filesystem roots and the user home directory, cannot be deleted.
- **Shell Execution**: Commands execute with arguments safely shell-quoted for the host operating system. Commands reject placeholders inside user quotes, heredocs, or backticks, command output is capped at 2 MiB, and execution within archive files is prohibited.
- **Plugins**: Plugins use declarative TOML manifests and contain no executable UI code. Every plugin starts disabled and is bound to the SHA-256 hash of its manifest; any file modification automatically revokes trust until re-approved. Plugin previewers run in the background with strict timeouts and process group termination.
