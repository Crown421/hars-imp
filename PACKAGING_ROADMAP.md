# COPR/RPM Packaging Roadmap

Goal: make `hars-imp` and `idle-inhibitor` installable as Fedora-native RPMs from one COPR project, then consume them from the BlueBuild image. Keep chezmoi focused on user config and service enablement.

## Target Layout

Use one COPR project with one package per app:

```text
COPR project: <user>/tools

Package: hars-imp
  Source repo: https://github.com/<user>/hars-imp
  Spec file: hars-imp.spec

Package: idle-inhibitor
  Source repo: https://github.com/<user>/idle-inhibitor
  Spec file: idle-inhibitor.spec
```

No third packaging repo is needed. Put each spec and service file next to the app it packages.

Recommended repo layout for this project:

```text
hars-imp.spec
packaging/
  hars-imp.service
  config.example.toml
```

The RPM should install:

```text
/usr/bin/hars-imp
/usr/lib/systemd/user/hars-imp.service
/usr/share/doc/hars-imp/config.example.toml
/usr/share/licenses/hars-imp/LICENSE
```

## Fedora Devcontainer

A Fedora-based devcontainer is available at:

```text
.devcontainer/fedora/devcontainer.json
```

Use it when iterating on RPM packaging. The existing Debian devcontainer is still available for normal Rust development.

There is no official `mcr.microsoft.com/devcontainers/base` Fedora variant; the official base image set covers Alpine, Debian, and Ubuntu. The Fedora container therefore starts from `fedora:${FEDORA_VERSION}` and recreates the useful parts of the Debian container with Fedora packages:

- common shell/development utilities, zsh, sudo, Git, ripgrep
- `mise`, installed from the upstream `jdxcode/mise` COPR as recommended by mise for Fedora/RHEL
- `pre-commit` and `chezmoi` from Fedora packages
- Rust/project tools in a separate Dockerfile layer
- RPM/COPR packaging tools in a separate Dockerfile layer

Useful commands inside the Fedora container:

```sh
cargo test
cargo clippy --all-targets --all-features
rpmdev-setuptree
rpmlint hars-imp.spec
copr-cli --help
rust2rpm --help
```

`mock` may require extra container privileges depending on the host/devcontainer runtime. Treat COPR as the authoritative clean Fedora builder if local `mock` is awkward.

## Phase 1: Create COPR Project Manually

1. Log into https://copr.fedorainfracloud.org/.
2. Create a new project, for example `<user>/tools`.
3. Enable chroots that match your BlueBuild base and near-future target, for example:
   - current Fedora stable x86_64
   - current Fedora stable+1 x86_64 if available
   - Rawhide only if you want early breakage detection
4. Add a short project description: personal Fedora packages for BlueBuild images.

Install and configure the CLI locally only after the project exists:

```sh
sudo dnf install copr-cli
```

Then copy the API token from the COPR website into:

```text
~/.config/copr
```

## Phase 2: Check Whether Fedora Has The Rust Dependencies

Start by exploring the non-vendored path. If it works, it is cleaner. If it gets noisy, use vendoring first and revisit later.

Inside the Fedora devcontainer:

```sh
rust2rpm --help
```

Generate a first-pass spec or inspect what `rust2rpm` would produce. Then compare dependencies from:

```sh
cargo tree
```

against Fedora packages with queries like:

```sh
dnf repoquery 'rust-*-devel' | rg rumqttc
dnf repoquery 'rust-*-devel' | rg zbus
dnf repoquery 'rust-*-devel' | rg sysinfo
```

Decision rule:

- If all direct and transitive Rust crates are available in Fedora for the target chroots, try a Fedora-style non-vendored spec.
- If crate coverage is incomplete or version mismatches dominate the work, use vendored dependencies for the first COPR release.

For personal COPR packages, vendoring is acceptable as a pragmatic first step.

## Phase 3: Add Vendored Packaging

The repo now has a starter binary-RPM path using `cargo-generate-rpm`, similar to `guayusa-wl`. This is intentionally simpler than COPR/SRPM packaging:

```sh
cargo build --release --target x86_64-unknown-linux-gnu
cargo generate-rpm
```

The GitHub release workflow uploads both the raw binary and `target/generate-rpm/*.rpm` for tagged releases. Use this as the first installable RPM format while the proper COPR package is still being developed.

The fuller COPR/SRPM path is still:

Create:

```text
hars-imp.spec
packaging/hars-imp.service
packaging/config.example.toml
```

The spec should use:

```spec
Source0: https://github.com/<user>/hars-imp/archive/refs/tags/v%{version}.tar.gz
Source1: hars-imp-%{version}-vendor.tar.gz
```

Generate the vendor source from the checked-in `Cargo.lock`:

```sh
mkdir -p .cargo
cargo vendor vendor > .cargo/config.toml
tar -czf hars-imp-0.2.0-vendor.tar.gz vendor .cargo
```

Build an SRPM:

```sh
rpmdev-setuptree
spectool -g -R hars-imp.spec
cp hars-imp-0.2.0-vendor.tar.gz ~/rpmbuild/SOURCES/
rpmbuild -bs hars-imp.spec
```

Submit the SRPM to COPR:

```sh
copr-cli build <user>/tools ~/rpmbuild/SRPMS/hars-imp-0.2.0-1*.src.rpm
```

Repeat the same shape in the `idle-inhibitor` repo.

## Phase 4: Add COPR Packages

After a manual SRPM build succeeds, create two packages in the COPR web UI:

```text
hars-imp
idle-inhibitor
```

Initial source type can remain manual/SRPM upload. Once the spec stabilizes, switch each package to an SCM source pointing at its own GitHub repo.

Recommended progression:

1. Manual SRPM upload until the spec builds.
2. GitHub Actions builds SRPM and submits it to COPR on tag.
3. Optional later: COPR SCM package builds directly from the tag/branch.

The GitHub Actions path is usually easier when vendored dependency tarballs are involved.

## Phase 5: Consume From BlueBuild

Once both packages are available in COPR, update the BlueBuild recipe to enable the COPR repo and install packages with DNF.

Conceptually:

```yaml
modules:
  - type: dnf
    repos:
      copr:
        - <user>/tools
    install:
      packages:
        - hars-imp
        - idle-inhibitor
```

Keep runtime config in chezmoi:

```text
~/.config/hars-imp/config.toml
~/.config/systemd/user/default.target.wants/hars-imp.service
```

The RPM owns the binary and user service unit. Chezmoi owns user-specific config and whether the service is enabled.

## Phase 6: Release Flow

For each release:

```sh
cargo test
cargo clippy --all-targets --all-features
cargo build --release
```

Then:

```sh
git tag v0.2.0
git push origin v0.2.0
```

Build and submit the SRPM:

```sh
cargo vendor vendor > .cargo/config.toml
tar -czf hars-imp-0.2.0-vendor.tar.gz vendor .cargo
rpmbuild -bs hars-imp.spec
copr-cli build <user>/tools ~/rpmbuild/SRPMS/hars-imp-0.2.0-1*.src.rpm
```

After COPR succeeds, rebuild the BlueBuild image.

## Later Improvements

- Automate SRPM creation and COPR submission from GitHub Actions.
- Add checksum/signature checks for vendor tarballs.
- Convert from vendored dependencies to Fedora-packaged Rust crates if dependency coverage is good enough.
- Add `rpmlint` to CI.
- Add a smoke test that installs the built RPM in a Fedora container and runs `hars-imp --version` once the app has a CLI version flag.
