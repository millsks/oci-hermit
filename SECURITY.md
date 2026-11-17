# Security policy

oci-hermit is a sandbox, so a way out of it is a security problem: guest code reaching the host, the guest reaching
the network, a bind mount reaching more than it should, or an image that is run although its digest does not match.
So is anything that makes oci-hermit need elevated rights.

## Reporting a vulnerability

Report it privately through GitHub:
**[Report a vulnerability](https://github.com/millsks/oci-hermit/security/advisories/new)**
(Security → Advisories → Report a vulnerability). The report is visible only to the maintainers until an advisory
is published.

Please do not open a public issue for a security problem, and please do not disclose it elsewhere before a fix is
out. If GitHub's form is not available to you, open a public issue asking for a private channel, with no details
in it, and one will be arranged.

**What to include:** the output of `oci-hermit version`, the host operating system and architecture, the command you
ran, and the smallest image or input that reproduces it. For anything involving a crafted image or archive, attach
it or say where to get it. Logs help: each sandbox's `console.log` and `qemu.log` (their folder is shown by
`oci-hermit machine info` under `run dir`).

## What to expect

| | |
|---|---|
| Acknowledgment | Within 3 business days. This is a small project with one maintainer; if you have heard nothing after that, ping the advisory thread. |
| Assessment | Within 10 business days: whether it is in scope, how severe, and a rough fix timeline. |
| Fix and disclosure | A fix on `main` (and in a release, once there are releases), then a published GitHub Security Advisory with a CVE where one is warranted. |
| Credit | You are named in the advisory unless you would rather not be. Say which you prefer. |

There is no bug bounty.

## Supported versions

**None yet: oci-hermit is pre-release.** There is no release to support (#39, #40, #41). Fixes go to `main`, and the
CI builds of `main` are the only builds. Once there are releases, the latest release will be the supported one, and
this section will say so.

## Scope

In scope: the host tool (`crates/oci-hermit`), the guest agent (`crates/hermit-agent`), the control protocol
(`crates/hermit-proto`), the guest kernel configuration, and the way oci-hermit builds and launches QEMU.

A vulnerability in QEMU or Linux itself belongs upstream. Tell us as well if oci-hermit's configuration exposes it
(a device or option the sandbox does not need, for example), because that part is ours to fix.

Documented differences from Docker are not vulnerabilities: published ports binding to 127.0.0.1 only, bind mounts
being read-only by default, and the guest having no network at all are deliberate. So are the limits of the current
release that the documentation states, such as there being no OS sandbox around the emulator yet (#38) and no image
signature verification yet (#42).
