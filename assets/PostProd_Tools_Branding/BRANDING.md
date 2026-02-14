# PostProd Tools Branding

## Brand Identity

**PostProd Tools (PPT)** is an audio post-production IDE built on the GPUI framework. The branding reflects a technical, node-based aesthetic inspired by audio signal flow and circuit design.

### Icon

The app icon is a blue hexagonal frame with "PPT" lettering, overlaid with circuit-node connection lines and dots. It conveys precision engineering and connectivity — core to the tool's purpose of bridging Pro Tools automation with an intelligent IDE.

- **Source file:** `ppt_icon_final_v15_match_2.png` (640x640 JPEG)
- **Color:** Electric blue (#0A7AFF approximate) on dark background
- **Shape:** Hexagon with internal circuit-node motif

### Logo (Wordmark)

The horizontal logo pairs the hexagonal PPT icon (small) with the "PostProd Tools" wordmark in a clean sans-serif typeface. "PostProd" is rendered in white and "Tools" in gray, creating a visual hierarchy.

- **Source file:** `logo_v15_final_no_dot.png` (640x640 JPEG)

## What's Implemented

### App Icons (all platforms)

All default Zed icons have been replaced with the PPT hexagonal icon across every release channel:

| File | Size | Purpose |
|------|------|---------|
| `crates/zed/resources/app-icon.png` | 512x512 | macOS stable icon |
| `crates/zed/resources/app-icon@2x.png` | 1024x1024 | macOS stable Retina icon |
| `crates/zed/resources/app-icon-dev.png` | 512x512 | macOS dev channel |
| `crates/zed/resources/app-icon-dev@2x.png` | 1024x1024 | macOS dev channel Retina |
| `crates/zed/resources/app-icon-nightly.png` | 512x512 | macOS nightly channel |
| `crates/zed/resources/app-icon-nightly@2x.png` | 1024x1024 | macOS nightly channel Retina |
| `crates/zed/resources/app-icon-preview.png` | 512x512 | macOS preview channel |
| `crates/zed/resources/app-icon-preview@2x.png` | 1024x1024 | macOS preview channel Retina |
| `crates/zed/resources/Document.icns` | 16–1024 | macOS document type icon |
| `crates/zed/resources/windows/app-icon.ico` | 16–256 | Windows stable icon |
| `crates/zed/resources/windows/app-icon-dev.ico` | 16–256 | Windows dev channel |
| `crates/zed/resources/windows/app-icon-nightly.ico` | 16–256 | Windows nightly channel |
| `crates/zed/resources/windows/app-icon-preview.ico` | 16–256 | Windows preview channel |

### Bundle Identifiers

Already configured in `crates/zed/Cargo.toml` for all release channels:

- `com.caio-ze.protools-studio` (stable)
- `com.caio-ze.protools-studio-dev` (dev)
- `com.caio-ze.protools-studio-nightly` (nightly)
- `com.caio-ze.protools-studio-preview` (preview)

## Not Yet Implemented

| Item | Notes |
|------|-------|
| **In-app SVG logo** | `assets/images/zed_logo.svg` still uses Zed's logo. Low priority — welcome/onboarding screens are disabled in this fork. Needs an SVG source. |
| **macOS .app bundle** | Build infrastructure exists (`script/bundle-mac`) but not yet used. Will be generated when a distribution build is ready. |
| **Code signing & notarization** | Requires Apple Developer Program ($99/year). Not needed for local development. Ad-hoc signing works for small-scale distribution. |
| **Splash screen / About dialog** | Still shows Zed branding. Can be updated when in-app SVG is ready. |

## Source Files

The `OLD/` directory contains previous iterations of the branding (v2 through v14) kept for reference. The final versions are:

- `ppt_icon_final_v15_match_2.png` — App icon (current)
- `logo_v15_final_no_dot.png` — Horizontal wordmark logo (current)
