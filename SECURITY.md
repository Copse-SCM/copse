# Security

## Reporting a problem

Please don't open a public issue for a security problem. Report it privately through
GitHub's **Report a vulnerability** button on the repository's Security tab (private
vulnerability reporting), so it can be fixed before it's public.

Helpful things to include: what an attacker could do, the copse version (`copse version`),
and steps or a repository that shows it. You'll get a reply within a week.

## What counts

copse treats repositories, servers and remotes it talks to as untrusted. Problems worth
reporting include:

- a repository, pack or server reply that makes copse write outside the working folder,
  into `.copse`, or through a symlink
- objects accepted whose contents don't match their ids
- command or option injection through URLs, remote names, submodule addresses or file names
- credentials shown, logged or sent somewhere they shouldn't go
- a crash or hang caused by data from a server or another person's repository

## Supported versions

Until 1.0, fixes go into the latest release only.
