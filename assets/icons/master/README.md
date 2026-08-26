# Icon master files

Source PNGs for `scripts/build-icons.sh`. Drop your masters here, run the script,
commit the regenerated outputs under `assets/PaneFlow.icns` and
`src-app/assets/icons/paneflow.png`.

This fork is macOS only. The script does not write Linux hicolor PNGs, a
Windows `.ico`, or anything under `packaging/wix/`.

| File | Required | Used for |
|---|---|---|
| `paneflow-icon-macos-1024.png` | yes | Plated macOS artwork. The legacy ICNS fallback applies the Apple-style inset and rounded mask only to this source. Also downscaled to the GPUI runtime icon. |
| `paneflow-icon-template-1024.png` | no | macOS menubar Template image. Pure black silhouette on alpha, no chrome, no fill. AppKit applies the system tint at runtime. |

## Regenerating

The pipeline requires ImageMagick 6 or 7 (for the plated rounded mask) plus
`iconutil` (ships with Xcode). It validates ImageMagick before writing any
output.

```bash
bash scripts/build-icons.sh
git add assets/PaneFlow.icns src-app/assets/icons/paneflow.png
git commit -m "chore(brand): regenerate icons from master"
```

If no macOS master is present the script no-ops with a warning and keeps the
existing committed icons.

`.github/workflows/release.yml` does not run this script. Commit regenerated
icons so local `cargo build` and the release bundle pick up the new artwork
without needing ImageMagick on the runner.
