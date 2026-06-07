# Security Policy

## Supported versions

Security fixes target the latest release (see [CHANGELOG.md](CHANGELOG.md));
there are no maintained backport branches.

## Reporting a vulnerability

Please report vulnerabilities **privately** via GitHub's security advisory
form: [github.com/Sohex/musefs/security/advisories/new](https://github.com/Sohex/musefs/security/advisories/new).
Do not open a public issue for an undisclosed vulnerability.

You can expect an acknowledgment within a few days. Confirmed issues are
fixed as a priority, the fix is noted in the changelog, and you will be
credited in the advisory unless you prefer otherwise.

## What counts

musefs's primary threat surface is **parsing untrusted media files**: the
scanner probes arbitrary bytes at scan time, and the serve path re-parses
file fronts at resolve/read time. Anything a crafted file can do beyond
"fail to scan with a controlled error" is in scope — memory unsafety,
panics reachable from file contents, unbounded allocation, and hangs.
Parser denial-of-service findings are real vulnerabilities here, not mere
robustness bugs: several (a VorbisComment pre-allocation OOM, an MP4
box-bounds overflow, an ID3v2 allocation bomb) have been found by the
project's fuzz targets and fixed — see [CHANGELOG.md](CHANGELOG.md).
Those fuzz and property suites run continuously
([CONTRIBUTING.md](CONTRIBUTING.md#test-tiers-beyond-cargo-test)); a fuzz
reproducer is the ideal report attachment.

Also in scope: anything that lets a crafted *database* (the mount trusts its
`--db` only as far as the documented contract) or a hostile local writer
violate the read-only guarantee on backing files.
