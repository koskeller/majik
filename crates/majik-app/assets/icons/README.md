# Icons

Generated — do not edit by hand. These are [HugeIcons](https://hugeicons.com) from the free
`@hugeicons/core-free-icons` package (stroke-rounded style, MIT), the set `../flarly` uses, written
out as 24×24 stroke SVGs at stroke width 2 by `packaging/icons.mjs` from the name → export map in
`packaging/icons.json`. To add or change an icon, edit the manifest and run

    node packaging/icons.mjs

The five window glyphs gpui-component's title bar draws on Windows/Linux — `window-close`,
`window-minimize`, `window-maximize`, `window-restore`, `resize-corner` — are not in the manifest
and are kept as they are. Tests in `assets.rs` check that the manifest, the files and every icon
name the code references still agree.
