# Regenerating this icon set

The whole set here is generated from the committed source art in `src/`
beside this file (`src/orrerix-icon.png`) via:

```sh
npx tauri icon icons/src/orrerix-icon.png
```

run from `src-tauri/`. This does not invoke `rustc` — safe to run locally.

(#3315 moved that art out of the repo root, where nothing but this README
named it, into `src/` here. It is BUILD SOURCE, not a docs asset: it is the
input the set below is generated from, so it belongs beside the set. The
`src/` subdirectory keeps the 1.1 MB master visibly apart from the generated
set; `tauri.conf.json` names its five bundle icons individually rather than by
glob, so nothing here is picked up by path shape either way.)

## Regen overwrites the hand-hinted 16px and 24px entries — re-splice them

`icon.ico`'s 16×16 and 24×24 entries are **not** generator output. The
generic downscale of the full painterly mark reads as an unrecognizable
amber blob at those sizes (the crescent, stars and gradients don't survive),
so those two entries were replaced with hand-authored, pixel-hinted
16×16/24×24 art — nothing else in `icon.ico`, or any other file in this
directory, is hand-edited.

**A plain `tauri icon` regen silently discards that fix**, because it writes
every entry from the single source PNG, hand-hinted ones included, without
warning. After regenerating, re-splice the two small entries back in from
the committed sources:

- `src/orrerix-icon-16.png`
- `src/orrerix-icon-24.png`

There is no committed splice script; the two PNGs need to land back in
`icon.ico`'s 16×16/24×24 directory entries specifically (every other entry
— 32/48/64/256 — is fine to take from a fresh regen as-is). Verify the
result by decoding the untouched entries against the pre-regen file
(they should be identical unless the source art changed) and confirming
each entry's embedded PNG header reports its directory-claimed size.

If the source mark changes enough that the hand-hinted 16/24 art no longer
matches it, both small PNGs need a fresh pass, not just a re-splice — the
existing art is tied to this mark's specific silhouette and palette.
