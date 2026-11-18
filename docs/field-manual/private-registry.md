# Pull from a private registry

**When:** your images are in a registry that needs a login (GitHub Container Registry, GitLab, Artifactory, Nexus,
Harbor, a cloud registry), or in one whose certificate is issued by your company's own certificate authority.

Pulls always happen in the host tool. The credentials never reach the sandbox, and the sandbox has no network.

## 1. Log in

```bash
oci-hermit login ghcr.io
```

It asks for the user name and the password (the password is not shown as you type), checks them with the registry,
and only then saves them:

```console
$ oci-hermit login ghcr.io
Username: your-user
Password:
Login Succeeded!
```

A wrong password is reported and nothing is saved:

```console
$ oci-hermit login -u your-user ghcr.io
Password:
oci-hermit: login to ghcr.io failed: the registry rejected the user name or password
```

Most registries (GHCR, GitLab, Harbor, Quay, Artifactory) take a personal access token in place of the password;
create one with read access to packages. With no registry, `login` logs in to Docker Hub.

**In scripts and CI**, give the password on stdin, never as an argument, where other users on the machine could see
it in the process list:

=== "macOS and Linux"

    ```bash
    echo "$GHCR_TOKEN" | oci-hermit login -u your-user --password-stdin ghcr.io
    ```

=== "Windows"

    ```powershell
    $env:GHCR_TOKEN | oci-hermit login -u your-user --password-stdin ghcr.io
    ```

`-p` (`--password`) works too, with a warning. The login is stored where Podman stores it (see the table below),
readable by you only, so Podman and skopeo use it as well. `--authfile <FILE>` stores it somewhere else.

To see who you are logged in as, or to log out:

```bash
oci-hermit login --get-login ghcr.io     # prints the user name
oci-hermit logout ghcr.io                # removes that login
oci-hermit logout --all                  # removes every login in the file
```

`logout` only changes the file `login` writes. A login made with `docker login` lives in Docker's own file;
`logout` says so and points you at `docker logout`.

## 2. See which logins oci-hermit can find

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
  login file: /Users/me/.config/containers/auth.json (where login writes)
  plain http: none (OCI_HERMIT_INSECURE_REGISTRIES)
```

| # | File | Written by |
| --- | --- | --- |
| 1 | `$REGISTRY_AUTH_FILE` | `oci-hermit login` and `podman login` when the variable is set, or you |
| 2 | `$XDG_RUNTIME_DIR/containers/auth.json` | `oci-hermit login` and `podman login` on Linux |
| 3 | `~/.config/containers/auth.json` (`$XDG_CONFIG_HOME/containers/auth.json` when that is set) | `oci-hermit login` and `podman login` on macOS and Windows, and on Linux without `$XDG_RUNTIME_DIR` |
| 4 | `~/.docker/config.json` (`$DOCKER_CONFIG/config.json` when that is set) | `docker login` |

For each registry, oci-hermit takes the first file that has a login for it, so a login made with `oci-hermit login`
wins over one in Docker's file. On Windows, `~` is your user profile (`%USERPROFILE%`).

## 3. Or use a login you already have

Logins made with Podman or Docker work without logging in again. If either is installed (it does not have to be
able to run containers), you can log in with it instead:

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

## 4. Or write an auth file yourself

Where `oci-hermit login` cannot be run interactively and stdin is not an option, write the file it would have
written. Most registries accept a personal access token in
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

## 5. Pull

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
machine is therefore trusted with nothing more to do.

When it is not installed, and you cannot install it machine-wide without admin rights, give it to oci-hermit for
that registry alone. Put the CA certificate (PEM, the text that starts `-----BEGIN CERTIFICATE-----`) in a folder
named after the registry, host and port as you write them in image names, with a `.crt` extension:

=== "macOS and Linux"

    ```bash
    mkdir -p ~/.config/containers/certs.d/registry.corp.example:5000
    cp corp-root-ca.pem ~/.config/containers/certs.d/registry.corp.example:5000/ca.crt
    oci-hermit pull registry.corp.example:5000/team/app:1.4
    ```

=== "Windows"

    A folder name cannot hold a `:` on Windows, so drop it, as Docker does: `registry.corp.example:5000` is
    `registry.corp.example5000`.

    ```powershell
    $dir = "$HOME\.config\containers\certs.d\registry.corp.example5000"
    New-Item -ItemType Directory -Force $dir
    Copy-Item corp-root-ca.pem "$dir\ca.crt"
    oci-hermit pull registry.corp.example:5000/team/app:1.4
    ```

This is the `certs.d` layout Podman and Docker use, so a folder you already set up for them works too. oci-hermit
looks for the registry's folder in these, in order, and uses the first it finds:

1. `$XDG_CONFIG_HOME/containers/certs.d`, or `~/.config/containers/certs.d` (Podman, yours)
2. `/etc/containers/certs.d` (Podman, the whole machine; macOS and Linux)
3. `~/.docker/certs.d` (Docker)
4. `/etc/docker/certs.d` on macOS and Linux, `%ProgramData%\docker\certs.d` on Windows (Docker, the whole machine)

`pull --cert-dir DIR` and `login --cert-dir DIR` use the files in `DIR` instead. A registry's CAs are trusted for
that registry only, on top of the system's, never for any other host. `machine info` lists the folders and what each
holds for which registry.

## A registry that wants a client certificate

Some registries ask for a client certificate (mutual TLS) as well as, or instead of, a login. Put the certificate
and its private key, both PEM, in the registry's folder beside its CA, as a `.cert` and a `.key` with the same
name:

```
~/.config/containers/certs.d/registry.corp.example:5000/
  ca.crt          the registry's CA (if the system does not trust it already)
  client.cert     your certificate
  client.key      its private key
```

A `.cert` without its `.key` (or the other way round) is an error, so a half-copied setup is reported instead of
failing later. On macOS and Linux, keep the key readable by you only (`chmod 600 client.key`): oci-hermit warns when
other users can read it. It never prints a key.

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
| `pulling the manifest of …: error sending request for url (https://…)` | The registry cannot be reached: a typo in the name, no network, or a proxy. |
| `… invalid peer certificate … (if … uses its own CA, put that CA in …)` | The registry's certificate is from a CA the system does not trust. Add it as [above](#a-registry-behind-a-company-certificate-authority). |
| `… (the registry may want a client certificate: …)` | The registry asked for a client certificate and got none, or one it does not accept. See [client certificates](#a-registry-that-wants-a-client-certificate). |
| `pulling the manifest of …: 401 … (no login for … was found …)` | No login for this registry was found. Check `machine info` and the host name the login is filed under. |
| `… the token server … answered 401` or `403 DENIED` | The login was refused, or it cannot see this repository. |
| `pulling the manifest of …: 404 MANIFEST_UNKNOWN` or `NAME_UNKNOWN` | The repository or tag is wrong, or your login cannot see it. |
| `… does not match its digest: the image is corrupt or has been altered` | A blob did not hash to its digest, so the image was not stored. Try again; if it repeats, something between you and the registry is changing the data. |

More in [Troubleshooting](../troubleshooting.md#registry-errors).
