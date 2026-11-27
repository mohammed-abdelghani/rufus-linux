# Security Policy

rufus-linux performs **privileged, destructive disk operations** — it writes
partition tables, formats filesystems, and flashes raw images to block
devices. Please take reports seriously and test only on disposable media.

## Reporting a vulnerability

Please **do not** open a public issue for security vulnerabilities.

Report privately through GitHub's **private vulnerability reporting**
(Security Advisories) for this repository. If that feature is not enabled on
the repository, contact the maintainer listed in
[.github/workflows/release.yml](.github/workflows/release.yml) instead.

Please include:

- A description of the vulnerability and its impact.
- Steps to reproduce, with exact commands or GUI actions.
- The rufus-linux version or release, install method, distro, and kernel.
- Any relevant log output (`~/.config/rufus-linux/rufus.log`).
- Do **not** include private data such as disk serial numbers, hostnames, or
  personal files.

## Supported versions

Only the **latest release** receives security fixes. Please upgrade to the
most recent release before reporting issues on older versions.
