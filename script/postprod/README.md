# PostProd Tools Scripts

Custom scripts for the PostProd IDE fork. Upstream Zed scripts remain in the parent `script/` directory and are not modified (Rule 1: Sacred Upstream).

## Our scripts

| Script | Purpose |
|--------|---------|
| `dev-deploy` | Build release binary (`--run` to launch) |
| `dev-clean` | Launch with `~/PostProd_IDE_clean/` sandbox |
| `package-postprod` | Create distributable `.tar.gz` archive (builds into `target/`, never touches `~/PostProd_IDE/`) |
| `test-versioning` | Test versioning & packaging pipeline |

## Upstream Zed scripts we use

| Script | What it does | Why it matters |
|--------|-------------|----------------|
| `../clippy` | Runs `cargo clippy --release --all-targets --all-features -- --deny warnings`, then `cargo machete` (unused deps), then `typos` (spell check) | Three tools in one. Use `./script/clippy` instead of bare `cargo clippy`. Supports `-p crate_name` for single-crate checks. |
| `../generate-licenses` | Scans all ~220 crates with `cargo-about`, generates `assets/licenses.md` | Useful before releases. The `licenses/zed-licenses.toml` config defines accepted licenses (MIT, Apache-2.0, BSD, MPL, etc.) and **fails on unexpected licenses** like AGPL. Good for auditing the dependency tree before shipping to clients. |

## Upstream Zed scripts — not relevant to us

| Script | What it does | Why we don't use it |
|--------|-------------|---------------------|
| `../bootstrap` | Sets up Zed's collab server (minio, sqlx, foreman) | We don't run the collab server |
| `../bundle-mac` | macOS `.app` bundle + code signing + notarization + DMG | Uses Zed Industries signing identity. Reference material if we ever ship a `.app` bundle |
| `../uninstall.sh` | Uninstalls Zed from user machines | Wrong product |
| `../lib/blob-store.sh` | DigitalOcean Spaces upload for Zed releases | Their cloud infra |
| `../lib/deploy-helpers.sh` | Kubernetes helpers for Zed's collab server | Their server infra |
| `../lib/bump-version.sh` | Zed's crate version bumping (`cargo-edit`) | We use git tags, not crate versions |
| `../terms/` | Zed's terms of service (RTF/JSON) | Their legal docs |
