# Security policy

Security fixes target the latest published release. Report vulnerabilities
privately through [GitHub private vulnerability reporting](https://github.com/talaagent/ffcine/security/advisories/new).
If that form is unavailable, email **me@ianm.com**. Avoid public disclosure until
a fix is available.

Include the affected version, enabled features, native library versions, and a
minimal reproduction. Treat media as untrusted input: malformed containers,
shaders, LUTs, subtitles, project files, and native decoder behavior can all matter.
Use synthetic or redistributable fixtures and remove personal footage, location
metadata, and upload credentials from reports.

Vulnerabilities in ffmpeg, libplacebo, or a GPU driver should also be reported to
the relevant upstream project. ffcine's updates do not automatically update
system-installed native libraries.
