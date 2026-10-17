# protoHDMIcapture brand assets

The logo, the app icon and the social preview (#35). These assets were made
with an AI tool.

## Files

| File | What it is | Used by |
|---|---|---|
| `logo-lockup-for-dark-bg.svg` | The logo for a dark background (774×160, text as shapes) | the README header |
| `logo-lockup-for-light-bg.svg` | The same for a light background | the README header |
| `social-preview.png` | 1280×640, what GitHub shows when the repo's link is shared | the repo's Settings → Social preview (uploaded by hand) |
| `../../packaging/linux/icons/hicolor/` | The app icon, 16–512 px and a scalable SVG, in the freedesktop layout | `packaging/` (installed to `share/icons/hicolor/`); `Icon=protohdmicapture` in the `.desktop` files |
| `../../packaging/windows/protohdmicapture.ico` | The app icon, 16–256 px | the Windows exe's icon resource (`viewer/build.rs`) |

The Linux and Windows icons are the same tile.

## README header

GitHub shows the logo that suits its theme:

```html
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/logo-lockup-for-dark-bg.svg">
    <img alt="protoHDMIcapture" src="assets/brand/logo-lockup-for-light-bg.svg" width="480">
  </picture>
</p>
```

## Colours

| Role | Colour |
|---|---|
| Accent | #3FBF6A |
| Ink, on dark / on light | #ECEAE4 / #17181A |
| "proto", on dark / on light | #9A978F / #6B6962 |
| Tile | #1D1F22 |
