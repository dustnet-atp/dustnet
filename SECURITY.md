# Security Policy

Dustnet treats ATP servers, AML, WASM, redirects, and protocol sequencing as
fully hostile. The operating system and terminal emulator are trusted.

## Supported releases

The 0.x line is pre-production and receives best-effort fixes. Once
1.0 ships, the latest release line is supported and the immediately previous
minor receives security fixes for 90 days.

## Reporting

Report vulnerabilities through the repository's private
[security-advisory form](https://github.com/dustnet-atp/dustnet/security/advisories/new).
Do not open a public issue. We aim to acknowledge reports within three business days,
fix critical issues within seven days and high issues within fourteen days,
and coordinate disclosure within 90 days.

Stable release is blocked by any open critical or high finding. Medium findings
must be fixed or carry a documented threat-model rationale. This project has
not had a professional independent audit and must not be described as audited.
