# Pull from a private registry

**When:** your images are in a registry that needs a login (GitHub Container Registry, GitLab, Artifactory, Nexus,
Harbor, a cloud registry), or in one whose certificate is issued by your company's own certificate authority.

!!! note "`oci-hermit login` is coming"

    `oci-hermit login` and `logout` are planned in #75. Until then, oci-hermit uses the logins Podman and Docker
    already keep, or an auth file you write yourself, as described below.

Pulls always happen in the host tool. The credentials never reach the sandbox, and the sandbox has no network.

## 1. See which logins oci-hermit can find

```bash
oci-hermit machine info
```

The last section lists every place it looks for an auth file, in order, and whether each one exists:

```console
registries (pulls happen on the host, never in the sandbox):
  auth files: searched in this order; the first with a login for the registry is used
    1. [not set]   $REGISTRY_AUTH_FILE
    2. [not set]   $XDG_RUNTIME_DIR/containers/auth.json
    3. [not found] /Users/me/.config/containers/auth.json
    4. [found]     /Users/me/.docker/config.json
  plain http: none (OCI_HERMIT_INSECURE_REGISTRIES)
```

| # | File | Written by |
| --- | --- | --- |
| 1 | `$REGISTRY_AUTH_FILE` | you, or Podman when the variable is set |
| 2 | `$XDG_RUNTIME_DIR/containers/auth.json` | `podman login` on Linux |
| 3 | `~/.config/containers/auth.json` (`$XDG_CONFIG_HOME/containers/auth.json` when that is set) | `podman login` on macOS and Windows, or with `--authfile` |
| 4 | `~/.docker/config.json` (`$DOCKER_CONFIG/config.json` when that is set) | `docker login` |

For each registry, oci-hermit takes the first file that has a login for it. On Windows, `~` is your user profile
(`%USERPROFILE%`).

## 2. Log in with a tool you already have

If Podman or Docker is installed (it does not have to be able to run containers), log in with it:

```bash
podman login ghcr.io
# or
docker login ghcr.io
```

Then pull as usual:

```bash
oci-hermit pull ghcr.io/your-org/your-app:1.2
```

**Credential helpers work too.** When an auth file names a helper, in `credHelpers` for one registry or `credsStore`
for all of them, oci-hermit runs `docker-credential-<name> get`, as Docker does. Docker Desktop's `desktop` helper,
`osxkeychain`, `wincred`, `pass`, `ecr-login` and the cloud providers' helpers all work, as long as the helper is on
your `PATH`.

Logins that hand out an identity token instead of a username and password (`<token>` as the username, as
`az acr login` does) are not supported yet. Log in with a username and an access token instead.

## 3. Or write an auth file yourself

Without Podman or Docker, write the file they would have written. Most registries accept a personal access token in
place of a password; create one with read access to packages.

The login is `username:token`, base64-encoded:

=== "macOS and Linux"

    ```bash
    printf '%s' 'your-user:your-token' | base64
    ```

=== "Windows"

    ```powershell
    [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes('your-user:your-token'))
    ```

Put it in a file, under the registry's host name:

```json
{
  "auths": {
    "ghcr.io": { "auth": "eW91ci11c2VyOnlvdXItdG9rZW4=" }
  }
}
```

Save it as `~/.config/containers/auth.json` (where `podman login` would put it), or anywhere you like and point
`REGISTRY_AUTH_FILE` at it:

=== "macOS and Linux"

    ```bash
    export REGISTRY_AUTH_FILE="$HOME/registry-auth.json"
    chmod 600 "$REGISTRY_AUTH_FILE"
    oci-hermit machine info
    ```

=== "Windows"

    ```powershell
    $env:REGISTRY_AUTH_FILE = "$HOME\registry-auth.json"
    oci-hermit machine info
    ```

`machine info` now shows `[found]` next to it. The file holds a secret: keep it readable only by you, and out of
version control.

## 4. Pull

```bash
oci-hermit pull ghcr.io/your-org/your-app:1.2
```

A pull prints each blob as it arrives, then the manifest's digest:

```console
$ oci-hermit pull public.ecr.aws/docker/library/alpine:3.20
ab3fe4defd29: pulled (628 B)
3f26bc2dec0b: pulled (4.1 MB)
Digest: sha256:45e09956dc667c5eff3583c9d94830261fb1ca0be10a0a7db36266edf5de9e1d
Status: pulled public.ecr.aws/docker/library/alpine:3.20 for linux/arm64
```

Every manifest, config and layer is checked against its sha256 digest before it is stored; a mismatch stops the pull
and the image is not stored. Throttling and server errors (429, 502, 503, 504) are retried four times, after 1, 2, 4 and 8
seconds. `run` pulls a missing image the same way, so `oci-hermit run ghcr.io/your-org/your-app:1.2` works too.

## A registry behind a company certificate authority

oci-hermit checks registry certificates against the operating system's trust store: the Windows certificate store,
the macOS keychain, or the system certificates on Linux. A company root certificate that IT has installed on the
machine is therefore trusted with nothing more to do. If a pull fails with a certificate error, check the same URL
in a browser on the same machine: if the browser does not trust it either, the certificate is missing from the
system store, and IT needs to install it.

Per-registry certificate files (`certs.d`, as Podman and Docker use them) and client certificates are planned in
#86.

## A registry over plain HTTP

A local test registry often has no TLS at all. Name it in `OCI_HERMIT_INSECURE_REGISTRIES`, as `host:port`, comma
separated:

=== "macOS and Linux"

    ```bash
    export OCI_HERMIT_INSECURE_REGISTRIES=localhost:5000
    oci-hermit pull localhost:5000/my-app:dev
    ```

=== "Windows"

    ```powershell
    $env:OCI_HERMIT_INSECURE_REGISTRIES = "localhost:5000"
    oci-hermit pull localhost:5000/my-app:dev
    ```

Only the registries named there are reached over HTTP; every other one still needs HTTPS.

## When it fails

| Message | What it means |
| --- | --- |
| `logging in to pull …: error sending request for url (https://…/v2/)` | The registry cannot be reached: a typo in the name, no network, a proxy, or a certificate the system does not trust. |
| `pulling the manifest of …: Not authorized` | No login for this registry was found, or it was refused. Check `machine info` and the host name the login is filed under. |
| `pulling the manifest of …: … does not exist` | The repository or tag is wrong, or your login cannot see it. |
| `… does not match its digest: the image is corrupt or has been altered` | A blob did not hash to its digest, so the image was not stored. Try again; if it repeats, something between you and the registry is changing the data. |

More in [Troubleshooting](../troubleshooting.md#registry-errors).
